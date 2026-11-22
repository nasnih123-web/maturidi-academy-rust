//! Wave 7 integration tests: admin content center.
//!
//! Adapters registry, unified inbox (merge + cursor pagination),
//! item detail, placements (upsert/list/reorder/patch/delete),
//! distribution (get/set/policy), calendar + conflicts, single and
//! bulk transitions, audit feed, AI sources + reindex.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the
//! end (`wave7_admin_content_pg_smoke`, gated on `WAVE2_PG_URL`)
//! replays the critical path (placements → transition → audit)
//! against one local Postgres test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::{Duration, Utc};
use maturidi_api_rs::{build_router, init_test_schema, AppState, Config, Db, IntoArg};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave7-admin-content-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave7".to_string(),
        captcha_enabled: false,
        // Dead port: native routes must never touch the origin.
        origin_url: "http://127.0.0.1:9".to_string(),
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

/// Register a user with `content.bulk`.
async fn admin_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["content.bulk"]).await;
    (uid, token)
}

// ============================ seeds =====================================

async fn seed_page(env: &Env, title_fa: &str, status: &str, secs_ago: i64) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(secs_ago);
    env.db
        .execute(
            "INSERT INTO pages (id, slug, title, blocks, seo_meta, content_type, published_at, \
             created_at, updated_at, workflow_status, version, created_by, distribution) \
             VALUES (?, ?, ?, '[]', '{}', 'page', NULL, ?, ?, ?, 1, NULL, NULL)",
            &[
                id.into_arg(),
                format!("slug-{id}").into_arg(),
                json!({"fa": title_fa, "ar": title_fa, "en": title_fa}).into_arg(),
                now.into_arg(),
                now.into_arg(),
                status.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_library_item(
    env: &Env,
    title_fa: &str,
    status: &str,
    secs_ago: i64,
    ai_scopes: Vec<&str>,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(secs_ago);
    let published_at = if status == "published" {
        Some(now)
    } else {
        None
    };
    env.db
        .execute(
            "INSERT INTO library_items (id, kind, title, description, workflow_status, \
             visibility, version, ai_scopes, created_at, updated_at, published_at) \
             VALUES (?, 'pdf', ?, NULL, ?, 'public', 1, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                json!({"fa": title_fa, "ar": title_fa, "en": title_fa}).into_arg(),
                status.into_arg(),
                json!(ai_scopes).into_arg(),
                now.into_arg(),
                now.into_arg(),
                published_at.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_submission(env: &Env, author_id: Uuid, status: &str, secs_ago: i64) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(secs_ago);
    env.db
        .execute(
            "INSERT INTO fatwa_submissions (id, title, body, author_id, workflow_status, \
             version, created_at, updated_at) VALUES (?, ?, 'متن فتوا', ?, ?, 1, ?, ?)",
            &[
                id.into_arg(),
                json!({"fa": "فتوای تست", "ar": "فتوای تست", "en": "test"}).into_arg(),
                author_id.into_arg(),
                status.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_course(env: &Env, title_fa: &str, active: bool, secs_ago: i64) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(secs_ago);
    env.db
        .execute(
            "INSERT INTO courses (id, title, description, is_active, created_at) \
             VALUES (?, ?, NULL, ?, ?)",
            &[
                id.into_arg(),
                json!({"fa": title_fa, "ar": title_fa, "en": title_fa}).into_arg(),
                active.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_social_post(env: &Env, author_id: Uuid, status: &str, secs_ago: i64) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(secs_ago);
    env.db
        .execute(
            "INSERT INTO social_posts (id, author_id, body, status, created_at, updated_at) \
             VALUES (?, ?, 'پست تست اجتماعی', ?, ?, ?)",
            &[
                id.into_arg(),
                author_id.into_arg(),
                status.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_place(env: &Env, title_fa: &str, status: &str, secs_ago: i64) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(secs_ago);
    env.db
        .execute(
            "INSERT INTO religious_places (id, name, lat, lng, workflow_status, version, \
             created_at, updated_at) VALUES (?, ?, 35.0, 51.0, ?, 1, ?, ?)",
            &[
                id.into_arg(),
                json!({"fa": title_fa, "ar": title_fa, "en": title_fa}).into_arg(),
                status.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_chunk(env: &Env, text: &str, status: &str, ai_scopes: Vec<&str>) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO fiqh_knowledge_chunks (id, chunk_text, workflow_status, ai_scopes, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                text.into_arg(),
                status.into_arg(),
                json!(ai_scopes).into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

struct PlacementArgs<'a> {
    domain: &'a str,
    content_type: &'a str,
    content_id: &'a str,
    surface: &'a str,
    slot: Option<&'a str>,
    sort_order: i64,
}

async fn put_placement(env: &Env, token: &str, args: PlacementArgs<'_>) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::PUT,
            "/api/v1/admin/content/placements",
            Some(json!({
                "domain": args.domain,
                "content_type": args.content_type,
                "content_id": args.content_id,
                "surface": args.surface,
                "slot": args.slot,
                "sort_order": args.sort_order,
            })),
        ),
        token,
    ))
    .await
}

// ============================ adapters ==================================

#[tokio::test]
async fn adapters_list() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-admin@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/adapters", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().expect("adapters array");
    assert_eq!(items.len(), 11, "{body}");
    // Sorted by (domain, content_type).
    let keys: Vec<(String, String)> = items
        .iter()
        .map(|a| {
            (
                a["domain"].as_str().unwrap().to_string(),
                a["content_type"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
    let by_key = |d: &str, ct: &str| {
        items
            .iter()
            .find(|a| a["domain"] == d && a["content_type"] == ct)
            .unwrap()
            .clone()
    };
    assert_eq!(
        by_key("content", "page")["supports_transition"],
        json!(true)
    );
    assert_eq!(by_key("content", "page")["label"], json!("صفحه"));
    assert_eq!(
        by_key("fatwa", "knowledge_chunk")["supports_transition"],
        json!(false)
    );
    assert_eq!(by_key("lms", "course")["supports_transition"], json!(false));
    assert_eq!(
        by_key("social", "social_post")["supports_transition"],
        json!(false)
    );
    assert_eq!(
        by_key("library", "library_item")["supports_transition"],
        json!(true)
    );
    assert_eq!(
        by_key("places", "religious_place")["supports_transition"],
        json!(true)
    );
}

#[tokio::test]
async fn adapters_require_auth_and_permission() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/admin/content/adapters", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    assert_eq!(err_code(&body), "unauthorized");
    let (_, token, _) = register_user(&env, "acc-plain@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/adapters", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
}

// ============================ items =====================================

#[tokio::test]
async fn items_list_empty() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-empty@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["next_cursor"], Value::Null);
}

#[tokio::test]
async fn items_list_merged_and_shaped() {
    let env = Env::sqlite().await;
    let (uid, token) = admin_user(&env, "acc-merged@example.com").await;
    let page_id = seed_page(&env, "صفحهٔ تست", "draft", 300).await;
    let lib_id = seed_library_item(&env, "جزوهٔ تست", "published", 100, vec![]).await;
    let course_id = seed_course(&env, "دورهٔ تست", true, 200).await;
    let social_id = seed_social_post(&env, as_uuid(&uid), "published", 150).await;
    let place_id = seed_place(&env, "مکان تست", "draft", 250).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 5, "{body}");
    // Newest first.
    assert_eq!(items[0]["id"], json!(lib_id.to_string()));
    assert_eq!(items[0]["domain"], json!("library"));
    assert_eq!(items[0]["status"], json!("published"));
    assert_eq!(items[0]["title"], json!("جزوهٔ تست"));
    assert_eq!(items[0]["version"], json!(1));
    assert_eq!(items[1]["id"], json!(social_id.to_string()));
    assert_eq!(items[1]["domain"], json!("social"));
    assert_eq!(items[1]["status"], json!("published"));
    assert_eq!(items[2]["id"], json!(course_id.to_string()));
    assert_eq!(items[2]["status"], json!("published"));
    assert_eq!(items[2]["version"], Value::Null);
    assert_eq!(items[2]["author_id"], Value::Null);
    assert_eq!(items[2]["author_name"], Value::Null);
    assert_eq!(items[3]["id"], json!(place_id.to_string()));
    assert_eq!(items[3]["domain"], json!("places"));
    assert_eq!(items[3]["status"], json!("draft"));
    assert_eq!(items[4]["id"], json!(page_id.to_string()));
    assert_eq!(items[4]["domain"], json!("content"));
    assert_eq!(items[4]["content_type"], json!("page"));
    assert_eq!(items[4]["status"], json!("draft"));
    assert_eq!(body["next_cursor"], Value::Null);
}

#[tokio::test]
async fn items_list_filters_and_errors() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-filters@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ یکتا", "draft", 10, vec![]).await;
    seed_page(&env, "صفحهٔ دیگر", "draft", 20).await;
    // Domain filter.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items?domain=library",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], json!(lib_id.to_string()));
    // Text search.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items?q=%D8%AC%D8%B2%D9%88%D9%87",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1, "{body}");
    // Unknown domain → 404 adapter_not_found.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items?domain=nope", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "adapter_not_found");
    // Unknown content_type with a known domain → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items?domain=library&content_type=nope",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "adapter_not_found");
    // Bad channel → 422 bad_channel.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items?distribution_channel=nope",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_channel");
    // Bad cursor → 400 invalid_cursor.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items?cursor=!!!", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_cursor");
    // Limit out of range → 422.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items?limit=0", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

#[tokio::test]
async fn items_list_pagination() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-page@example.com").await;
    let newest = seed_library_item(&env, "آیتم یک", "draft", 10, vec![]).await;
    seed_library_item(&env, "آیتم دو", "draft", 20, vec![]).await;
    let oldest = seed_library_item(&env, "آیتم سه", "draft", 30, vec![]).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items?limit=2", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], json!(newest.to_string()));
    let cursor = body["next_cursor"]
        .as_str()
        .expect("next_cursor")
        .to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/content/items?limit=2&cursor={cursor}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"], json!(oldest.to_string()));
    assert_eq!(body["next_cursor"], Value::Null);
}

#[tokio::test]
async fn items_list_placement_and_distribution_filters() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-pdf@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ فیلتر", "published", 10, vec![]).await;
    seed_library_item(&env, "جزوهٔ دیگر", "published", 20, vec![]).await;
    // Placement filter: only the placed item survives.
    let (status, _) = put_placement(
        &env,
        &token,
        PlacementArgs {
            domain: "library",
            content_type: "library_item",
            content_id: &lib_id.to_string(),
            surface: "homepage.hero",
            slot: None,
            sort_order: 0,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items?placement=homepage.hero",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], json!(lib_id.to_string()));
    // Distribution filter: disabling web hides the item on that channel.
    let (status, _) = env
        .call(authed(
            req(
                Method::PUT,
                &format!(
                    "/api/v1/admin/content/distribution?domain=library&content_type=library_item&id={lib_id}"
                ),
                Some(json!({ "channel": "web", "enabled": false })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items?distribution_channel=web",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert!(!ids.contains(&lib_id.to_string().as_str()), "{body}");
}

// ============================ detail ====================================

#[tokio::test]
async fn item_detail_library() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-detail@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ جزئیات", "draft", 10, vec!["fatwa"]).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/content/items/library/library_item/{lib_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["domain"], json!("library"));
    assert_eq!(body["preview"]["title"], json!("جزوهٔ جزئیات"));
    assert_eq!(body["workflow"]["status"], json!("draft"));
    assert_eq!(
        body["workflow"]["allowed_transitions"],
        json!(["archived", "in_review"])
    );
    assert_eq!(body["workflow"]["version"], json!(1));
    assert_eq!(body["ai_scopes"], json!(["fatwa"]));
    assert_eq!(body["placements"], json!([]));
    assert_eq!(
        body["distribution"]["channels"]["web"]["source"],
        json!("default")
    );
    assert_eq!(
        body["distribution"]["channels"]["web"]["enabled"],
        json!(true)
    );
    assert_eq!(body["schedules"], json!([]));
    assert_eq!(body["related"], json!([]));
    assert_eq!(body["versions"], json!([]));
    assert_eq!(body["seo"], Value::Null);
    assert_eq!(body["audit"], json!([]));
}

#[tokio::test]
async fn item_detail_404s() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-d404@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items/nope/nope/00000000-0000-0000-0000-000000000000",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "adapter_not_found");
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/items/library/library_item/00000000-0000-0000-0000-000000000000",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "not_found");
}

// ============================ placements ==================================

#[tokio::test]
async fn placements_crud() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-plc@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ جایگذاری", "published", 10, vec![]).await;
    let (status, body) = put_placement(
        &env,
        &token,
        PlacementArgs {
            domain: "library",
            content_type: "library_item",
            content_id: &lib_id.to_string(),
            surface: "homepage.hero",
            slot: Some("main"),
            sort_order: 2,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let pid = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["surface"], json!("homepage.hero"));
    assert_eq!(body["slot"], json!("main"));
    assert_eq!(body["sort_order"], json!(2));
    assert_eq!(body["audience"], json!({}));
    assert_eq!(body["active"], json!(true));
    // Upsert with the same key keeps the id.
    let (status, body2) = put_placement(
        &env,
        &token,
        PlacementArgs {
            domain: "library",
            content_type: "library_item",
            content_id: &lib_id.to_string(),
            surface: "homepage.hero",
            slot: Some("main"),
            sort_order: 5,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body2}");
    assert_eq!(body2["id"], json!(pid));
    assert_eq!(body2["sort_order"], json!(5));
    // List with filters.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/placements?surface=homepage.hero",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    // Patch.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/content/placements/{pid}"),
                Some(json!({ "sort_order": 9, "locale": "fa", "active": false })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sort_order"], json!(9));
    assert_eq!(body["locale"], json!("fa"));
    assert_eq!(body["active"], json!(false));
    // Bad patch field → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/content/placements/{pid}"),
                Some(json!({ "domain": "library" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_patch_field");
    // Delete.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/content/placements/{pid}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["message"], json!("placement حذف شد"));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/placements", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
    // Delete again → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/content/placements/{pid}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "not_found");
    // Malformed id → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                "/api/v1/admin/content/placements/not-a-uuid",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

#[tokio::test]
async fn placements_upsert_validations() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-pv@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ اعتبارسنجی", "draft", 10, vec![]).await;
    // Missing surface → 422 validation_error (Pydantic).
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/content/placements",
                Some(json!({
                    "domain": "library",
                    "content_type": "library_item",
                    "content_id": lib_id.to_string(),
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    // Empty surface → 422 missing_surface (service).
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/content/placements",
                Some(json!({
                    "domain": "library",
                    "content_type": "library_item",
                    "content_id": lib_id.to_string(),
                    "surface": "",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "missing_surface");
    // Unknown adapter → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/content/placements",
                Some(json!({
                    "domain": "nope",
                    "content_type": "nope",
                    "content_id": lib_id.to_string(),
                    "surface": "homepage.hero",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "adapter_not_found");
    // Bad window → 422 bad_window.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/content/placements",
                Some(json!({
                    "domain": "library",
                    "content_type": "library_item",
                    "content_id": lib_id.to_string(),
                    "surface": "homepage.hero",
                    "starts_at": "2026-12-02T00:00:00Z",
                    "ends_at": "2026-12-01T00:00:00Z",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_window");
}

#[tokio::test]
async fn placements_reorder() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-reord@example.com").await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let lib_id =
            seed_library_item(&env, &format!("جزوهٔ {i}"), "published", 10 + i, vec![]).await;
        let (status, body) = put_placement(
            &env,
            &token,
            PlacementArgs {
                domain: "library",
                content_type: "library_item",
                content_id: &lib_id.to_string(),
                surface: "homepage.hero",
                slot: None,
                sort_order: i,
            },
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        ids.push(body["id"].as_str().unwrap().to_string());
    }
    let reversed: Vec<String> = ids.iter().rev().cloned().collect();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/placements/reorder",
                Some(json!({ "ordered_ids": reversed })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["updated"], json!(3));
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/placements?surface=homepage.hero",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed: Vec<String> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listed, reversed);
    // Bad id → 422, and the earlier ids are NOT reordered (two-pass).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/placements/reorder",
                Some(json!({ "ordered_ids": [ids[0].clone(), "not-a-uuid"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_placement_id");
    // Unknown id → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/placements/reorder",
                Some(json!({ "ordered_ids": ["00000000-0000-0000-0000-000000000000"] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "placement_not_found");
    // Empty → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/placements/reorder",
                Some(json!({ "ordered_ids": [] })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

// ============================ distribution ================================

#[tokio::test]
async fn distribution_get_set() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-dist@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ توزیع", "published", 10, vec![]).await;
    let url = format!(
        "/api/v1/admin/content/distribution?domain=library&content_type=library_item&id={lib_id}"
    );
    let (status, body) = env.call(authed(req(Method::GET, &url, None), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["domain"], json!("library"));
    for ch in [
        "web",
        "mobile",
        "desktop",
        "telegram",
        "ai_knowledge",
        "search",
        "embed",
    ] {
        assert_eq!(body["channels"][ch]["source"], json!("default"), "{body}");
        assert_eq!(body["channels"][ch]["enabled"], json!(true), "{body}");
    }
    // Missing query param → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/distribution?domain=library",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    // Set one channel.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &url,
                Some(json!({
                    "channel": "telegram",
                    "enabled": false,
                    "schedule_tz": "Asia/Tehran",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channel"], json!("telegram"));
    assert_eq!(body["enabled"], json!(false));
    assert_eq!(body["schedule_tz"], json!("Asia/Tehran"));
    assert_eq!(body["source"], json!("explicit"));
    // GET reflects the explicit row; others stay default.
    let (status, body) = env.call(authed(req(Method::GET, &url, None), &token)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channels"]["telegram"]["source"], json!("explicit"));
    assert_eq!(body["channels"]["telegram"]["enabled"], json!(false));
    assert_eq!(body["channels"]["web"]["source"], json!("default"));
    // Bad channel → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &url,
                Some(json!({ "channel": "nope", "enabled": true })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_channel");
    // Bad window → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &url,
                Some(json!({
                    "channel": "web",
                    "enabled": true,
                    "scheduled_at": "2026-12-02T00:00:00Z",
                    "until_at": "2026-12-01T00:00:00Z",
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_window");
    // Unknown adapter → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/distribution?domain=nope&content_type=nope&id=x",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "adapter_not_found");
}

#[tokio::test]
async fn distribution_native_source_for_cms() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-natdist@example.com").await;
    // A page with a native distribution JSONB.
    let id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO pages (id, slug, title, blocks, seo_meta, content_type, \
             created_at, updated_at, workflow_status, version, created_by, distribution) \
             VALUES (?, ?, ?, '[]', '{}', 'page', ?, ?, 'draft', 1, NULL, ?)",
            &[
                id.into_arg(),
                format!("slug-{id}").into_arg(),
                json!({"fa": "صفحه", "ar": "صفحه", "en": "page"}).into_arg(),
                now.into_arg(),
                now.into_arg(),
                json!({"web": true, "mobile": false, "desktop": false, "telegram": false, "ai_knowledge": false}).into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!(
                    "/api/v1/admin/content/distribution?domain=content&content_type=page&id={id}"
                ),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["channels"]["web"]["source"], json!("native"));
    assert_eq!(body["channels"]["web"]["enabled"], json!(true));
    assert_eq!(body["channels"]["mobile"]["source"], json!("native"));
    assert_eq!(body["channels"]["mobile"]["enabled"], json!(false));
    // search/embed are not in the native dict → default.
    assert_eq!(body["channels"]["search"]["source"], json!("default"));
}

#[tokio::test]
async fn distribution_policy_patch() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-pol@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ سیاست", "published", 10, vec![]).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/content/items/library/library_item/{lib_id}/distribution"),
                Some(json!({ "policy": { "web": false, "mobile": true } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["policy"]["web"]["enabled"], json!(false));
    assert_eq!(body["policy"]["web"]["source"], json!("explicit"));
    assert_eq!(body["policy"]["mobile"]["enabled"], json!(true));
    // Bad channel in the policy → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/content/items/library/library_item/{lib_id}/distribution"),
                Some(json!({ "policy": { "nope": true } })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_channel");
    // Empty policy → 422 validation_error.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/content/items/library/library_item/{lib_id}/distribution"),
                Some(json!({ "policy": {} })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

// ============================ calendar ====================================

#[tokio::test]
async fn calendar_feed_and_conflicts() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-cal@example.com").await;
    // Published 1 hour ago → inside the window.
    let lib_id = seed_library_item(&env, "جزوهٔ منتشر", "published", 3600, vec![]).await;
    // Scheduled distribution 1 hour from now.
    let future = (Utc::now() + Duration::seconds(3600)).to_rfc3339();
    let (status, _) = env
        .call(authed(
            req(
                Method::PUT,
                &format!(
                    "/api/v1/admin/content/distribution?domain=library&content_type=library_item&id={lib_id}"
                ),
                Some(json!({
                    "channel": "web",
                    "enabled": true,
                    "scheduled_at": future,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    // Two overlapping placements in the same slot.
    let other_id = seed_library_item(&env, "جزوهٔ دوم", "published", 7200, vec![]).await;
    let start = (Utc::now() + Duration::seconds(100)).to_rfc3339();
    let end = (Utc::now() + Duration::seconds(5000)).to_rfc3339();
    for cid in [&lib_id, &other_id] {
        let (status, _) = env
            .call(authed(
                req(
                    Method::PUT,
                    "/api/v1/admin/content/placements",
                    Some(json!({
                        "domain": "library",
                        "content_type": "library_item",
                        "content_id": cid.to_string(),
                        "surface": "homepage.hero",
                        "slot": "main",
                        "starts_at": start,
                        "ends_at": end,
                    })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
    }
    let from = (Utc::now() - Duration::seconds(86400)).to_rfc3339();
    let to = (Utc::now() + Duration::seconds(86400)).to_rfc3339();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!(
                    "/api/v1/admin/content/calendar?date_from={}&date_to={}",
                    from.replace('+', "%2B"),
                    to.replace('+', "%2B")
                ),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let kinds: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"published"), "{body}");
    assert!(kinds.contains(&"scheduled"), "{body}");
    assert_eq!(body["conflicts"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["conflicts"][0]["surface"], json!("homepage.hero"));
    assert_eq!(body["conflicts"][0]["slot"], json!("main"));
    // Missing dates → 422.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/calendar", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    // Bad date → 422 bad_datetime.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/calendar?date_from=nah&date_to=nah",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_datetime");
}

// ============================ transitions =================================

#[tokio::test]
async fn transition_single_library() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-tr@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ گذار", "draft", 10, vec![]).await;
    let url = format!("/api/v1/admin/content/items/library/library_item/{lib_id}/transition");
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "in_review" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ok"], json!(true));
    assert_eq!(body["from"], json!("draft"));
    assert_eq!(body["to"], json!("in_review"));
    // Same target again → skipped.
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "in_review" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["skipped"], json!(true));
    assert_eq!(body["reason"], json!("already_in_target_status"));
    // Illegal edge → 422 invalid_transition.
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "published" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");
    // Unknown target → 422 unknown_status.
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "nope" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_status");
    // Unsupported adapter (course) → 422 transition_unsupported.
    let course_id = seed_course(&env, "دورهٔ گذار", true, 10).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/content/items/lms/course/{course_id}/transition"),
                Some(json!({ "to": "archived" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "transition_unsupported");
    // Missing item → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/items/library/library_item/00000000-0000-0000-0000-000000000000/transition",
                Some(json!({ "to": "in_review" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "not_found");
}

#[tokio::test]
async fn transition_submission_approver_gate() {
    let env = Env::sqlite().await;
    let (author_id, token) = admin_user(&env, "acc-subtr@example.com").await;
    let sub_id = seed_submission(&env, as_uuid(&author_id), "draft", 10).await;
    let url = format!("/api/v1/admin/content/items/fatwa/fatwa_submission/{sub_id}/transition");
    // Author → in_review works.
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "in_review" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["to"], json!("in_review"));
    // Approve without fatwa.approve → 403.
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "approved" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
    // Grant fatwa.approve → approve works.
    grant(&env, &author_id, &["fatwa.approve"]).await;
    let (status, body) = env
        .call(authed(
            req(Method::POST, &url, Some(json!({ "to": "approved" }))),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["to"], json!("approved"));
}

#[tokio::test]
async fn transition_cms_page_publish_sets_defaults() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-cmstr@example.com").await;
    let page_id = seed_page(&env, "صفحهٔ انتشار", "approved", 10).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/content/items/content/page/{page_id}/transition"),
                Some(json!({ "to": "published" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["to"], json!("published"));
    // published_at set + distribution defaulted on the row.
    let row = env
        .db
        .fetch_optional(
            "SELECT published_at, distribution, version FROM pages WHERE id = ?",
            &[page_id.into_arg()],
        )
        .await
        .unwrap()
        .unwrap();
    assert!(row
        .try_get::<Option<chrono::DateTime<Utc>>, _>("published_at")
        .unwrap()
        .is_some());
    let dist: Value = row.try_get("distribution").unwrap();
    assert_eq!(dist["web"], json!(true));
    assert_eq!(dist["mobile"], json!(false));
    let version: i32 = row.try_get("version").unwrap();
    assert_eq!(version, 2);
}

// ============================ bulk ========================================

#[tokio::test]
async fn bulk_transition_and_export() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-bulk@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ گروهی یک", "draft", 10, vec![]).await;
    let page_id = seed_page(&env, "صفحهٔ گروهی", "draft", 20).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [
                        { "domain": "library", "content_type": "library_item", "id": lib_id.to_string() },
                        { "domain": "content", "content_type": "page", "id": page_id.to_string() },
                        { "domain": "nope", "content_type": "nope", "id": "x" },
                    ],
                    "action": "transition",
                    "params": { "to": "in_review" },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], json!("transition"));
    assert_eq!(body["total"], json!(3));
    assert_eq!(body["succeeded"], json!(2));
    assert_eq!(body["failed"], json!(1));
    assert_eq!(body["skipped"], json!(0));
    assert_eq!(body["results"][0]["ok"], json!(true));
    assert_eq!(body["results"][0]["from"], json!("draft"));
    assert_eq!(body["results"][0]["to"], json!("in_review"));
    assert_eq!(body["results"][2]["ok"], json!(false));
    assert_eq!(
        body["results"][2]["error"]["code"],
        json!("adapter_not_found")
    );
    // Same transition again → both skipped.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [
                        { "domain": "library", "content_type": "library_item", "id": lib_id.to_string() },
                        { "domain": "content", "content_type": "page", "id": page_id.to_string() },
                    ],
                    "action": "transition",
                    "params": { "to": "in_review" },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["succeeded"], json!(2));
    assert_eq!(body["skipped"], json!(2));
    // Export.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [
                        { "domain": "library", "content_type": "library_item", "id": lib_id.to_string() },
                    ],
                    "action": "export",
                    "params": {},
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["export"].as_array().unwrap().len(), 1);
    assert_eq!(body["export"][0]["title"], json!("جزوهٔ گروهی یک"));
    // Aliases set_placement / set_distribution.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [
                        { "domain": "library", "content_type": "library_item", "id": lib_id.to_string() },
                    ],
                    "action": "set_distribution",
                    "params": { "channels": { "web": { "enabled": false } } },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["action"], json!("distribution"));
    assert_eq!(body["succeeded"], json!(1));
}

#[tokio::test]
async fn bulk_validation_errors() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-bulkerr@example.com").await;
    // Bad action → 422 bad_bulk_action.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [{ "domain": "library", "content_type": "library_item", "id": "x" }],
                    "action": "nope",
                    "params": {},
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_bulk_action");
    // Empty items → 422 validation_error.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({ "items": [], "action": "export", "params": {} })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    // Missing `to` for a transition → per-item error, not a 422.
    let lib_id = seed_library_item(&env, "جزوهٔ بدون مقصد", "draft", 10, vec![]).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [{ "domain": "library", "content_type": "library_item", "id": lib_id.to_string() }],
                    "action": "transition",
                    "params": {},
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["failed"], json!(1));
    assert_eq!(body["results"][0]["error"]["code"], json!("missing_target"));
    // Bad channel in a schedule → per-item error.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/bulk",
                Some(json!({
                    "items": [{ "domain": "library", "content_type": "library_item", "id": lib_id.to_string() }],
                    "action": "schedule",
                    "params": { "channels": ["nope"] },
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["results"][0]["error"]["code"], json!("bad_channel"));
}

// ============================ audit =======================================

#[tokio::test]
async fn audit_feed_filters() {
    let env = Env::sqlite().await;
    let (uid, token) = admin_user(&env, "acc-audit@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ حسابرسی", "draft", 10, vec![]).await;
    // Produce audit rows: a transition + a placement upsert.
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/content/items/library/library_item/{lib_id}/transition"),
                Some(json!({ "to": "in_review" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = put_placement(
        &env,
        &token,
        PlacementArgs {
            domain: "library",
            content_type: "library_item",
            content_id: &lib_id.to_string(),
            surface: "homepage.hero",
            slot: None,
            sort_order: 0,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // Filtered by domain + content_type (domain alone is a 404 in
    // Python too: `get_adapter(domain, content_type or "")`).
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/audit?domain=library&content_type=library_item",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let actions: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"content.transition"), "{body}");
    assert!(actions.contains(&"library.item.transition"), "{body}");
    // Filtered by item id.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/content/audit?id={lib_id}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .all(|a| a["entity_id"] == lib_id.to_string()),
        "{body}"
    );
    // Filtered by actor.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/content/audit?actor_id={uid}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.as_array().unwrap().is_empty(), "{body}");
    // Bad actor_id → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/audit?actor_id=nope",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_actor_id");
    // Unknown domain → 404.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/audit?domain=nope", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "adapter_not_found");
}

// ============================ AI sources ==================================

#[tokio::test]
async fn ai_sources_and_reindex() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user(&env, "acc-ai@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ هوش", "published", 10, vec!["fatwa", "quiz"]).await;
    seed_library_item(&env, "جزوهٔ بدون اسکوپ", "published", 20, vec![]).await;
    seed_chunk(&env, "متن chunk دانش", "approved", vec!["fatwa"]).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/ai-sources", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2, "{body}");
    let lib = items
        .iter()
        .find(|i| i["id"] == lib_id.to_string())
        .expect("library source");
    assert_eq!(lib["reindex_supported"], json!(true));
    assert_eq!(lib["ai_scopes"], json!(["fatwa", "quiz"]));
    let chunk = items
        .iter()
        .find(|i| i["content_type"] == "knowledge_chunk")
        .expect("chunk source");
    assert_eq!(chunk["reindex_supported"], json!(false));
    // Scope filter.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/ai-sources?scope=quiz",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    // Reindex → 202, run queued, enqueued false.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/content/ai-sources/library/library_item/{lib_id}/reindex"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    let run_id = body["run_id"].as_str().expect("run_id").to_string();
    assert_eq!(body["status"], json!("queued"));
    let row = env
        .db
        .fetch_optional(
            "SELECT source_type, status, stats FROM ingestion_runs WHERE id = ?",
            &[as_uuid(&run_id).into_arg()],
        )
        .await
        .unwrap()
        .expect("ingestion run row");
    let source_type: String = row.try_get("source_type").unwrap();
    let run_status: String = row.try_get("status").unwrap();
    let stats: Value = row.try_get("stats").unwrap();
    assert_eq!(source_type, "file");
    assert_eq!(run_status, "queued");
    assert_eq!(stats["enqueued"], json!(false));
    assert_eq!(stats["run_config"]["ai_scopes"], json!(["fatwa", "quiz"]));
    // Reindex on a non-library adapter → 422.
    let course_id = seed_course(&env, "دورهٔ هوش", true, 10).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/content/ai-sources/lms/course/{course_id}/reindex"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "reindex_unsupported");
    // Reindex on a missing item → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/content/ai-sources/library/library_item/00000000-0000-0000-0000-000000000000/reindex",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "not_found");
}

// ============================ Postgres smoke ==============================

/// Critical path on Postgres: placement upsert → list → transition →
/// audit feed. Gated on `WAVE2_PG_URL`.
async fn pg_drop_and_init(url: &str) -> Db {
    let db = Db::connect(url).await.expect("postgres connect");
    // Drop every table `TEST_SCHEMA` creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run would
    // fail the schema apply. The list is derived from the schema itself
    // (wave5 pattern). Binaries must run sequentially — the smokes
    // share one database.
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
    init_test_schema(&db).await.expect("test schema");
    db
}

#[tokio::test]
async fn wave7_admin_content_pg_smoke() {
    let pg_url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("wave7_admin_content_pg_smoke: WAVE2_PG_URL unset, skipping");
            return;
        }
    };
    let db = pg_drop_and_init(&pg_url).await;
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };
    let (_, token) = admin_user(&env, "acc-pg-smoke@example.com").await;
    let lib_id = seed_library_item(&env, "جزوهٔ دود", "draft", 10, vec!["fatwa"]).await;
    // Placement upsert → list.
    let (status, body) = put_placement(
        &env,
        &token,
        PlacementArgs {
            domain: "library",
            content_type: "library_item",
            content_id: &lib_id.to_string(),
            surface: "homepage.hero",
            slot: Some("main"),
            sort_order: 0,
        },
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/placements?surface=homepage.hero",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");
    // Transition draft → in_review.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/content/items/library/library_item/{lib_id}/transition"),
                Some(json!({ "to": "in_review" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["to"], json!("in_review"));
    // Audit feed shows the transition.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/content/audit?domain=library&content_type=library_item",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let actions: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"content.transition"), "{body}");
    // Inbox lists the item with its placement-backed detail.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/content/items", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(body["items"][0]["status"], json!("in_review"));
}
