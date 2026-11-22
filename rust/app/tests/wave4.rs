//! Wave 4 integration tests: quran (reader, providers, notes,
//! reading-state, tasbih, adhan) and hifz (attempts, peek, settings,
//! listen position, stats, review queue, SRS, plans, dashboard,
//! activity, suggested, tests, mutashabihat, tasmi, data purge).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the end
//! (`wave4_pg_smoke`, gated on `WAVE2_PG_URL`) replays the critical
//! path against a local Postgres test database.
//!
//! ASR endpoints (`grade`, `grade-word`, `voice-search`, test `answer`)
//! are deliberately NOT native: they are sidecar-contract routes
//! (ADR-134), and the live WebSocket stream answers 503
//! `hifz_live_unavailable` — both are asserted here (see
//! `hifz_grade_sidecar_and_stream_unavailable` and
//! `hifz_stream_unavailable`).

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::Utc;
use maturidi_api_rs::{
    build_router, encrypt_value, init_test_schema, AppState, Config, Db, IntoArg,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "wave2-test-jwt-secret-that-is-long-enough".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave4-storage-{}",
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
/// (non-privileged, so no TOTP enrolment is needed). The role name is
/// unique per call and permission rows are inserted only once, so the
/// helper can be called for several users in one test.
async fn grant(env: &Env, user_id: &str, keys: &[&str]) {
    // Bind a real DateTime, not an RFC3339 string: Postgres rejects a
    // `text` parameter for a TIMESTAMPTZ column (error 42804), while
    // sqlite silently accepts it.
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

// ============================ Quran: reader ===========================

#[tokio::test]
async fn quran_surahs_and_surah() {
    let env = Env::sqlite().await;
    // Offline defaults: bundled translation/tafsir, no reciter audio
    // (the built-in defaults point at AlQuran Cloud).
    set_setting(&env, "quran.translations", &json!(["fa.rowwad"])).await;
    set_setting(&env, "quran.tafsirs", &json!(["fa.mokhtasar"])).await;
    set_setting(&env, "quran.reciters", &json!([])).await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/surahs", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(114), "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 114, "114 surahs");
    assert_eq!(items[0]["number"], json!(1));
    assert_eq!(items[0]["name_ar"], json!("الفاتحة"));
    assert_eq!(items[0]["ayahs"], json!(7));
    assert_eq!(items[113]["number"], json!(114));
    assert_eq!(items[113]["ayahs"], json!(6));

    // Default translations are AlQuran Cloud (online); the offline test
    // asks for the bundled editions explicitly.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/surah/1?translation=fa.rowwad&tafsir=fa.mokhtasar",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["surah"], json!(1));
    assert_eq!(body["name_ar"], json!("الفاتحة"));
    assert_eq!(body["edition_ids"][0], json!("quran-uthmani"));
    // Self-hosted text: all 7 ayahs, first is the basmala.
    let ayahs = body["ayahs"].as_array().unwrap();
    assert_eq!(ayahs.len(), 7);
    let first_editions = ayahs[0]["editions"].as_array().unwrap();
    assert_eq!(first_editions[0]["edition"], json!("quran-uthmani"));
    assert!(first_editions[0]["text"].as_str().unwrap().contains("بِسْمِ"));

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/surah/115", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "surah_not_found");

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/surah/abc", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn quran_ayah_self_hosted_and_bundled() {
    let env = Env::sqlite().await;
    set_setting(&env, "quran.translations", &json!(["fa.rowwad"])).await;
    set_setting(&env, "quran.tafsirs", &json!(["fa.mokhtasar"])).await;
    set_setting(&env, "quran.reciters", &json!([])).await;
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/ayah/1/1?translation=fa.rowwad",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["surah"], json!(1));
    assert_eq!(body["ayah"], json!(1));
    let editions = body["editions"].as_array().unwrap();
    assert_eq!(editions[0]["edition"], json!("quran-uthmani"));
    let text = editions[0]["text"].as_str().unwrap();
    assert!(text.contains("بِسْمِ"), "uthmani text: {text}");

    // Bundled Persian translation (offline, no AlQuran Cloud call).
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/ayah/1/1?translation=fa.rowwad",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let editions = body["editions"].as_array().unwrap();
    assert!(
        editions
            .iter()
            .any(|e| e["edition"] == "fa.rowwad" && e["text"].is_string()),
        "bundled fa.rowwad present: {body}"
    );
    // Bundled tafsir.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/ayah/1/1?translation=fa.rowwad&tafsir=fa.mokhtasar",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let editions = body["editions"].as_array().unwrap();
    assert!(
        editions
            .iter()
            .any(|e| e["edition"] == "fa.mokhtasar" && e["text"].is_string()),
        "bundled fa.mokhtasar present: {body}"
    );

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/ayah/1/8", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "ayah_not_found");

    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/ayah/2/286?translation=fa.rowwad",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ayah"], json!(286));
}

#[tokio::test]
async fn quran_reciters_and_timing() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/recitation/reciters", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let reciters = body["reciters"].as_array().unwrap();
    assert_eq!(reciters.len(), 12, "12 timing reciters");
    assert_eq!(reciters[0]["id"], json!("alafasy"));
    let generated = body["generated_reciters"].as_array().unwrap();
    assert!(
        generated
            .iter()
            .any(|g| g["id"] == "ar.husary" && g["surahs"] == 2),
        "{body}"
    );

    // Generated timing: only surahs 1 and 67 are served for ar.husary.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/recitation/timing/generated?edition=ar.husary&surah=1",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["reciter"], json!("ar.husary"));
    assert_eq!(body["surah"], json!(1));
    let ayahs = body["ayahs"].as_array().unwrap();
    assert!(!ayahs.is_empty(), "timed ayahs for Fatiha");
    let words = ayahs[0]["words"].as_array().unwrap();
    assert!(!words.is_empty(), "timing words for Fatiha");

    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/recitation/timing/generated?edition=ar.husary&surah=2",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "recitation_timing_unavailable");
}

#[tokio::test]
async fn quran_providers_and_munajat() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/providers", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["active_provider"], json!("self_hosted"));
    assert_eq!(body["text_edition"], json!("quran-uthmani"));
    assert_eq!(body["reciters"][0], json!("ar.alafasy"));
    let bundled = body["bundled_translations"].as_array().unwrap();
    assert!(bundled.iter().any(|v| v == "fa.rowwad"), "{body}");
    let providers = body["providers"].as_array().unwrap();
    assert_eq!(providers[0]["id"], json!("alquran_cloud"));
    assert_eq!(providers[1]["id"], json!("self_hosted"));

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/munajat/status", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["window"]["start"], json!("02:00"));
    assert_eq!(body["window"]["end"], json!("05:00"));
    assert!(body["enabled"].is_boolean(), "{body}");
}

// ============================ Quran: notes ===========================

#[tokio::test]
async fn quran_notes_crud() {
    let env = Env::sqlite().await;
    // Unauthenticated -> 401.
    let (status, _) = env
        .call(req(
            Method::POST,
            "/api/v1/quran/notes",
            Some(json!({"surah": 1, "ayah": 1, "note_text": "note"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (uid, token, _) = register_user(&env, "notes@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/notes",
                Some(json!({"surah": 1, "ayah": 1, "note_text": "first note"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let note_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["surah"], json!(1));
    assert_eq!(body["ayah"], json!(1));

    // Invalid ayah -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/notes",
                Some(json!({"surah": 1, "ayah": 99, "note_text": "bad"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "ayah_not_found");

    // Second note, then list with limit=1 pagination.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/notes",
                Some(json!({"surah": 2, "ayah": 255, "note_text": "second note"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/notes?limit=1", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let cursor = body["next_cursor"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/notes?limit=1&after={cursor}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert!(body["next_cursor"].is_null(), "{body}");

    // Update (note_out has no version field; the bump is internal).
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/quran/notes/{note_id}"),
                Some(json!({"note_text": "edited note"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["note_text"], json!("edited note"));

    // Another user's note is 404, not 403 (no GET-by-id route exists,
    // so the cross-user check goes through PUT).
    let (_, token2, _) = register_user(&env, "notes2@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/quran/notes/{note_id}"),
                Some(json!({"note_text": "hacked"})),
            ),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "note_not_found");
    let _ = uid;

    // Delete.
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/notes/{note_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Deleted: a further PUT is 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/quran/notes/{note_id}"),
                Some(json!({"note_text": "gone"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

// ============================ Quran: reading state ===========================

#[tokio::test]
async fn quran_reading_state() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "read@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/reading-state", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bookmarks"], json!([]));
    assert_eq!(body["last_read"], Value::Null);

    let now = Utc::now().to_rfc3339();
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/reading-state",
                Some(json!({
                    "bookmarks": [],
                    "last_read": {"surah": 2, "ayah": 255, "view": "mushaf", "mushaf": "1405", "savedAt": 100},
                    "updated_at": 100,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["last_read"]["surah"], json!(2));
    assert_eq!(body["last_read"]["view"], json!("mushaf"));
    assert_eq!(body["updated_at"], json!(100));
    let _ = now;

    // Stale PUT (updated_at < stored) -> 409 with the stored state body,
    // NOT the error envelope.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/reading-state",
                Some(json!({
                    "bookmarks": [],
                    "last_read": {"surah": 1, "ayah": 1, "view": "mushaf", "mushaf": "1405", "savedAt": 50},
                    "updated_at": 50,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        body["last_read"]["surah"],
        json!(2),
        "stored state returned"
    );
    assert!(body["error"].is_null(), "{body}");

    // Invalid bookmark -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/reading-state",
                Some(json!({
                    "bookmarks": [{"id": "b1", "kind": "ayah", "surah": 200, "ayah": 1}],
                    "updated_at": 200,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ============================ Tasbih ===========================

#[tokio::test]
async fn tasbih_flow() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "tasbih@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/tasbih/sessions",
                Some(json!({"dhikr_text": "subhanallah", "target_count": 3})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let session_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["count"], json!(0));
    assert_eq!(body["completed"], json!(false));

    // Idempotent count: replaying the same key returns the same count.
    for _ in 0..2 {
        let (status, body) = env
            .call(authed(
                req(
                    Method::POST,
                    &format!("/api/v1/tasbih/sessions/{session_id}/count"),
                    Some(json!({"amount": 1, "idempotency_key": "k1"})),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["count"], json!(1));
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/tasbih/sessions/{session_id}/count"),
                Some(json!({"amount": 1, "idempotency_key": "k2"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], json!(2));

    // Third count completes the session (target 3).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/tasbih/sessions/{session_id}/count"),
                Some(json!({"amount": 1, "idempotency_key": "k3"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["count"], json!(3));
    assert_eq!(body["completed"], json!(true));
    assert!(body["ended_at"].is_string(), "{body}");

    // Counting an ended session -> 409.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/tasbih/sessions/{session_id}/count"),
                Some(json!({"amount": 1, "idempotency_key": "k4"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "session_ended");

    // Daily aggregate.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/tasbih/today", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let totals = body["totals"].as_array().unwrap();
    assert_eq!(totals.len(), 1);
    assert_eq!(totals[0]["dhikr_text"], json!("subhanallah"));
    assert_eq!(totals[0]["total_count"], json!(3));

    // Another user's session is 404.
    let (_, token2, _) = register_user(&env, "tasbih2@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/tasbih/sessions/{session_id}/count"),
                Some(json!({"amount": 1, "idempotency_key": "k9"})),
            ),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "session_not_found");
    let _ = uid;
}

// ============================ Adhan ===========================

#[tokio::test]
async fn adhan_times_options_range() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/adhan/options", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["methods"].as_array().unwrap().len(), 10);
    assert_eq!(body["methods"][0]["id"], json!("council"));
    assert_eq!(body["cities"].as_array().unwrap().len(), 7);

    // Tehran by city name.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/times?city=tehran&date=2026-03-21",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let times = &body["times"];
    for key in [
        "fajr", "sunrise", "dhuhr", "asr", "maghrib", "isha", "imsak", "midnight",
    ] {
        assert!(times[key].is_string(), "missing {key}: {body}");
    }
    assert_eq!(body["method"], json!("council"));
    assert_eq!(body["madhab"], json!("hanafi"));
    assert_eq!(body["date"], json!("2026-03-21"));

    // Torbat-e Jam coordinates with method council -> the Torbat table.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/times?lat=35.24306&lng=60.625&method=council&date=2026-03-21",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["source"], json!("table"), "{body}");

    // Invalid method -> 422; missing location -> 400.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/times?city=tehran&method=nope",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/adhan/times", None))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "missing_location");

    // Range: end before start -> 400; span > 365 days -> 422.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/range?city=tehran&start=2026-03-22&end=2026-03-21",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_range");
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/range?city=tehran&start=2026-01-01&end=2027-06-01",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "range_too_long");
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/range?city=tehran&start=2026-03-21&end=2026-03-23",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["days"].as_array().unwrap().len(), 3);
}

// ============================ Custom provider key ===========================

#[tokio::test]
async fn quran_custom_provider_key() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "prov@example.com").await;
    // Without platform.settings -> 403.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/registry/custom-providers/foo/key",
                Some(json!({"key": "secret"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    grant(&env, &uid, &["platform.settings"]).await;

    // Unknown custom provider -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/registry/custom-providers/foo/key",
                Some(json!({"key": "secret"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "custom_provider_not_found");

    // Register a custom provider in settings, then store + delete its key.
    set_setting(
        &env,
        "quran.custom_providers",
        &json!([{"id": "myprov", "name": "My Provider", "base_url": "https://example.com", "type": "audio"}]),
    )
    .await;
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/registry/custom-providers/myprov/key",
                Some(json!({"key": "s3cr3t"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["provider_id"], json!("myprov"));
    assert_eq!(body["key_set"], json!(true));
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/registry/custom-providers/myprov/key",
                Some(json!({"key": null})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["key_set"], json!(false));
}

// ============================ Hifz: status ===========================

#[tokio::test]
async fn hifz_status_public() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/hifz/status", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // No ASR engine is embedded in the Rust binary (ADR-031): the
    // status honestly reports the selected engine as unavailable.
    assert_eq!(body["engine"], json!("zipformer"));
    assert_eq!(body["available"], json!(false));
    assert_eq!(body["map_available"], json!(false));
    assert!(body["reason"].is_string(), "{body}");
    // Invalid engine id -> available=false with the invalid-engine reason.
    set_setting(&env, "quran.asr_engine", &json!("bogus")).await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/quran/hifz/status", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["engine"], json!("bogus"));
    assert_eq!(body["available"], json!(false));
}

// ============================ Hifz: attempts ===========================

async fn create_attempt(env: &Env, token: &str) -> Value {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/attempts",
                Some(json!({
                    "surah": 1, "start_ayah": 1, "end_ayah": 7,
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    body
}

#[tokio::test]
async fn hifz_attempts_crud() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "hifz@example.com").await;

    // Unauthenticated -> 401.
    let (status, _) = env
        .call(req(
            Method::POST,
            "/api/v1/quran/hifz/attempts",
            Some(json!({
                "surah": 1, "start_ayah": 1, "end_ayah": 7,
            })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Invalid range -> 422 hifz_range_invalid.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/attempts",
                Some(json!({
                    "surah": 1, "start_ayah": 7, "end_ayah": 1,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_range_invalid");

    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["status"], json!("in_progress"));
    assert_eq!(body["engine"], json!("zipformer"));
    assert_eq!(body["surah"], json!(1));
    assert_eq!(body["start_ayah"], json!(1));
    assert_eq!(body["end_ayah"], json!(7));
    assert_eq!(body["current_surah"], json!(1));
    assert_eq!(body["current_ayah"], json!(1));

    // Resume returns the open attempt.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/attempts/resume", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(attempt_id));

    // List + detail (empty words/mistakes).
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/attempts", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["words"], json!([]));
    assert_eq!(body["mistakes"], json!([]));

    // Position update.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/position"),
                Some(json!({"current_surah": 1, "current_ayah": 3})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["current_ayah"], json!(3));

    // Pause / resume are no-ops in the same state.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/pause"),
                Some(json!({})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("paused"));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/resume"),
                Some(json!({})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("in_progress"));

    // Complete, then position -> 409 and resume -> null.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/complete"),
                Some(json!({})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("completed"));
    assert!(body["completed_at"].is_string(), "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/position"),
                Some(json!({"current_surah": 1, "current_ayah": 4})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "hifz_attempt_completed");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/attempts/resume", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, Value::Null);

    // Cross-user access is 404.
    let (_, token2, _) = register_user(&env, "hifz2@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}"),
                None,
            ),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "hifz_attempt_not_found");

    // Delete.
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ============================ Hifz: peek / mistakes / dispute ===========================

#[tokio::test]
async fn hifz_peek_and_dispute() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "peek@example.com").await;
    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();

    // Peek the first word of 1:1 (kind=word = "<" next word).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "word", "word_indices": [0]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revealed"].as_array().unwrap().len(), 1);
    assert_eq!(body["revealed"][0]["word"], json!("بِسْمِ"));
    assert_eq!(body["revealed"][0]["word_index"], json!(0));

    // Peek whole ayah (kind=ayah = "<<" rest of ayah).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "ayah", "word_indices": [1, 2]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revealed"].as_array().unwrap().len(), 2);

    // Bad word index -> 422 hifz_word_index; bad kind -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "word", "word_indices": [99]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_word_index");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "nope", "word_indices": [0]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_peek_kind");

    // Peek mistakes are grouped (peek excluded from the grouped view).
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/mistakes"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0, "peek excluded");

    // The mistake rows exist in the attempt detail; delete one.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mistakes = body["mistakes"].as_array().unwrap();
    assert_eq!(mistakes.len(), 3);
    let mistake_id = mistakes[0]["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/hifz/mistakes/{mistake_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Dispute without a word score -> 404 hifz_word_score_not_found
    // (no grade endpoint is native, so no score row exists).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/dispute"),
                Some(json!({"word_score_id": Uuid::new_v4().to_string()})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "hifz_word_score_not_found");
}

// ============================ Hifz: settings / listen position ===========================

#[tokio::test]
async fn hifz_settings() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "settings@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["daily_goal_ayahs"], json!(0));
    assert_eq!(body["freeze_on_error"], json!(false));

    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/hifz/settings",
                Some(json!({"daily_goal_ayahs": 5, "freeze_on_error": true})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["daily_goal_ayahs"], json!(5));
    assert_eq!(body["freeze_on_error"], json!(true));

    // Out-of-range goal -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/hifz/settings",
                Some(json!({"daily_goal_ayahs": 7000})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn hifz_listen_position() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "listen@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/listen-position", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, Value::Null);

    // Unknown reciter / bad speed / bad repeat -> 422 domain codes.
    let base = json!({
        "reciter": "alafasy", "surah": 2, "ayah": 1, "progress_ms": 0,
        "speed": 1.0, "repeat_mode": "off",
    });
    let mut bad = base.clone();
    bad["reciter"] = json!("no.such.reciter");
    let (status, body) = env
        .call(authed(
            req(Method::PUT, "/api/v1/quran/hifz/listen-position", Some(bad)),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_listen_reciter_unknown");
    let mut bad = base.clone();
    bad["speed"] = json!(3.0);
    let (status, body) = env
        .call(authed(
            req(Method::PUT, "/api/v1/quran/hifz/listen-position", Some(bad)),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_listen_speed_invalid");
    let mut bad = base.clone();
    bad["repeat_mode"] = json!("nope");
    let (status, body) = env
        .call(authed(
            req(Method::PUT, "/api/v1/quran/hifz/listen-position", Some(bad)),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_listen_repeat_invalid");
    let mut bad = base.clone();
    bad["repeat_mode"] = json!("range");
    let (status, body) = env
        .call(authed(
            req(Method::PUT, "/api/v1/quran/hifz/listen-position", Some(bad)),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_listen_range_missing");

    // Valid PUT applies.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/hifz/listen-position",
                Some(base.clone()),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applied"], json!(true));
    assert_eq!(body["position"]["surah"], json!(2));
    let stored = &body["position"];
    let stored_updated_at = stored["updated_at"].as_str().unwrap().to_string();

    // Stale PUT -> 200 applied=false, conflict=true, stored position.
    let mut stale = base.clone();
    stale["updated_at"] = json!("2000-01-01T00:00:00Z");
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/quran/hifz/listen-position",
                Some(stale),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applied"], json!(false));
    assert_eq!(body["conflict"], json!(true));
    assert_eq!(body["position"]["updated_at"], json!(stored_updated_at));

    // Delete is idempotent.
    let (status, _) = env
        .call(authed(
            req(Method::DELETE, "/api/v1/quran/hifz/listen-position", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/listen-position", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, Value::Null);
}

// ============================ Hifz: stats / review / admin ===========================

#[tokio::test]
async fn hifz_stats_review_admin() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "stats@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/stats", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], json!(0));
    assert_eq!(body["due_ayahs"], json!(0));

    // A peek-only attempt makes the ayah due.
    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 2, "kind": "word", "word_indices": [0]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/stats", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], json!(1));
    assert_eq!(body["due_ayahs"], json!(1), "{body}");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/review", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["surah"], json!(1));
    assert_eq!(body[0]["ayah"], json!(2));

    // Admin list needs platform.settings.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/admin/attempts", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    grant(&env, &uid, &["platform.settings"]).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/admin/attempts", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Admin delete (no ownership check) removes the attempt.
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/hifz/admin/attempts/{attempt_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/stats", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], json!(0));
}

// ============================ Hifz: SRS reviews ===========================

#[tokio::test]
async fn hifz_reviews_sm2() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "review@example.com").await;

    // Neither quality nor counts -> 422 hifz_review_quality.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/reviews",
                Some(json!({"surah": 1, "ayah": 1})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_review_quality");

    // Quality 5: first SM-2 review -> reps 1, interval 1, ease 2.6.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/reviews",
                Some(json!({"surah": 1, "ayah": 1, "quality": 5})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repetitions"], json!(1));
    assert_eq!(body["interval_days"], json!(1));
    assert!(
        (body["ease"].as_f64().unwrap() - 2.6).abs() < 1e-9,
        "{body}"
    );
    assert!(body["due_at"].is_string(), "{body}");

    // Counts derive quality: 5 correct / 0 wrong -> quality 5.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/reviews",
                Some(json!({"surah": 1, "ayah": 2, "counts": {"correct": 5, "wrong": 0}})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repetitions"], json!(1));

    // Quality 2 is a lapse: reps reset to 0, interval 1, ease -0.2.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/reviews",
                Some(json!({"surah": 1, "ayah": 1, "quality": 2})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repetitions"], json!(0));
    assert_eq!(body["interval_days"], json!(1));
    assert!(
        (body["ease"].as_f64().unwrap() - 2.08).abs() < 1e-9,
        "{body}"
    );

    // Push 1:1's due_at into the past, then /reviews/due lists it (srs).
    env.db
        .execute(
            "UPDATE hifz_reviews SET due_at = datetime('now', '-1 day') WHERE surah = 1 AND ayah = 1",
            &[],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/reviews/due", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["surah"], json!(1));
    assert_eq!(items[0]["ayah"], json!(1));
    assert_eq!(items[0]["source"], json!("srs"));
    assert_eq!(items[0]["repetitions"], json!(0));
}

// ============================ Hifz: plans ===========================

#[tokio::test]
async fn hifz_plans() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "plans@example.com").await;

    // Bad kind / bad range -> 422 domain codes.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/plans",
                Some(json!({
                    "title": "t", "kind": "nope",
                    "start_surah": 1, "start_ayah": 1, "end_surah": 1, "end_ayah": 7,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_plan_kind");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/plans",
                Some(json!({
                    "title": "t", "kind": "memorize",
                    "start_surah": 2, "start_ayah": 1, "end_surah": 1, "end_ayah": 7,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_plan_range");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/plans",
                Some(json!({
                    "title": "Fatiha", "kind": "memorize",
                    "start_surah": 1, "start_ayah": 1, "end_surah": 1, "end_ayah": 7,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plan_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["total_ayahs"], json!(7));
    assert_eq!(body["done_ayahs"], json!(0));

    // Get -> 7 items, reverse order for direction=reverse is checked below.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/plans/{plan_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 7);
    assert_eq!(items[0]["surah"], json!(1));
    assert_eq!(items[0]["ayah"], json!(1));
    let item_id = items[0]["id"].as_str().unwrap().to_string();

    // PATCH item -> plan + item; done_ayahs becomes 1.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/plans/{plan_id}/items/{item_id}"),
                Some(json!({"status": "done"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["done_ayahs"], json!(1));
    assert_eq!(body["item"]["status"], json!("done"));

    // Invalid plan status -> 422 hifz_plan_status.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/plans/{plan_id}"),
                Some(json!({"status": "nope"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_plan_status");

    // SRS rows survive plan deletion: record a review, delete the plan,
    // the review is still due-listable.
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/reviews",
                Some(json!({"surah": 1, "ayah": 1, "quality": 5})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/hifz/plans/{plan_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/plans", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
}

// ============================ Hifz: dashboard / activity / suggested ===========================

#[tokio::test]
async fn hifz_dashboard_activity_suggested() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "dash@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/dashboard", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], json!(0));
    assert_eq!(body["streak_current"], json!(0));
    assert_eq!(body["goal_ayahs"], json!(0));
    assert_eq!(body["due_count"], json!(0));
    assert_eq!(body["weakest_ayahs"], json!([]));
    assert_eq!(body["daily_series"].as_array().unwrap().len(), 30);

    // Suggested: no memorized ayah yet -> setup_required.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/suggested", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], json!(false));
    assert_eq!(body["reason"], json!("setup_required"));

    // Activity: days=7 -> 7 entries; days=3 -> 422.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/activity?days=7", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["days"].as_array().unwrap().len(), 7);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/activity?days=3", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Memorize 1:1 (plan item done) but <10 graded words -> insufficient_history.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/plans",
                Some(json!({
                    "title": "Fatiha", "kind": "memorize",
                    "start_surah": 1, "start_ayah": 1, "end_surah": 1, "end_ayah": 7,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plan_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/plans/{plan_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let item_id = body["items"][0]["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/plans/{plan_id}/items/{item_id}"),
                Some(json!({"status": "done"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/suggested", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], json!(false));
    assert_eq!(body["reason"], json!("insufficient_history"));

    // Seed 10 graded words + a mistake on 1:1 via SQL (no ASR in the
    // Rust binary, so word scores cannot be produced through the API).
    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();
    let now = Utc::now();
    for i in 0..10i64 {
        env.db
            .execute(
                "INSERT INTO hifz_word_scores (id, attempt_id, surah, ayah, word_index, expected_text, status, prob, heard, mistake_type, disputed, created_at, updated_at) \
                 VALUES (?, ?, 1, 1, ?, ?, 'wrong', 0.4, NULL, 'wrong_word', FALSE, ?, ?)",
                &[
                    Uuid::new_v4().into_arg(),
                    as_uuid(&attempt_id).into_arg(),
                    i.into_arg(),
                    format!("w{i}").into_arg(),
                    now.into_arg(),
                    now.into_arg(),
                ],
            )
            .await
            .unwrap();
    }
    env.db
        .execute(
            "INSERT INTO hifz_mistakes (id, attempt_id, user_id, surah, ayah, word_index, mistake_type, expected_text, heard, created_at) \
             VALUES (?, ?, ?, 1, 1, 0, 'wrong_word', 'w0', NULL, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(&attempt_id).into_arg(),
                as_uuid(&uid).into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/suggested", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], json!(true), "{body}");
    let cards = body["cards"].as_array().unwrap();
    assert_eq!(cards.len(), 1);
    assert_eq!(cards[0]["kind"], json!("weakest_link"));
    assert_eq!(cards[0]["surah"], json!(1));
    assert_eq!(cards[0]["ayah"], json!(1));
}

// ============================ Hifz: tests ===========================

#[tokio::test]
async fn hifz_tests_flow() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "tests@example.com").await;

    // Validation order: length -> stop_rule -> range.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tests",
                Some(json!({
                    "surah_start": 1, "surah_end": 2, "question_length": "huge",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_test_length");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tests",
                Some(json!({
                    "surah_start": 2, "surah_end": 1,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_test_range");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tests",
                Some(json!({
                    "surah_start": 1, "surah_end": 2,
                    "question_count": 2, "question_length": "medium",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let test_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["status"], json!("in_progress"));
    // Create returns the raw passages (no question ids); GET for the rows.
    let questions = body["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 2);
    assert!(questions[0]["prompt"].is_string(), "{body}");
    assert!(questions[0]["surah"].as_i64().unwrap() <= 2, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/tests/{test_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let questions = body["questions"].as_array().unwrap();
    assert_eq!(questions.len(), 2);
    let q1 = questions[0]["id"].as_str().unwrap().to_string();
    let q2 = questions[1]["id"].as_str().unwrap().to_string();
    let q2_surah = questions[1]["surah"].as_i64().unwrap();
    let q2_ayah_start = questions[1]["ayah_start"].as_i64().unwrap();

    // Skip the first question; skipping again -> 422 closed.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/tests/{test_id}/questions/{q1}/skip"),
                Some(json!({})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("skipped"));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/tests/{test_id}/questions/{q1}/skip"),
                Some(json!({})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_test_question_closed");

    // Peek on the second question, then remove the peek event.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/tests/{test_id}/questions/{q2}/peek"),
                Some(json!({"surah": q2_surah, "ayah": q2_ayah_start, "word_index": 0})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["peek_count"], json!(1));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/tests/{test_id}/questions/{q2}/events/0/remove"),
                Some(json!({"removed": true})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["peek_count"], json!(0));
    assert_eq!(body["removed"], json!(true));

    // Complete, then list shows answered/skipped counts.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/tests/{test_id}/complete"),
                Some(json!({})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("completed"));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/tests", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["answered"], json!(0));

    // Delete.
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/hifz/tests/{test_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ============================ Hifz: mutashabihat ===========================

#[tokio::test]
async fn hifz_mutashabihat() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "mut@example.com").await;

    // The basmala phrase opens 1:1 and (in the Tanzil text) every other
    // surah's first ayah except 9:1, plus 27:30 — 114 occurrences.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/quran/hifz/mutashabihat?surah=1&ayah=1",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let groups = body.as_array().unwrap();
    assert!(!groups.is_empty(), "{body}");
    let basmala = groups
        .iter()
        .find(|g| g["phrase"].as_str().unwrap().contains("بسم"));
    assert!(basmala.is_some(), "{body}");
    assert!(
        basmala.unwrap()["occurrences"].as_array().unwrap().len() >= 2,
        "{body}"
    );

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/quran/hifz/mutashabihat/phrase?phrase=%D8%A8%D8%B3%D9%85%20%D8%A7%D9%84%D9%84%D9%87%20%D8%A7%D9%84%D8%B1%D8%AD%D9%85%D9%86%20%D8%A7%D9%84%D8%B1%D8%AD%D9%8A%D9%85",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["occurrences"].as_array().unwrap().len(), 114);

    // /count returns the total number of known groups (no phrase param).
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/mutashabihat/count", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["phrase_count"].as_i64().unwrap() > 0, "{body}");

    // Missing params -> 422.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/mutashabihat", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ============================ Hifz: tasmi ===========================

#[tokio::test]
async fn hifz_tasmi_flow() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "tasmi@example.com").await;

    // Bad method -> 422; bad tasmi8 variant -> 500 (Python's ValueError).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tasmi/sessions",
                Some(json!({
                    "method": "nope", "start_surah": 1, "start_ayah": 1,
                    "end_surah": 1, "end_ayah": 2,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_tasmi_method");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tasmi/sessions",
                Some(json!({
                    "method": "tasmi8", "variant": "nope",
                    "start_surah": 1, "start_ayah": 1, "end_surah": 1, "end_ayah": 2,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");

    // method_6446: read 6 / recite 4 / read 4 / recite 6.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tasmi/sessions",
                Some(json!({
                    "method": "method_6446", "start_surah": 1, "start_ayah": 1,
                    "end_surah": 1, "end_ayah": 2,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let session_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["method"], json!("method_6446"));
    assert_eq!(body["current"]["set_pos"], json!(0));
    assert_eq!(body["current"]["set_kind"], json!("read"));
    assert_eq!(body["current"]["set_target"], json!(6));
    // Tap 6 times -> next set (recite 4).
    for _ in 0..6 {
        let (status, body) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/quran/hifz/tasmi/sessions/{session_id}"),
                    Some(json!({"action": "tap"})),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{session_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["current"]["set_pos"], json!(1));
    assert_eq!(body["current"]["set_kind"], json!("recite"));
    assert_eq!(body["current"]["set_target"], json!(4));

    // tasmi8: 8 stages, next_stage walks them; rate sets a review due.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tasmi/sessions",
                Some(json!({
                    "method": "tasmi8", "start_surah": 1, "start_ayah": 1,
                    "end_surah": 1, "end_ayah": 2,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let t8 = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["current"]["stage"], json!("meet"));
    assert_eq!(body["current"]["stage_total"], json!(8));
    assert_eq!(body["state"]["stages"].as_array().unwrap().len(), 8);
    // tap is not a tasmi8 action -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{t8}"),
                Some(json!({"action": "tap"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_tasmi_action");
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{t8}"),
                Some(json!({"action": "next_stage"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["current"]["stage"], json!("first_recall"));
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{t8}"),
                Some(json!({"action": "rate", "rating": "good"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"]["last_rating"], json!("good"));
    assert!(body["state"]["review_due_at"].is_string(), "{body}");

    // Pause / resume / complete; actions on a completed session -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{t8}"),
                Some(json!({"action": "pause"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("paused"));
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{t8}"),
                Some(json!({"action": "complete"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("completed"));
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{t8}"),
                Some(json!({"action": "next_stage"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "hifz_tasmi_closed");

    // Delete the 6446 session.
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/quran/hifz/tasmi/sessions/{session_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ============================ Hifz: data purge ===========================

#[tokio::test]
async fn hifz_data_purge() {
    let env = Env::sqlite().await;
    // The purge rate limit is 3/hour per session, and it counts failed
    // attempts too — so the validation failures use user A while the
    // successful purge uses user B.
    let (uid, token, _) = register_user(&env, "purge@example.com").await;
    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "word", "word_indices": [0]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/data/preview", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(2), "{body}"); // 1 attempt + 1 mistake
    assert_eq!(body["counts"]["hifz_attempts"], json!(1));

    // No confirm -> 422; extra field -> 422; bad idempotency key -> 422.
    let key = Uuid::new_v4().to_string();
    for (payload, code) in [
        (
            json!({"confirm": false, "idempotency_key": key}),
            "hifz_purge_not_confirmed",
        ),
        (
            json!({"confirm": true, "idempotency_key": key, "extra": 1}),
            "validation_error",
        ),
        (
            json!({"confirm": true, "idempotency_key": "not-a-uuid"}),
            "hifz_purge_bad_idempotency_key",
        ),
    ] {
        let (status, body) = env
            .call(authed(
                req(Method::POST, "/api/v1/quran/hifz/data/purge", Some(payload)),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(err_code(&body), code, "{body}");
    }
    let _ = uid;

    // Confirmed purge (user B, fresh rate-limit bucket) deletes
    // everything and reports the counts.
    let (_uid2, token2, _) = register_user(&env, "purge2@example.com").await;
    let body = create_attempt(&env, &token2).await;
    let attempt_id2 = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id2}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "word", "word_indices": [0]})),
            ),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/data/purge",
                Some(json!({"confirm": true, "idempotency_key": key})),
            ),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["deleted"]["hifz_attempts"], json!(1));
    assert_eq!(body["deleted"]["hifz_mistakes"], json!(1));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/data/preview", None),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));
}

// ============================ Hifz: ASR proxy + live stream ===========================

#[tokio::test]
async fn hifz_grade_sidecar_and_stream_unavailable() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "grade@example.com").await;
    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();

    // The grade endpoint is NOT native (no ASR in the Rust binary):
    // it is a sidecar-contract route (ADR-134), and with SIDECAR_URL
    // unset the sidecar answers 503 hifz_asr_unavailable.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/grade"),
                Some(json!({"surah": 1, "ayah": 1})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "hifz_asr_unavailable");

    // The live WebSocket stream cannot be relayed by the HTTP proxy, so
    // the route is registered explicitly as 503 hifz_live_unavailable.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/stream"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "hifz_live_unavailable");
}

// ============================ Postgres smoke ===========================

/// Postgres smoke test, gated on `WAVE2_PG_URL`. Runs the critical
/// path (quran ayah, notes, hifz attempt + peek + review, plan, test,
/// dashboard) against a local Postgres test database. Run the test
/// binaries sequentially — the smokes share one database.
#[tokio::test]
async fn wave4_pg_smoke() {
    let url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            eprintln!("WAVE2_PG_URL not set — skipping Postgres smoke test");
            return;
        }
    };
    let db = Db::connect(&url).await.expect("pg connect");
    // Drop every table `TEST_SCHEMA` creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run (Wave 2
    // uses the same database) would fail the schema apply. The table
    // list is derived from the schema itself, so it cannot go stale.
    let mut tables = Vec::new();
    for part in maturidi_api_rs::TEST_SCHEMA.split("CREATE TABLE") {
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
    init_test_schema(&db).await.expect("pg schema");
    let env = Env {
        state: AppState::with_db(test_config(), Some(db.clone())),
        db,
    };

    let (_uid, token, _) = register_user(&env, "smoke@example.com").await;
    set_setting(&env, "quran.translations", &json!(["fa.rowwad"])).await;
    set_setting(&env, "quran.tafsirs", &json!(["fa.mokhtasar"])).await;
    set_setting(&env, "quran.reciters", &json!([])).await;

    // Quran: surah + ayah from the embedded Tanzil text.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/surah/1?translation=fa.rowwad",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ayahs"].as_array().unwrap().len(), 7);
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/quran/ayah/1/1?translation=fa.rowwad",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let editions = body["editions"].as_array().unwrap();
    assert_eq!(editions[0]["edition"], json!("quran-uthmani"));
    assert!(
        editions[0]["text"].as_str().unwrap().contains("بِسْمِ"),
        "{body}"
    );

    // Notes: create -> list (UUID + JSONB on PG).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/notes",
                Some(json!({"surah": 1, "ayah": 1, "note_text": "smoke note"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/notes", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // Adhan: Tehran times.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/adhan/times?city=tehran&date=2026-03-21",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["times"]["fajr"].is_string(), "{body}");

    // Hifz: attempt -> peek -> stats -> review -> plan -> test -> dashboard.
    let body = create_attempt(&env, &token).await;
    let attempt_id = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/quran/hifz/attempts/{attempt_id}/peek"),
                Some(json!({"surah": 1, "ayah": 1, "kind": "word", "word_indices": [0]})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/stats", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], json!(1));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/reviews",
                Some(json!({"surah": 1, "ayah": 1, "quality": 5})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["repetitions"], json!(1));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/plans",
                Some(json!({
                    "title": "Smoke", "kind": "memorize",
                    "start_surah": 1, "start_ayah": 1, "end_surah": 1, "end_ayah": 7,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/quran/hifz/tests",
                Some(json!({
                    "surah_start": 1, "surah_end": 1, "question_count": 1,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["questions"].as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/quran/hifz/dashboard", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["attempts"], json!(1));
    assert_eq!(body["daily_series"].as_array().unwrap().len(), 30);
}
