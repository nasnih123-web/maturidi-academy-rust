//! Wave 7 (places + widgets) integration tests.
//!
//! Places: public list (published only, bbox + type filter), anonymous
//! submission (draft-only), public detail (published only), admin CRUD
//! + §9 workflow transitions + draft-only delete, all behind
//!   `places.manage`.
//!
//! Widgets: the API-key / embed-token gate (one scope per widget),
//! prayer-times (shared adhan engine), ayah-of-day (deterministic
//! rotation + bundled provider), latest-fatwa, zakat-mini (real Wave 6
//! engine; no approved rule → honest 503).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the
//! end (`wave7_places_widgets_pg_smoke`, gated on `WAVE2_PG_URL`)
//! replays place submission → admin transition → widgets prayer-times
//! against one local Postgres test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::{Datelike, Duration, Utc};
use maturidi_api_rs::{
    build_router, encrypt_value, init_test_schema, issue_embed_token, AppState, Config, Db,
    IntoArg, TEST_SCHEMA,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave7-places-widgets-only".to_string(),
        encryption_key: "test-encryption-key-wave7".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave7-places-widgets-storage-{}",
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

    /// Like `call`, but returns the raw body and headers (for the
    /// ayah-of-day `Cache-Control` header).
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

fn req_key(method: Method, uri: &str, key: &str) -> Request<Body> {
    let mut r = req(method, uri, None);
    r.headers_mut().insert("x-api-key", key.parse().unwrap());
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

/// Register a user with `places.manage`.
async fn admin_places_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["places.manage"]).await;
    (uid, token)
}

/// Register a user with `api_keys.manage` (to mint widget keys).
async fn admin_keys_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["api_keys.manage"]).await;
    (uid, token)
}

/// Create an API key with all four widget scopes. Returns (id, raw).
async fn create_widget_key(env: &Env, token: &str) -> (String, String) {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/api-keys",
                Some(json!({
                    "name": "widget key",
                    "scopes": ["prayer-times", "ayah-of-day", "latest-fatwa", "zakat-mini"],
                    "rate_limit_per_min": 60,
                    "quota_monthly": 10000,
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let raw = body["key"].as_str().expect("raw key").to_string();
    assert!(raw.starts_with("mk_"), "raw key prefix: {raw}");
    let id = body["id"].as_str().unwrap().to_string();
    (id, raw)
}

/// Store a platform setting (encrypted envelope, like `stage_setting`).
async fn set_setting(env: &Env, key: &str, value: &Value) {
    let envelope = encrypt_value(&test_config(), &serde_json::to_string(value).unwrap());
    env.db
        .execute(
            "INSERT INTO platform_settings (key, value, encryption_key_version) VALUES (?, ?, 1) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            &[key.into_arg(), envelope.into_arg()],
        )
        .await
        .unwrap();
}

/// Offline Quran defaults: bundled translation/tafsir, no reciter audio
/// (the built-in defaults point at AlQuran Cloud).
async fn bundled_quran_settings(env: &Env) {
    set_setting(env, "quran.translations", &json!(["fa.rowwad"])).await;
    set_setting(env, "quran.tafsirs", &json!(["fa.mokhtasar"])).await;
    set_setting(env, "quran.reciters", &json!([])).await;
}

/// SYNTHETIC TEST FIXTURE — not fiqh: zakat rate 0.1, gold nisab 100 g.
fn synthetic_zakat_rule() -> Value {
    json!({
        "note": "SYNTHETIC TEST FIXTURE — not fiqh",
        "zakat_rate": "0.1",
        "nisab_basis": "gold",
        "nisab_gold_grams": "100",
        "nisab_silver_grams": "700",
        "asset_categories": {
            "cash": {"type": "value"},
            "trade_goods": {"type": "value"},
            "gold_grams": {"type": "grams", "price": "gold_per_gram"},
        },
    })
}

/// Seed an approved zakat rule (madhab NULL, in-window) directly.
async fn seed_zakat_rule(env: &Env) {
    let now = Utc::now();
    let rule = synthetic_zakat_rule();
    env.db
        .execute(
            "INSERT INTO zakat_rule_configs (id, madhab, source, version, effective_from, \
             status, rule_data, asset_categories, created_at) \
             VALUES (?, ?, 'test', 1, ?, 'approved', ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                (Option::<String>::None).into_arg(),
                (now - Duration::days(365)).into_arg(),
                rule.clone().into_arg(),
                rule["asset_categories"].clone().into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
}

/// Seed a fresh `nisab_price_cache` row (source "default") so zakat
/// calculations have gold/silver prices without a live fetch.
async fn seed_price_cache(env: &Env, gold: &str, silver: &str) {
    let fetched_at = Utc::now();
    env.db
        .execute(
            "INSERT INTO nisab_price_cache (id, source, currency, gold_per_gram, \
             silver_per_gram, fetched_at) VALUES (?, 'default', 'TST', \
             CAST(? AS NUMERIC(20,6)), CAST(? AS NUMERIC(20,6)), ?)",
            &[
                Uuid::new_v4().into_arg(),
                gold.to_string().into_arg(),
                silver.to_string().into_arg(),
                fetched_at.into_arg(),
            ],
        )
        .await
        .unwrap();
}

fn place_body(lat: f64, lng: f64) -> Value {
    json!({
        "name": {"fa": "مسجد تست", "ar": "مسجد اختبار", "en": "Test Mosque"},
        "description": {"fa": "توضیح", "ar": "وصف", "en": "Description"},
        "lat": lat,
        "lng": lng,
        "type": "mosque",
        "city": "Torbat-e Jam",
    })
}

async fn create_place_via_api(env: &Env, token: &str, lat: f64, lng: f64) -> String {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/places",
                Some(place_body(lat, lng)),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
    assert_eq!(body["version"], json!(1));
    body["id"].as_str().unwrap().to_string()
}

async fn transition_via_api(env: &Env, token: &str, id: &str, target: &str) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            &format!("/api/v1/admin/places/{id}/transition"),
            Some(json!({"target": target})),
        ),
        token,
    ))
    .await
}

async fn publish_via_api(env: &Env, token: &str, id: &str) {
    for to in ["in_review", "approved", "published"] {
        let (status, body) = transition_via_api(env, token, id, to).await;
        assert_eq!(status, StatusCode::OK, "transition to {to}: {body}");
    }
}

// ============================ places: public ============================

#[tokio::test]
async fn public_list_empty_and_bbox_filters() {
    let env = Env::sqlite().await;
    let (status, body) = env.call(req(Method::GET, "/api/v1/places", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));
    assert_eq!(body["items"], json!([]));

    // limit validation: 0 and 501 are 422 (FastAPI ge=1, le=500).
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/places?limit=0", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/places?limit=501", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Bad bbox value is a 422 too.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/places?min_lat=abc", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn submit_place_is_draft_only() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/places/submissions",
            Some(place_body(35.24, 60.62)),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
    assert_eq!(body["version"], json!(1));
    assert_eq!(body["name"]["fa"], json!("مسجد تست"));
    assert_eq!(body["city"], json!("Torbat-e Jam"));
    let id = body["id"].as_str().unwrap().to_string();

    // A draft never reaches the public map or detail page.
    let (status, body) = env.call(req(Method::GET, "/api/v1/places", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));
    let (status, body) = env
        .call(req(Method::GET, &format!("/api/v1/places/{id}"), None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "place_not_found");

    // Unknown type → 422; bad latitude → 422; missing name → 422.
    let mut bad = place_body(35.24, 60.62);
    bad["type"] = json!("nope");
    let (status, body) = env
        .call(req(Method::POST, "/api/v1/places/submissions", Some(bad)))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_place_type");
    let mut bad = place_body(95.0, 60.62);
    bad["type"] = json!("mosque");
    let (status, body) = env
        .call(req(Method::POST, "/api/v1/places/submissions", Some(bad)))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/places/submissions",
            Some(json!({"lat": 35.0, "lng": 60.0})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Malformed UUID in the path → 422 (FastAPI UUID param).
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/places/not-a-uuid", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn public_list_bbox_and_type_filters() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_places_user(&env, "admin@example.com").await;
    let id_tehran = create_place_via_api(&env, &admin, 35.72, 51.33).await;
    let id_mashhad = create_place_via_api(&env, &admin, 36.29, 59.61).await;
    publish_via_api(&env, &admin, &id_tehran).await;
    publish_via_api(&env, &admin, &id_mashhad).await;

    let (status, body) = env.call(req(Method::GET, "/api/v1/places", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(2));

    // Bounding box around Tehran only.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/places?min_lat=35&max_lat=36&min_lng=51&max_lng=52",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(id_tehran));

    // Type filter with no match.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/places?type=museum", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));

    // Public detail works once published.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/places/{id_mashhad}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["lat"], json!(36.29));
}

// ============================ places: admin ============================

#[tokio::test]
async fn admin_create_requires_permission() {
    let env = Env::sqlite().await;
    // Guest → 401.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/admin/places",
            Some(place_body(35.24, 60.62)),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Authed without the permission → 403.
    let (_, user_token, _) = register_user(&env, "user@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/places",
                Some(place_body(35.24, 60.62)),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (_, admin) = admin_places_user(&env, "admin@example.com").await;
    // 360 reference to a missing library item → 422 invalid_360_ref.
    let mut bad = place_body(35.24, 60.62);
    bad["video_360_id"] = json!(Uuid::new_v4().to_string());
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/places", Some(bad)),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_360_ref");

    // 360 reference to a non-360 library item → 422 as well.
    let item_id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO library_items (id, title, workflow_status, visibility, kind, \
             created_at, updated_at) VALUES (?, ?, 'draft', 'public', 'video', ?, ?)",
            &[
                item_id.into_arg(),
                json!({"fa": "v"}).into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let mut bad = place_body(35.24, 60.62);
    bad["video_360_id"] = json!(item_id.to_string());
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/places", Some(bad)),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_360_ref");

    // The same item re-typed as video_360 is accepted.
    env.db
        .execute(
            "UPDATE library_items SET kind = 'video_360' WHERE id = ?",
            &[item_id.into_arg()],
        )
        .await
        .unwrap();
    let mut ok = place_body(35.24, 60.62);
    ok["video_360_id"] = json!(item_id.to_string());
    ok["images"] = json!([{"url": "https://example.com/a.jpg", "caption": "a"}]);
    ok["video_url"] = json!("https://example.com/watch");
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/places", Some(ok)),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["video_360_id"], json!(item_id.to_string()));
    assert_eq!(body["images"][0]["url"], json!("https://example.com/a.jpg"));
    assert_eq!(body["video_url"], json!("https://example.com/watch"));

    // Partial LocalizedText: missing ar/en default to "" (Pydantic).
    let mut partial = place_body(35.24, 60.62);
    partial["name"] = json!({"fa": "فقط فارسی"});
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/places", Some(partial)),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["name"], json!({"fa": "فقط فارسی", "ar": "", "en": ""}));

    // Bad image URL scheme → 422; too many images → 422.
    let mut bad = place_body(35.24, 60.62);
    bad["images"] = json!([{"url": "javascript:alert(1)"}]);
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/places", Some(bad)),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn admin_workflow_transitions() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_places_user(&env, "admin@example.com").await;
    let id = create_place_via_api(&env, &admin, 35.24, 60.62).await;

    // No skipping: draft → published is 422 invalid_transition.
    let (status, body) = transition_via_api(&env, &admin, &id, "published").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");
    assert_eq!(body["error"]["details"]["from"], json!("draft"));
    assert_eq!(body["error"]["details"]["to"], json!("published"));
    // Unknown target → 422 unknown_status.
    let (status, body) = transition_via_api(&env, &admin, &id, "nope").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_status");
    // Missing target → 422 validation_error.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/places/{id}/transition"),
                Some(json!({})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Full chain; version bumps each step.
    for (to, version) in [("in_review", 2), ("approved", 3), ("published", 4)] {
        let (status, body) = transition_via_api(&env, &admin, &id, to).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["workflow_status"], json!(to));
        assert_eq!(body["version"], json!(version));
    }
    // Published can only go to archived.
    let (status, body) = transition_via_api(&env, &admin, &id, "draft").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");
    let (status, body) = transition_via_api(&env, &admin, &id, "archived").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Archived re-enters at draft.
    let (status, body) = transition_via_api(&env, &admin, &id, "draft").await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Unknown place → 404.
    let (status, body) =
        transition_via_api(&env, &admin, &Uuid::new_v4().to_string(), "draft").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "place_not_found");
}

#[tokio::test]
async fn admin_update_patch_semantics() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_places_user(&env, "admin@example.com").await;
    let id = create_place_via_api(&env, &admin, 35.24, 60.62).await;

    // city: null clears; lat: null is ignored; name is replaced.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/places/{id}"),
                Some(json!({
                    "city": null,
                    "lat": null,
                    "name": {"fa": "نام تازه", "ar": "", "en": ""},
                })),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["city"], json!(null));
    assert_eq!(body["lat"], json!(35.24));
    assert_eq!(body["name"]["fa"], json!("نام تازه"));
    assert_eq!(body["version"], json!(2));

    // Retyping to an unknown type → 422; unknown fields are ignored.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/places/{id}"),
                Some(json!({"type": "nope", "bogus_field": 1})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_place_type");

    // Legacy type keep: a row typed `shrine` (pre-globe) stays editable
    // without retyping, but changing it to another unknown type fails.
    let legacy_id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO religious_places (id, name, lat, lng, type, workflow_status, \
             version, created_at, updated_at) VALUES (?, ?, 36.28, 59.61, 'shrine', \
             'draft', 1, ?, ?)",
            &[
                legacy_id.into_arg(),
                json!({"fa": "حرم", "ar": "", "en": ""}).into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/places/{legacy_id}"),
                Some(json!({"name": {"fa": "حرم تازه", "ar": "", "en": ""}})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["type"], json!("shrine"));
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/places/{legacy_id}"),
                Some(json!({"type": "shrine"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

#[tokio::test]
async fn admin_delete_only_draft() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_places_user(&env, "admin@example.com").await;
    let id = create_place_via_api(&env, &admin, 35.24, 60.62).await;
    let (status, _body) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/admin/places/{id}"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/admin/places/{id}"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // A published place must be archived first.
    let id2 = create_place_via_api(&env, &admin, 35.24, 60.62).await;
    publish_via_api(&env, &admin, &id2).await;
    let (status, body) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/admin/places/{id2}"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "place_not_deletable");
}

#[tokio::test]
async fn admin_list_status_filter() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_places_user(&env, "admin@example.com").await;
    let id_draft = create_place_via_api(&env, &admin, 35.24, 60.62).await;
    let id_pub = create_place_via_api(&env, &admin, 36.29, 59.61).await;
    publish_via_api(&env, &admin, &id_pub).await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/places", None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(2));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/places?status=published", None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(id_pub));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/places?status=draft", None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(id_draft));
    // Admin detail sees drafts too.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/places/{id_draft}"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
}

#[tokio::test]
async fn places_types_setting_overrides_defaults() {
    let env = Env::sqlite().await;
    // A custom `places.types` list is honoured; `mosque` drops out.
    set_setting(
        &env,
        "places.types",
        &json!([{"key": "tekke", "emoji": "📍", "fa": "تکیه", "ar": "تكية", "en": "Tekke"}]),
    )
    .await;
    let mut body = place_body(35.24, 60.62);
    body["type"] = json!("tekke");
    let (status, body) = env
        .call(req(Method::POST, "/api/v1/places/submissions", Some(body)))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let mut body = place_body(35.24, 60.62);
    body["type"] = json!("mosque");
    let (status, body) = env
        .call(req(Method::POST, "/api/v1/places/submissions", Some(body)))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_place_type");

    // A malformed stored setting falls back to the defaults.
    set_setting(&env, "places.types", &json!("not-a-list")).await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/places/submissions",
            Some(place_body(35.24, 60.62)),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

// ============================ widgets: gate ============================

#[tokio::test]
async fn widgets_gate_key_and_embed_token() {
    let env = Env::sqlite().await;
    let (_, keys_admin) = admin_keys_user(&env, "keys@example.com").await;
    let (key_id, raw) = create_widget_key(&env, &keys_admin).await;

    // No credential at all → 401 api_key_required.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35&lng=51",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "api_key_required");
    // Unknown key → 401 api_key_invalid.
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35&lng=51",
            "mk_bogus",
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "api_key_invalid");
    // A raw key in the query string deliberately does NOT authenticate.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/widgets/prayer-times?lat=35&lng=51&api_key={raw}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "api_key_required");

    // Embed token for the right widget works; for another widget it is
    // 401 (the token is bound to one widget).
    let token = issue_embed_token(&test_config(), as_uuid(&key_id), "prayer-times");
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/widgets/prayer-times?lat=35&lng=51&embed_token={token}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/widgets/ayah-of-day?embed_token={token}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "api_key_invalid");
}

#[tokio::test]
async fn widgets_scope_denied() {
    let env = Env::sqlite().await;
    let (_, keys_admin) = admin_keys_user(&env, "keys2@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/api-keys",
                Some(json!({
                    "name": "prayer only",
                    "scopes": ["prayer-times"],
                    "rate_limit_per_min": 60,
                    "quota_monthly": 10000,
                })),
            ),
            &keys_admin,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let raw = body["key"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(req_key(Method::GET, "/api/v1/widgets/ayah-of-day", &raw))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "api_key_scope_denied");
    // The in-scope widget still works.
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35&lng=51",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ============================ widgets: prayer-times ============================

#[tokio::test]
async fn widgets_prayer_times() {
    let env = Env::sqlite().await;
    let (_, keys_admin) = admin_keys_user(&env, "keys3@example.com").await;
    let (_id, raw) = create_widget_key(&env, &keys_admin).await;

    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35.72&lng=51.33",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let today = Utc::now().date_naive().format("%Y-%m-%d").to_string();
    assert_eq!(body["date"], json!(today));
    assert_eq!(body["method"], json!("mwl"));
    assert_eq!(body["madhab"], json!("shafi"));
    assert_eq!(body["tz"], json!(3.5));
    let times = body["times"].as_object().expect("times object");
    for key in [
        "imsak", "fajr", "sunrise", "dhuhr", "asr", "maghrib", "isha", "midnight",
    ] {
        let v = times.get(key).expect("time key").as_str().unwrap();
        assert_eq!(v.len(), 5, "{key}: {v}");
        assert_eq!(&v[2..3], ":");
    }
    // Hanafi asr is later than Shafi asr for the same day/place.
    let (status, shafi) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35.72&lng=51.33&madhab=shafi",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{shafi}");
    let (status, hanafi) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35.72&lng=51.33&madhab=hanafi",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{hanafi}");
    assert_ne!(shafi["times"]["asr"], hanafi["times"]["asr"]);

    // Out-of-range coordinates → 422 bad_coordinates (service check).
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=200&lng=51",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_coordinates");
    // Unknown method / madhab → 422 bad_prayer_params.
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35&lng=51&method=nope",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_prayer_params");
    // Missing lat → 422; tz out of range → 422.
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lng=51",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=35&lng=51&tz=99",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ============================ widgets: ayah-of-day ============================

const AYAH_ROTATION: &[(i64, i64)] = &[
    (2, 255),
    (13, 28),
    (94, 6),
    (2, 152),
    (39, 53),
    (55, 13),
    (3, 139),
    (65, 3),
    (94, 5),
    (20, 114),
];

#[tokio::test]
async fn widgets_ayah_of_day() {
    let env = Env::sqlite().await;
    bundled_quran_settings(&env).await;
    let (_, keys_admin) = admin_keys_user(&env, "keys4@example.com").await;
    let (_id, raw) = create_widget_key(&env, &keys_admin).await;

    let today = Utc::now().date_naive();
    let idx = (today.ordinal() % AYAH_ROTATION.len() as u32) as usize;
    let (surah, ayah) = AYAH_ROTATION[idx];

    let (status, headers, bytes) = env
        .call_raw(req_key(Method::GET, "/api/v1/widgets/ayah-of-day", &raw))
        .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(
        headers.get("cache-control").and_then(|v| v.to_str().ok()),
        Some("public, max-age=3600")
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["surah"], json!(surah));
    assert_eq!(body["ayah"], json!(ayah));
    assert_eq!(body["date"], json!(today.format("%Y-%m-%d").to_string()));
    let editions = body["bundle"]["editions"].as_array().expect("editions");
    assert!(!editions.is_empty(), "bundle editions: {body}");
    assert_eq!(body["bundle"]["surah"], json!(surah));
    assert_eq!(body["bundle"]["ayah"], json!(ayah));
    let text = editions[0]["text"].as_str().unwrap();
    assert!(!text.is_empty(), "first edition text is empty");

    // Second call the same day returns the same ayah (cache path).
    let (status, body) = env
        .call(req_key(Method::GET, "/api/v1/widgets/ayah-of-day", &raw))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["surah"], json!(surah));
    assert_eq!(body["ayah"], json!(ayah));
}

// ============================ widgets: latest-fatwa ============================

#[tokio::test]
async fn widgets_latest_fatwa() {
    let env = Env::sqlite().await;
    let (_, keys_admin) = admin_keys_user(&env, "keys5@example.com").await;
    let (_id, raw) = create_widget_key(&env, &keys_admin).await;

    let (status, body) = env
        .call(req_key(Method::GET, "/api/v1/widgets/latest-fatwa", &raw))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "no_published_fatwa");

    // A draft is invisible; the published row wins, excerpt capped.
    let now = Utc::now();
    let author = Uuid::new_v4();
    let long_body = "x".repeat(500);
    env.db
        .execute(
            "INSERT INTO fatwa_submissions (id, title, body, author_id, workflow_status, \
             version, created_at, updated_at) VALUES (?, ?, ?, ?, 'draft', 1, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                json!({"fa": "پیش‌نویس", "ar": "", "en": ""}).into_arg(),
                "draft body".into_arg(),
                author.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let pub_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO fatwa_submissions (id, title, body, author_id, workflow_status, \
             version, published_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'published', \
             1, ?, ?, ?)",
            &[
                pub_id.into_arg(),
                json!({"fa": "فتوای تازه", "ar": "", "en": ""}).into_arg(),
                long_body.clone().into_arg(),
                author.into_arg(),
                now.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req_key(Method::GET, "/api/v1/widgets/latest-fatwa", &raw))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(pub_id.to_string()));
    assert_eq!(body["title"]["fa"], json!("فتوای تازه"));
    assert_eq!(body["excerpt"].as_str().unwrap().chars().count(), 400);
    assert!(body["published_at"].is_string());
}

// ============================ widgets: zakat-mini ============================

#[tokio::test]
async fn widgets_zakat_mini() {
    let env = Env::sqlite().await;
    let (_, keys_admin) = admin_keys_user(&env, "keys6@example.com").await;
    let (_id, raw) = create_widget_key(&env, &keys_admin).await;

    // No approved rule → honest 503.
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/zakat-mini?amount=1000",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "zakat_rule_not_configured");
    // Bad / negative amounts → 422 bad_amount (before the rule check
    // for bad_amount; the rule is still missing here for the negative
    // case only if parsing passes — negative parses, so it 422s too).
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/zakat-mini?amount=abc",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_amount");
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/zakat-mini?amount=-5",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_amount");

    // With an approved rule + fresh prices: 1000 cash meets the 100 g
    // gold nisab at 10/g → nisab 1000.0000, due 100.0000 at rate 0.1.
    seed_zakat_rule(&env).await;
    seed_price_cache(&env, "10", "1").await;
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/zakat-mini?amount=1000",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["amount"], json!("1000"));
    assert_eq!(body["currency"], json!("TST"));
    assert_eq!(body["zakat_due"], json!("100.0000"));
    assert_eq!(body["nisab"], json!("1000.0000"));
    assert_eq!(body["detail"]["result"]["status"], json!("ok"));
    assert_eq!(body["detail"]["result"]["nisab_met"], json!(true));
    assert!(body["detail"]["id"].is_string());

    // Below nisab → due 0.0000, nisab still reported.
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/zakat-mini?amount=500",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["zakat_due"], json!("0.0000"));
    assert_eq!(body["nisab"], json!("1000.0000"));
    assert_eq!(body["detail"]["result"]["nisab_met"], json!(false));
}

// ============================ Postgres smoke ============================

/// Critical path on Postgres: place submission → admin transition →
/// widgets prayer-times. Gated on `WAVE2_PG_URL`.
#[tokio::test]
async fn wave7_places_widgets_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok() else {
        return;
    };
    let db = Db::connect(&url).await.expect("postgres connect");
    // Drop every table `TEST_SCHEMA` creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run would
    // fail the schema apply. The list is derived from the schema itself
    // (wave5 pattern). Binaries must run sequentially — the smokes
    // share one database.
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

    // Submit anonymously, publish as admin, see it on the public map.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/places/submissions",
            Some(place_body(21.4225, 39.8262)),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_str().unwrap().to_string();
    let (uid, token, _) = register_user(&env, "pg-admin@example.com").await;
    grant(&env, &uid, &["places.manage", "api_keys.manage"]).await;
    publish_via_api(&env, &token, &id).await;
    let (status, body) = env.call(req(Method::GET, "/api/v1/places", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(id));

    // Widgets prayer-times answers with the key on Postgres too.
    let (_key_id, raw) = create_widget_key(&env, &token).await;
    let (status, body) = env
        .call(req_key(
            Method::GET,
            "/api/v1/widgets/prayer-times?lat=21.4225&lng=39.8262",
            &raw,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["times"]["fajr"].as_str().unwrap().len(), 5);
}
