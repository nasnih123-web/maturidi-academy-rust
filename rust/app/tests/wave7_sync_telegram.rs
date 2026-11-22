//! Wave 7 integration tests: sync + telegram.
//!
//! Sync: idempotent push (note / progress / dhikr), pull by cursor,
//! device cursors, note conflicts + resolution, tombstones +
//! resurrection, offline-pack.
//!
//! Telegram: webhook secret verification + callback apply, connect
//! status/disconnect, connect code paths (honest 503 without the
//! MTProto client), policies, publish / log / retry / approve /
//! reject, import gating.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot`.
//! The Postgres smoke test at the end
//! (`wave7_sync_telegram_pg_smoke`, gated on `WAVE2_PG_URL`) replays
//! the sync push → pull → cursor path and the telegram
//! connect-status → policies path against a local Postgres test
//! database (schema dropped and re-applied first).

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::{Duration, Utc};
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
        jwt_secret: "test-jwt-secret-for-wave7-sync-telegram-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave7".to_string(),
        captcha_enabled: false,
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

fn req_with_headers(
    method: Method,
    uri: &str,
    body: Option<Value>,
    headers: &[(&str, &str)],
) -> Request<Body> {
    let mut r = req(method, uri, body);
    for (k, v) in headers {
        r.headers_mut().insert(
            axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            v.parse().unwrap(),
        );
    }
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

/// Register a user with `telegram.manage`.
async fn telegram_admin(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["telegram.manage"]).await;
    (uid, token)
}

/// Register a user with `fatwa.approve` (callback actor).
async fn approver_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["fatwa.approve"]).await;
    (uid, token)
}

/// Seed one `platform_settings` row exactly as the app writes it
/// (Fernet envelope).
async fn seed_setting(env: &Env, key: &str, plaintext: &str) {
    let envelope = encrypt_value(&env.state.config, plaintext);
    env.db
        .execute(
            "INSERT INTO platform_settings (key, value, encryption_key_version) VALUES (?, ?, ?) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, encryption_key_version = excluded.encryption_key_version",
            &[
                (key).into_arg(),
                (envelope).into_arg(),
                (env.state.config.encryption_key_version).into_arg(),
            ],
        )
        .await
        .unwrap();
}

/// HMAC-SHA256 hex (the `tg:` callback signature), implemented with
/// sha2 so the test does not depend on the app's internal helper.
fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut k = key.to_vec();
    if k.len() > 64 {
        k = Sha256::digest(&k).to_vec();
    }
    k.resize(64, 0);
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(msg);
    let inner_hash = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner_hash);
    let out = outer.finalize();
    out.iter().map(|b| format!("{b:02x}")).collect()
}

async fn seed_lesson(env: &Env) -> Uuid {
    let lesson_id = Uuid::new_v4();
    let course_id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO lessons (id, course_id, title, body, duration_minutes, sort_order, \
             is_free_preview, created_at) VALUES (?, ?, ?, '[]', 10, 0, FALSE, ?)",
            &[
                (lesson_id).into_arg(),
                (course_id).into_arg(),
                (json!({"fa": "درس تست"})).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    lesson_id
}

async fn seed_library_item(env: &Env) -> Uuid {
    let item_id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO library_items (id, title, workflow_status, visibility, kind, \
             created_at, updated_at) VALUES (?, ?, 'published', 'public', 'video', ?, ?)",
            &[
                (item_id).into_arg(),
                (json!({"fa": "کتاب تست"})).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    item_id
}

async fn push_op(env: &Env, token: &str, device_id: &str, op: Value) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            "/api/v1/sync/push",
            Some(json!({ "device_id": device_id, "ops": [op] })),
        ),
        token,
    ))
    .await
}

// ============================ Sync =====================================

#[tokio::test]
async fn sync_push_note_pull_cursor() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync1@example.com").await;
    let note_id = Uuid::new_v4();
    let (status, body) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-note-1",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 2, "ayah": 255, "note_text": "یادداشت تست", "is_private": true},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("applied"));
    assert_eq!(body["results"][0]["version"], json!(1));

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/sync/pull?cursor=0&device_id=dev-1",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["new_cursor"], json!(1));
    assert_eq!(body["changes"].as_array().unwrap().len(), 1);
    assert_eq!(body["changes"][0]["entity_type"], json!("user_ayah_note"));
    assert_eq!(body["changes"][0]["op_type"], json!("upsert"));
    assert_eq!(
        body["changes"][0]["payload"]["note_text"],
        json!("یادداشت تست")
    );
    assert_eq!(body["changes"][0]["device_id"], json!("dev-1"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/sync/cursor?device_id=dev-1", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cursor_seq"], json!(1));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/sync/devices", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["device_id"], json!("dev-1"));
    assert_eq!(body["items"][0]["cursor_seq"], json!(1));
}

#[tokio::test]
async fn sync_push_replay_idempotent() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync2@example.com").await;
    let note_id = Uuid::new_v4();
    let op = json!({
        "client_op_id": "op-replay-1",
        "entity_type": "user_ayah_note",
        "entity_id": note_id.to_string(),
        "op_type": "upsert",
        "payload": {"surah": 1, "ayah": 1, "note_text": "فاتحه"},
        "base_version": 0,
    });
    let (status, first) = push_op(&env, &token, "dev-1", op.clone()).await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = push_op(&env, &token, "dev-1", op).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(second["results"][0], first["results"][0]);
    // Exactly one changelog row: the replay did not re-apply.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/sync/pull?cursor=0&device_id=dev-1",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changes"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn sync_push_progress_max_wins() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync3@example.com").await;
    let lesson_id = seed_lesson(&env).await;
    let op = |client_op_id: &str, status: &str, pos: i64| {
        json!({
            "client_op_id": client_op_id,
            "entity_type": "user_progress",
            "entity_id": lesson_id.to_string(),
            "op_type": "upsert",
            "payload": {"status": status, "position_seconds": pos},
            "base_version": 0,
        })
    };
    let (status, body) = push_op(&env, &token, "dev-1", op("op-p1", "in_progress", 10)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("applied"));
    assert_eq!(body["results"][0]["version"], json!(1));
    // A smaller position never wins; the version still bumps.
    let (status, body) = push_op(&env, &token, "dev-1", op("op-p2", "in_progress", 5)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["version"], json!(2));
    // A higher status rank wins.
    let (status, body) = push_op(&env, &token, "dev-1", op("op-p3", "completed", 7)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["version"], json!(3));
    let row = env
        .db
        .fetch_one(
            "SELECT status, position_seconds, version FROM user_progress WHERE lesson_id = ?",
            &[lesson_id.into_arg()],
        )
        .await
        .unwrap();
    let status_s: String = row.try_get("status").unwrap();
    let pos: i32 = row.try_get("position_seconds").unwrap();
    let version: i32 = row.try_get("version").unwrap();
    assert_eq!(status_s, "completed");
    assert_eq!(pos, 10);
    assert_eq!(version, 3);
    // Unknown lesson → rejected, never a conflict.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/sync/push",
                Some(json!({
                    "device_id": "dev-1",
                    "ops": [{
                        "client_op_id": "op-p4",
                        "entity_type": "user_progress",
                        "entity_id": Uuid::new_v4().to_string(),
                        "op_type": "upsert",
                        "payload": {"status": "in_progress", "position_seconds": 1},
                    }],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("rejected"));
    assert_eq!(body["results"][0]["detail"], json!("lesson_not_found"));
}

#[tokio::test]
async fn sync_push_dhikr_max_wins() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync4@example.com").await;
    let entity_id = "2026-11-22:سبحان الله";
    let op = |client_op_id: &str, total: i64| {
        json!({
            "client_op_id": client_op_id,
            "entity_type": "dhikr_daily_count",
            "entity_id": entity_id,
            "op_type": "upsert",
            "payload": {"total_count": total},
            "base_version": 0,
        })
    };
    let (status, body) = push_op(&env, &token, "dev-1", op("op-d1", 33)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["version"], json!(1));
    let (status, body) = push_op(&env, &token, "dev-1", op("op-d2", 10)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["version"], json!(2));
    let row = env
        .db
        .fetch_one(
            "SELECT total_count FROM dhikr_daily_counts WHERE dhikr_text = ?",
            &[("سبحان الله").into_arg()],
        )
        .await
        .unwrap();
    let total: i32 = row.try_get("total_count").unwrap();
    assert_eq!(total, 33);
    // A payload date that disagrees with the entity id is rejected.
    let (status, body) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-d3",
            "entity_type": "dhikr_daily_count",
            "entity_id": entity_id,
            "op_type": "upsert",
            "payload": {"total_count": 40, "date": "2026-11-23"},
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("rejected"));
    assert_eq!(
        body["results"][0]["detail"],
        json!("dhikr_id_payload_mismatch")
    );
}

#[tokio::test]
async fn sync_note_conflict_and_resolve() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync5@example.com").await;
    let note_id = Uuid::new_v4();
    let (status, body) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-c1",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 2, "ayah": 255, "note_text": "نسخهٔ اول"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Concurrent edit from another device (stale base_version).
    let (status, body) = push_op(
        &env,
        &token,
        "dev-2",
        json!({
            "client_op_id": "op-c2",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 2, "ayah": 255, "note_text": "نسخهٔ دوم"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("conflicted"));
    assert_eq!(body["results"][0]["detail"], json!("concurrent_edit"));
    let conflict_id = body["results"][0]["conflict_id"]
        .as_str()
        .expect("conflict id")
        .to_string();
    // Same content from a stale base is applied, not conflicted.
    let (status, body) = push_op(
        &env,
        &token,
        "dev-2",
        json!({
            "client_op_id": "op-c3",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 2, "ayah": 255, "note_text": "نسخهٔ اول"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("applied"));
    assert_eq!(body["results"][0]["detail"], json!("already_in_sync"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/sync/conflicts", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["id"], json!(conflict_id));
    assert_eq!(body["items"][0]["resolution"], json!("pending"));
    assert_eq!(
        body["items"][0]["local_payload"]["note_text"],
        json!("نسخهٔ دوم")
    );
    assert_eq!(
        body["items"][0]["remote_payload"]["note_text"],
        json!("نسخهٔ اول")
    );

    // Merged without a payload is a 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/sync/conflicts/{conflict_id}/resolve"),
                Some(json!({ "resolution": "merged" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "merged_payload_required");

    // keep_local wins: the device text becomes version 2.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/sync/conflicts/{conflict_id}/resolve"),
                Some(json!({ "resolution": "keep_local", "note": "دستگاه برنده" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["resolution"], json!("keep_local"));
    assert_eq!(body["applied_version"], json!(2));
    let row = env
        .db
        .fetch_one(
            "SELECT note_text, version FROM user_ayah_notes WHERE id = ?",
            &[note_id.into_arg()],
        )
        .await
        .unwrap();
    let text: String = row.try_get("note_text").unwrap();
    assert_eq!(text, "نسخهٔ دوم");

    // Already resolved → 409; pending list is empty, full list has it.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/sync/conflicts/{conflict_id}/resolve"),
                Some(json!({ "resolution": "keep_remote" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "conflict_already_resolved");
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/sync/conflicts?only_pending=false",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["resolution"], json!("keep_local"));
    assert_eq!(body["items"][0]["resolution_note"], json!("دستگاه برنده"));
}

#[tokio::test]
async fn sync_delete_tombstone_and_resurrect() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync6@example.com").await;
    let note_id = Uuid::new_v4();
    let (status, _) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-t1",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 36, "ayah": 1, "note_text": "یس"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Delete (base_version 1, the note's version).
    let (status, body) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-t2",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "delete",
            "payload": {},
            "base_version": 1,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("applied"));
    assert_eq!(body["results"][0]["version"], json!(1));
    // Pull publishes the delete too.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/sync/pull?cursor=0&device_id=dev-1",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["changes"].as_array().unwrap().len(), 2);
    assert_eq!(body["changes"][1]["op_type"], json!("delete"));
    // A stale upsert against the tombstone conflicts (nothing lost).
    let (status, body) = push_op(
        &env,
        &token,
        "dev-2",
        json!({
            "client_op_id": "op-t3",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 36, "ayah": 1, "note_text": "یس — دوباره"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("conflicted"));
    assert_eq!(body["results"][0]["detail"], json!("upsert_on_tombstone"));
    let conflict_id = body["results"][0]["conflict_id"]
        .as_str()
        .expect("conflict id")
        .to_string();
    // keep_local resurrects the note at version 1.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/sync/conflicts/{conflict_id}/resolve"),
                Some(json!({ "resolution": "keep_local" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["applied_version"], json!(1));
    let row = env
        .db
        .fetch_one(
            "SELECT COUNT(*) AS total FROM user_ayah_notes WHERE id = ?",
            &[note_id.into_arg()],
        )
        .await
        .unwrap();
    let n: i64 = row.try_get("total").unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn sync_offline_pack_started_first() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync7@example.com").await;
    let lesson_id = seed_lesson(&env).await;
    let _item_id = seed_library_item(&env).await;
    let (status, _) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-k1",
            "entity_type": "user_progress",
            "entity_id": lesson_id.to_string(),
            "op_type": "upsert",
            "payload": {"status": "in_progress", "position_seconds": 3},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/sync/offline-pack", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["kind"], json!("lesson"));
    assert_eq!(items[0]["ref_id"], json!(lesson_id.to_string()));
    assert_eq!(items[0]["score"], json!(100));
    assert_eq!(items[0]["reason"], json!("started"));
    assert_eq!(items[1]["kind"], json!("library_item"));
    assert_eq!(items[1]["score"], json!(50));
    assert_eq!(items[1]["reason"], json!("popular"));
    assert!(body["generated_at"].is_string());
}

#[tokio::test]
async fn sync_auth_and_validation() {
    let env = Env::sqlite().await;
    let (_uid, token, _) = register_user(&env, "sync8@example.com").await;
    // No token → 401.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/sync/push",
            Some(json!({ "device_id": "dev-1", "ops": [] })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Empty ops → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/sync/push",
                Some(json!({ "device_id": "dev-1", "ops": [] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    // Unknown entity type → 422 (Pydantic Literal).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/sync/push",
                Some(json!({
                    "device_id": "dev-1",
                    "ops": [{
                        "client_op_id": "op-x",
                        "entity_type": "user_reading_state",
                        "entity_id": "x",
                        "op_type": "upsert",
                    }],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Pull without device_id → 422.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/sync/pull?cursor=0", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // Unknown conflict → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/sync/conflicts/{}/resolve", Uuid::new_v4()),
                Some(json!({ "resolution": "keep_local" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "conflict_not_found");
    // A client_op_id owned by another user → 409 for the whole batch.
    let (_uid2, token2, _) = register_user(&env, "sync8b@example.com").await;
    let note_id = Uuid::new_v4();
    let (status, _) = push_op(
        &env,
        &token,
        "dev-1",
        json!({
            "client_op_id": "op-owned",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 1, "ayah": 2, "note_text": "مال من"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = push_op(
        &env,
        &token2,
        "dev-9",
        json!({
            "client_op_id": "op-owned",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 1, "ayah": 2, "note_text": "مال دیگری"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "sync_op_id_conflict");
}

// ============================ Telegram =====================================

#[tokio::test]
async fn telegram_status_public_disabled() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/telegram/status/public", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "enabled": false }));
    // Enabled needs telegram.enabled AND a token.
    seed_setting(&env, "telegram.enabled", "true").await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/telegram/status/public", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "enabled": false }));
    seed_setting(&env, "telegram.bot_token", "123456:ABC").await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/telegram/status/public", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "enabled": true }));
}

#[tokio::test]
async fn telegram_connect_status_and_disconnect() {
    let env = Env::sqlite().await;
    let (_uid, token) = telegram_admin(&env, "tg1@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/connect/status", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "connected": false, "phone_masked": null }));
    seed_setting(&env, "telegram.session", "1:session-bytes").await;
    seed_setting(&env, "telegram.phone_masked", "+98****7890").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/connect/status", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body,
        json!({ "connected": true, "phone_masked": "+98****7890" })
    );
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/telegram/connect/disconnect", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "connected": false, "phone_masked": null }));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/connect/status", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "connected": false, "phone_masked": null }));
    // A plain user (no telegram.manage) gets 403.
    let (_uid2, token2, _) = register_user(&env, "tg1b@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/connect/status", None),
            &token2,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
}

#[tokio::test]
async fn telegram_connect_code_paths_unavailable() {
    let env = Env::sqlite().await;
    let (_uid, token) = telegram_admin(&env, "tg2@example.com").await;
    // Bad phone → 422 before anything else.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/connect/request-code",
                Some(json!({ "phone": "abc" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // No api_id/api_hash configured → 422 telegram_not_configured.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/connect/request-code",
                Some(json!({ "phone": "+989123456789" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "telegram_not_configured");
    // Non-numeric api_id → 422 telegram_bad_api_id.
    seed_setting(&env, "telegram.api_id", "not-a-number").await;
    seed_setting(&env, "telegram.api_hash", "hashvalue").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/connect/request-code",
                Some(json!({ "phone": "+989123456789" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "telegram_bad_api_id");
    // Configured, but no MTProto client in the Rust binary → honest 503.
    seed_setting(&env, "telegram.api_id", "12345").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/connect/request-code",
                Some(json!({ "phone": "+989123456789" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "telegram_unavailable");
    // Confirm: expired pending code → 422.
    seed_setting(&env, "telegram.pending_phone", "+989123456789").await;
    seed_setting(&env, "telegram.pending_phone_code_hash", "hash").await;
    let past = (Utc::now() - Duration::minutes(11)).to_rfc3339();
    seed_setting(&env, "telegram.pending_expires_at", &past).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/connect/confirm-code",
                Some(json!({ "phone": "+989123456789", "code": "12345" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "code_expired_or_invalid");
    // Valid pending code, configured creds → still 503 (no MTProto).
    let future = (Utc::now() + Duration::minutes(9)).to_rfc3339();
    seed_setting(&env, "telegram.pending_expires_at", &future).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/connect/confirm-code",
                Some(json!({ "phone": "+989123456789", "code": "12345" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "telegram_unavailable");
}

#[tokio::test]
async fn telegram_webhook_secret_and_callback() {
    let env = Env::sqlite().await;
    // No secret configured at all → 401.
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            Some(json!({})),
            &[("x-telegram-bot-api-secret-token", "s3cret")],
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "bad_webhook_secret");
    seed_setting(&env, "telegram.webhook_secret", "s3cret").await;
    // Wrong secret → 401.
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            Some(json!({})),
            &[("x-telegram-bot-api-secret-token", "wrong")],
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    // Right secret, invalid JSON → 400 bad_update.
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            None,
            &[("x-telegram-bot-api-secret-token", "s3cret")],
        ))
        .await;
    // Empty body is not JSON: the handler answers 400 bad_update.
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "bad_update");
    // Right secret, plain update → ignored.
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            Some(json!({ "update_id": 1, "message": { "text": "hi" } })),
            &[("x-telegram-bot-api-secret-token", "s3cret")],
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "ok": true, "ignored": true }));
    // Unknown nonce → 404 callback_not_found.
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            Some(json!({ "callback_query": { "id": "cq-1", "data": "tg:deadbeef" } })),
            &[("x-telegram-bot-api-secret-token", "s3cret")],
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "callback_not_found");
}

#[tokio::test]
async fn telegram_webhook_callback_approves_submission() {
    let env = Env::sqlite().await;
    let (actor_id, _actor_token) = approver_user(&env, "tg3@example.com").await;
    seed_setting(&env, "telegram.webhook_secret", "s3cret").await;
    // A fatwa submission in_review.
    let submission_id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO fatwa_submissions (id, title, body, author_id, workflow_status, \
             version, created_at, updated_at) VALUES (?, ?, ?, ?, 'in_review', 1, ?, ?)",
            &[
                (submission_id).into_arg(),
                (json!({"fa": "سوال فقهی"})).into_arg(),
                ("متن سوال".to_string()).into_arg(),
                (as_uuid(&actor_id)).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    // A one-shot callback bound to (submission, approve, actor).
    let nonce = "a1b2c3d4e5f60718";
    let signature = hmac_sha256_hex(
        test_config().encryption_key.as_bytes(),
        format!("{nonce}:approve:{submission_id}").as_bytes(),
    );
    let expires = Utc::now() + Duration::hours(24);
    env.db
        .execute(
            "INSERT INTO telegram_callbacks (nonce, source_content_type, source_content_id, \
             op, actor_user_id, signature, expires_at, created_at) \
             VALUES (?, 'fatwa_submission', ?, 'approve', ?, ?, ?, ?)",
            &[
                (nonce).into_arg(),
                (submission_id.to_string()).into_arg(),
                (as_uuid(&actor_id)).into_arg(),
                (signature).into_arg(),
                (expires).into_arg(),
                (now).into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            Some(json!({ "callback_query": { "id": "cq-9", "data": format!("tg:{nonce}") } })),
            &[("x-telegram-bot-api-secret-token", "s3cret")],
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["result"],
        json!({
            "content_type": "fatwa_submission",
            "content_id": submission_id.to_string(),
            "op": "approve",
            "to": "approved",
        })
    );
    let row = env
        .db
        .fetch_one(
            "SELECT workflow_status FROM fatwa_submissions WHERE id = ?",
            &[submission_id.into_arg()],
        )
        .await
        .unwrap();
    let st: String = row.try_get("workflow_status").unwrap();
    assert_eq!(st, "approved");
    // Second use of the same button → 409 callback_used.
    let (status, body) = env
        .call(req_with_headers(
            Method::POST,
            "/api/v1/telegram/webhook",
            Some(json!({ "callback_query": { "id": "cq-10", "data": format!("tg:{nonce}") } })),
            &[("x-telegram-bot-api-secret-token", "s3cret")],
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "callback_used");
}

#[tokio::test]
async fn telegram_policies_crud() {
    let env = Env::sqlite().await;
    let (_uid, token) = telegram_admin(&env, "tg4@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/distribute/policies", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));
    // Unknown policy name → 422 bad_policy.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/telegram/distribute/policies/library_item",
                Some(json!({ "policy": "bogus" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_policy");
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/telegram/distribute/policies/library_item",
                Some(json!({
                    "policy": "manual_approval",
                    "target_channels": ["@chan1"],
                    "spacing_seconds": 30,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["content_type"], json!("library_item"));
    assert_eq!(body["policy"], json!("manual_approval"));
    assert_eq!(body["target_channels"], json!(["@chan1"]));
    assert_eq!(body["spacing_seconds"], json!(30));
    assert_eq!(body["require_approval"], json!(true));
    assert_eq!(body["is_enabled"], json!(true));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/distribute/policies", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    // Partial update: only the sent fields change.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/telegram/distribute/policies/library_item",
                Some(json!({ "spacing_seconds": 5 })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["policy"], json!("never"));
    assert_eq!(body["spacing_seconds"], json!(5));
    assert_eq!(body["target_channels"], json!(["@chan1"]));
}

#[tokio::test]
async fn telegram_publish_flow() {
    let env = Env::sqlite().await;
    let (_uid, token) = telegram_admin(&env, "tg5@example.com").await;
    let item_id = seed_library_item(&env).await;
    // No target channel anywhere → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/distribute/publish",
                Some(json!({ "content_type": "library_item", "content_id": item_id.to_string() })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "no_target_channel");
    // Policy with a target channel, then publish.
    let (status, _) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/telegram/distribute/policies/library_item",
                Some(json!({ "policy": "manual_approval", "target_channels": ["@chan1"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/distribute/publish",
                Some(json!({ "content_type": "library_item", "content_id": item_id.to_string() })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["status"], json!("pending"));
    assert_eq!(body[0]["error"], json!("worker unavailable — retry later"));
    assert_eq!(body[0]["distribution_target"], json!("@chan1"));
    let published_id = body[0]["id"].as_str().unwrap().to_string();
    // Idempotent re-publish returns the same row.
    let (status, body2) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/distribute/publish",
                Some(json!({ "content_type": "library_item", "content_id": item_id.to_string() })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body2}");
    assert_eq!(body2[0]["id"], json!(published_id));
    // Log.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/telegram/distribute/log?content_type=library_item",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    // Retry keeps it pending (no worker to enqueue to).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/telegram/distribute/retry/{published_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("pending"));
    // Approve runs send_pending; without a bot runtime it fails honestly.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/telegram/distribute/approve/{published_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("failed"));
    assert_eq!(body["retry_count"], json!(1));
    assert_eq!(body["error"], json!("bot unavailable or send failed"));
    // Reject the failed row, then approve is a 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/telegram/distribute/reject/{published_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("rejected"));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/telegram/distribute/approve/{published_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_status");
    // Unknown published row → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/telegram/distribute/retry/{}", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "publish_not_found");
    // Content that does not exist → 404 content_not_found.
    let (status, _) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/telegram/distribute/policies/religious_place",
                Some(json!({ "target_channels": ["@chan1"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/distribute/publish",
                Some(json!({ "content_type": "religious_place", "content_id": Uuid::new_v4().to_string() })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "content_not_found");
}

#[tokio::test]
async fn telegram_import_requires_connection() {
    let env = Env::sqlite().await;
    let (_uid, token) = telegram_admin(&env, "tg6@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/import",
                Some(json!({ "channel": "@chan", "limit": 10 })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "telegram_not_connected");
    seed_setting(&env, "telegram.session", "1:session-bytes").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/telegram/import",
                Some(json!({ "channel": "@chan", "limit": 10 })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(
        body,
        json!({ "enqueued": false, "channel": "@chan", "limit": 10 })
    );
}

// ============================ Postgres smoke ==============================

/// Critical paths on Postgres: sync push → pull → cursor, and
/// telegram connect-status → policies. Gated on `WAVE2_PG_URL`.
#[tokio::test]
async fn wave7_sync_telegram_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok().filter(|u| !u.is_empty()) else {
        eprintln!("wave7_sync_telegram_pg_smoke: WAVE2_PG_URL unset, skipping");
        return;
    };
    let db = Db::connect(&url).await.expect("postgres connect");
    // Drop every table `TEST_SCHEMA` creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run would
    // fail the schema apply. The list is derived from the schema itself
    // (wave6 pattern). Binaries must run sequentially — the smokes
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

    // Sync: push a note, pull it back, read the cursor.
    let (_uid, token, _) = register_user(&env, "pg-sync@example.com").await;
    let note_id = Uuid::new_v4();
    let (status, body) = push_op(
        &env,
        &token,
        "pg-dev",
        json!({
            "client_op_id": "op-pg-1",
            "entity_type": "user_ayah_note",
            "entity_id": note_id.to_string(),
            "op_type": "upsert",
            "payload": {"surah": 2, "ayah": 255, "note_text": "یادداشت"},
            "base_version": 0,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["status"], json!("applied"));
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/sync/pull?cursor=0&device_id=pg-dev",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["new_cursor"], json!(1));
    assert_eq!(body["changes"].as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/sync/cursor?device_id=pg-dev", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["cursor_seq"], json!(1));

    // Telegram: connect-status (disconnected) → policies round-trip.
    let (_uid2, admin_token) = telegram_admin(&env, "pg-tg@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/connect/status", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "connected": false, "phone_masked": null }));
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/telegram/distribute/policies/library_item",
                Some(json!({ "policy": "auto_publish", "target_channels": ["@chan1"] })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["policy"], json!("auto_publish"));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/telegram/distribute/policies", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["target_channels"], json!(["@chan1"]));
}
