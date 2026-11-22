//! Wave 7 integration tests: finance.
//!
//! Online donation intents (validation, idempotency replay, gateway
//! config gating), ZarinPal/IDPay callbacks, webhook dedupe and
//! validation, donation methods, manual donations (submit → approve /
//! reject, receipt scan gate, proof image), campaigns (public + admin
//! CRUD), admin donation/ledger lists with cursor pagination,
//! reconciliation, gateway statuses, FX rates, recurring donations,
//! and the sidecar receipt route (503 `receipt_pdf_unavailable` with
//! `SIDECAR_URL` unset).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB, no live gateway. The Postgres
//! smoke test at the end (`wave7_finance_pg_smoke`, gated on
//! `WAVE2_PG_URL`) replays donate → manual-donation → approve against
//! a local Postgres test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::Utc;
use maturidi_api_rs::{
    build_router, encrypt_value, init_test_schema, AppState, Config, Db, IntoArg, TEST_SCHEMA,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave7-finance-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave7".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave7-finance-storage-{}",
            std::env::temp_dir().display(),
            std::process::id()
        ),
        ..Default::default()
    }
}

struct Env {
    state: AppState,
    db: Db,
}

impl Env {
    async fn sqlite() -> Self {
        let db = Db::connect("sqlite://:memory:")
            .await
            .expect("sqlite connect");
        init_test_schema(&db).await.expect("test schema");
        let state = AppState::with_db(test_config(), Some(db.clone()));
        Self { state, db }
    }

    fn router(&self) -> Router {
        build_router(self.state.clone())
    }

    async fn call(&self, req: Request<Body>) -> (StatusCode, Value) {
        let res = self.router().oneshot(req).await.expect("oneshot");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
        (status, value)
    }

    async fn call_raw(&self, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let res = self.router().oneshot(req).await.expect("oneshot");
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, headers, bytes.to_vec())
    }
}

fn req(method: Method, uri: &str, body: Option<Value>) -> Request<Body> {
    let builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))));
    match body {
        Some(v) => builder
            .body(Body::from(serde_json::to_string(&v).unwrap()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    }
}

fn req_raw(method: Method, uri: &str, body: Vec<u8>, content_type: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", content_type)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(body))
        .unwrap()
}

fn req_idem(method: Method, uri: &str, body: Option<Value>, idem: &str) -> Request<Body> {
    let mut r = req(method, uri, body);
    r.headers_mut()
        .insert("idempotency-key", idem.parse().unwrap());
    r
}

fn authed(request: Request<Body>, token: &str) -> Request<Body> {
    let mut request = request;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn err_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<no error code>")
}

fn as_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).expect("valid uuid")
}

/// Register (email + password) then log in. Returns (user_id, access, refresh).
async fn register_user(env: &Env, email: &str) -> (String, String, String) {
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "full_name": "Test User",
                "email": email,
                "password": "correct-horse-battery",
                "accept_terms": true,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "register {email}: {body}");
    let user_id = body["id"].as_str().expect("register id").to_string();
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": email, "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "login {email}: {body}");
    assert_eq!(body["must_enroll_2fa"], json!(false));
    (
        user_id,
        body["access_token"].as_str().unwrap().to_string(),
        body["refresh_token"].as_str().unwrap().to_string(),
    )
}

/// Deterministic permission id from the key (SHA-256, UUIDv5 layout),
/// so the same key can be granted to several users in one test without
/// violating the UNIQUE constraint on `permissions.key`.
fn perm_id_for(key: &str) -> Uuid {
    let mut h = Sha256::digest(key.as_bytes());
    h[6] = (h[6] & 0x0f) | 0x50;
    h[8] = (h[8] & 0x3f) | 0x80;
    Uuid::from_bytes(h[..16].try_into().unwrap())
}

/// Grant permission keys to a user through one fresh `editor` role
/// (non-privileged, so no TOTP enrolment is needed).
async fn grant(env: &Env, user_id: &str, keys: &[&str]) {
    let now = Utc::now();
    let role_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO roles (id, name, created_at) VALUES (?, ?, ?)",
            &[
                role_id.into_arg(),
                format!("editor-{role_id}").into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    for key in keys {
        let perm_id = perm_id_for(key);
        let existing = env
            .db
            .fetch_optional(
                "SELECT id FROM permissions WHERE key = ?",
                &[(*key).into_arg()],
            )
            .await
            .unwrap();
        if existing.is_none() {
            env.db
                .execute(
                    "INSERT INTO permissions (id, key, created_at) VALUES (?, ?, ?)",
                    &[perm_id.into_arg(), (*key).into_arg(), now.into_arg()],
                )
                .await
                .unwrap();
        }
        env.db
            .execute(
                "INSERT INTO role_permissions (role_id, permission_id) VALUES (?, ?)",
                &[role_id.into_arg(), perm_id.into_arg()],
            )
            .await
            .unwrap();
    }
    env.db
        .execute(
            "INSERT INTO user_roles (user_id, role_id) VALUES (?, ?)",
            &[as_uuid(user_id).into_arg(), role_id.into_arg()],
        )
        .await
        .unwrap();
}

/// Register a user with `finance.manage`.
async fn admin_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["finance.manage"]).await;
    (uid, token)
}

// ============================ seeds =====================================

async fn seed_setting(env: &Env, key: &str, plaintext: &str) {
    env.db
        .execute(
            "INSERT INTO platform_settings (key, value, encryption_key_version) VALUES (?, ?, 1) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            &[
                (key).into_arg(),
                encrypt_value(&env.state.config, plaintext).into_arg(),
            ],
        )
        .await
        .unwrap();
}

async fn seed_campaign(env: &Env, title_fa: &str, status: &str) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO donation_campaigns (id, title, description, goal_amount, raised_amount, status, cover, created_at, updated_at) VALUES (?, ?, NULL, 1000000, 0, ?, NULL, ?, NULL)",
            &[
                (id).into_arg(),
                (json!({"fa": title_fa, "ar": title_fa, "en": title_fa})).into_arg(),
                (status).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_intent(
    env: &Env,
    key: &str,
    authority: Option<&str>,
    status: &str,
    gateway: &str,
    amount: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO payment_intents (id, idempotency_key, amount, currency, campaign_id, donor_id, donor_name, status, gateway, authority, created_at, updated_at) VALUES (?, ?, ?, 'IRR', NULL, NULL, NULL, ?, ?, ?, ?, NULL)",
            &[
                (id).into_arg(),
                (key).into_arg(),
                (amount).into_arg(),
                (status).into_arg(),
                (gateway).into_arg(),
                (authority).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_donation(env: &Env, intent_id: Uuid, ref_id: &str, amount: i64) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO donations (id, donor_id, payment_intent_id, campaign_id, gateway, gateway_reference, status, amount, currency, donor_name, created_at) VALUES (?, NULL, ?, NULL, 'zarinpal', ?, 'succeeded', ?, 'IRR', NULL, ?)",
            &[
                (id).into_arg(),
                (intent_id).into_arg(),
                (ref_id).into_arg(),
                (amount).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

fn title(fa: &str) -> Value {
    json!({"fa": fa, "ar": fa, "en": fa})
}

async fn create_campaign_via_api(env: &Env, token: &str, fa: &str) -> String {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/finance/campaigns",
                Some(json!({
                    "title": title(fa),
                    "description": title("توضیح"),
                    "goal_amount": 1_000_000,
                    "status": "active",
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create campaign: {body}");
    body["id"].as_str().unwrap().to_string()
}

// ============================ multipart ===================================

const BOUNDARY: &str = "----testboundary1234";

enum Part {
    Text(String),
    File {
        filename: String,
        content_type: String,
        bytes: Vec<u8>,
    },
}

fn multipart_body(fields: &[(&str, Part)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, part) in fields {
        out.extend_from_slice(format!("--{BOUNDARY}\r\n").as_bytes());
        match part {
            Part::Text(text) => {
                out.extend_from_slice(
                    format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
                );
                out.extend_from_slice(text.as_bytes());
            }
            Part::File {
                filename,
                content_type,
                bytes,
            } => {
                out.extend_from_slice(
                    format!(
                        "Content-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {content_type}\r\n\r\n"
                    )
                    .as_bytes(),
                );
                out.extend_from_slice(bytes);
            }
        }
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(format!("--{BOUNDARY}--\r\n").as_bytes());
    out
}

fn multipart_req(method: Method, uri: &str, fields: &[(&str, Part)]) -> Request<Body> {
    let body = multipart_body(fields);
    Request::builder()
        .method(method)
        .uri(uri)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={BOUNDARY}"),
        )
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(body))
        .unwrap()
}

fn png_bytes() -> Vec<u8> {
    let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
    v.extend_from_slice(b"fake-png-receipt-bytes");
    v
}

const EICAR_BYTES: &[u8] = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";

/// Submit a card manual donation over HTTP and return (proof_id, intent_id).
async fn submit_card_manual(
    env: &Env,
    token: Option<&str>,
    amount: i64,
    proof_text: &str,
) -> (StatusCode, Value) {
    let request = multipart_req(
        Method::POST,
        "/api/v1/finance/donate/manual",
        &[
            ("method", Part::Text("card".to_string())),
            ("amount", Part::Text(amount.to_string())),
            ("proof_text", Part::Text(proof_text.to_string())),
        ],
    );
    let request = match token {
        Some(t) => authed(request, t),
        None => request,
    };
    env.call(request).await
}

async fn seed_card_method(env: &Env) {
    seed_setting(env, "donation.card_enabled", "true").await;
    seed_setting(env, "donation.card_number", "6037-9911-1234-5678").await;
    seed_setting(env, "donation.card_holder", "آکادمی ماتریدی").await;
}

// ============================ donate ======================================

#[tokio::test]
async fn donate_requires_idempotency_key() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "missing_idempotency_key");
}

#[tokio::test]
async fn donate_validation_errors() {
    let env = Env::sqlite().await;
    for (body, field) in [
        (json!({"amount": 0}), "amount"),
        (json!({"amount": -5}), "amount"),
        (json!({"amount": "abc"}), "amount"),
        (json!({"amount": 100, "currency": "USD"}), "currency"),
        (json!({"amount": 100, "gateway": "stripe"}), "gateway"),
        (
            json!({"amount": 100, "donor_name": "x".repeat(257)}),
            "donor_name",
        ),
    ] {
        let (status, body) = env
            .call(req_idem(
                Method::POST,
                "/api/v1/finance/donate",
                Some(body),
                "key-validation",
            ))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {body}");
        assert_eq!(err_code(&body), "validation_error", "{field}: {body}");
    }
    // Over the 10^11 IRR cap → 400, not 422.
    let (status, body) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000_000_001i64})),
            "key-cap",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "amount_too_large");
}

#[tokio::test]
async fn donate_unconfigured_gateway() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000})),
            "key-unconfigured",
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "payment_not_configured");
    assert_eq!(body["error"]["details"]["gateway"], json!("zarinpal"));
}

#[tokio::test]
async fn donate_online_disabled() {
    let env = Env::sqlite().await;
    seed_setting(&env, "donation.online_enabled", "false").await;
    let (status, body) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000})),
            "key-disabled",
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "donation_method_disabled");
}

#[tokio::test]
async fn donate_campaign_not_found() {
    let env = Env::sqlite().await;
    let missing = Uuid::new_v4();
    let (status, body) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000, "campaign_id": missing.to_string()})),
            "key-campaign",
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "campaign_not_found");
}

#[tokio::test]
async fn donate_replay_returns_existing_intent() {
    let env = Env::sqlite().await;
    let intent_id = seed_intent(
        &env,
        "key-replay",
        Some("AUTH-1"),
        "pending",
        "zarinpal",
        250_000,
    )
    .await;
    let (status, body) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 250_000})),
            "key-replay",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["intent_id"], json!(intent_id.to_string()));
    assert_eq!(body["authority"], json!("AUTH-1"));
    assert_eq!(
        body["pay_url"],
        json!("https://sandbox.zarinpal.com/pg/StartPay/AUTH-1")
    );
    assert_eq!(body["status"], json!("pending"));
    assert_eq!(body["amount"], json!(250_000));
    // No second intent row was created.
    let count: i64 = env
        .db
        .fetch_one("SELECT COUNT(*) AS c FROM payment_intents", &[])
        .await
        .unwrap()
        .try_get("c")
        .unwrap();
    assert_eq!(count, 1);
}

// ============================ callbacks ===================================

#[tokio::test]
async fn callback_requires_authority_and_finds_intent() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/finance/callback", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/finance/callback?Authority=NOPE&Status=OK",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "intent_not_found");
}

#[tokio::test]
async fn callback_cancel_marks_failed() {
    let env = Env::sqlite().await;
    let intent_id = seed_intent(
        &env,
        "key-cancel",
        Some("AUTH-CANCEL"),
        "pending",
        "zarinpal",
        100_000,
    )
    .await;
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/finance/callback?Authority=AUTH-CANCEL&Status=NOK",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["intent_id"], json!(intent_id.to_string()));
    assert_eq!(body["status"], json!("failed"));
    assert_eq!(body["donation_id"], Value::Null);
    let row = env
        .db
        .fetch_one(
            "SELECT status FROM payment_intents WHERE id = ?",
            &[(intent_id).into_arg()],
        )
        .await
        .unwrap();
    let status: String = row.try_get("status").unwrap();
    assert_eq!(status, "failed");
    // Replaying the same cancel is a no-op, still 200.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/finance/callback?Authority=AUTH-CANCEL&Status=NOK",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("failed"));
}

#[tokio::test]
async fn callback_succeeded_replays() {
    let env = Env::sqlite().await;
    let intent_id = seed_intent(
        &env,
        "key-done",
        Some("AUTH-DONE"),
        "succeeded",
        "zarinpal",
        100_000,
    )
    .await;
    let donation_id = seed_donation(&env, intent_id, "REF-1", 100_000).await;
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/finance/callback?Authority=AUTH-DONE&Status=OK",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("succeeded"));
    assert_eq!(body["donation_id"], json!(donation_id.to_string()));
}

#[tokio::test]
async fn idpay_callback_validation() {
    let env = Env::sqlite().await;
    // Missing `id` → 400 bad_callback_payload.
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/callback/idpay",
            b"status=1".to_vec(),
            "application/x-www-form-urlencoded",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "bad_callback_payload");
    // Unknown tx → 404.
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/callback/idpay",
            b"id=TX-9&order_id=ORD-9&status=1".to_vec(),
            "application/x-www-form-urlencoded",
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "intent_not_found");
    // A succeeded intent replays without a gateway call.
    let intent_id = seed_intent(
        &env,
        "key-idpay-done",
        Some("TX-1:ORD-1"),
        "succeeded",
        "idpay",
        50_000,
    )
    .await;
    let donation_id = seed_donation(&env, intent_id, "TRACK-1", 50_000).await;
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/callback/idpay",
            b"id=TX-1&order_id=ORD-1&status=100&track_id=TRACK-1".to_vec(),
            "application/x-www-form-urlencoded",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("succeeded"));
    assert_eq!(body["donation_id"], json!(donation_id.to_string()));
}

// ============================ webhook =====================================

#[tokio::test]
async fn webhook_validation_and_dedupe() {
    let env = Env::sqlite().await;
    // Unknown gateway → 400 (config check runs first).
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/webhook/stripe",
            b"{}".to_vec(),
            "application/json",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "unknown_gateway");

    // Unconfigured zarinpal → 503 before any payload handling
    // (valid JSON: Python parses the body before the config check,
    // so a malformed body is 400 even when unconfigured).
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/webhook/zarinpal",
            b"{}".to_vec(),
            "application/json",
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "payment_not_configured");

    seed_setting(&env, "payment.zarinpal_key", "test-merchant").await;
    // Bad JSON → 400 bad_webhook_payload.
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/webhook/zarinpal",
            b"not json".to_vec(),
            "application/json",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "bad_webhook_payload");
    // Missing authority → 400.
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/webhook/zarinpal",
            b"{}".to_vec(),
            "application/json",
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "bad_webhook_payload");

    // A processed payload replays as a duplicate with no second effect.
    let raw = b"{\"authority\": \"AUTH-1\", \"amount\": 100}".to_vec();
    let hash = Sha256::digest(&raw)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    env.db
        .execute(
            "INSERT INTO payment_webhook_log (id, gateway, payload_hash, status, processed_at) VALUES (?, 'zarinpal', ?, 'processed', ?)",
            &[
                (Uuid::new_v4()).into_arg(),
                (hash).into_arg(),
                (Utc::now()).into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req_raw(
            Method::POST,
            "/api/v1/finance/webhook/zarinpal",
            raw,
            "application/json",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["already_processed"], json!(true));
    assert_eq!(body["status"], json!("duplicate"));
}

// ============================ methods =====================================

#[tokio::test]
async fn donation_methods_empty_then_card_and_online() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/finance/donation-methods", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["methods"], json!([]));

    seed_card_method(&env).await;
    seed_setting(&env, "payment.zarinpal_key", "test-merchant").await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/finance/donation-methods", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let methods = body["methods"].as_array().unwrap();
    assert_eq!(methods.len(), 2);
    assert_eq!(methods[0]["method"], json!("online"));
    assert_eq!(methods[0]["gateways"], json!(["zarinpal"]));
    assert_eq!(methods[1]["method"], json!("card"));
    assert_eq!(methods[1]["currency"], json!("IRR"));
    assert_eq!(methods[1]["card_number"], json!("6037-9911-1234-5678"));
    assert_eq!(methods[1]["card_holder"], json!("آکادمی ماتریدی"));
}

// ============================ campaigns ===================================

#[tokio::test]
async fn campaign_crud() {
    let env = Env::sqlite().await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin@example.com").await;
    let campaign_id = create_campaign_via_api(&env, &admin_token, "کمپین تست").await;

    // Public list + get (active only).
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/finance/campaigns", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"]["fa"], json!("کمپین تست"));
    assert_eq!(body[0]["goal_amount"], json!(1_000_000));
    assert_eq!(body[0]["raised_amount"], json!(0));
    assert_eq!(body[0]["progress"], json!(0.0));
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/finance/campaigns/{campaign_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Admin update (PUT) then PATCH.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/admin/finance/campaigns/{campaign_id}"),
                Some(json!({
                    "title": title("کمپین تازه"),
                    "description": title("توضیح تازه"),
                    "goal_amount": 2_000_000,
                    "status": "active",
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["goal_amount"], json!(2_000_000));
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/finance/campaigns/{campaign_id}"),
                Some(json!({
                    "title": title("کمپین تازه"),
                    "goal_amount": 2_000_000,
                    "status": "draft",
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("draft"));

    // A draft is invisible on the public side.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/finance/campaigns/{campaign_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "campaign_not_found");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/finance/campaigns", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);

    // Admin list still sees it; delete → 204; then it is gone.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/campaigns", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (status, _body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/finance/campaigns/{campaign_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/campaigns", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn campaign_delete_blocked_by_donation() {
    let env = Env::sqlite().await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin2@example.com").await;
    let campaign_id = seed_campaign(&env, "کمپین با اهدا", "active").await;
    let intent_id = seed_intent(&env, "key-camp", None, "succeeded", "zarinpal", 10_000).await;
    env.db
        .execute(
            "UPDATE payment_intents SET campaign_id = ? WHERE id = ?",
            &[(campaign_id).into_arg(), (intent_id).into_arg()],
        )
        .await
        .unwrap();
    seed_donation(&env, intent_id, "REF-CAMP", 10_000).await;
    env.db
        .execute(
            "UPDATE donations SET campaign_id = ? WHERE payment_intent_id = ?",
            &[(campaign_id).into_arg(), (intent_id).into_arg()],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/finance/campaigns/{campaign_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "campaign_has_donations");
}

#[tokio::test]
async fn campaign_admin_requires_permission() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "plain@example.com").await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/admin/finance/campaigns", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/campaigns", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
}

// ============================ manual donations ============================

#[tokio::test]
async fn manual_donation_validation() {
    let env = Env::sqlite().await;
    seed_card_method(&env).await;
    // Unknown method → 400.
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("bank".to_string())),
                ("amount", Part::Text("1000".to_string())),
                ("proof_text", Part::Text("TRACK-1".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "unknown_method");
    // No proof at all → 422 proof_required.
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("1000".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "proof_required");
    // Non-positive amount → 400 invalid_amount.
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("0".to_string())),
                ("proof_text", Part::Text("TRACK-1".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_amount");
    // Receipt bytes that are not an image → 415.
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("1000".to_string())),
                (
                    "receipt",
                    Part::File {
                        filename: "receipt.txt".to_string(),
                        content_type: "text/plain".to_string(),
                        bytes: b"this is not an image".to_vec(),
                    },
                ),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
    assert_eq!(err_code(&body), "unsupported_receipt_image");
    // EICAR bytes inside a valid PNG → 422 file_infected (scan gate,
    // ADR-051). Bare EICAR bytes would fail the image sniff first (415).
    let mut eicar_png = png_bytes();
    eicar_png.extend_from_slice(EICAR_BYTES);
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("1000".to_string())),
                (
                    "receipt",
                    Part::File {
                        filename: "receipt.png".to_string(),
                        content_type: "image/png".to_string(),
                        bytes: eicar_png,
                    },
                ),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "file_infected");
    assert_eq!(
        body["error"]["details"]["signature"],
        json!("Eicar-Test-Signature")
    );
    // Oversized receipt → 413 receipt_too_large.
    let big = vec![0u8; 5 * 1024 * 1024 + 1];
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("1000".to_string())),
                (
                    "receipt",
                    Part::File {
                        filename: "big.png".to_string(),
                        content_type: "image/png".to_string(),
                        bytes: big,
                    },
                ),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(err_code(&body), "receipt_too_large");
    assert_eq!(
        body["error"]["details"]["max_bytes"],
        json!(5 * 1024 * 1024)
    );
    // Disabled method → 403.
    let env2 = Env::sqlite().await;
    let (status, body) = env2
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("1000".to_string())),
                ("proof_text", Part::Text("TRACK-1".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "donation_method_disabled");
}

#[tokio::test]
async fn manual_donation_approve_flow() {
    let env = Env::sqlite().await;
    seed_card_method(&env).await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin3@example.com").await;
    let campaign_id = create_campaign_via_api(&env, &admin_token, "کمپین دستی").await;

    // Submit with a PNG receipt + campaign.
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("250000".to_string())),
                ("campaign_id", Part::Text(campaign_id.clone())),
                ("proof_text", Part::Text("TRACK-APPROVE-1".to_string())),
                (
                    "receipt",
                    Part::File {
                        filename: "receipt.png".to_string(),
                        content_type: "image/png".to_string(),
                        bytes: png_bytes(),
                    },
                ),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], json!("pending"));
    assert_eq!(body["method"], json!("card"));
    assert_eq!(body["amount"], json!(250_000));
    assert_eq!(body["currency"], json!("IRR"));
    let proof_id = body["id"].as_str().unwrap().to_string();
    let intent_id = body["intent_id"].as_str().unwrap().to_string();

    // Admin list shows it as pending with a proof image.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/manual-donations", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["status"], json!("pending"));
    assert_eq!(body["items"][0]["has_proof_image"], json!(true));
    assert_eq!(body["items"][0]["proof_text"], json!("TRACK-APPROVE-1"));

    // The proof image round-trips for the admin.
    let (status, headers, bytes) = env
        .call_raw(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/finance/manual-donations/{proof_id}/proof-image"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "image/png"
    );
    assert_eq!(bytes, png_bytes());

    // Approve → donation + ledger + campaign credit.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof_id}/approve"),
                Some(json!({"note": "تأیید شد"})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("approved"));
    assert_eq!(body["amount_irr"], json!(250_000));
    assert_eq!(body["fx_rate_irr"], Value::Null);
    assert_eq!(body["fx_source"], Value::Null);
    assert_eq!(body["review_note"], json!("تأیید شد"));

    let donation = env
        .db
        .fetch_one(
            "SELECT id, gateway_reference, status FROM donations WHERE payment_intent_id = ?",
            &[(as_uuid(&intent_id)).into_arg()],
        )
        .await
        .unwrap();
    let gateway_reference: String = donation.try_get("gateway_reference").unwrap();
    assert_eq!(gateway_reference, "TRACK-APPROVE-1");
    let dstatus: String = donation.try_get("status").unwrap();
    assert_eq!(dstatus, "succeeded");
    let donation_id: Uuid = donation.try_get("id").unwrap();
    let ledger = env
        .db
        .fetch_one(
            "SELECT entry_type, amount, reference FROM ledger_entries WHERE donation_id = ?",
            &[(donation_id).into_arg()],
        )
        .await
        .unwrap();
    let entry_type: String = ledger.try_get("entry_type").unwrap();
    assert_eq!(entry_type, "donation_in");
    let lamount: i64 = ledger.try_get("amount").unwrap();
    assert_eq!(lamount, 250_000);
    let reference: String = ledger.try_get("reference").unwrap();
    assert_eq!(reference, format!("donation:{donation_id}"));

    // Campaign raised amount grew by the IRR credit.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/finance/campaigns/{campaign_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["raised_amount"], json!(250_000));
    assert_eq!(body["progress"], json!(0.25));

    // A second review is a 409.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof_id}/approve"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "already_reviewed");
}

#[tokio::test]
async fn manual_donation_reject_flow() {
    let env = Env::sqlite().await;
    seed_card_method(&env).await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin4@example.com").await;
    let (status, body) = submit_card_manual(&env, None, 75_000, "TRACK-REJECT-1").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let proof_id = body["id"].as_str().unwrap().to_string();
    let intent_id = body["intent_id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof_id}/reject"),
                Some(json!({"note": "رسید نامعتبر"})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("rejected"));
    // No financial records; the intent is cancelled.
    let count: i64 = env
        .db
        .fetch_one("SELECT COUNT(*) AS c FROM donations", &[])
        .await
        .unwrap()
        .try_get("c")
        .unwrap();
    assert_eq!(count, 0);
    let row = env
        .db
        .fetch_one(
            "SELECT status FROM payment_intents WHERE id = ?",
            &[(as_uuid(&intent_id)).into_arg()],
        )
        .await
        .unwrap();
    let status: String = row.try_get("status").unwrap();
    assert_eq!(status, "cancelled");
}

#[tokio::test]
async fn manual_donation_duplicate_reference() {
    let env = Env::sqlite().await;
    seed_card_method(&env).await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin5@example.com").await;
    let (status, body) = submit_card_manual(&env, None, 10_000, "SAME-HASH").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let proof1 = body["id"].as_str().unwrap().to_string();
    let (status, body) = submit_card_manual(&env, None, 10_000, "SAME-HASH").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let proof2 = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof1}/approve"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    // The same tracking text for a second donation → 409, still pending.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof2}/approve"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "duplicate_reference");
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/finance/manual-donations?status=pending",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], json!(proof2));
}

#[tokio::test]
async fn manual_crypto_approve_with_manual_rate() {
    let env = Env::sqlite().await;
    seed_setting(&env, "donation.crypto_enabled", "true").await;
    seed_setting(
        &env,
        "donation.crypto_wallets",
        r#"[{"currency": "USDT", "network": "TRC20", "address": "TXabc123"}]"#,
    )
    .await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin6@example.com").await;
    let campaign_id = create_campaign_via_api(&env, &admin_token, "کمپین کریپتو").await;
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("crypto".to_string())),
                ("amount", Part::Text("100".to_string())),
                ("campaign_id", Part::Text(campaign_id.clone())),
                ("wallet_currency", Part::Text("usdt".to_string())),
                ("wallet_network", Part::Text("trc20".to_string())),
                ("proof_text", Part::Text("0xHASH-CRYPTO".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["currency"], json!("USDT"));
    let proof_id = body["id"].as_str().unwrap().to_string();
    // Approve with a manual rate: 100 USDT × 600_000 IRR.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof_id}/approve"),
                Some(json!({"rate_irr": 600_000})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("approved"));
    assert_eq!(body["amount_irr"], json!(60_000_000));
    assert_eq!(body["fx_rate_irr"], json!(600_000));
    assert_eq!(body["fx_source"], json!("manual"));
    assert_eq!(body["wallet_label"], json!("USDT · TRC20"));
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/finance/campaigns/{campaign_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["raised_amount"], json!(60_000_000));

    // An unavailable wallet → 400 wallet_not_available.
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("crypto".to_string())),
                ("amount", Part::Text("5".to_string())),
                ("wallet_currency", Part::Text("BTC".to_string())),
                ("wallet_network", Part::Text("".to_string())),
                ("proof_text", Part::Text("0xHASH-BTC".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "wallet_not_available");
}

// ============================ recurring ===================================

#[tokio::test]
async fn recurring_create() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "donor@example.com").await;
    // Auth is required.
    let (status, _) = env
        .call(req(
            Method::POST,
            "/api/v1/finance/recurring",
            Some(json!({"amount": 50_000})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/finance/recurring",
                Some(json!({"amount": 50_000, "interval": "yearly"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["status"], json!("active"));
    assert_eq!(body["interval"], json!("yearly"));
    assert_eq!(body["next_charge_at"], Value::Null);
    // Bad interval → 422; unknown campaign → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/finance/recurring",
                Some(json!({"amount": 50_000, "interval": "daily"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/finance/recurring",
                Some(json!({"amount": 50_000, "campaign_id": Uuid::new_v4().to_string()})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "campaign_not_found");
}

// ============================ admin lists / misc ==========================

#[tokio::test]
async fn admin_donations_and_ledger_pagination() {
    let env = Env::sqlite().await;
    seed_card_method(&env).await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin7@example.com").await;
    // Two approved manual donations → two donations + two ledger rows.
    for (amount, proof) in [(10_000, "PAGE-1"), (20_000, "PAGE-2")] {
        let (status, body) = submit_card_manual(&env, None, amount, proof).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let proof_id = body["id"].as_str().unwrap().to_string();
        let (status, _) = env
            .call(authed(
                req(
                    Method::POST,
                    &format!("/api/v1/admin/finance/manual-donations/{proof_id}/approve"),
                    None,
                ),
                &admin_token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/donations?limit=1", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let cursor = body["next_cursor"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/finance/donations?limit=1&cursor={cursor}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["next_cursor"], Value::Null);
    // Bad cursor → 400 bad_cursor; limit 0 → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/finance/donations?cursor=!!!",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "bad_cursor");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/donations?limit=0", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Ledger mirrors the donations.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/ledger", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["items"][0]["entry_type"], json!("donation_in"));
    assert_eq!(body["items"][0]["currency"], json!("IRR"));
}

#[tokio::test]
async fn reconcile_and_gateway_statuses() {
    let env = Env::sqlite().await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin8@example.com").await;
    // Statuses: nothing configured, sandbox by default.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/gateways", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["zarinpal"],
        json!({"configured": false, "sandbox": true})
    );
    assert_eq!(body["idpay"], json!({"configured": false, "sandbox": true}));
    // Reconcile with no gateway configured → 503 payment_not_configured.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/reconcile", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "payment_not_configured");
    // Unknown gateway → 400.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/finance/reconcile?gateway=stripe",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "unknown_gateway");
    // Configured, but no gateway exposes a report → 503 (never faked).
    seed_setting(&env, "payment.zarinpal_key", "test-merchant").await;
    seed_setting(&env, "payment.idpay_key", "test-idpay").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/reconcile", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "gateway_report_unavailable");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/gateways", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["zarinpal"]["configured"], json!(true));
    assert_eq!(body["idpay"]["configured"], json!(true));
}

#[tokio::test]
async fn fx_rate_fixed_invalid_and_unavailable() {
    let env = Env::sqlite().await;
    let (_uid, admin_token) = admin_user(&env, "fin-admin9@example.com").await;
    // IRR is fixed at 1, no network needed.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/finance/fx-rate?currency=irr",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["currency"], json!("IRR"));
    assert_eq!(body["rate_irr"], json!(1));
    assert_eq!(body["source"], json!("fixed"));
    // Missing currency → 422; bad currency → 400.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/fx-rate", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/finance/fx-rate?currency=U%24D",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_currency");
    // XXX has no market anywhere → 502 fx_unavailable.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/finance/fx-rate?currency=XXX",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert_eq!(err_code(&body), "fx_unavailable");
}

#[tokio::test]
async fn receipt_route_is_sidecar_not_native() {
    // FIN-01 / ADR-134: the receipt PDF needs reportlab, so the route
    // is a sidecar-contract route — with SIDECAR_URL unset it answers
    // 503 receipt_pdf_unavailable.
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "owner@example.com").await;
    let id = Uuid::new_v4();
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/finance/receipts/{id}"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "receipt_pdf_unavailable");
}

// ============================ Postgres smoke ===============================

async fn wave7_finance_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok() else {
        return;
    };
    let db = Db::connect(&url).await.expect("postgres connect");
    // Drop every table `TEST_SCHEMA` creates first (wave6 pattern).
    let mut tables = Vec::new();
    for part in TEST_SCHEMA.split("CREATE TABLE") {
        let name: String = part
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            tables.push(name);
        }
    }
    for table in tables {
        db.execute(&format!("DROP TABLE IF EXISTS {table} CASCADE"), &[])
            .await
            .unwrap();
    }
    init_test_schema(&db).await.expect("test schema");
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };

    // Donate: missing key → 422, unconfigured gateway → 503.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/finance/donate",
            Some(json!({"amount": 100_000})),
            "pg-key-1",
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "payment_not_configured");

    // Campaign → manual donation → approve, on real Postgres.
    let (_uid, admin_token) = admin_user(&env, "pg-fin-admin@example.com").await;
    seed_card_method(&env).await;
    let campaign_id = create_campaign_via_api(&env, &admin_token, "کمپین PG").await;
    let (status, body) = env
        .call(multipart_req(
            Method::POST,
            "/api/v1/finance/donate/manual",
            &[
                ("method", Part::Text("card".to_string())),
                ("amount", Part::Text("400000".to_string())),
                ("campaign_id", Part::Text(campaign_id.clone())),
                ("proof_text", Part::Text("PG-TRACK-1".to_string())),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let proof_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/finance/manual-donations/{proof_id}/approve"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("approved"));
    assert_eq!(body["amount_irr"], json!(400_000));
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/finance/campaigns/{campaign_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["raised_amount"], json!(400_000));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/finance/ledger", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["amount"], json!(400_000));
}

#[tokio::test]
async fn wave7_finance_pg_smoke_test() {
    if std::env::var("WAVE2_PG_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .is_none()
    {
        eprintln!("wave7_finance_pg_smoke: WAVE2_PG_URL unset, skipping");
        return;
    }
    wave7_finance_pg_smoke().await;
}
