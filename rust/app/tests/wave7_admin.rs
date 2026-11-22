//! Wave 7 (admin core) integration tests.
//!
//! Settings (GET/PUT/masked secrets/re-encrypt), dead-letter events,
//! font uploads (magic bytes + ADR-051 scan gate), sync devices,
//! builds (releases/prereqs/rebuild guard/release idempotency),
//! connections status + tests, notification broadcasts (preview +
//! in_app fan-out), roles CRUD + guards, and the users 360° dashboard
//! lifecycle (dashboard/stats/trash/overview/roles/suspend/activate/
//! soft-delete/restore/permanent-delete).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the
//! end (`wave7_admin_pg_smoke`, gated on `WAVE2_PG_URL`) replays the
//! settings → roles → users-dashboard path against one local Postgres
//! test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::Utc;
use maturidi_api_rs::{build_router, init_test_schema, AppState, Config, Db, IntoArg, TEST_SCHEMA};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave7-admin-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave7".to_string(),
        captcha_enabled: false,
        // Dead port: native routes must never touch the origin.
        origin_url: "http://127.0.0.1:9".to_string(),
        storage_dir: format!(
            "{}/wave7-admin-storage-{}",
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

    async fn sqlite_with_config(config: Config) -> Self {
        let db = Db::connect("sqlite://:memory:")
            .await
            .expect("sqlite connect");
        init_test_schema(&db).await.expect("test schema");
        let state = AppState::with_db(config, Some(db.clone()));
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

const ADMIN_PERMS: &[&str] = &[
    "platform.settings",
    "sync.manage",
    "builds.manage",
    "connections.test",
    "notifications.broadcast",
    "roles.manage",
    "users.view360",
    "users.manage",
];

/// Register a user with every admin-core permission.
async fn admin_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, ADMIN_PERMS).await;
    (uid, token)
}

fn setting<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["settings"]
        .as_array()
        .expect("settings array")
        .iter()
        .find(|s| s["key"].as_str() == Some(key))
        .unwrap_or_else(|| panic!("setting {key} missing in {body}"))
}

/// Build a multipart/form-data request body (font upload).
fn multipart_req(
    uri: &str,
    token: &str,
    fields: &[(&str, Option<&str>, Vec<u8>)],
) -> Request<Body> {
    let boundary = "----wave7testboundary";
    let mut body: Vec<u8> = Vec::new();
    for (name, filename, bytes) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        match filename {
            Some(fname) => body.extend_from_slice(
                format!(
                    "Content-Disposition: form-data; name=\"{name}\"; filename=\"{fname}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
                )
                .as_bytes(),
            ),
            None => body.extend_from_slice(
                format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
            ),
        }
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {token}"))
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(body))
        .unwrap();
    request
}

// ===========================================================================
// Settings
// ===========================================================================

#[tokio::test]
async fn admin_settings_get_put() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-settings@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["settings"].is_array());
    assert!(body["version"].is_number());
    // Every def is present, with its declared type.
    assert_eq!(setting(&body, "quran.font_size")["type"], json!("json"));
    assert_eq!(setting(&body, "ai.claude_api_key")["masked"], json!(true));

    // Unknown key -> 400 unknown_setting.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(json!({ "settings": { "no.such.key": "x" } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "unknown_setting");

    // Out-of-range font size -> 400 invalid_setting_value.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(json!({ "settings": { "quran.font_size": 5 } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_setting_value");

    // Localized round-trip.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(
                    json!({ "settings": { "branding.app_display_name": { "fa": "آکادمی تست" } } }),
                ),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["updated"], json!(["branding.app_display_name"]));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        setting(&body, "branding.app_display_name")["value"],
        json!({ "fa": "آکادمی تست" })
    );

    // Secret is masked, and "***" leaves it unchanged.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(json!({ "settings": { "ai.claude_api_key": "sk-test-secret-value" } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(setting(&body, "ai.claude_api_key")["value"], json!("***"));

    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(json!({ "settings": { "ai.claude_api_key": "***" } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["updated"], json!([]));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(setting(&body, "ai.claude_api_key")["value"], json!("***"));
}

#[tokio::test]
async fn admin_settings_reencrypt() {
    // Config with a v0 key mapping to the same material as the current
    // (v1) key, so a stored v0 envelope is decryptable and rotatable.
    let mut config = test_config();
    config.encryption_keys = json!({ "0": config.encryption_key.clone() }).to_string();
    let env = Env::sqlite_with_config(config).await;
    let (_, token) = admin_user(&env, "admin-reenc@example.com").await;
    // One encrypted secret so rotation has something to do.
    let (status, _) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(json!({ "settings": { "ai.claude_api_key": "sk-rotate-me" } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    // Backdate the envelope to v0 (same token bytes, old version stamp).
    let row = env
        .db
        .fetch_one(
            "SELECT value FROM platform_settings WHERE key = ?",
            &["ai.claude_api_key".into_arg()],
        )
        .await
        .unwrap();
    let mut envelope: Value = row.try_get("value").unwrap();
    assert_eq!(envelope["v"], json!(1));
    envelope["v"] = json!(0);
    env.db
        .execute(
            "UPDATE platform_settings SET value = ? WHERE key = ?",
            &[envelope.into_arg(), "ai.claude_api_key".into_arg()],
        )
        .await
        .unwrap();

    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/settings/reencrypt", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["settings_rotated"], json!(1));
    assert_eq!(body["settings_current"], json!(0));
    assert_eq!(body["unreadable"], json!(0));
    // Second run: already current, nothing rotates.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/settings/reencrypt", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["settings_rotated"], json!(0));
    assert_eq!(body["settings_current"], json!(1));
    // The secret still reads back masked after rotation.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(setting(&body, "ai.claude_api_key")["value"], json!("***"));
}

// ===========================================================================
// Dead-letter events
// ===========================================================================

#[tokio::test]
async fn admin_dead_letter_retry() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-dead@example.com").await;
    let now = Utc::now();
    let event_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO domain_events (event_id, aggregate_type, aggregate_id, event_type, payload, schema_version, source_client, created_at, failed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                event_id.into_arg(),
                "platform".into_arg(),
                "platform".into_arg(),
                "platform.settings_updated".into_arg(),
                json!({}).into_arg(),
                1.into_arg(),
                "api".into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/events/dead-letter", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["events"][0]["event_id"], json!(event_id.to_string()));

    // Bad UUID -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/events/dead-letter/not-a-uuid/retry",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Unknown UUID -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/events/dead-letter/{}/retry", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "not_found");

    // Retry clears the dead letter.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/events/dead-letter/{event_id}/retry"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/events/dead-letter", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));

    // Second retry -> 400 not_dead_letter.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/events/dead-letter/{event_id}/retry"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "not_dead_letter");
}

// ===========================================================================
// Fonts
// ===========================================================================

#[tokio::test]
async fn admin_fonts() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-fonts@example.com").await;

    // Invalid locale.
    let (status, body) = env
        .call(multipart_req(
            "/api/v1/admin/fonts",
            &token,
            &[
                ("locale", None, b"xx".to_vec()),
                ("family", None, b"Vazirmatn".to_vec()),
                ("file", Some("font.woff2"), b"wOF2abcd".to_vec()),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_locale");

    // Bad magic bytes.
    let (status, body) = env
        .call(multipart_req(
            "/api/v1/admin/fonts",
            &token,
            &[
                ("locale", None, b"fa".to_vec()),
                ("family", None, b"Vazirmatn".to_vec()),
                ("file", Some("font.woff2"), b"XXXXabcd".to_vec()),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_font_file");

    // EICAR test string inside a woff2 -> 422 file_infected (ADR-051 gate).
    let mut eicar = b"wOF2".to_vec();
    eicar.extend_from_slice(
        b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*",
    );
    let (status, body) = env
        .call(multipart_req(
            "/api/v1/admin/fonts",
            &token,
            &[
                ("locale", None, b"fa".to_vec()),
                ("family", None, b"Vazirmatn".to_vec()),
                ("file", Some("font.woff2"), eicar),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "file_infected");

    // Valid upload -> 201, and the setting now carries the font.
    let (status, body) = env
        .call(multipart_req(
            "/api/v1/admin/fonts",
            &token,
            &[
                ("locale", None, b"fa".to_vec()),
                ("family", None, b"Vazirmatn".to_vec()),
                ("file", Some("font.woff2"), b"wOF2validfontbytes".to_vec()),
            ],
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["locale"], json!("fa"));
    assert_eq!(body["family"], json!("Vazirmatn"));
    assert!(body["url"].as_str().unwrap().contains("/static/fonts/fa/"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        setting(&body, "fonts.fa")["value"]["family"],
        json!("Vazirmatn")
    );

    // Delete -> font cleared.
    let (status, body) = env
        .call(authed(
            req(Method::DELETE, "/api/v1/admin/fonts/fa", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["family"], json!(null));
    assert_eq!(body["url"], json!(null));
}

// ===========================================================================
// Sync devices
// ===========================================================================

#[tokio::test]
async fn admin_sync_devices() {
    let env = Env::sqlite().await;
    let (uid, token) = admin_user(&env, "admin-sync@example.com").await;
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO device_cursors (id, user_id, device_id, cursor_seq, updated_at) VALUES (?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(&uid).into_arg(),
                "dev-cursor".into_arg(),
                7i64.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    env.db
        .execute(
            "INSERT INTO user_devices (id, user_id, device_id, platform, last_seen, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(&uid).into_arg(),
                "dev-plain".into_arg(),
                "android".into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/sync/devices", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let devices = body["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    let cursor = devices
        .iter()
        .find(|d| d["device_id"] == "dev-cursor")
        .expect("cursor device");
    assert_eq!(cursor["cursor_seq"], json!(7));
    let plain = devices
        .iter()
        .find(|d| d["device_id"] == "dev-plain")
        .expect("plain device");
    assert_eq!(plain["platform"], json!("android"));
}

// ===========================================================================
// Builds
// ===========================================================================

#[tokio::test]
async fn admin_builds() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-builds@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/builds/releases", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    // Prereqs: no GitHub dispatch token in the test env.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/builds/prereqs", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider"]["configured"], json!(false));
    // Platforms is an object keyed by platform (Python returns a dict).
    assert!(body["platforms"].is_object());
    assert_eq!(body["platforms"]["android"]["ready"], json!(false));
    assert_eq!(
        body["platforms"]["android"]["missing"],
        json!(["google_play", "cafebazaar"])
    );

    // Rebuild without a configured provider -> 503, no row created.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/builds/rebuild",
                Some(json!({ "platform": "android" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "build_provider_not_configured");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/builds/requests", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    // Bad platform -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/builds/releases",
                Some(json!({ "platform": "watchos", "version": "1.0.0" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_platform");

    // Create a release -> 201; identical repost returns the same row;
    // a differing field -> 409 release_conflict.
    let release = json!({
        "platform": "android",
        "version": "1.2.3",
        "build_number": "42",
        "artifact_key": "builds/android/1.2.3/app.apk",
        "sha256": "a".repeat(64),
        "file_size_bytes": 12345,
        "signed": true,
    });
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/builds/releases",
                Some(release.clone()),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let rel_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["has_file"], json!(true));
    // artifact_key rows have no presigned URL in the Rust binary (ADM-04).
    assert_eq!(body["download_url"], json!(null));

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/builds/releases",
                Some(release.clone()),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], json!(rel_id));

    let mut conflict = release.clone();
    conflict["sha256"] = json!("b".repeat(64));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/builds/releases",
                Some(conflict),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "release_conflict");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/builds/releases", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
}

// ===========================================================================
// Connections
// ===========================================================================

#[tokio::test]
async fn admin_connections() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-conn@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/connections/status", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for key in ["ai", "sms", "telegram", "payment", "maps", "signing"] {
        assert!(body.get(key).is_some(), "missing {key} in {body}");
    }
    assert_eq!(body["payment"]["configured"], json!(false));
    assert_eq!(body["ai"]["configured"], json!(false));

    // Unknown service -> 404.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/connections/test/nope", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "unknown_service");

    // Payment is honestly unavailable.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/connections/test/payment", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("payment_not_available"));

    // AI unconfigured -> honest failure, not a fake probe.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/connections/test/ai", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("ai_not_configured"));
}

// ===========================================================================
// Notifications
// ===========================================================================

#[tokio::test]
async fn admin_broadcast() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-bc@example.com").await;
    let (_, _plain_token, _) = register_user(&env, "plain-bc@example.com").await;

    // Empty users cohort -> 422 bad_cohort; a cohort matching nobody
    // -> 422 empty_cohort; bad channel -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/notifications/broadcast",
                Some(json!({
                    "title": "t", "body": "b",
                    "channels": ["in_app"],
                    "cohort": { "type": "users", "user_ids": [] },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_cohort");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/notifications/broadcast",
                Some(json!({
                    "title": "t", "body": "b",
                    "channels": ["in_app"],
                    "cohort": { "type": "roles", "roles": ["no-such-role"] },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "empty_cohort");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/notifications/broadcast",
                Some(json!({
                    "title": "t", "body": "b",
                    "channels": ["smoke_signal"],
                    "cohort": { "type": "all" },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_channel");

    // Preview is a dry run: 2 active recipients, in_app for both.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/notifications/preview",
                Some(json!({
                    "title": "سلام", "body": "خبر مهم",
                    "channels": ["in_app"],
                    "cohort": { "type": "all" },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["recipients"], json!(2));
    assert_eq!(body["per_channel"]["in_app"], json!(2));
    assert_eq!(body["dry_run"], json!(true));

    // Real broadcast: in_app delivery is real (notification rows).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/notifications/broadcast",
                Some(json!({
                    "title": "سلام", "body": "خبر مهم",
                    "channels": ["in_app"],
                    "cohort": { "type": "all" },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("done"));
    assert_eq!(body["total_recipients"], json!(2));
    assert_eq!(body["total_sent"], json!(2));
    assert_eq!(body["total_failed"], json!(0));
    let bc_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/notifications/broadcasts", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/notifications/broadcasts/{bc_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deliveries"].as_array().unwrap().len(), 2);
    assert!(body["deliveries"]
        .as_array()
        .unwrap()
        .iter()
        .all(|d| d["status"] == "sent"));

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/notifications/delivery-stats",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stats"]["in_app"]["sent"], json!(2));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/notifications/channels", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let channels = body["channels"].as_array().unwrap();
    assert_eq!(channels.len(), 4);
    let in_app = channels
        .iter()
        .find(|c| c["key"] == "in_app")
        .expect("in_app channel");
    assert_eq!(in_app["configured"], json!(true));
}

// ===========================================================================
// Roles
// ===========================================================================

#[tokio::test]
async fn admin_roles() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "admin-roles@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/roles", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The admin's own editor role is listed.
    assert!(!body.as_array().unwrap().is_empty());

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/roles/permissions", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["key"] == "platform.settings"));

    // Unknown permission key -> 422, and no half-inserted role.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "bad-role", "permission_keys": ["no.such.perm"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_permission");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/roles", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "bad-role"));

    // Create with a real permission -> 201.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "moderator", "permission_keys": ["users.view360"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let role_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["permissions"][0]["key"], json!("users.view360"));

    // Duplicate name -> 409.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "moderator" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "role_exists");

    // Rename + change permissions.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/admin/roles/{role_id}"),
                Some(json!({ "name": "mod2", "permission_keys": ["users.manage"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["name"], json!("mod2"));
    assert_eq!(body["permissions"][0]["key"], json!("users.manage"));

    // super_admin is protected from rename.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "super_admin" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sa_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/admin/roles/{sa_id}"),
                Some(json!({ "name": "renamed" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "protected_role");
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/roles/{sa_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "protected_role");

    // Role with a member cannot be deleted.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "staffed" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let staffed_id = body["id"].as_str().unwrap().to_string();
    let (victim_id, _, _) = register_user(&env, "victim@example.com").await;
    env.db
        .execute(
            "INSERT INTO user_roles (user_id, role_id) VALUES (?, ?)",
            &[
                as_uuid(&victim_id).into_arg(),
                as_uuid(&staffed_id).into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/roles/{staffed_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "role_has_members");

    // Empty role deletes cleanly.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/roles/{role_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

// ===========================================================================
// Users dashboard + lifecycle
// ===========================================================================

#[tokio::test]
async fn admin_users_lifecycle() {
    let env = Env::sqlite().await;
    let (admin_id, token) = admin_user(&env, "admin-users@example.com").await;
    let (user_id, _, _) = register_user(&env, "member@example.com").await;
    // Mark the member as pending registration (admin-invited, no password set yet).
    env.db
        .execute(
            "INSERT INTO password_reset_tokens (id, user_id, token_hash, purpose, expires_at, used, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(&user_id).into_arg(),
                "hash".into_arg(),
                "password_setup".into_arg(),
                (Utc::now() + chrono::Duration::hours(24)).into_arg(),
                false.into_arg(),
                Utc::now().into_arg(),
            ],
        )
        .await
        .unwrap();

    // Dashboard lists both alive users.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users/dashboard", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(2));
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let member = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == user_id)
        .expect("member in dashboard");
    assert_eq!(member["registration_status"], json!("pending"));
    assert_eq!(member["profile_complete"], json!(false));

    // Stats.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users/stats", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_users"], json!(2));
    assert_eq!(body["pending"], json!(1));
    assert_eq!(body["completed"], json!(1));
    assert_eq!(body["trashed"], json!(0));

    // Trash is empty.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users/trash", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));

    // Overview.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/users/{user_id}/overview"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["profile"]["id"], json!(user_id));
    assert!(body["roles"].is_array());
    assert!(body["activity_timeline"].is_array());
    assert!(body["activity_heatmap"].is_array());

    // Overview of a missing user -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/users/{}/overview", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Revoke sessions first (login created one; patching roles below
    // also revokes, so this must come first).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/users/{user_id}/sessions/revoke"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["revoked_sessions"].as_i64().unwrap() >= 1);

    // Patch roles: give the member the users.view360 permission via a role.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "viewer", "permission_keys": ["users.view360"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let viewer_role = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/users/{user_id}/roles"),
                Some(json!({ "role_ids": [viewer_role] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["role_ids"], json!([viewer_role]));

    // Suspend (idempotent) then activate.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/users/{user_id}/suspend"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_active"], json!(false));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/users/{user_id}/suspend"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_active"], json!(false));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/users/{user_id}/activate"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_active"], json!(true));

    // A donation by the member survives permanent delete (anonymised).
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO donations (id, donor_id, amount, currency, status, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(&user_id).into_arg(),
                50000i64.into_arg(),
                "IRR".into_arg(),
                "succeeded".into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();

    // Soft delete -> trash; restore -> back.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/users/{user_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["trashed"], json!(true));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users/trash", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/users/{user_id}/restore"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["restored"], json!(true));

    // Permanent delete requires trash first.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/users/{user_id}/permanent"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "not_trashed");

    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/users/{user_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/users/{user_id}/permanent"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"], json!(true));

    // User is gone from the dashboard, and the donation row survives
    // with donor_id anonymised to NULL.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users/dashboard", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(admin_id));
    let row = env
        .db
        .fetch_one("SELECT donor_id FROM donations", &[])
        .await
        .unwrap();
    let donor: Option<Uuid> = row.try_get("donor_id").unwrap();
    assert_eq!(donor, None);
}

// ===========================================================================
// Users guards + authz
// ===========================================================================

#[tokio::test]
async fn admin_users_guards() {
    let env = Env::sqlite().await;
    let (admin_id, token) = admin_user(&env, "admin-guards@example.com").await;
    let (user_id, _, _) = register_user(&env, "guard-member@example.com").await;

    // Self-suspend -> 422 self_action.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/users/{admin_id}/suspend"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "self_action");

    // Patch roles: empty -> 422 empty_roles.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/users/{user_id}/roles"),
                Some(json!({ "role_ids": [] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "empty_roles");

    // Patch roles: unknown role id -> 422 bad_role_id.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/users/{user_id}/roles"),
                Some(json!({ "role_ids": [Uuid::new_v4().to_string()] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_role_id");

    // Patch roles: unknown role name -> 422 bad_role_name.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/users/{user_id}/roles"),
                Some(json!({ "role_names": ["no-such-role"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_role_name");

    // Self lockout: admin holds super_admin; removing it from self -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "super_admin", "permission_keys": ["roles.manage"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sa_id = body["id"].as_str().unwrap().to_string();
    env.db
        .execute(
            "INSERT INTO user_roles (user_id, role_id) VALUES (?, ?)",
            &[as_uuid(&admin_id).into_arg(), as_uuid(&sa_id).into_arg()],
        )
        .await
        .unwrap();
    // A super_admin without TOTP trips the live 2FA re-check on every
    // privileged call (403 requires_2fa); a dummy totp_secret satisfies
    // it, as in production once TOTP is enrolled.
    env.db
        .execute(
            "UPDATE users SET totp_secret = ? WHERE id = ?",
            &[
                "dummy-totp-secret".into_arg(),
                as_uuid(&admin_id).into_arg(),
            ],
        )
        .await
        .unwrap();
    // A second role to replace with, so the request is not empty.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "plain2" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plain_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/users/{admin_id}/roles"),
                Some(json!({ "role_ids": [plain_id] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "self_lockout");
}

#[tokio::test]
async fn admin_authz() {
    let env = Env::sqlite().await;
    // No token -> 401.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/admin/settings", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Plain user without the permission -> 403.
    let (_, plain_token, _) = register_user(&env, "plain-authz@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

// ===========================================================================
// Postgres smoke
// ===========================================================================

#[tokio::test]
async fn wave7_admin_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok() else {
        return;
    };
    let db = Db::connect(&url).await.expect("postgres connect");
    // Drop every table `TEST_SCHEMA` creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run would
    // fail the schema apply. The list is derived from the schema itself
    // (wave5/wave6 pattern). Binaries must run sequentially — the smokes
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

    // Settings: PUT a localized value, GET it back.
    let (_, admin_token) = admin_user(&env, "pg-admin@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/settings",
                Some(json!({ "settings": { "branding.app_display_name": { "fa": "آکادمی" } } })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/settings", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        setting(&body, "branding.app_display_name")["value"],
        json!({ "fa": "آکادمی" })
    );

    // Roles: create + list.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/roles",
                Some(json!({ "name": "pg-moderator", "permission_keys": ["users.view360"] })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/roles", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "pg-moderator"));

    // Users dashboard: admin + one member.
    let (_, _, _) = register_user(&env, "pg-member@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/users/dashboard", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(2));
}
