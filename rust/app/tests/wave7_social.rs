//! Wave 7 (social) integration tests.
//!
//! Groups, feed/posts, comments, likes, follows, reports + moderation
//! actions, banned words, and the social notifications surface.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the end
//! (`wave7_social_pg_smoke`, gated on `WAVE2_PG_URL`) replays the
//! critical path (group → post → like → comment) against a local
//! Postgres test database.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::{DateTime, Duration, Utc};
use maturidi_api_rs::{
    build_router, init_test_schema, AppState, Arg, Config, Db, IntoArg, TEST_SCHEMA,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave7-social-tests-only".to_string(),
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

/// Register a user with `social.moderate` (group create + report queue).
async fn moderator(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["social.moderate"]).await;
    (uid, token)
}

/// Register a user with `moderation.words`.
async fn words_admin(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["moderation.words"]).await;
    (uid, token)
}

fn localized(fa: &str, ar: &str, en: &str) -> Value {
    json!({ "fa": fa, "ar": ar, "en": en })
}

async fn create_group(
    env: &Env,
    token: &str,
    name_fa: &str,
    slug: Option<&str>,
    is_public: bool,
) -> (StatusCode, Value) {
    let mut body = json!({
        "name": localized(name_fa, name_fa, name_fa),
        "is_public": is_public,
    });
    if let Some(s) = slug {
        body["slug"] = json!(s);
    }
    env.call(authed(
        req(Method::POST, "/api/v1/social/groups", Some(body)),
        token,
    ))
    .await
}

async fn create_post(
    env: &Env,
    token: &str,
    body_text: &str,
    group_id: Option<&str>,
) -> (StatusCode, Value) {
    let mut body = json!({ "body": body_text });
    if let Some(g) = group_id {
        body["group_id"] = json!(g);
    }
    env.call(authed(
        req(Method::POST, "/api/v1/social/posts", Some(body)),
        token,
    ))
    .await
}

async fn add_banned_word(env: &Env, token: &str, word: &str) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            "/api/v1/moderation/banned-words",
            Some(json!({ "word": word })),
        ),
        token,
    ))
    .await
}

// --- groups ------------------------------------------------------------------

#[tokio::test]
async fn create_group_requires_moderate() {
    let env = Env::sqlite().await;
    let (_, plain_token, _) = register_user(&env, "plain@example.com").await;
    let (status, body) = create_group(&env, &plain_token, "گروه تست", None, true).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");

    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (status, body) = create_group(&env, &mod_token, "گروه تست", None, true).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["slug"], "گروه-تست");
    assert_eq!(body["member_count"], 1);
    assert_eq!(body["is_member"], true);
    assert_eq!(body["name"]["fa"], "گروه تست");
}

#[tokio::test]
async fn create_group_duplicate_slug() {
    let env = Env::sqlite().await;
    let (_, token) = moderator(&env, "mod@example.com").await;
    let (status, _) = create_group(&env, &token, "گروه یک", Some("dup-slug"), true).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = create_group(&env, &token, "گروه دو", Some("dup-slug"), true).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "duplicate_group_slug");
}

#[tokio::test]
async fn list_groups_pagination() {
    let env = Env::sqlite().await;
    let (_, token) = moderator(&env, "mod@example.com").await;
    for name in ["گروه یک", "گروه دو", "گروه سه"] {
        let (status, _) = create_group(&env, &token, name, None, true).await;
        assert_eq!(status, StatusCode::CREATED);
    }
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/groups?limit=2", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert!(body["next_cursor"].is_string());
    assert!(body["items"][0]["is_member"].as_bool().unwrap());
    let cursor = body["next_cursor"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/social/groups?limit=2&after={cursor}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["next_cursor"], Value::Null);
}

#[tokio::test]
async fn get_group_not_found_and_bad_uuid() {
    let env = Env::sqlite().await;
    let (_, token) = moderator(&env, "mod@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/social/groups/{}", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "group_not_found");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/groups/not-a-uuid", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

#[tokio::test]
async fn join_leave_idempotent() {
    let env = Env::sqlite().await;
    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (_, user_token, _) = register_user(&env, "user@example.com").await;
    let (_, group) = create_group(&env, &mod_token, "گروه تست", None, true).await;
    let gid = group["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/groups/{gid}/join"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["member_count"], 2);
    assert_eq!(body["is_member"], true);
    // Join again: no double count.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/groups/{gid}/join"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["member_count"], 2);

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/groups/{gid}/leave"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["member_count"], 1);
    assert_eq!(body["is_member"], false);
    // Leave again: stays 1 (the creator).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/groups/{gid}/leave"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["member_count"], 1);
}

// --- posts, feed, likes, comments --------------------------------------------

#[tokio::test]
async fn feed_and_post_crud() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "user@example.com").await;
    let (status, post) = create_post(&env, &token, "سلام به همه", None).await;
    assert_eq!(status, StatusCode::CREATED, "{post}");
    assert_eq!(post["author_id"], uid);
    assert_eq!(post["status"], "published");
    assert_eq!(post["likes_count"], 0);
    assert_eq!(post["liked"], false);
    let pid = post["id"].as_str().unwrap().to_string();

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/feed", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], pid);

    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/social/posts/{pid}"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["body"], "سلام به همه");

    // Validation: empty / too-long body.
    let (status, body) = create_post(&env, &token, "", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
    let (status, body) = create_post(&env, &token, &"x".repeat(5001), None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn private_group_post_rules() {
    let env = Env::sqlite().await;
    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (_, outsider_token, _) = register_user(&env, "out@example.com").await;
    let (_, group) = create_group(&env, &mod_token, "گروه خصوصی", None, false).await;
    let gid = group["id"].as_str().unwrap().to_string();
    assert_eq!(group["is_public"], false);

    // Non-member cannot post to a private group.
    let (status, body) = create_post(&env, &outsider_token, "پست خصوصی", Some(&gid)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "group_not_joined");

    // After joining, posting works.
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/groups/{gid}/join"),
                None,
            ),
            &outsider_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = create_post(&env, &outsider_token, "پست خصوصی", Some(&gid)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    // Missing group → 404.
    let (status, body) = create_post(
        &env,
        &outsider_token,
        "پست",
        Some(&Uuid::new_v4().to_string()),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "group_not_found");

    // Group posts listing is group-scoped.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/social/groups/{gid}/posts"),
                None,
            ),
            &outsider_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/social/groups/{}/posts", Uuid::new_v4()),
                None,
            ),
            &outsider_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn like_unlike_idempotent() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "user@example.com").await;
    let (_, other_token, _) = register_user(&env, "other@example.com").await;
    let (_, post) = create_post(&env, &token, "پست لایک", None).await;
    let pid = post["id"].as_str().unwrap().to_string();

    let like = |tok: &str| {
        env.call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{pid}/like"),
                None,
            ),
            tok,
        ))
    };
    let (status, body) = like(&token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "liked": true, "likes_count": 1 }));
    // Second like: same state, no double count.
    let (status, body) = like(&token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "liked": true, "likes_count": 1 }));
    let (status, body) = like(&other_token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "liked": true, "likes_count": 2 }));

    // Feed reflects the liked flag for the liker only.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/feed", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["liked"], true);
    assert_eq!(body["items"][0]["likes_count"], 2);

    let unlike = |tok: &str| {
        env.call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/social/posts/{pid}/like"),
                None,
            ),
            tok,
        ))
    };
    let (status, body) = unlike(&token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "liked": false, "likes_count": 1 }));
    let (status, body) = unlike(&token).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "liked": false, "likes_count": 1 }));
}

#[tokio::test]
async fn removed_post_rejects_like_and_comment() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "user@example.com").await;
    let (_, post) = create_post(&env, &token, "پست حذف‌شونده", None).await;
    let pid = post["id"].as_str().unwrap().to_string();
    env.db
        .execute(
            "UPDATE social_posts SET status = 'removed' WHERE id = ?",
            &[as_uuid(&pid).into_arg()],
        )
        .await
        .unwrap();

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{pid}/like"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "post_not_found");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{pid}/comments"),
                Some(json!({ "body": "کامنت" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Removed posts disappear from the feed.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/feed", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn comments_flow_oldest_first() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "user@example.com").await;
    let (_, post) = create_post(&env, &token, "پست کامنت", None).await;
    let pid = post["id"].as_str().unwrap().to_string();
    for text in ["کامنت اول", "کامنت دوم", "کامنت سوم"] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::POST,
                    &format!("/api/v1/social/posts/{pid}/comments"),
                    Some(json!({ "body": text })),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["body"], text);
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/social/posts/{pid}/comments?limit=2"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["body"], "کامنت اول");
    assert_eq!(body["items"][1]["body"], "کامنت دوم");
    assert!(body["next_cursor"].is_string());
    let cursor = body["next_cursor"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/social/posts/{pid}/comments?limit=2&after={cursor}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["body"], "کامنت سوم");
    assert_eq!(body["next_cursor"], Value::Null);

    // Post shows the comment count.
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/social/posts/{pid}"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["comment_count"], 3);

    // Comment on a missing post → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{}/comments", Uuid::new_v4()),
                Some(json!({ "body": "x" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

// --- banned words --------------------------------------------------------------

#[tokio::test]
async fn banned_word_filter_normalizes() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "user@example.com").await;
    let (_, wtoken) = words_admin(&env, "words@example.com").await;
    // Banned word written with Arabic Yeh (U+064A); the post uses Farsi
    // Yeh (U+06CC) — the normalizer must still match.
    let (status, body) = add_banned_word(&env, &wtoken, "تستي").await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = create_post(&env, &token, "این یک تستی است", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "content_policy_violation");
    // The matched word is never echoed back.
    assert!(!body.to_string().contains("تست"));

    // Diacritics are stripped before matching: banned "سلام" matches
    // "سَلَام" in a comment.
    let (status, _) = add_banned_word(&env, &wtoken, "سلام").await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, post) = create_post(&env, &token, "پست سالم", None).await;
    let pid = post["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{pid}/comments"),
                Some(json!({ "body": "سَلَام بر شما" })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "content_policy_violation");

    // Clean content still passes.
    let (status, _) = create_post(&env, &token, "متن کاملاً سالم", None).await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn banned_words_crud() {
    let env = Env::sqlite().await;
    let (_, plain_token, _) = register_user(&env, "plain@example.com").await;
    let (_, wtoken) = words_admin(&env, "words@example.com").await;

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/moderation/banned-words", None),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/moderation/banned-words", None),
            &wtoken,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!([]));

    let (status, word) = add_banned_word(&env, &wtoken, "  بد  ").await;
    assert_eq!(status, StatusCode::CREATED, "{word}");
    assert_eq!(word["word"], "بد");
    let wid = word["id"].as_str().unwrap().to_string();

    let (status, body) = add_banned_word(&env, &wtoken, "بد").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "duplicate_banned_word");
    let (status, body) = add_banned_word(&env, &wtoken, "   ").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "empty_word");

    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/moderation/banned-words/{wid}"),
                None,
            ),
            &wtoken,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/moderation/banned-words/{wid}"),
                None,
            ),
            &wtoken,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "banned_word_not_found");
}

// --- follows -------------------------------------------------------------------

#[tokio::test]
async fn follows_flow() {
    let env = Env::sqlite().await;
    let (uid_a, token_a, _) = register_user(&env, "a@example.com").await;
    let (uid_b, _, _) = register_user(&env, "b@example.com").await;

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/social/follows",
                Some(json!({ "followed_id": uid_b })),
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["follower_id"], uid_a);
    assert_eq!(body["followed_id"], uid_b);
    let fid = body["id"].as_str().unwrap().to_string();
    // Following again returns the same row.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/social/follows",
                Some(json!({ "followed_id": uid_b })),
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], fid);

    // Self-follow → 422; missing user → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/social/follows",
                Some(json!({ "followed_id": uid_a })),
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "self_follow");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/social/follows",
                Some(json!({ "followed_id": Uuid::new_v4().to_string() })),
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "user_not_found");

    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/social/follows/{uid_b}"),
                None,
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "unfollowed": true }));
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/social/follows/{uid_b}"),
                None,
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "unfollowed": false }));
}

// --- reports + moderation -------------------------------------------------------

async fn report(
    env: &Env,
    token: &str,
    target_type: &str,
    target_id: &str,
    reason: &str,
) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            "/api/v1/social/report",
            Some(json!({
                "target_type": target_type,
                "target_id": target_id,
                "reason": reason,
            })),
        ),
        token,
    ))
    .await
}

async fn report_action(
    env: &Env,
    token: &str,
    report_id: &str,
    action: &str,
    note: Option<&str>,
) -> (StatusCode, Value) {
    let mut body = json!({ "action": action });
    if let Some(n) = note {
        body["note"] = json!(n);
    }
    env.call(authed(
        req(
            Method::POST,
            &format!("/api/v1/admin/social/reports/{report_id}/action"),
            Some(body),
        ),
        token,
    ))
    .await
}

#[tokio::test]
async fn reports_queue_and_triage_dismiss() {
    let env = Env::sqlite().await;
    let (_, author_token, _) = register_user(&env, "author@example.com").await;
    let (_, reporter_token, _) = register_user(&env, "reporter@example.com").await;
    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (_, post) = create_post(&env, &author_token, "پست گزارش‌شونده", None).await;
    let pid = post["id"].as_str().unwrap().to_string();

    // Plain users cannot read the queue.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/social/reports", None),
            &reporter_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (status, rep) = report(&env, &reporter_token, "post", &pid, "محتوای نامناسب").await;
    assert_eq!(status, StatusCode::CREATED, "{rep}");
    assert_eq!(rep["status"], "new");
    assert_eq!(rep["target_type"], "post");
    assert_eq!(rep["triaged_at"], Value::Null);
    assert_eq!(rep["actioned_at"], Value::Null);
    // SLA ≈ 24 h after the report time.
    let reported: DateTime<Utc> =
        DateTime::parse_from_rfc3339(rep["reported_at"].as_str().unwrap())
            .unwrap()
            .with_timezone(&Utc);
    let sla: DateTime<Utc> = DateTime::parse_from_rfc3339(rep["sla_due_at"].as_str().unwrap())
        .unwrap()
        .with_timezone(&Utc);
    assert_eq!(sla - reported, Duration::hours(24));
    let rid = rep["id"].as_str().unwrap().to_string();

    // Bad target type / missing target.
    let (status, body) = report(&env, &reporter_token, "video", &pid, "x").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = report(
        &env,
        &reporter_token,
        "post",
        &Uuid::new_v4().to_string(),
        "x",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "report_target_not_found");

    // Queue with a status filter.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/social/reports?status=new", None),
            &mod_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/social/reports?status=dismissed",
                None,
            ),
            &mod_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);

    // Triage only from `new`.
    let (status, body) = report_action(&env, &mod_token, &rid, "triage", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "triaged");
    assert_eq!(body["action"], "triage");
    assert!(body["triaged_at"].is_string());
    let (status, body) = report_action(&env, &mod_token, &rid, "triage", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_report_state");

    // Dismiss.
    let (status, body) = report_action(&env, &mod_token, &rid, "dismiss", Some("بی‌مورد")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "dismissed");
    assert_eq!(body["note"], "بی‌مورد");
    assert!(body["actioned_at"].is_string());

    // Unknown action → 422; missing report → 404.
    let (status, body) = report_action(&env, &mod_token, &rid, "explode", None).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = report_action(
        &env,
        &mod_token,
        &Uuid::new_v4().to_string(),
        "dismiss",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "report_not_found");
}

#[tokio::test]
async fn report_remove_content() {
    let env = Env::sqlite().await;
    let (_, author_token, _) = register_user(&env, "author@example.com").await;
    let (_, reporter_token, _) = register_user(&env, "reporter@example.com").await;
    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (_, post) = create_post(&env, &author_token, "پست تخلف", None).await;
    let pid = post["id"].as_str().unwrap().to_string();
    let (_, rep) = report(&env, &reporter_token, "post", &pid, "تخلف").await;
    let rid = rep["id"].as_str().unwrap().to_string();

    let (status, body) = report_action(&env, &mod_token, &rid, "remove_content", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "actioned");
    assert_eq!(body["action"], "remove_content");

    // The post is gone from the feed and can no longer be liked.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/feed", None),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/social/posts/{pid}"), None),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "removed");
}

#[tokio::test]
async fn report_warn_user_notifies_author() {
    let env = Env::sqlite().await;
    let (_, author_token, _) = register_user(&env, "author@example.com").await;
    let (_, reporter_token, _) = register_user(&env, "reporter@example.com").await;
    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (_, post) = create_post(&env, &author_token, "پست اخطار", None).await;
    let pid = post["id"].as_str().unwrap().to_string();
    let (_, rep) = report(&env, &reporter_token, "post", &pid, "تخلف").await;
    let rid = rep["id"].as_str().unwrap().to_string();

    let (status, body) =
        report_action(&env, &mod_token, &rid, "warn_user", Some("لطفاً رعایت کنید")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "actioned");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/notifications", None),
            &author_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["notif_type"], "social.warning");
    assert_eq!(body["items"][0]["title"]["fa"], "اخطار نظارت");
    assert_eq!(body["items"][0]["body"], "لطفاً رعایت کنید");
    assert_eq!(body["items"][0]["data"]["report_id"], rid);
    // The reporter got no notification.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/notifications", None),
            &reporter_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn report_ban_user_deactivates_and_revokes() {
    let env = Env::sqlite().await;
    let (uid_bad, bad_token, _) = register_user(&env, "bad@example.com").await;
    let (_, reporter_token, _) = register_user(&env, "reporter@example.com").await;
    let (_, mod_token) = moderator(&env, "mod@example.com").await;
    let (_, post) = create_post(&env, &bad_token, "پست بن", None).await;
    let pid = post["id"].as_str().unwrap().to_string();

    // Seed a live session + refresh token for the target, then ban.
    let now = Utc::now();
    let session_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO sessions (id, user_id, device_info, ip, last_active_at, revoked, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                session_id.into_arg(),
                as_uuid(&uid_bad).into_arg(),
                "test".into_arg(),
                "127.0.0.1".into_arg(),
                now.into_arg(),
                false.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    env.db
        .execute(
            "INSERT INTO refresh_tokens (id, user_id, session_id, token_hash, revoked, expires_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                as_uuid(&uid_bad).into_arg(),
                session_id.into_arg(),
                "hash".into_arg(),
                false.into_arg(),
                (now + Duration::hours(1)).into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();

    let (_, rep) = report(&env, &reporter_token, "post", &pid, "تخلف شدید").await;
    let rid = rep["id"].as_str().unwrap().to_string();
    let (status, body) = report_action(&env, &mod_token, &rid, "ban_user", None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "actioned");

    let row = env
        .db
        .fetch_one(
            "SELECT is_active FROM users WHERE id = ?",
            &[as_uuid(&uid_bad).into_arg()],
        )
        .await
        .unwrap();
    let is_active: bool = row.try_get("is_active").unwrap();
    assert!(!is_active);
    let row = env
        .db
        .fetch_one(
            "SELECT revoked FROM sessions WHERE id = ?",
            &[session_id.into_arg()],
        )
        .await
        .unwrap();
    let revoked: bool = row.try_get("revoked").unwrap();
    assert!(revoked);
    let row = env
        .db
        .fetch_one(
            "SELECT revoked FROM refresh_tokens WHERE user_id = ?",
            &[as_uuid(&uid_bad).into_arg()],
        )
        .await
        .unwrap();
    let revoked: bool = row.try_get("revoked").unwrap();
    assert!(revoked);
}

// --- notifications ---------------------------------------------------------------

async fn seed_notification(env: &Env, user_id: &str, title_fa: &str) -> String {
    let now = Utc::now();
    let id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO notifications (id, user_id, notif_type, title, body, data, read_at, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                as_uuid(user_id).into_arg(),
                "social.warning".into_arg(),
                json!({ "fa": title_fa, "ar": title_fa, "en": title_fa }).into_arg(),
                "متن".into_arg(),
                json!({ "k": 1 }).into_arg(),
                Arg::NullTime,
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id.to_string()
}

#[tokio::test]
async fn notifications_list_read_unread_count() {
    let env = Env::sqlite().await;
    let (uid_a, token_a, _) = register_user(&env, "a@example.com").await;
    let (_, token_b, _) = register_user(&env, "b@example.com").await;

    // Unauthenticated → 401.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/social/notifications", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

    let nid = seed_notification(&env, &uid_a, "اعلان تست").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/social/notifications/unread-count",
                None,
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "unread": 1 }));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/notifications", None),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["read_at"], Value::Null);
    assert_eq!(body["items"][0]["data"]["k"], 1);

    // Another user's notification is not readable.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/notifications/{nid}/read"),
                None,
            ),
            &token_b,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "notification_not_found");

    // Mark read: persists (SOC-02), and the unread count drops.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/notifications/{nid}/read"),
                None,
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["read_at"].is_string());
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/social/notifications/unread-count",
                None,
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "unread": 0 }));
    // Second read is idempotent.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/notifications/{nid}/read"),
                None,
            ),
            &token_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["read_at"].is_string());
}

#[tokio::test]
async fn invalid_cursor_is_400() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "user@example.com").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/social/feed?after=!!!not-a-cursor",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_cursor");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/groups?limit=0", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// --- Postgres smoke ------------------------------------------------------------

/// Wave 7 social Postgres smoke: group → post → like → comment against a
/// local Postgres test database. Gated on `WAVE2_PG_URL`.
#[tokio::test]
async fn wave7_social_pg_smoke() {
    let pg_url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("wave7_social_pg_smoke: WAVE2_PG_URL unset, skipping");
            return;
        }
    };
    let db = Db::connect(&pg_url).await.expect("pg connect");
    // Drop every table TEST_SCHEMA creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run would
    // fail the schema apply. The table list is derived from the schemas
    // themselves, so it cannot go stale.
    let mut tables = Vec::new();
    for schema in [TEST_SCHEMA] {
        for part in schema.split("CREATE TABLE") {
            let name: String = part
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                tables.push(name);
            }
        }
    }
    for table in tables {
        db.execute(&format!("DROP TABLE IF EXISTS {table} CASCADE"), &[])
            .await
            .unwrap();
    }
    init_test_schema(&db).await.expect("pg test schema");
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };

    let (_, mod_token) = moderator(&env, "pg-mod@example.com").await;
    let (_, user_token, _) = register_user(&env, "pg-user@example.com").await;

    // Group.
    let (status, group) = create_group(&env, &mod_token, "گروه PG", None, true).await;
    assert_eq!(status, StatusCode::CREATED, "{group}");
    let gid = group["id"].as_str().unwrap().to_string();
    assert_eq!(group["member_count"], 1);

    // Post in the group.
    let (status, post) = create_post(&env, &user_token, "پست PG", Some(&gid)).await;
    // The user is not a member yet, but the group is public, so posting works.
    assert_eq!(status, StatusCode::CREATED, "{post}");
    let pid = post["id"].as_str().unwrap().to_string();

    // Like.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{pid}/like"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({ "liked": true, "likes_count": 1 }));

    // Comment.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/social/posts/{pid}/comments"),
                Some(json!({ "body": "کامنت PG" })),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["body"], "کامنت PG");

    // Feed shows the post with its counts and liked flag.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/social/feed", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["likes_count"], 1);
    assert_eq!(body["items"][0]["comment_count"], 1);
    assert_eq!(body["items"][0]["liked"], true);
}
