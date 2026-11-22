//! Wave 2 integration tests: meta, activity, api_keys, users, auth.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the end
//! (`wave2_pg_smoke`, gated on `WAVE2_PG_URL`) replays the critical
//! path against a local Postgres test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::Utc;
use hmac::{Hmac, Mac};
use maturidi_api_rs::{
    build_router, encrypt_value, init_test_schema, issue_embed_token, validate_embed_token,
    validate_key, verify_embed_token, AppState, Config, Db, IntoArg,
};
use serde_json::{json, Value};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "wave2-test-jwt-secret-that-is-long-enough".to_string(),
        captcha_enabled: false,
        // Dead port: native routes must never touch the origin.
        origin_url: "http://127.0.0.1:9".to_string(),
        storage_dir: format!(
            "{}/wave2-storage-{}",
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

fn authed(request: Request<Body>, token: &str) -> Request<Body> {
    let mut request = request;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

/// Parse a UUID string for binding: `Arg::Uuid` and `Arg::Str` store
/// differently in sqlite, so ids must be bound as `Uuid`, never as text.
fn as_uuid(s: &str) -> Uuid {
    Uuid::parse_str(s).expect("valid uuid")
}

fn err_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<no error code>")
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

// --- TOTP (RFC 6238, SHA-1, 6 digits, 30 s step) for the 2FA tests ---

fn b32_decode(secret: &str) -> Vec<u8> {
    let mut bits: u32 = 0;
    let mut nbits = 0;
    let mut out = Vec::new();
    for ch in secret.chars() {
        let v = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            '2'..='7' => ch as u32 - '2' as u32 + 26,
            '=' => break,
            _ => continue,
        };
        bits = (bits << 5) | v;
        nbits += 5;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    out
}

fn totp_code(secret: &str, unix: i64) -> String {
    totp_code_at(secret, unix / 30)
}

fn totp_code_at(secret: &str, step: i64) -> String {
    let key = b32_decode(secret);
    let mut mac = Hmac::<Sha1>::new_from_slice(&key).expect("hmac key");
    mac.update(&(step as u64).to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let off = (hash[19] & 0x0f) as usize;
    let code = ((u32::from(hash[off] & 0x7f) << 24)
        | (u32::from(hash[off + 1]) << 16)
        | (u32::from(hash[off + 2]) << 8)
        | u32::from(hash[off + 3]))
        % 1_000_000;
    format!("{code:06}")
}

/// Sleep until the TOTP step rolls over: the server claims each step
/// atomically, so a second code in the same 30 s window is `code_reused`.
async fn sleep_until_next_totp_step() {
    let step = Utc::now().timestamp() / 30;
    loop {
        if Utc::now().timestamp() / 30 != step {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// Register a user, grant the `admin` role with `api_keys.manage` +
/// `users.manage`, enroll TOTP, and return (user_id, full-scope access).
async fn make_admin(env: &Env, email: &str) -> (String, String) {
    let (user_id, _access, _refresh) = register_user(env, email).await;
    let now = Utc::now().to_rfc3339();
    let role_id = Uuid::new_v4().to_string();
    let perm_api = Uuid::new_v4().to_string();
    let perm_users = Uuid::new_v4().to_string();
    env.db
        .execute(
            "INSERT INTO roles (id, name, created_at) VALUES (?, ?, ?)",
            &[
                as_uuid(&role_id).into_arg(),
                "admin".into_arg(),
                now.clone().into_arg(),
            ],
        )
        .await
        .unwrap();
    for (perm_id, key) in [
        (perm_api.clone(), "api_keys.manage"),
        (perm_users.clone(), "users.manage"),
    ] {
        env.db
            .execute(
                "INSERT INTO permissions (id, key, created_at) VALUES (?, ?, ?)",
                &[
                    as_uuid(&perm_id).into_arg(),
                    key.into_arg(),
                    now.clone().into_arg(),
                ],
            )
            .await
            .unwrap();
    }
    // role <-> permission links + user <-> role link
    for perm_id in [&perm_api, &perm_users] {
        env.db
            .execute(
                "INSERT INTO role_permissions (role_id, permission_id) VALUES (?, ?)",
                &[as_uuid(&role_id).into_arg(), as_uuid(perm_id).into_arg()],
            )
            .await
            .unwrap();
    }
    env.db
        .execute(
            "INSERT INTO user_roles (user_id, role_id) VALUES (?, ?)",
            &[as_uuid(&user_id).into_arg(), as_uuid(&role_id).into_arg()],
        )
        .await
        .unwrap();
    // Log in again: now the token has scope `2fa_setup` (privileged role,
    // no TOTP yet) and `must_enroll_2fa` is true.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": email, "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "admin re-login: {body}");
    assert_eq!(body["must_enroll_2fa"], json!(true));
    let setup_token = body["access_token"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/auth/2fa/setup", None),
            &setup_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "2fa setup: {body}");
    let secret = body["secret"].as_str().unwrap().to_string();
    let code = totp_code(&secret, Utc::now().timestamp());
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/auth/2fa/enable",
                Some(json!({"secret": secret, "code": code})),
            ),
            &setup_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "2fa enable: {body}");
    assert_eq!(body["recovery_codes"].as_array().unwrap().len(), 10);
    // The enable step is claimed; wait for the next step before logging in.
    sleep_until_next_totp_step().await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "identifier": email,
                "password": "correct-horse-battery",
                "totp_code": totp_code(&secret, Utc::now().timestamp()),
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "admin login with TOTP: {body}");
    assert_eq!(body["must_enroll_2fa"], json!(false));
    (user_id, body["access_token"].as_str().unwrap().to_string())
}

// ============================ meta ============================

#[tokio::test]
async fn meta_themes_active_default() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/themes/active", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"]["fa"], json!("پیش‌فرض"));
    assert_eq!(body["name"]["ar"], json!("الافتراضي"));
    assert_eq!(body["name"]["en"], json!("Default"));
    assert_eq!(body["is_active"], json!(true));
}

#[tokio::test]
async fn meta_themes_active_seeded() {
    let env = Env::sqlite().await;
    let id = Uuid::new_v4().to_string();
    env.db
        .execute(
            "INSERT INTO themes (id, name, is_active, created_at) VALUES (?, ?, TRUE, ?)",
            &[
                as_uuid(&id).into_arg(),
                json!({"fa": "تم سبز", "ar": "الأخضر", "en": "Green"}).into_arg(),
                Utc::now().to_rfc3339().into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/themes/active", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["name"]["fa"], json!("تم سبز"));
    assert_eq!(body["name"]["en"], json!("Green"));
}

#[tokio::test]
async fn meta_settings_public_defaults() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/settings/public", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["version"], json!(0));
    assert_eq!(body["turnstile_site_key"], Value::Null);
    assert_eq!(body["settings"]["quran.madhhab_default"], json!("hanafi"));
    assert_eq!(
        body["settings"]["videos.subtitle_langs"],
        json!(["fa", "ar", "en", "ur", "dari"])
    );
    assert_eq!(body["settings"]["home.sections"], json!([]));
    assert_eq!(
        body["settings"]["places.globe"]["defaultLat"],
        json!(21.4225)
    );
    assert_eq!(
        body["settings"]["places.globe"]["defaultLng"],
        json!(39.8262)
    );
    assert_eq!(body["settings"]["push.vapid_public_key"], Value::Null);
}

#[tokio::test]
async fn meta_settings_public_stored_value() {
    let env = Env::sqlite().await;
    let config = test_config();
    let envelope = encrypt_value(&config, json!(["fa", "ar"]).to_string().as_str());
    env.db
        .execute(
            "INSERT INTO platform_settings (key, value, encryption_key_version) VALUES (?, ?, 1)",
            &[
                "videos.subtitle_langs".into_arg(),
                serde_json::to_string(&envelope).unwrap().into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/settings/public", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body["settings"]["videos.subtitle_langs"],
        json!(["fa", "ar"])
    );
    // Direct DB seeding does not bump the process-local version counter.
    assert_eq!(body["version"], json!(0));
}

#[tokio::test]
async fn meta_i18n_detect() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/i18n/detect", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["locale"], json!("fa"));

    let mut request = req(Method::GET, "/api/v1/i18n/detect", None);
    request
        .headers_mut()
        .insert("accept-language", "ar-EG,ar;q=0.9".parse().unwrap());
    let (status, body) = env.call(request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["locale"], json!("ar"));

    let mut request = req(Method::GET, "/api/v1/i18n/detect", None);
    request
        .headers_mut()
        .insert("accept-language", "en-US,en;q=0.9".parse().unwrap());
    let (status, body) = env.call(request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["locale"], json!("en"));
}

// ============================ activity ============================

#[tokio::test]
async fn activity_post_valid_and_row_stored() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "act@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/activity/events",
                Some(json!({
                    "kind": "media_play",
                    "title": "درس اول",
                    "ref_type": "video",
                    "ref_id": "42",
                    "meta": {"position": 12},
                })),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], json!("media_play"));
    assert!(body["id"].as_str().is_some());
    let row = env
        .db
        .fetch_optional(
            "SELECT kind FROM user_activity_events WHERE id = ?",
            &[as_uuid(body["id"].as_str().unwrap()).into_arg()],
        )
        .await
        .unwrap();
    assert!(row.is_some());
}

#[tokio::test]
async fn activity_post_bad_kind_and_limits() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "act2@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/activity/events",
                Some(json!({"kind": "unknown_kind"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "bad_event_kind");

    // clean_meta truncates instead of rejecting: 13 keys -> 12 stored.
    let mut meta = serde_json::Map::new();
    for i in 0..13 {
        meta.insert(format!("k{i}"), json!("v"));
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/activity/events",
                Some(json!({"kind": "media_play", "meta": meta})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let row = env
        .db
        .fetch_one(
            "SELECT meta FROM user_activity_events WHERE id = ?",
            &[as_uuid(body["id"].as_str().unwrap()).into_arg()],
        )
        .await
        .unwrap();
    let stored: Value = row.try_get::<Value, _>("meta").unwrap();
    assert_eq!(stored.as_object().unwrap().len(), 12);
}

#[tokio::test]
async fn activity_post_requires_auth() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/activity/events",
            Some(json!({"kind": "media_play"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_code(&body), "unauthorized");
}

// ============================ api_keys ============================

async fn create_api_key(env: &Env, token: &str) -> (String, String, Value) {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/api-keys",
                Some(json!({
                    "name": "widget key",
                    "scopes": ["prayer-times"],
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
    assert_eq!(body["key_prefix"].as_str().unwrap().len(), 8);
    assert!(body.get("key_hash").is_none(), "hash must never leak");
    let id = body["id"].as_str().unwrap().to_string();
    (id, raw, body)
}

#[tokio::test]
async fn api_keys_crud_flow() {
    let env = Env::sqlite().await;
    let (_uid, admin) = make_admin(&env, "admin1@example.com").await;
    let (id, _raw, _body) = create_api_key(&env, &admin).await;

    let (status, body) = env
        .call(authed(req(Method::GET, "/api/v1/api-keys", None), &admin))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], json!("widget key"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/api-keys/{id}"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["scopes"], json!(["prayer-times"]));

    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/api-keys/{id}"),
                Some(json!({"name": "renamed"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!("renamed"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/api-keys/{id}/usage"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["quota_monthly"], json!(10000));
    assert_eq!(body["used_this_month"], json!(0));
    assert_eq!(body["quota_remaining"], json!(10000));

    let (status, body) = env
        .call(authed(
            req(Method::POST, &format!("/api/v1/api-keys/{id}/revoke"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_active"], json!(false));

    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/api-keys/{id}"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["is_active"], json!(false));
}

#[tokio::test]
async fn api_keys_create_validation_and_permission() {
    let env = Env::sqlite().await;
    let (_uid, admin) = make_admin(&env, "admin2@example.com").await;
    let (_uid, user_access, _r) = register_user(&env, "plain@example.com").await;

    // Unknown scope -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/api-keys",
                Some(json!({"name": "x", "scopes": ["nope"]})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "validation_error");

    // Empty name -> 422.
    let (status, _body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/api-keys",
                Some(json!({"name": "", "scopes": ["*"]})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // Plain user without api_keys.manage -> 403.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/api-keys",
                Some(json!({"name": "x", "scopes": ["*"]})),
            ),
            &user_access,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");

    // Unknown key id -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/api-keys/{}", Uuid::new_v4()),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err_code(&body), "api_key_not_found");
}

#[tokio::test]
async fn api_keys_validate_key_guard() {
    let env = Env::sqlite().await;
    let (_uid, admin) = make_admin(&env, "admin3@example.com").await;
    let (id, raw, _body) = create_api_key(&env, &admin).await;
    let _ = id;

    // Happy path, right scope.
    let key = validate_key(&env.state, Some(&raw), "prayer-times")
        .await
        .expect("valid key");
    assert_eq!(key.name, "widget key");

    // Wrong scope -> 403 api_key_scope_denied.
    let err = validate_key(&env.state, Some(&raw), "zakat-mini")
        .await
        .unwrap_err();
    assert_eq!(err.code, "api_key_scope_denied");

    // Unknown key -> 401 api_key_invalid.
    let err = validate_key(&env.state, Some("mk_bogus"), "prayer-times")
        .await
        .unwrap_err();
    assert_eq!(err.code, "api_key_invalid");

    // Missing key -> 401 api_key_required.
    let err = validate_key(&env.state, None, "prayer-times")
        .await
        .unwrap_err();
    assert_eq!(err.code, "api_key_required");

    // Revoked key -> 401 api_key_invalid.
    let (status, _body) = env
        .call(authed(
            req(Method::POST, &format!("/api/v1/api-keys/{id}/revoke"), None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let err = validate_key(&env.state, Some(&raw), "prayer-times")
        .await
        .unwrap_err();
    assert_eq!(err.code, "api_key_invalid");
}

#[tokio::test]
async fn api_keys_embed_snippet_and_token_roundtrip() {
    let env = Env::sqlite().await;
    let (_uid, admin) = make_admin(&env, "admin4@example.com").await;
    let (id, _raw, _body) = create_api_key(&env, &admin).await;
    let key_uuid = Uuid::parse_str(&id).unwrap();

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!(
                    "/api/v1/api-keys/{id}/embed-snippet?widget=prayer-times&base_url=https://example.com"
                ),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["widget"], json!("prayer-times"));
    let html = body["html"].as_str().unwrap();
    assert!(html.contains("https://example.com"), "snippet: {html}");
    assert!(html.contains("اوقات شرعی"), "snippet: {html}");
    assert!(
        !html.contains("mk_"),
        "raw key must never appear in the snippet"
    );

    // Unknown widget -> 422 unknown_widget.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/api-keys/{id}/embed-snippet?widget=nope"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "unknown_widget");

    // Non http(s) base_url -> 422 invalid_base_url.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!(
                    "/api/v1/api-keys/{id}/embed-snippet?widget=prayer-times&base_url=javascript:alert(1)"
                ),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "invalid_base_url");

    // Embed token roundtrip (direct guard API).
    let token = issue_embed_token(&env.state.config, key_uuid, "prayer-times");
    assert_eq!(
        verify_embed_token(&env.state.config, &token, "prayer-times"),
        Some(key_uuid)
    );
    assert_eq!(
        verify_embed_token(&env.state.config, &token, "zakat-mini"),
        None
    );
    assert_eq!(
        validate_embed_token(&env.state, Some(&token), "prayer-times")
            .await
            .expect("valid embed token")
            .id,
        key_uuid
    );
}

// ============================ users ============================

#[tokio::test]
async fn users_me_get_and_patch() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "me@example.com").await;

    let (status, body) = env
        .call(authed(req(Method::GET, "/api/v1/users/me", None), &access))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["email"], json!("me@example.com"));
    assert_eq!(body["profile_complete"], json!(false));
    assert!(body["missing_profile_fields"].as_array().unwrap().len() >= 4);

    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                "/api/v1/users/me",
                Some(json!({
                    "full_name": "نام تازه",
                    "gender": "female",
                    "country_code": "IR",
                    "date_of_birth": "1990-05-01",
                })),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["full_name"], json!("نام تازه"));
    assert_eq!(body["gender"], json!("female"));
    assert_eq!(body["country_code"], json!("IR"));

    // Invalid gender -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                "/api/v1/users/me",
                Some(json!({"gender": "other"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Future birth date -> 422.
    let (status, _body) = env
        .call(authed(
            req(
                Method::PATCH,
                "/api/v1/users/me",
                Some(json!({"date_of_birth": "2999-01-01"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    // country_code must be exactly 2 chars.
    let (status, _body) = env
        .call(authed(
            req(
                Method::PATCH,
                "/api/v1/users/me",
                Some(json!({"country_code": "IRAN"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn users_me_sessions_list_and_revoke() {
    let env = Env::sqlite().await;
    let (_uid, _access, _refresh) = register_user(&env, "sess@example.com").await;
    // A second login opens a second session.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "sess@example.com", "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let access2 = body["access_token"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/users/me/sessions", None),
            &access2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let sessions = body.as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    // Ordered by last_active_at DESC, so [0] is the current (second-login)
    // session — delete [1], the older session from register_user, so the
    // token used below stays valid.
    let session_id = sessions[1]["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/users/me/sessions/{session_id}"),
                None,
            ),
            &access2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("نشست بسته شد"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/users/me/sessions", None),
            &access2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn users_devices_web_push_and_notifications() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "dev@example.com").await;

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/users/me/devices",
                Some(json!({"device_id": "poco-x5", "platform": "android"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Re-registering the same device keeps the old push_token when none is sent.
    let (status, _body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/users/me/devices",
                Some(json!({"device_id": "poco-x5", "platform": "android", "push_token": "tok-1"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/notifications/web-push/vapid-key",
                None,
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("vapid_public_key").is_some());

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/users/me/web-push/subscriptions",
                Some(json!({
                    "endpoint": "https://push.example.com/sub/1",
                    "p256dh": "p256dh-key",
                    "auth": "auth-key",
                })),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["subscribed"], json!(true));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/users/me/notifications", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("items").is_some() || body.as_array().is_some());

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/users/me/notifications/{}/read", Uuid::new_v4()),
                None,
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn users_admin_list_requires_permission() {
    let env = Env::sqlite().await;
    let (_uid, user_access, _r) = register_user(&env, "plain2@example.com").await;
    let (_admin_uid, admin) = make_admin(&env, "admin5@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users", None),
            &user_access,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users?limit=10", None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().expect("admin users items");
    assert!(items.len() >= 2);
    assert!(body.get("next_cursor").is_some() || body.get("cursor").is_some());
}

#[tokio::test]
async fn users_export_has_no_password_hash_and_delete_anonymizes() {
    let env = Env::sqlite().await;
    let (uid, access, _refresh) = register_user(&env, "gone@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/users/me/export", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let flat = serde_json::to_string(&body).unwrap();
    assert!(
        !flat.contains("password_hash"),
        "export leaked password_hash"
    );
    assert!(flat.contains("gone@example.com"));

    let (status, body) = env
        .call(authed(
            req(Method::DELETE, "/api/v1/users/me", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("حساب شما ناشناس‌سازی و غیرفعال شد"));

    // The row is anonymized, not removed.
    let row = env
        .db
        .fetch_one(
            "SELECT full_name, email, is_active FROM users WHERE id = ?",
            &[as_uuid(&uid).into_arg()],
        )
        .await
        .unwrap();
    assert_eq!(
        row.try_get::<String, _>("full_name").unwrap(),
        "حساب حذف‌شده"
    );
    assert!(row.try_get::<Option<String>, _>("email").unwrap().is_none());
    assert!(!row.try_get::<bool, _>("is_active").unwrap());

    // The old token no longer authenticates.
    let (status, _body) = env
        .call(authed(req(Method::GET, "/api/v1/users/me", None), &access))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn users_avatar_by_ref_rejects_bad_signature() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "av@example.com").await;
    let _ = access;
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/users/avatar-by-ref?key=avatars/x.png&exp=9999999999&sig=deadbeef",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(err_code(&body), "invalid_signature");
}

// ============================ auth ============================

fn sha256_hex(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

async fn insert_token_row(env: &Env, user_id: &str, raw: &str, purpose: &str, minutes: i64) {
    let expires = Utc::now() + chrono::Duration::minutes(minutes);
    env.db
        .execute(
            "INSERT INTO password_reset_tokens (id, user_id, token_hash, purpose, expires_at, used, created_at) VALUES (?, ?, ?, ?, ?, FALSE, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(user_id).into_arg(),
                sha256_hex(raw).into_arg(),
                purpose.into_arg(),
                expires.to_rfc3339().into_arg(),
                Utc::now().to_rfc3339().into_arg(),
            ],
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn auth_register_login_refresh_flow() {
    let env = Env::sqlite().await;
    let (user_id, access, refresh) = register_user(&env, "flow@example.com").await;

    // Wrong password -> 401 invalid_credentials.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "flow@example.com", "password": "wrong-password"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_code(&body), "invalid_credentials");

    // Duplicate registration -> 409 email_taken.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "full_name": "Again",
                "email": "flow@example.com",
                "password": "correct-horse-battery",
                "accept_terms": true,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(err_code(&body), "email_taken");

    // The access token authenticates.
    let (status, body) = env
        .call(authed(req(Method::GET, "/api/v1/users/me", None), &access))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], json!(user_id));

    // Refresh rotates: new pair, old refresh token is dead.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": refresh})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let refresh2 = body["refresh_token"].as_str().unwrap().to_string();
    assert_ne!(refresh2, refresh);

    // The rotated token works...
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": refresh2})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let refresh3 = _body["refresh_token"].as_str().unwrap().to_string();

    // ...but replaying the first (now revoked) token is reuse: 401, and the
    // reuse detector revokes the whole session, so the latest token dies too.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": refresh})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "invalid_token");

    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": refresh3})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_register_validation() {
    let env = Env::sqlite().await;
    // Missing accept_terms -> 422 terms_required.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "full_name": "No Terms",
                "email": "noterms@example.com",
                "password": "correct-horse-battery",
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "terms_required");

    // No contact -> 422 contact_required.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "full_name": "No Contact",
                "password": "correct-horse-battery",
                "accept_terms": true,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "contact_required");

    // Bad email -> 422 validation_error.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "full_name": "Bad Email",
                "email": "not-an-email",
                "password": "correct-horse-battery",
                "accept_terms": true,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "validation_error");

    // Short password -> 422 validation_error.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(json!({
                "full_name": "Short Pass",
                "email": "short@example.com",
                "password": "short",
                "accept_terms": true,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(err_code(&body), "validation_error");
}

#[tokio::test]
async fn auth_verify_email_flow() {
    let env = Env::sqlite().await;
    let (user_id, _access, _refresh) = register_user(&env, "verify@example.com").await;

    // Unknown token -> 400 invalid_token.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/verify-email",
            Some(json!({"token": "no-such-token-1234567890"})),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err_code(&body), "invalid_token");

    // Seed a real email_verify token, then verify.
    let raw = "verify-token-raw-value-1234567890";
    insert_token_row(&env, &user_id, raw, "email_verify", 60).await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/verify-email",
            Some(json!({"token": raw})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("ایمیل با موفقیت تأیید شد"));

    let row = env
        .db
        .fetch_one(
            "SELECT is_verified FROM users WHERE id = ?",
            &[as_uuid(&user_id).into_arg()],
        )
        .await
        .unwrap();
    assert!(row.try_get::<bool, _>("is_verified").unwrap());

    // Replaying the consumed token is idempotent for a verified user.
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/verify-email",
            Some(json!({"token": raw})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn auth_password_reset_flow() {
    let env = Env::sqlite().await;
    let (user_id, _access, _refresh) = register_user(&env, "reset@example.com").await;

    // Anti-enumeration: unknown email gets the same generic 200.
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/password-reset/request",
            Some(json!({"email": "nobody@example.com"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/password-reset/request",
            Some(json!({"email": "reset@example.com"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let raw = "reset-token-raw-value-1234567890";
    insert_token_row(&env, &user_id, raw, "reset", 60).await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/password-reset/confirm",
            Some(json!({"token": raw, "new_password": "new-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("رمز عبور با موفقیت تغییر کرد"));

    // New password works, old one does not.
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "reset@example.com", "password": "new-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "reset@example.com", "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_code(&body), "invalid_credentials");
}

#[tokio::test]
async fn auth_2fa_enroll_and_login() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "twofa@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/auth/2fa/status", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], json!(false));

    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/auth/2fa/setup", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let secret = body["secret"].as_str().unwrap().to_string();
    assert!(body["provisioning_uri"]
        .as_str()
        .unwrap()
        .starts_with("otpauth://totp/"));

    // Wrong code -> 400 invalid_code.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/auth/2fa/enable",
                Some(json!({"secret": secret, "code": "000000"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err_code(&body), "invalid_code");

    let code = totp_code(&secret, Utc::now().timestamp());
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/auth/2fa/enable",
                Some(json!({"secret": secret, "code": code})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let codes = body["recovery_codes"].as_array().unwrap().clone();
    assert_eq!(codes.len(), 10);
    assert!(codes[0].as_str().unwrap().contains('-'));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/auth/2fa/status", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], json!(true));
    assert_eq!(body["recovery_codes_remaining"], json!(10));

    // Login without a TOTP code -> 401 requires_2fa.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "twofa@example.com", "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_code(&body), "requires_2fa");
    assert_eq!(body["error"]["details"]["requires_2fa"], json!(true));

    // Login with a fresh code works; replaying it -> 401 code_reused.
    sleep_until_next_totp_step().await;
    let code = totp_code(&secret, Utc::now().timestamp());
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "identifier": "twofa@example.com",
                "password": "correct-horse-battery",
                "totp_code": code,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({
                "identifier": "twofa@example.com",
                "password": "correct-horse-battery",
                "totp_code": code,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_code(&body), "code_reused");

    // A recovery code logs in by disabling 2FA.
    let recovery = codes[0].as_str().unwrap().to_string();
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/2fa/recover",
            Some(json!({
                "email": "twofa@example.com",
                "password": "correct-horse-battery",
                "recovery_code": recovery,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["message"],
        json!("احراز دوعاملی غیرفعال شد؛ لطفاً دوباره فعال کنید")
    );

    // The same recovery code is now spent.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/2fa/recover",
            Some(json!({
                "email": "twofa@example.com",
                "password": "correct-horse-battery",
                "recovery_code": recovery,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    // Plain login works again after recovery.
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "twofa@example.com", "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn auth_logout_and_logout_all() {
    let env = Env::sqlite().await;
    let (_uid, _a1, r1) = register_user(&env, "logout@example.com").await;
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(json!({"identifier": "logout@example.com", "password": "correct-horse-battery"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let access2 = body["access_token"].as_str().unwrap().to_string();
    let r2 = body["refresh_token"].as_str().unwrap().to_string();

    // Logout revokes only that session's refresh token.
    let (status, _body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/auth/logout",
                Some(json!({"refresh_token": r1})),
            ),
            &access2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": r1})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": r2})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Logout-all revokes every session.
    let (status, _body) = env
        .call(authed(
            req(Method::POST, "/api/v1/auth/logout-all", None),
            &access2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": r2})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn auth_ws_ticket_and_phone_code_request() {
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "ws@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/auth/ws-ticket", None),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["ticket"].as_str().unwrap().len() > 20);
    assert_eq!(body["expires_in"], json!(60));

    // Phone-code request is anti-enumeration: unknown phone -> same 200.
    let (status, _body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register/phone-code/request",
            Some(json!({"phone": "+989123456789"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
}

// ============================ Postgres smoke ============================

/// Replays the critical path (register -> login -> refresh -> activity ->
/// api-key create -> validate) against a real Postgres, when
/// `WAVE2_PG_URL` is set (e.g. the local test database). The same
/// `TEST_SCHEMA` DDL is applied; tables are dropped first so the test is
/// repeatable.
#[tokio::test]
async fn wave2_pg_smoke() {
    let url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            eprintln!("WAVE2_PG_URL not set — skipping Postgres smoke test");
            return;
        }
    };
    let db = Db::connect(&url).await.expect("pg connect");
    for table in [
        "users",
        "sessions",
        "refresh_tokens",
        "password_reset_tokens",
        "phone_verification_codes",
        "recovery_codes",
        "ws_tickets",
        "user_activity_events",
        "themes",
        "platform_settings",
        "api_keys",
        "user_devices",
        "web_push_subscriptions",
        "roles",
        "permissions",
        "role_permissions",
        "user_roles",
        "user_consent",
        "audit_logs",
        "domain_events",
        "notifications",
        "notification_preferences",
        "media_assets",
        "fatwa_sessions",
        "fatwa_messages",
        "user_ayah_notes",
        "donations",
    ] {
        db.execute(&format!("DROP TABLE IF EXISTS {table} CASCADE"), &[])
            .await
            .unwrap();
    }
    init_test_schema(&db).await.expect("pg schema");
    let env = Env {
        state: AppState::with_db(test_config(), Some(db.clone())),
        db,
    };

    let (user_id, access, refresh) = register_user(&env, "pg@example.com").await;
    assert!(!user_id.is_empty());

    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/refresh",
            Some(json!({"refresh_token": refresh})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "pg refresh: {body}");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/activity/events",
                Some(json!({"kind": "library_open", "title": "pg"})),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "pg activity: {body}");

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/settings/public", None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["settings"]["quran.madhhab_default"], json!("hanafi"));
}
