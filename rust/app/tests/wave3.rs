//! Wave 3 integration tests: home, content, articles, fatwa.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the end
//! (`wave3_pg_smoke`, gated on `WAVE2_PG_URL`) replays the critical
//! path against a local Postgres test database.
//!
//! Permissions are granted through a non-privileged `editor` role, so
//! a plain full-scope login token passes `require_permission` without
//! TOTP enrolment (privileged `admin`/`super_admin` roles would need
//! it — see Wave 2's `make_admin`).

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
            "{}/wave3-storage-{}",
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

fn loc(fa: &str) -> Value {
    json!({ "fa": fa, "ar": fa, "en": fa })
}

// ============================ Home ============================

#[tokio::test]
async fn home_sections_and_search() {
    let env = Env::sqlite().await;
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO home_sections (id, key, sort_order, visible, title, subtitle, data, empty_title, empty_hint, created_at, updated_at) VALUES (?, 'hero', 1, TRUE, ?, NULL, NULL, NULL, NULL, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                loc("آموزش قرآن").into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/home/sections", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sections"].as_array().unwrap().len(), 1);
    assert_eq!(body["sections"][0]["key"], json!("hero"));
    assert_eq!(body["theme"], Value::Null);

    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/search?q=%D9%82%D8%B1%D8%A2%D9%86",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["results"][0]["type"], json!("section"));

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/search?q=%20%20", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_query");

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/search?q=test&type=bogus", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(0));
}

#[tokio::test]
async fn home_calendar_theme_echoes_setting() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/content/public/calendar/theme",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["theme_id"], Value::Null);
    assert_eq!(body["event_id"], Value::Null);
    let month = body["hijri"]["month"].as_i64().unwrap();
    let day = body["hijri"]["day"].as_i64().unwrap();
    assert!((1..=12).contains(&month), "month {month}");
    assert!((1..=30).contains(&day), "day {day}");

    let theme_id = Uuid::new_v4().to_string();
    let event_id = Uuid::new_v4().to_string();
    let envelope = encrypt_value(
        &test_config(),
        &serde_json::to_string(&json!({
            "event_id": event_id,
            "theme_id": theme_id,
            "activated_at": Utc::now().to_rfc3339(),
        }))
        .unwrap(),
    );
    env.db
        .execute(
            "INSERT INTO platform_settings (key, value, encryption_key_version) VALUES ('cms.calendar.active', ?, 1)",
            &[envelope.into_arg()],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/content/public/calendar/theme",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["theme_id"], json!(theme_id));
    assert_eq!(body["event_id"], json!(event_id));
}

// ============================ Content ============================

async fn content_admin(env: &Env) -> String {
    let (user_id, access, _refresh) = register_user(env, "cms@example.com").await;
    grant(env, &user_id, &["content.create", "content.publish"]).await;
    access
}

#[tokio::test]
async fn cms_page_crud_and_workflow() {
    let env = Env::sqlite().await;
    let token = content_admin(&env).await;

    // Create: bad slug -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/pages",
                Some(json!({
                    "slug": "Bad Slug!",
                    "title": loc("شرایط"),
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/pages",
                Some(json!({
                    "slug": "terms",
                    "title": loc("شرایط استفاده"),
                    "blocks": [],
                    "content_type": "article",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let page_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["workflow_status"], json!("draft"));
    assert_eq!(body["version"], json!(1));

    // Duplicate slug -> 409.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/pages",
                Some(json!({ "slug": "terms", "title": loc("x") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "slug_taken");

    // Get + update (version bumps).
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/content/pages/{page_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/content/pages/{page_id}"),
                Some(json!({ "title": loc("شرایط جدید") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(2));
    assert_eq!(body["title"]["fa"], json!("شرایط جدید"));

    // Walk the machine to published (draft -> published is not an edge).
    for target in ["in_review", "approved"] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::POST,
                    &format!("/api/v1/content/pages/{page_id}/transition"),
                    Some(json!({ "to": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "to {target}: {body}");
    }
    // Publish: published_at set + web-only default distribution.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/content/pages/{page_id}/transition"),
                Some(json!({ "to": "published" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("published"));
    assert!(body["published_at"].is_string(), "{body}");
    assert_eq!(body["distribution"]["web"], json!(true));
    assert_eq!(body["distribution"]["mobile"], json!(false));

    // published -> in_review is not an allowed edge.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/content/pages/{page_id}/transition"),
                Some(json!({ "to": "in_review" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");

    // Public reads see the published page.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/content/public/pages/slug/terms",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slug"], json!("terms"));
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/content/public/pages", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);

    // Delete = archive; the public page disappears.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/content/pages/{page_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("archived"));
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/content/public/pages/slug/terms",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "page_not_found");
}

#[tokio::test]
async fn cms_control_center_and_unknown_resource() {
    let env = Env::sqlite().await;
    let token = content_admin(&env).await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/content/items", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "items": [], "total": 0 }));

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/content/items/article/00000000-0000-0000-0000-000000000000/preview",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "unknown_content_type");

    // GET exists for /content/items, so POST is 405.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/content/items", Some(json!({}))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/content/nope", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "http_error");

    // Unauthenticated list -> 401.
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/content/pages", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cms_list_cursor_and_filters() {
    let env = Env::sqlite().await;
    let token = content_admin(&env).await;
    for i in 0..3 {
        let (status, _) = env
            .call(authed(
                req(
                    Method::POST,
                    "/api/v1/content/quotes",
                    Some(json!({
                        "text": loc(&format!("سخن {i}")),
                        "scholar": loc("عالم"),
                    })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/content/quotes?limit=2", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(3));
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let cursor = body["next_cursor"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/content/quotes?limit=2&cursor={cursor}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["next_cursor"], Value::Null);

    // Bad cursor -> 400.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/content/quotes?cursor=!!!", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_cursor");

    // Unknown workflow_status -> 422 on a workflow resource...
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/content/quotes?workflow_status=bogus",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_status");
    // ...but silently ignored on nav (no workflow filter there).
    let (status, _) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/content/nav?workflow_status=bogus",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn cms_nav_public_tree_and_window() {
    let env = Env::sqlite().await;
    let token = content_admin(&env).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/nav",
                Some(json!({ "label": loc("خانه"), "url": "/fa" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let root_id = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/nav",
                Some(json!({
                    "label": loc("قرآن"),
                    "url": "/quran",
                    "parent_id": root_id,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // Future-dated item is outside its window and must not appear.
    let future = (Utc::now() + chrono::Duration::days(30)).to_rfc3339();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/nav",
                Some(json!({
                    "label": loc("آینده"),
                    "url": "/future",
                    "starts_at": future,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/content/public/nav", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["children"].as_array().unwrap().len(), 1);
    assert_eq!(body[0]["children"][0]["label"]["fa"], json!("قرآن"));
}

#[tokio::test]
async fn cms_transition_permission_comes_from_body() {
    let env = Env::sqlite().await;
    let (user_id, access, _refresh) = register_user(&env, "cms2@example.com").await;
    grant(&env, &user_id, &["content.create"]).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/pages",
                Some(json!({ "slug": "p1", "title": loc("ص") })),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let page_id = body["id"].as_str().unwrap().to_string();

    // to=published needs content.publish -> 403 (before any 404/422).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/content/pages/{page_id}/transition"),
                Some(json!({ "to": "published" })),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // to=in_review needs only content.create -> 200.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/content/pages/{page_id}/transition"),
                Some(json!({ "to": "in_review" })),
            ),
            &access,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("in_review"));
}

// ============================ Articles ============================

async fn articles_admin(env: &Env) -> (String, String) {
    let (user_id, access, _refresh) = register_user(env, "art@example.com").await;
    grant(env, &user_id, &["knowledge.manage", "fatwa.approve"]).await;
    (user_id, access)
}

#[tokio::test]
async fn articles_public_flow() {
    let env = Env::sqlite().await;
    let (_uid, token) = articles_admin(&env).await;

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles/categories",
                Some(json!({ "slug": "aqeedah", "title": loc("عقیده") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cat_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({
                    "title": loc("مقالهٔ اول"),
                    "body": { "type": "doc", "content": [] },
                    "slug": "first-article",
                    "category_id": cat_id,
                    "author_name": "نویسنده",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
    let art_id = body["id"].as_str().unwrap().to_string();

    // Not public while draft.
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/articles/first-article", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Publish (fatwa.approve permission).
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/articles/{art_id}/status"),
                Some(json!({ "status": "published" })),
            ),
            &token,
        ))
        .await;
    // draft -> published is not an allowed edge.
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");
    let mut last = Value::Null;
    for target in ["in_review", "approved", "published"] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/articles/{art_id}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "to {target}: {body}");
        last = body;
    }
    assert_eq!(last["workflow_status"], json!("published"));
    assert!(last["published_at"].is_string());

    // Public list (default per_page 10) + detail counts a view each call.
    let (status, body) = env.call(req(Method::GET, "/api/v1/articles", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["per_page"], json!(10));
    assert_eq!(body["items"][0]["category"]["slug"], json!("aqeedah"));
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/articles/first-article", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["view_count"], json!(1));
    let (_status, body) = env
        .call(req(Method::GET, "/api/v1/articles/first-article", None))
        .await;
    assert_eq!(body["view_count"], json!(2));
    // Meta does not count a view.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/articles/first-article/meta",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slug"], json!("first-article"));
    let (_status, body) = env
        .call(req(Method::GET, "/api/v1/articles/first-article", None))
        .await;
    assert_eq!(body["view_count"], json!(3));
    // Share.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/articles/first-article/share",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["share_count"], json!(1));

    // Categories with counts.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/articles/categories", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["slug"], json!("aqeedah"));
    assert_eq!(body[0]["published_count"], json!(1));
}

#[tokio::test]
async fn articles_admin_guards() {
    let env = Env::sqlite().await;
    let (_uid, token) = articles_admin(&env).await;
    let (plain_id, plain_access, _r) = register_user(&env, "plain@example.com").await;
    let _ = plain_id;

    // No permission -> 403.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({
                    "title": loc("x"),
                    "body": {},
                    "slug": "x",
                })),
            ),
            &plain_access,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // Bad slug (whitespace) -> 422; unknown category -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({ "title": loc("x"), "body": {}, "slug": "bad slug" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({
                    "title": loc("x"),
                    "body": {},
                    "slug": "x",
                    "category_id": Uuid::new_v4().to_string(),
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "category_not_found");

    // Create + duplicate slug -> 409; admin list default per_page 50.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({ "title": loc("x"), "body": {}, "slug": "x" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({ "title": loc("y"), "body": {}, "slug": "x" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "article_slug_exists");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/articles", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["per_page"], json!(50));
    assert_eq!(body["total"], json!(1));
    // Admin list items carry id + workflow_status.
    assert!(body["items"][0]["id"].is_string());
    assert_eq!(body["items"][0]["workflow_status"], json!("draft"));
}

#[tokio::test]
async fn articles_category_crud() {
    let env = Env::sqlite().await;
    let (_uid, token) = articles_admin(&env).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles/categories",
                Some(json!({ "slug": "Bad Slug", "title": loc("x") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles/categories",
                Some(json!({ "slug": "fiqh", "title": loc("فقه") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cat_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles/categories",
                Some(json!({ "slug": "fiqh", "title": loc("فقه") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "category_slug_exists");
    // Use it, then delete is blocked.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({
                    "title": loc("x"),
                    "body": {},
                    "slug": "x",
                    "category_id": cat_id,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/articles/categories/{cat_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "category_in_use");
    // Rename + delete an unused one.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/admin/articles/categories/{cat_id}"),
                Some(json!({ "slug": "fiqh2", "title": loc("فقه ۲") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slug"], json!("fiqh2"));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles/categories",
                Some(json!({ "slug": "tmp", "title": loc("موقت") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let tmp_id = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/articles/categories/{tmp_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ============================ Fatwa ============================

async fn fatwa_admin(env: &Env) -> (String, String) {
    let (user_id, access, _refresh) = register_user(env, "fatwa-admin@example.com").await;
    grant(
        env,
        &user_id,
        &[
            "knowledge.manage",
            "fatwa.approve",
            "fatwa.answer_cluster",
            "platform.settings",
        ],
    )
    .await;
    (user_id, access)
}

async fn publish_topic(env: &Env, token: &str, title: &str) -> String {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/topics",
                Some(json!({
                    "title": loc(title),
                    "description": null,
                    "category_id": null,
                    "author_name": "مفتی",
                    "featured_image_url": null,
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let topic_id = body["id"].as_str().unwrap().to_string();
    for target in ["in_review", "approved", "published"] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/fatwa/topics/{topic_id}/status"),
                    Some(json!({ "status": target })),
                ),
                token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "topic to {target}: {body}");
    }
    topic_id
}

#[tokio::test]
async fn fatwa_topics_public_flow() {
    let env = Env::sqlite().await;
    let (_uid, token) = fatwa_admin(&env).await;

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/categories",
                Some(json!({ "slug": "salah", "title": loc("نماز") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cat_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/topics",
                Some(json!({
                    "title": loc("نماز جمعه"),
                    "description": loc("دربارهٔ نماز جمعه"),
                    "category_id": cat_id,
                    "author_name": "مفتی",
                    "featured_image_url": "https://example.com/img.jpg",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let topic_id = body["id"].as_str().unwrap().to_string();

    // Not public while draft.
    let (status, _) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwa/topics/{topic_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    for target in ["in_review", "approved", "published"] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/fatwa/topics/{topic_id}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    // Position (draft) is visible in detail but not in compare.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/fatwa/topics/{topic_id}/positions"),
                Some(json!({
                    "madhab": "hanafi",
                    "position_text": loc("واجب است"),
                    "evidence_refs": ["حدیث"],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let pos_id = body["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/fatwa/topics", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));
    assert_eq!(body["items"][0]["category"]["slug"], json!("salah"));

    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwa/topics/{topic_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["view_count"], json!(1));
    assert_eq!(body["positions"].as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwa/compare/{topic_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["positions"].as_array().unwrap().len(), 0);

    // Publish the position -> compare shows it.
    for target in ["in_review", "approved"] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/fatwa/positions/{pos_id}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwa/compare/{topic_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["positions"].as_array().unwrap().len(), 1);

    // Share increments.
    let (status, body) = env
        .call(req(
            Method::POST,
            &format!("/api/v1/fatwa/topics/{topic_id}/share"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["share_count"], json!(1));

    // Public categories-with-counts.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/fatwa/topics/categories", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["slug"], json!("salah"));
    assert_eq!(body[0]["published_count"], json!(1));
}

#[tokio::test]
async fn fatwa_chunks_and_verify() {
    let env = Env::sqlite().await;
    let (_uid, token) = fatwa_admin(&env).await;
    let chunk_text = "نماز جمعه بر هر مسلمان بالغ واجب است";

    // Bad ai_scope -> 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/chunks",
                Some(json!({
                    "chunk_text": chunk_text,
                    "ai_scopes": ["bogus"],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_ai_scopes");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/chunks",
                Some(json!({
                    "chunk_text": chunk_text,
                    "topic_category": "salah",
                    "madhab_scope": "hanafi",
                    "ai_scopes": ["fatwa", "verification"],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
    let chunk_id = body["id"].as_str().unwrap().to_string();

    for target in ["in_review", "approved"] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/fatwa/chunks/{chunk_id}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "chunk to {target}: {body}");
    }

    // Identical text: lexical 1.0 + semantic 1.0 = 2.0 >= 0.95 -> verified.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/verify/check",
            Some(json!({ "text": chunk_text })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["verdict"], json!("verified"));
    assert_eq!(body["matched"][0]["chunk_id"], json!(chunk_id));
    assert_eq!(body["matched"][0]["score"], json!(2.0));

    // Second identical call hits the cache (score forced 0.0).
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/verify/check",
            Some(json!({ "text": chunk_text })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["verdict"], json!("verified"));
    assert_eq!(body["note"], json!("نتیجهٔ کش‌شده"));
    assert_eq!(body["matched"][0]["score"], json!(0.0));

    // Unrelated text -> not_found.
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/verify/check",
            Some(json!({ "text": "آب و هوای امروز تهران چگونه است؟" })),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["verdict"], json!("not_found"));

    // Chunk list + UUID cursor.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/fatwa/chunks?limit=1", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["next_cursor"], Value::Null);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/fatwa/chunks?after=not-a-uuid",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
}

#[tokio::test]
async fn fatwa_quiz_flow() {
    let env = Env::sqlite().await;
    let (user_id, token) = fatwa_admin(&env).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/chunks",
                Some(json!({
                    "chunk_text": "زکات بر طلا و نقره واجب است",
                    "topic_category": "zakat",
                    "ai_scopes": ["quiz"],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let chunk_id = body["id"].as_str().unwrap().to_string();
    for target in ["in_review", "approved"] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/fatwa/chunks/{chunk_id}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }

    // No quiz questions yet -> 404 no_quiz_questions.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/fatwa/quiz/generate?count=1", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "no_quiz_questions");

    // Guards: bad correct_option + non-quiz chunk.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/quiz/questions",
                Some(json!({
                    "question_text": loc("زکات بر چه چیزی واجب است؟"),
                    "options": [loc("طلا"), loc("آهن")],
                    "correct_option": 5,
                    "source_chunk_id": chunk_id,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_correct_option");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/chunks",
                Some(json!({
                    "chunk_text": "chunk بدون scope آزمون",
                    "ai_scopes": ["fatwa"],
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let plain_chunk = body["id"].as_str().unwrap().to_string();
    for target in ["in_review", "approved"] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/fatwa/chunks/{plain_chunk}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/quiz/questions",
                Some(json!({
                    "question_text": loc("سوال؟"),
                    "options": [loc("الف"), loc("ب")],
                    "correct_option": 0,
                    "source_chunk_id": plain_chunk,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "chunk_not_quiz_eligible");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/quiz/questions",
                Some(json!({
                    "question_text": loc("زکات بر چه چیزی واجب است؟"),
                    "options": [loc("طلا"), loc("آهن")],
                    "correct_option": 0,
                    "source_chunk_id": chunk_id,
                    "difficulty": "easy",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let question_id = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["workflow_status"], json!("draft"));

    // Python has no HTTP route to approve a quiz question, so the test
    // seeds the approved state directly (the same out-of-band step the
    // Python system requires) and exercises generate/submit over HTTP.
    env.db
        .execute(
            "UPDATE quiz_questions SET workflow_status='approved' WHERE id=?",
            &[as_uuid(&question_id).into_arg()],
        )
        .await
        .unwrap();
    let _ = user_id;

    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/fatwa/quiz/generate?count=5", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["questions"].as_array().unwrap().len(), 1);
    assert!(body["questions"][0].get("correct_option").is_none());
    let attempt_id = body["attempt_id"].as_str().unwrap().to_string();
    let qid = body["questions"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(qid, question_id);

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/quiz/submit",
                Some(json!({
                    "attempt_id": attempt_id,
                    "answers": { qid.clone(): 0 },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["score"], json!(100.0));
    assert_eq!(body["correct"], json!(1));

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/quiz/submit",
                Some(json!({
                    "attempt_id": attempt_id,
                    "answers": { qid: 1 },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["score"], json!(0.0));

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/quiz/submit",
                Some(json!({
                    "attempt_id": Uuid::new_v4().to_string(),
                    "answers": {},
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "attempt_not_found");
}

#[tokio::test]
async fn fatwa_submissions_flow() {
    let env = Env::sqlite().await;
    let (author_id, author_token, _r) = register_user(&env, "author@example.com").await;
    grant(&env, &author_id, &["fatwa.submit"]).await;
    let (other_id, other_token, _r) = register_user(&env, "other@example.com").await;
    grant(&env, &other_id, &["fatwa.submit"]).await;
    let (approver_id, approver_token, _r) = register_user(&env, "approver@example.com").await;
    grant(&env, &approver_id, &["fatwa.approve", "knowledge.manage"]).await;

    // Blank body -> 422 empty_body.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/submissions",
                Some(json!({ "title": loc("سوال"), "body": "   " })),
            ),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "empty_body");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/submissions",
                Some(json!({
                    "title": loc("حکم زکات طلا"),
                    "body": "آیا بر طلای زینتی زکات واجب است؟",
                    "madhab": "hanafi",
                    "evidence": "دلیل",
                })),
            ),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
    assert_eq!(body["version"], json!(1));
    let sub_id = body["id"].as_str().unwrap().to_string();

    // Another author cannot view it; the approver can.
    let (status, _) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/fatwa/submissions/{sub_id}"),
                None,
            ),
            &other_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/fatwa/submissions/{sub_id}"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Author's list shows only their own; approver sees all.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/fatwa/submissions", None),
            &other_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    let (_status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/fatwa/submissions", None),
            &approver_token,
        ))
        .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    // Submit -> in_review; author may not approve; approver approves.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub_id}/submit"),
                None,
            ),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("in_review"));
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub_id}/approve"),
                None,
            ),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub_id}/approve"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("approved"));
    assert!(body["reviewed_by"].is_string());

    // Publish bumps version and sets published_at.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub_id}/publish"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("published"));
    assert_eq!(body["version"], json!(2));
    assert!(body["published_at"].is_string());

    // add-to-rag -> 201, then 409.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub_id}/add-to-rag"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body["chunk_id"].is_string());
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub_id}/add-to-rag"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "already_in_rag");

    // Reject path on a second submission (note accepted, never stored).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/submissions",
                Some(json!({ "title": loc("سوال دوم"), "body": "متن" })),
            ),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let sub2 = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub2}/submit"),
                None,
            ),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/submissions/{sub2}/reject"),
                Some(json!({ "note": "نیاز به دلیل بیشتر" })),
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("rejected"));
}

#[tokio::test]
async fn fatwa_clusters_flow() {
    let env = Env::sqlite().await;
    let (asker_id, asker_token, _r) = register_user(&env, "asker@example.com").await;
    grant(&env, &asker_id, &["fatwa.submit"]).await;
    let (_ans_id, ans_token) = fatwa_admin(&env).await;
    let question = "آیا نماز جمعه بر مسافر واجب است؟";

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/questions",
                Some(json!({ "text": question, "source": "direct" })),
            ),
            &asker_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["matched"], json!(false));
    assert_eq!(body["similarity"], Value::Null);
    let cluster_id = body["cluster_id"].as_str().unwrap().to_string();

    // Identical question matches the cluster (hash cosine 1.0 >= 0.82).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/questions",
                Some(json!({ "text": question, "source": "chat" })),
            ),
            &asker_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["matched"], json!(true));
    assert_eq!(body["cluster_id"], json!(cluster_id));
    assert_eq!(body["similarity"], json!(1.0));
    assert_eq!(body["suggestion"], Value::Null);

    // Answer: both member rows belong to the same user -> notified 1.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/clusters/{cluster_id}/answer"),
                Some(json!({ "answer": "بر مسافر واجب نیست" })),
            ),
            &ans_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("answered"));
    assert_eq!(body["notified_count"], json!(1));

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/fatwa/clusters/{cluster_id}"),
                None,
            ),
            &ans_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["member_count"], json!(2));
    assert_eq!(body["members"].as_array().unwrap().len(), 2);

    // Answer + publish creates a published topic.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/fatwa/questions",
                Some(json!({ "text": "حکم روزهٔ مسافر چیست؟", "source": "direct" })),
            ),
            &asker_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cluster2 = body["cluster_id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwa/clusters/{cluster2}/answer"),
                Some(json!({
                    "answer": "مسافر می‌تواند افطار کند",
                    "publish": {
                        "title": "روزهٔ مسافر",
                        "evidence": null,
                        "positions": { "shafii": "جایز است" },
                        "category_id": null,
                        "featured_image_url": null,
                        "author_name": null,
                    },
                })),
            ),
            &ans_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let topic_id = body["published_topic_id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwa/topics/{topic_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["positions"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn fatwa_saved_flow() {
    let env = Env::sqlite().await;
    let (_uid, admin_token) = fatwa_admin(&env).await;
    let (_user_id, user_token, _r) = register_user(&env, "reader@example.com").await;
    let topic_id = publish_topic(&env, &admin_token, "حکم وضو").await;

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwas/{topic_id}/saved"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(true));
    // Idempotent re-save.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwas/{topic_id}/saved"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(true));
    // Guest always sees saved=false.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwas/{topic_id}/saved"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(false));
    // List shows it.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/fatwas/saved", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    // Unsave.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/fatwas/{topic_id}/saved"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(false));
    let (_status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/fatwas/saved", None),
            &user_token,
        ))
        .await;
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    // Saving a draft topic -> 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/topics",
                Some(json!({ "title": loc("پیش‌نویس"), "description": null })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let draft_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/fatwas/{draft_id}/saved"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "topic_not_found");
}

#[tokio::test]
async fn fatwa_trusted_sources() {
    let env = Env::sqlite().await;
    let (_uid, token) = fatwa_admin(&env).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/trusted-sources",
                Some(json!({
                    "name": "S1",
                    "base_url": "https://example.com/docs",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["domain"], json!("example.com"));
    assert_eq!(body["enabled"], json!(false));
    let src_id = body["id"].as_str().unwrap().to_string();

    // PATCH sets last_checked_at even though nothing was "checked".
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/fatwa/trusted-sources/{src_id}"),
                Some(json!({
                    "name": "S1",
                    "base_url": "https://example.com/docs",
                    "enabled": true,
                    "allowed_paths": [],
                    "priority": 0,
                    "citation_required": true,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["last_checked_at"].is_string(), "{body}");

    // Check against a dead local port -> 200 {ok:false, error: ConnectError}.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/trusted-sources",
                Some(json!({ "name": "Dead", "base_url": "http://127.0.0.1:9/" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let dead_id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/fatwa/trusted-sources/{dead_id}/check"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(false));
    assert_eq!(body["error"], json!("ConnectError"));

    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/fatwa/trusted-sources/{dead_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn fatwa_audit_and_ingest() {
    let env = Env::sqlite().await;
    let (_uid, token) = fatwa_admin(&env).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/chunks",
                Some(json!({ "chunk_text": "متن آزمایشی", "ai_scopes": [] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/fatwa/audit", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let actions: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"fatwa.chunk_created"), "{actions:?}");

    // Ingest trigger: 202 queued, enqueued=false (no ARQ from Rust).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/ingest",
                Some(json!({
                    "source_type": "text",
                    "payload": { "text": "hello" },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["status"], json!("queued"));
    let run_id = body["run_id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/fatwa/ingest/{run_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("queued"));
    assert_eq!(body["stats"]["enqueued"], json!(false));
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/fatwa/ingest/{}", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "run_not_found");
}

#[tokio::test]
async fn fatwa_categories_crud() {
    let env = Env::sqlite().await;
    let (_uid, token) = fatwa_admin(&env).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/categories",
                Some(json!({ "slug": "tahara", "title": loc("طهارت") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cat_id = body["id"].as_str().unwrap().to_string();
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/fatwa/categories",
                Some(json!({ "slug": "tahara", "title": loc("طهارت") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    // In use by a topic -> delete blocked.
    let topic_id = publish_topic(&env, &token, "موضوع طهارت").await;
    env.db
        .execute(
            "UPDATE fiqh_topics SET category_id=? WHERE id=?",
            &[as_uuid(&cat_id).into_arg(), as_uuid(&topic_id).into_arg()],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/fatwa/categories/{cat_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "category_in_use");
    // Rename works.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/admin/fatwa/categories/{cat_id}"),
                Some(json!({ "slug": "tahara2", "title": loc("طهارت ۲") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["slug"], json!("tahara2"));
}

// ============================ Postgres smoke ============================

/// Replays the Wave 3 critical path (CMS page, article, fatwa topic:
/// create -> publish -> public read) against a real Postgres, when
/// `WAVE2_PG_URL` is set. The same `TEST_SCHEMA` DDL is applied;
/// tables are dropped first so the test is repeatable.
#[tokio::test]
async fn wave3_pg_smoke() {
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

    let (_uid, token) = fatwa_admin(&env).await;

    // CMS page: create -> publish -> public slug read (JSONB + UUID on PG).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/pages",
                Some(json!({ "slug": "terms", "title": loc("شرایط") })),
            ),
            &token,
        ))
        .await;
    // fatwa_admin lacks content.* permissions -> 403 is the honest result
    // here; grant them and retry.
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    grant(
        &env,
        &_uid,
        &["content.create", "content.publish", "knowledge.manage"],
    )
    .await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/content/pages",
                Some(json!({ "slug": "terms", "title": loc("شرایط") })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let page_id = body["id"].as_str().unwrap().to_string();
    for target in ["in_review", "approved", "published"] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::POST,
                    &format!("/api/v1/content/pages/{page_id}/transition"),
                    Some(json!({ "to": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "page to {target}");
    }
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/content/public/pages/slug/terms",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "pg public page: {body}");
    assert_eq!(body["slug"], json!("terms"));

    // Article: create -> publish -> public list.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/articles",
                Some(json!({ "title": loc("مقاله"), "body": {}, "slug": "pg-article" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let art_id = body["id"].as_str().unwrap().to_string();
    for target in ["in_review", "approved", "published"] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::PATCH,
                    &format!("/api/v1/admin/articles/{art_id}/status"),
                    Some(json!({ "status": target })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, body) = env.call(req(Method::GET, "/api/v1/articles", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total"], json!(1));

    // Fatwa topic: create -> publish -> public detail (view++ on PG).
    let topic_id = publish_topic(&env, &token, "موضوع pg").await;
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/fatwa/topics/{topic_id}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "pg topic detail: {body}");
    assert_eq!(body["view_count"], json!(1));

    // Home sections read on PG.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/home/sections", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sections"], json!([]));
}
