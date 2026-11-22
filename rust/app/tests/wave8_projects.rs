//! Wave 8 integration tests: projects (PMS).
//!
//! Project CRUD + cursor pagination, the staff-reader gate, write
//! permission enforcement, task CRUD + kanban `move` transitions,
//! assignee present-vs-absent PATCH semantics, staff CRUD (409 /
//! 404s), the `by-entity` reverse lookup, assign notifications +
//! domain events, audit rows, and the due-soon cycle (counts, 20 h
//! anti-spam, unassigned rows still stamped).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives a test-only Router built directly
//! from the `routes_projects` handlers — `build_router` is NOT used
//! (the parent wires `lib.rs` later). Users are seeded by SQL and
//! tokens are minted in-test with the same HS256 claims shape as
//! `jwt::create_access_token` (`sid` omitted, so no session row is
//! needed). The Postgres smoke test at the end
//! (`wave8_projects_pg_smoke`, gated on `WAVE2_PG_URL`) replays
//! create project → create task → assign → move → due-soon cycle
//! against a local Postgres test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    routing::{get, post},
    Router,
};
use chrono::{Duration, Utc};
use maturidi_api_rs::{
    init_test_schema, routes_projects as rp, AppState, Config, Db, IntoArg, TEST_SCHEMA,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

const JWT_SECRET: &str = "test-jwt-secret-wave8";

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: JWT_SECRET.to_string(),
        encryption_key: "test-encryption-key-wave8".to_string(),
        captcha_enabled: false,
        // No origin: native routes must never touch another server.
        storage_dir: format!(
            "{}/wave8-projects-storage-{}",
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

    /// Test-only router built directly from the projects handlers.
    fn router(&self) -> Router {
        Router::new()
            .route(
                "/api/v1/projects",
                get(rp::list_projects).post(rp::create_project),
            )
            .route("/api/v1/projects/tasks/by-entity", get(rp::tasks_by_entity))
            .route(
                "/api/v1/projects/{project_id}",
                get(rp::get_project)
                    .patch(rp::update_project)
                    .delete(rp::delete_project),
            )
            .route(
                "/api/v1/projects/{project_id}/tasks",
                get(rp::list_tasks).post(rp::create_task),
            )
            .route(
                "/api/v1/projects/tasks/{task_id}",
                get(rp::get_task)
                    .patch(rp::update_task)
                    .delete(rp::delete_task),
            )
            .route("/api/v1/projects/tasks/{task_id}/move", post(rp::move_task))
            .route("/api/v1/staff", get(rp::list_staff).post(rp::create_staff))
            .route(
                "/api/v1/staff/{staff_id}",
                get(rp::get_staff)
                    .patch(rp::update_staff)
                    .delete(rp::delete_staff),
            )
            .route(
                "/api/v1/admin/projects/due-soon-cycle",
                post(rp::due_soon_cycle),
            )
            .with_state(self.state.clone())
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

/// Mint an HS256 access token with the same claims shape as
/// `jwt::create_access_token` (`sid` omitted → no session lookup).
fn mint_token(user_id: &Uuid) -> String {
    let now = Utc::now().timestamp();
    let claims = json!({
        "sub": user_id.to_string(),
        "type": "access",
        "scope": "full",
        "iat": now,
        "exp": now + 3600,
        "jti": format!("test-{}", Uuid::new_v4()),
    });
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256),
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(JWT_SECRET.as_bytes()),
    )
    .expect("mint token")
}

/// Seed a user by SQL. Returns (user_id, token).
async fn seed_user(env: &Env, email: &str) -> (Uuid, String) {
    let id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO users (id, full_name, email, password_hash, locale, \
             is_active, is_verified, created_at) VALUES (?, ?, ?, 'x', 'fa', TRUE, TRUE, ?)",
            &[
                (id).into_arg(),
                format!("Test {email}").into_arg(),
                (email).into_arg(),
                (Utc::now()).into_arg(),
            ],
        )
        .await
        .unwrap();
    (id, mint_token(&id))
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
async fn grant(env: &Env, user_id: Uuid, keys: &[&str]) {
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
            &[user_id.into_arg(), role_id.into_arg()],
        )
        .await
        .unwrap();
}

/// Seed a user with `projects.manage`. Returns (user_id, token).
async fn admin_user(env: &Env, email: &str) -> (Uuid, String) {
    let (uid, token) = seed_user(env, email).await;
    grant(env, uid, &["projects.manage"]).await;
    (uid, token)
}

/// Seed an active `staff_members` row for a user. Returns the staff id.
async fn seed_staff(env: &Env, user_id: Uuid, is_active: bool) -> Uuid {
    let id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO staff_members (id, user_id, title, department, joined_date, \
             is_active, created_at, updated_at) VALUES (?, ?, ?, ?, NULL, ?, ?, NULL)",
            &[
                (id).into_arg(),
                (user_id).into_arg(),
                (json!({"fa": "توسعه‌دهنده", "ar": "مطور", "en": "Developer"})).into_arg(),
                (json!({"fa": "فنی", "ar": "تقني", "en": "Tech"})).into_arg(),
                (is_active).into_arg(),
                (Utc::now()).into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

fn loc(fa: &str) -> Value {
    json!({ "fa": fa, "ar": fa, "en": fa })
}

async fn create_project(env: &Env, token: &str, title: &str) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            "/api/v1/projects",
            Some(json!({ "title": loc(title) })),
        ),
        token,
    ))
    .await
}

async fn create_task(env: &Env, token: &str, project_id: &str, body: Value) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            &format!("/api/v1/projects/{project_id}/tasks"),
            Some(body),
        ),
        token,
    ))
    .await
}

async fn count_rows(env: &Env, sql: &str, args: &[maturidi_api_rs::Arg]) -> i64 {
    let row = env.db.fetch_one(sql, args).await.unwrap();
    row.try_get::<i64, _>("n").unwrap()
}

// ============================ projects =====================================

#[tokio::test]
async fn projects_crud_pagination_and_bad_cursor() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "proj-admin@example.com").await;

    // Create three projects; create returns task_count 0 without a query.
    let mut ids = Vec::new();
    for title in ["پروژه یک", "پروژه دو", "پروژه سه"] {
        let (status, body) = create_project(&env, &admin_token, title).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["task_count"], json!(0));
        assert_eq!(body["status"], json!("active"));
        assert_eq!(body["created_by"], json!(_admin_id.to_string()));
        assert_eq!(body["updated_at"], Value::Null);
        ids.push(body["id"].as_str().unwrap().to_string());
    }

    // Get one (live task_count, still 0).
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/{}", ids[0]), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["task_count"], json!(0));
    assert_eq!(body["title"]["fa"], json!("پروژه یک"));

    // Patch: title + status; updated_at is set.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/projects/{}", ids[0]),
                Some(json!({ "title": loc("پروژه یکم"), "status": "on_hold" })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("on_hold"));
    assert_eq!(body["title"]["fa"], json!("پروژه یکم"));
    assert!(body["updated_at"].is_string(), "{body}");

    // Pagination: limit=2 → 2 items + next_cursor; follow it → 1 left.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects?limit=2", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let cursor = body["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/projects?limit=2&cursor={cursor}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["next_cursor"], Value::Null);

    // Bad cursor → 400 bad_cursor.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects?cursor=!!!not-base64", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "bad_cursor");

    // Bad status on create → 422; missing title → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/projects",
                Some(json!({ "title": loc("x"), "status": "bogus" })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/projects", Some(json!({}))),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn projects_unauthenticated_401() {
    let env = Env::sqlite().await;
    let (status, body) = env.call(req(Method::GET, "/api/v1/projects", None)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/projects",
            Some(json!({ "title": loc("x") })),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
}

#[tokio::test]
async fn staff_reader_gate() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "gate-admin@example.com").await;
    let (plain_id, plain_token) = seed_user(&env, "gate-plain@example.com").await;

    // Plain user: 403 forbidden with the staff-only message.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects", None),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
    assert_eq!(
        body["error"]["message"].as_str().unwrap(),
        "دسترسی کافی نیست — فقط اعضای تیم"
    );

    // Same user with an active staff row: 200 without the permission.
    seed_staff(&env, plain_id, true).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects", None),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Inactive staff row does NOT open the gate.
    let (inactive_id, inactive_token) = seed_user(&env, "gate-inactive@example.com").await;
    seed_staff(&env, inactive_id, false).await;
    let (status, _) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects", None),
            &inactive_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Permission holder: 200 (already covered, but explicit).
    let (status, _) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn writes_require_projects_manage() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "w-admin@example.com").await;
    let (_plain_id, plain_token) = seed_user(&env, "w-plain@example.com").await;
    let (_staff_id, staff_token) = seed_user(&env, "w-staff@example.com").await;
    seed_staff(&env, _staff_id, true).await;

    // Staff member without the permission cannot write.
    for (method, uri, body) in [
        (
            Method::POST,
            "/api/v1/projects".to_string(),
            Some(json!({ "title": loc("x") })),
        ),
        (Method::POST, "/api/v1/staff".to_string(), None),
    ] {
        let (status, resp) = env
            .call(authed(req(method, &uri, body), &staff_token))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{resp}");
        assert_eq!(err_code(&resp), "forbidden");
    }
    // Plain user cannot write either.
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/projects",
                Some(json!({ "title": loc("x") })),
            ),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // PATCH / DELETE on an existing project → 403 for the staff reader.
    let (_, created) = create_project(&env, &admin_token, "قابل‌حذف").await;
    let pid = created["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/projects/{pid}"),
                Some(json!({ "status": "archived" })),
            ),
            &staff_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/projects/{pid}"), None),
            &staff_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn project_not_found_404() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "nf-admin@example.com").await;
    let missing = Uuid::new_v4();
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/{missing}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "project_not_found");
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/projects/{missing}"),
                Some(json!({ "status": "completed" })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/projects/{missing}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn project_delete_removes_tasks() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "del-admin@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "حذف‌شدنی").await;
    let pid = project["id"].as_str().unwrap().to_string();
    let mut task_ids = Vec::new();
    for i in 0..2 {
        let (status, task) = create_task(
            &env,
            &admin_token,
            &pid,
            json!({ "title": loc(&format!("تسک {i}")) }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{task}");
        task_ids.push(task["id"].as_str().unwrap().to_string());
    }
    let (status, _) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/projects/{pid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    // Project gone, and both tasks gone with it.
    let (status, _) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/{pid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    for tid in &task_ids {
        let (status, body) = env
            .call(authed(
                req(Method::GET, &format!("/api/v1/projects/tasks/{tid}"), None),
                &admin_token,
            ))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(err_code(&body), "task_not_found");
    }
    // Audit row records the deleted task count.
    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM audit_logs WHERE action = 'projects.project.delete' AND entity_id = ?",
        &[pid.clone().into_arg()],
    )
    .await;
    assert_eq!(n, 1);
    let row = env
        .db
        .fetch_one(
            "SELECT old_values FROM audit_logs WHERE action = 'projects.project.delete' AND entity_id = ?",
            &[pid.into_arg()],
        )
        .await
        .unwrap();
    let old: Value = row.try_get("old_values").unwrap();
    assert_eq!(old["deleted_tasks"], json!(2));
}

// ============================ tasks ========================================

#[tokio::test]
async fn tasks_crud_and_filters() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "t-admin@example.com").await;
    let (assignee_id, _assignee_token) = seed_user(&env, "t-assignee@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "پروژه تسک").await;
    let pid = project["id"].as_str().unwrap().to_string();

    let (status, task) = create_task(
        &env,
        &admin_token,
        &pid,
        json!({
            "title": loc("تسک اول"),
            "description": loc("شرح"),
            "assignee_id": assignee_id.to_string(),
            "priority": "high",
            "related_entity_type": "library_item",
            "related_entity_id": Uuid::new_v4().to_string(),
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    assert_eq!(task["status"], json!("backlog"));
    assert_eq!(task["priority"], json!("high"));
    assert_eq!(task["assignee_id"], json!(assignee_id.to_string()));
    assert_eq!(task["project_id"], json!(pid));
    let tid = task["id"].as_str().unwrap().to_string();

    // Second task, no assignee, defaults.
    let (status, task2) =
        create_task(&env, &admin_token, &pid, json!({ "title": loc("تسک دوم") })).await;
    assert_eq!(status, StatusCode::CREATED, "{task2}");
    assert_eq!(task2["status"], json!("backlog"));
    assert_eq!(task2["priority"], json!("medium"));
    assert_eq!(task2["assignee_id"], Value::Null);

    // List: both; status filter; assignee filter.
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/{pid}/tasks"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/projects/{pid}/tasks?assignee_id={assignee_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], json!(tid));

    // Bad status filter → 422; bad limit → 422; missing project → 404.
    let (status, _) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/projects/{pid}/tasks?status=bogus"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, _) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/projects/{pid}/tasks?limit=0"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/projects/{}/tasks", Uuid::new_v4()),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "project_not_found");

    // Get + patch title + delete.
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/tasks/{tid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/projects/tasks/{tid}"),
                Some(json!({ "title": loc("تسک اول — ویرایش") })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["fa"], json!("تسک اول — ویرایش"));
    assert!(body["updated_at"].is_string());
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/projects/tasks/{tid}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/tasks/{tid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn task_move_transitions() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "mv-admin@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "کانبان").await;
    let pid = project["id"].as_str().unwrap().to_string();
    let (_, task) = create_task(&env, &admin_token, &pid, json!({ "title": loc("حرکت") })).await;
    let tid = task["id"].as_str().unwrap().to_string();

    async fn move_to(env: &Env, token: &str, tid: &str, status: &str) -> (StatusCode, Value) {
        env.call(authed(
            req(
                Method::POST,
                &format!("/api/v1/projects/tasks/{tid}/move"),
                Some(json!({ "status": status })),
            ),
            token,
        ))
        .await
    }

    // Illegal edge: backlog → review → 422 with {from, to, allowed}.
    let (status, body) = move_to(&env, &admin_token, &tid, "review").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_status_transition");
    assert_eq!(body["error"]["details"]["from"], json!("backlog"));
    assert_eq!(body["error"]["details"]["to"], json!("review"));
    assert_eq!(body["error"]["details"]["allowed"], json!(["todo"]));

    // Walk the board: backlog → todo → in_progress → review → done → todo.
    for (from_to, expected) in [
        ("todo", "todo"),
        ("in_progress", "in_progress"),
        ("review", "review"),
        ("done", "done"),
        ("todo", "todo"),
    ] {
        let (status, body) = move_to(&env, &admin_token, &tid, from_to).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], json!(expected));
    }
    // done → backlog is illegal (only todo reopens): fresh task walked
    // to done, so it is still in "done" when the illegal edge is tried.
    let (_, task2) = create_task(&env, &admin_token, &pid, json!({ "title": loc("حرکت ۲") })).await;
    let tid2 = task2["id"].as_str().unwrap().to_string();
    for step in ["todo", "in_progress", "review", "done"] {
        let (status, body) = move_to(&env, &admin_token, &tid2, step).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = move_to(&env, &admin_token, &tid2, "backlog").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["details"]["from"], json!("done"));
    assert_eq!(body["error"]["details"]["to"], json!("backlog"));
    assert_eq!(body["error"]["details"]["allowed"], json!(["todo"]));
    // Unknown status value → 422 validation, not the transition error.
    let (status, body) = move_to(&env, &admin_token, &tid, "bogus").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    // Missing task → 404.
    let (status, body) = move_to(&env, &admin_token, &Uuid::new_v4().to_string(), "todo").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "task_not_found");
}

#[tokio::test]
async fn task_patch_assignee_semantics() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "pa-admin@example.com").await;
    let (user_a, _) = seed_user(&env, "pa-a@example.com").await;
    let (user_b, _) = seed_user(&env, "pa-b@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "محول").await;
    let pid = project["id"].as_str().unwrap().to_string();
    let due = (Utc::now() + Duration::hours(5)).to_rfc3339();
    let (_, task) = create_task(
        &env,
        &admin_token,
        &pid,
        json!({ "title": loc("تسک"), "description": loc("شرح"), "due_date": due }),
    )
    .await;
    let tid = task["id"].as_str().unwrap().to_string();

    async fn patch_task(env: &Env, token: &str, tid: &str, body: Value) -> (StatusCode, Value) {
        env.call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/projects/tasks/{tid}"),
                Some(body),
            ),
            token,
        ))
        .await
    }

    // PATCH without assignee_id: stays unassigned.
    let (status, body) =
        patch_task(&env, &admin_token, &tid, json!({ "priority": "urgent" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["assignee_id"], Value::Null);
    assert_eq!(body["priority"], json!("urgent"));

    // PATCH assignee_id present → assign.
    let (status, body) = patch_task(
        &env,
        &admin_token,
        &tid,
        json!({ "assignee_id": user_a.to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["assignee_id"], json!(user_a.to_string()));

    // PATCH with another field only: assignee kept.
    let (status, body) = patch_task(&env, &admin_token, &tid, json!({ "priority": "low" })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["assignee_id"], json!(user_a.to_string()));

    // PATCH assignee_id present-null → unassign.
    let (status, body) = patch_task(
        &env,
        &admin_token,
        &tid,
        json!({ "assignee_id": Value::Null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["assignee_id"], Value::Null);

    // Re-assign to B, then null due_date/description CANNOT clear them.
    let (status, body) = patch_task(
        &env,
        &admin_token,
        &tid,
        json!({ "assignee_id": user_b.to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = patch_task(
        &env,
        &admin_token,
        &tid,
        json!({ "due_date": Value::Null, "description": Value::Null }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["assignee_id"], json!(user_b.to_string()));
    assert_eq!(body["due_date"], json!(due));
    assert_eq!(body["description"]["fa"], json!("شرح"));

    // Assign to a missing user → 404 assignee_not_found.
    let (status, body) = patch_task(
        &env,
        &admin_token,
        &tid,
        json!({ "assignee_id": Uuid::new_v4().to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "assignee_not_found");
}

#[tokio::test]
async fn task_create_assignee_not_found() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "an-admin@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "پ").await;
    let pid = project["id"].as_str().unwrap().to_string();
    let (status, body) = create_task(
        &env,
        &admin_token,
        &pid,
        json!({ "title": loc("x"), "assignee_id": Uuid::new_v4().to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "assignee_not_found");
}

#[tokio::test]
async fn tasks_by_entity_lookup() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "be-admin@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "پ").await;
    let pid = project["id"].as_str().unwrap().to_string();
    let entity_id = Uuid::new_v4();
    let other_id = Uuid::new_v4();
    for (etype, eid) in [
        ("library_item", entity_id),
        ("library_item", other_id),
        ("article", entity_id),
    ] {
        let (status, _) = create_task(
            &env,
            &admin_token,
            &pid,
            json!({
                "title": loc("وصل"),
                "related_entity_type": etype,
                "related_entity_id": eid.to_string(),
            }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    // Exact (type, id) match only.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!(
                    "/api/v1/projects/tasks/by-entity?related_entity_type=library_item&related_entity_id={entity_id}"
                ),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    // Missing params → 422 (and by-entity is NOT captured as a task id).
    let (status, _) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/projects/tasks/by-entity?related_entity_type=library_item",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects/tasks/by-entity", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn task_assigned_notification_event_audit() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "nt-admin@example.com").await;
    let (assignee_id, _) = seed_user(&env, "nt-assignee@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "پ").await;
    let pid = project["id"].as_str().unwrap().to_string();

    // Create with assignee → in_app notification row + task.assigned
    // event + projects.task.assign audit.
    let (status, task) = create_task(
        &env,
        &admin_token,
        &pid,
        json!({ "title": loc("تسک مهم"), "assignee_id": assignee_id.to_string() }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let tid = task["id"].as_str().unwrap().to_string();

    let row = env
        .db
        .fetch_one(
            "SELECT notif_type, title, body, data FROM notifications WHERE user_id = ?",
            &[assignee_id.into_arg()],
        )
        .await
        .unwrap();
    let notif_type: String = row.try_get("notif_type").unwrap();
    assert_eq!(notif_type, "task");
    let title: Value = row.try_get("title").unwrap();
    assert_eq!(title["fa"], json!("تسک جدید به شما محول شد"));
    let body_text: String = row.try_get("body").unwrap();
    assert_eq!(body_text, "«تسک مهم» به شما محول شد.");
    let data: Value = row.try_get("data").unwrap();
    assert_eq!(data["source"], json!("pms"));
    assert_eq!(data["kind"], json!("assigned"));
    assert_eq!(data["task_id"], json!(tid));
    assert_eq!(data["project_id"], json!(pid));

    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM domain_events WHERE aggregate_id = ? AND event_type = 'task.assigned'",
        &[tid.clone().into_arg()],
    )
    .await;
    assert_eq!(n, 1);
    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM audit_logs WHERE action = 'projects.task.assign' AND entity_id = ?",
        &[tid.clone().into_arg()],
    )
    .await;
    assert_eq!(n, 1);

    // Re-assign via PATCH → second assign event + second notification.
    let (user_b, _) = seed_user(&env, "nt-b@example.com").await;
    let (status, _) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/projects/tasks/{tid}"),
                Some(json!({ "assignee_id": user_b.to_string() })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM domain_events WHERE aggregate_id = ? AND event_type = 'task.assigned'",
        &[tid.clone().into_arg()],
    )
    .await;
    assert_eq!(n, 2);
    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM notifications WHERE user_id = ?",
        &[user_b.into_arg()],
    )
    .await;
    assert_eq!(n, 1);
}

// ============================ staff ========================================

#[tokio::test]
async fn staff_crud_and_active_filter() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "st-admin@example.com").await;
    let (user_id, _) = seed_user(&env, "st-user@example.com").await;

    let (status, staff) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/staff",
                Some(json!({
                    "user_id": user_id.to_string(),
                    "title": loc("مدرس"),
                    "department": loc("آموزش"),
                    "joined_date": "2026-01-15",
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{staff}");
    assert_eq!(staff["user_id"], json!(user_id.to_string()));
    assert_eq!(staff["joined_date"], json!("2026-01-15"));
    assert_eq!(staff["is_active"], json!(true));
    let sid = staff["id"].as_str().unwrap().to_string();

    // Get + list (active_only default true).
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/staff/{sid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/staff", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Deactivate → hidden by default, visible with active_only=false.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/staff/{sid}"),
                Some(json!({ "is_active": false, "title": loc("مدرس ارشد") })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["is_active"], json!(false));
    assert_eq!(body["title"]["fa"], json!("مدرس ارشد"));
    assert!(body["updated_at"].is_string());
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/staff", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/staff?active_only=false", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Delete → 204, then 404.
    let (status, _) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/staff/{sid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/staff/{sid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "staff_not_found");
}

#[tokio::test]
async fn staff_conflicts_and_not_found() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "sc-admin@example.com").await;
    let (user_id, _) = seed_user(&env, "sc-user@example.com").await;

    // Unknown user → 404 user_not_found.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/staff",
                Some(json!({
                    "user_id": Uuid::new_v4().to_string(),
                    "title": loc("x"),
                    "department": loc("y"),
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "user_not_found");

    // Create once, then duplicate → 409 staff_exists.
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/staff",
                Some(json!({
                    "user_id": user_id.to_string(),
                    "title": loc("x"),
                    "department": loc("y"),
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/staff",
                Some(json!({
                    "user_id": user_id.to_string(),
                    "title": loc("x"),
                    "department": loc("y"),
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "staff_exists");

    // Unknown staff id → 404 on GET/PATCH/DELETE.
    let missing = Uuid::new_v4();
    let (status, _) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/staff/{missing}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/staff/{missing}"),
                Some(json!({ "is_active": false })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env
        .call(authed(
            req(Method::DELETE, &format!("/api/v1/staff/{missing}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ============================ due-soon cycle ===============================

#[tokio::test]
async fn due_soon_cycle_counts_and_antispam() {
    let env = Env::sqlite().await;
    let (_admin_id, admin_token) = admin_user(&env, "ds-admin@example.com").await;
    let (assignee_id, _) = seed_user(&env, "ds-assignee@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "پ").await;
    let pid = project["id"].as_str().unwrap().to_string();
    let now = Utc::now();

    // Candidates: due in 2h (assigned), due in 2h (unassigned),
    // overdue 1h (assigned). Non-candidates: due in 30h, done + due 2h.
    let mut due_soon_assigned = Uuid::nil();
    let mut due_soon_unassigned = Uuid::nil();
    for (i, (due, assignee, status)) in [
        (now + Duration::hours(2), Some(assignee_id), "todo"),
        (now + Duration::hours(2), None, "todo"),
        (now - Duration::hours(1), Some(assignee_id), "todo"),
        (now + Duration::hours(30), Some(assignee_id), "todo"),
        (now + Duration::hours(2), Some(assignee_id), "done"),
    ]
    .into_iter()
    .enumerate()
    {
        let (code, task) = create_task(
            &env,
            &admin_token,
            &pid,
            json!({
                "title": loc(&format!("تسک سررسید {i}")),
                "due_date": due.to_rfc3339(),
                "assignee_id": assignee.map(|u| u.to_string()),
                "status": status,
            }),
        )
        .await;
        assert_eq!(code, StatusCode::CREATED, "{task}");
        let tid = task["id"].as_str().unwrap().to_string();
        if i == 0 {
            due_soon_assigned = Uuid::parse_str(&tid).unwrap();
        }
        if i == 1 {
            due_soon_unassigned = Uuid::parse_str(&tid).unwrap();
        }
    }

    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/projects/due-soon-cycle", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scanned"], json!(3));
    assert_eq!(body["notified"], json!(2));

    // Unassigned candidate is stamped even though nothing was delivered.
    let row = env
        .db
        .fetch_one(
            "SELECT due_notified_at FROM tasks WHERE id = ?",
            &[due_soon_unassigned.into_arg()],
        )
        .await
        .unwrap();
    assert!(row
        .try_get::<Option<chrono::DateTime<Utc>>, _>("due_notified_at")
        .unwrap()
        .is_some());

    // task.due_soon events only for the assigned candidates.
    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM domain_events WHERE event_type = 'task.due_soon'",
        &[],
    )
    .await;
    assert_eq!(n, 2);
    let row = env
        .db
        .fetch_one(
            "SELECT payload FROM domain_events WHERE aggregate_id = ? AND event_type = 'task.due_soon'",
            &[due_soon_assigned.to_string().into_arg()],
        )
        .await
        .unwrap();
    let payload: Value = row.try_get("payload").unwrap();
    assert_eq!(payload["assignee_id"], json!(assignee_id.to_string()));
    assert_eq!(payload["delivered"], json!(true));

    // Second run immediately: anti-spam → nothing scanned.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/projects/due-soon-cycle", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scanned"], json!(0));
    assert_eq!(body["notified"], json!(0));

    // 20 h anti-spam boundary: 21 h ago → candidate again; 19 h ago → not.
    env.db
        .execute(
            "UPDATE tasks SET due_notified_at = ? WHERE id = ?",
            &[
                (now - Duration::hours(21)).into_arg(),
                (due_soon_assigned).into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/projects/due-soon-cycle", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scanned"], json!(1));
    assert_eq!(body["notified"], json!(1));
    env.db
        .execute(
            "UPDATE tasks SET due_notified_at = ? WHERE id = ?",
            &[
                (now - Duration::hours(19)).into_arg(),
                (due_soon_assigned).into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/projects/due-soon-cycle", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scanned"], json!(0));
}

#[tokio::test]
async fn due_soon_cycle_requires_manage() {
    let env = Env::sqlite().await;
    let (plain_id, plain_token) = seed_user(&env, "ds-plain@example.com").await;
    seed_staff(&env, plain_id, true).await;
    // Staff reader without the permission → 403.
    let (status, _) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/projects/due-soon-cycle", None),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // No token → 401.
    let (status, _) = env
        .call(req(
            Method::POST,
            "/api/v1/admin/projects/due-soon-cycle",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ============================ audit + events ===============================

#[tokio::test]
async fn audit_rows_and_domain_events() {
    let env = Env::sqlite().await;
    let (admin_id, admin_token) = admin_user(&env, "au-admin@example.com").await;
    let (_, project) = create_project(&env, &admin_token, "پ").await;
    let pid = project["id"].as_str().unwrap().to_string();

    // project.created event + projects.project.create audit (actor staff).
    let row = env
        .db
        .fetch_one(
            "SELECT aggregate_type, event_type, payload FROM domain_events WHERE aggregate_id = ?",
            &[pid.clone().into_arg()],
        )
        .await
        .unwrap();
    let aggregate_type: String = row.try_get("aggregate_type").unwrap();
    let event_type: String = row.try_get("event_type").unwrap();
    assert_eq!(aggregate_type, "project");
    assert_eq!(event_type, "project.created");
    let row = env
        .db
        .fetch_one(
            "SELECT actor_id, actor_type, action FROM audit_logs WHERE entity_id = ? AND action = 'projects.project.create'",
            &[pid.clone().into_arg()],
        )
        .await
        .unwrap();
    let actor_id: Uuid = row.try_get("actor_id").unwrap();
    assert_eq!(actor_id, admin_id);
    let actor_type: String = row.try_get("actor_type").unwrap();
    assert_eq!(actor_type, "staff");

    // Move → task.status_changed event with {task_id, from, to, by}.
    let (_, task) = create_task(&env, &admin_token, &pid, json!({ "title": loc("جابه‌جا") })).await;
    let tid = task["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/projects/tasks/{tid}/move"),
                Some(json!({ "status": "todo" })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let row = env
        .db
        .fetch_one(
            "SELECT event_type, payload FROM domain_events WHERE aggregate_id = ? AND event_type = 'task.status_changed'",
            &[tid.clone().into_arg()],
        )
        .await
        .unwrap();
    let payload: Value = row.try_get("payload").unwrap();
    assert_eq!(payload["task_id"], json!(tid));
    assert_eq!(payload["from"], json!("backlog"));
    assert_eq!(payload["to"], json!("todo"));
    assert_eq!(payload["by"], json!(admin_id.to_string()));
    let n = count_rows(
        &env,
        "SELECT COUNT(*) AS n FROM audit_logs WHERE action = 'projects.task.move' AND entity_id = ?",
        &[tid.into_arg()],
    )
    .await;
    assert_eq!(n, 1);
}

// ============================ Postgres smoke ===============================

async fn wave8_projects_pg_smoke() {
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

    // Create project → create task → assign → move → due-soon cycle.
    let (admin_id, admin_token) = admin_user(&env, "pg-proj-admin@example.com").await;
    let (assignee_id, _) = seed_user(&env, "pg-proj-assignee@example.com").await;
    let (status, project) = create_project(&env, &admin_token, "پروژه PG").await;
    assert_eq!(status, StatusCode::CREATED, "{project}");
    let pid = project["id"].as_str().unwrap().to_string();
    assert_eq!(project["created_by"], json!(admin_id.to_string()));
    let due = (Utc::now() + Duration::hours(3)).to_rfc3339();
    let (status, task) = create_task(
        &env,
        &admin_token,
        &pid,
        json!({
            "title": loc("تسک PG"),
            "assignee_id": assignee_id.to_string(),
            "due_date": due,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{task}");
    let tid = task["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/projects/tasks/{tid}/move"),
                Some(json!({ "status": "todo" })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("todo"));
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/admin/projects/due-soon-cycle", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scanned"], json!(1));
    assert_eq!(body["notified"], json!(1));
    // Project get shows the live task count on Postgres too.
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/projects/{pid}"), None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["task_count"], json!(1));
}

#[tokio::test]
async fn wave8_projects_pg_smoke_test() {
    if std::env::var("WAVE2_PG_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .is_none()
    {
        eprintln!("wave8_projects_pg_smoke: WAVE2_PG_URL unset, skipping");
        return;
    }
    wave8_projects_pg_smoke().await;
}
