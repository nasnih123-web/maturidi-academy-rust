//! Wave 6 integration tests: library + LMS + calculators.
//!
//! Library: public list/search/detail/stream/signed download, admin CRUD,
//! multipart uploads, workflow, versions, revert, AI indexing, import,
//! reprocessing.
//!
//! LMS: curricula, courses, lessons, progress + automatic certificate
//! issue, lesson quiz, study report, recommendations, cohorts +
//! enrollments, prerequisites, certificate verify/download.
//!
//! Calculators: zakat, inheritance/miras, mosaref, nisab prices, and rule
//! configs.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The combined Postgres smoke test at
//! the end (`wave6_pg_smoke`, gated on `WAVE2_PG_URL`) replays the three
//! critical paths sequentially against one local Postgres test database.
//!
//! Expected calculator values are pinned against the Python engines, not
//! fiqh knowledge (see the calculators module header in the wave6_calc
//! source this file was merged from).

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::{Duration, Utc};
use maturidi_api_rs::{
    build_router, encrypt_value, init_test_schema, presigned_url, AppState, Config, Db, IntoArg,
    TEST_SCHEMA,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave6-library-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave6".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave6-library-storage-{}",
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
    /// stream redirect and the signed download, which answer outside
    /// the JSON envelope).
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

/// Register a user with `library.manage`.
async fn admin_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["library.manage"]).await;
    (uid, token)
}

/// Register a user with `library.manage` + `fatwa.approve`.
async fn approver_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["library.manage", "fatwa.approve"]).await;
    (uid, token)
}

/// Register a user with `lms.manage`.
async fn admin_user_lms(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["lms.manage"]).await;
    (uid, token)
}

/// Register a user with `calculators.manage` + `calculators.approve`.
async fn admin_user_calc(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["calculators.manage", "calculators.approve"]).await;
    (uid, token)
}

// ============================ seeds =====================================

struct ItemSeed {
    kind: &'static str,
    title_fa: &'static str,
    workflow_status: &'static str,
    visibility: &'static str,
    external_url: Option<&'static str>,
    media_key: Option<&'static str>,
    cover_media_key: Option<&'static str>,
    duration_seconds: Option<i64>,
    topic_tags: Vec<&'static str>,
    transcript: Option<&'static str>,
    mime_type: Option<&'static str>,
    ai_scopes: Vec<&'static str>,
    include_in_fatwa_rag: bool,
    created_secs_ago: i64,
}

impl Default for ItemSeed {
    fn default() -> Self {
        Self {
            kind: "pdf",
            title_fa: "جزوهٔ تست",
            workflow_status: "published",
            visibility: "public",
            external_url: None,
            media_key: Some("library/pdf/abc123.pdf"),
            cover_media_key: None,
            duration_seconds: None,
            topic_tags: vec![],
            transcript: None,
            mime_type: None,
            ai_scopes: vec![],
            include_in_fatwa_rag: false,
            created_secs_ago: 0,
        }
    }
}

async fn seed_item(env: &Env, s: ItemSeed) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(s.created_secs_ago);
    let published_at = if s.workflow_status == "published" {
        Some(now)
    } else {
        None
    };
    env.db
        .execute(
            "INSERT INTO library_items (id, kind, title, description, cover_media_key, \
             media_key, external_url, scholar_id, topic_tags, related_lesson_id, \
             religious_place_ref, visibility, workflow_status, version, ai_scopes, \
             include_in_fatwa_rag, ai_indexed, download_policy, transcript, \
             transcript_status, ocr_text, ocr_status, processing_error, duration_seconds, \
             file_size_bytes, mime_type, source_type, source_ref, created_by, created_at, \
             updated_at, published_at) \
             VALUES (?, ?, ?, NULL, ?, ?, ?, NULL, ?, NULL, NULL, ?, ?, 1, ?, ?, FALSE, NULL, ?, \
             'pending', NULL, 'pending', NULL, ?, NULL, ?, 'upload', NULL, NULL, ?, ?, ?)",
            &[
                id.into_arg(),
                s.kind.into_arg(),
                json!({"fa": s.title_fa, "ar": s.title_fa, "en": s.title_fa}).into_arg(),
                s.cover_media_key.into_arg(),
                s.media_key.into_arg(),
                s.external_url.into_arg(),
                json!(s.topic_tags).into_arg(),
                s.visibility.into_arg(),
                s.workflow_status.into_arg(),
                json!(s.ai_scopes).into_arg(),
                s.include_in_fatwa_rag.into_arg(),
                s.transcript.into_arg(),
                s.duration_seconds.into_arg(),
                s.mime_type.into_arg(),
                now.into_arg(),
                now.into_arg(),
                published_at.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

fn pdf_bytes() -> Vec<u8> {
    b"%PDF-1.4\n1 0 obj\n<< /Type /Catalog >>\nendobj\n".to_vec()
}

fn pdf_js_bytes() -> Vec<u8> {
    b"%PDF-1.4\n/JavaScript (alert(1))\n".to_vec()
}

fn mp4_bytes() -> Vec<u8> {
    b"\x00\x00\x00\x18ftypisom\x00\x00\x00\x00".to_vec()
}

const BOUNDARY: &str = "----testboundary1234";

enum Part {
    Text(String),
    File {
        filename: String,
        content_type: String,
        bytes: Vec<u8>,
    },
}

/// Build a multipart/form-data body: `metadata` text + optional files.
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

fn meta_json(value: Value) -> Part {
    Part::Text(serde_json::to_string(&value).unwrap())
}

fn title_json(fa: &str) -> Value {
    json!({"fa": fa, "ar": fa, "en": fa})
}

/// Create an item over HTTP (multipart upload) and return its id.
async fn create_pdf_via_api(env: &Env, token: &str, title_fa: &str) -> Uuid {
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({
                            "kind": "pdf",
                            "title": title_json(title_fa),
                            "visibility": "public",
                        })),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes: pdf_bytes(),
                        },
                    ),
                ],
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create pdf: {body}");
    as_uuid(body["id"].as_str().unwrap())
}

async fn transition_via_api(
    env: &Env,
    token: &str,
    item_id: Uuid,
    to: &str,
) -> (StatusCode, Value) {
    env.call(authed(
        req(
            Method::POST,
            &format!("/api/v1/admin/library/items/{item_id}/transition"),
            Some(json!({"to": to})),
        ),
        token,
    ))
    .await
}

// ============================ tests =======================================

#[tokio::test]
async fn public_list_visibility_matrix() {
    let env = Env::sqlite().await;
    let _public = seed_item(
        &env,
        ItemSeed {
            title_fa: "عمومی",
            visibility: "public",
            created_secs_ago: 30,
            ..Default::default()
        },
    )
    .await;
    let _registered = seed_item(
        &env,
        ItemSeed {
            title_fa: "ثبت‌شده",
            visibility: "registered",
            created_secs_ago: 20,
            ..Default::default()
        },
    )
    .await;
    let _scholars = seed_item(
        &env,
        ItemSeed {
            title_fa: "علما",
            visibility: "scholars_only",
            created_secs_ago: 10,
            ..Default::default()
        },
    )
    .await;
    let _draft = seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            ..Default::default()
        },
    )
    .await;

    // Guest: only public.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let titles: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"]["fa"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["عمومی"]);
    assert_eq!(body["next_cursor"], json!(null));

    // Logged-in user: public + registered.
    let (_, user_token, _) = register_user(&env, "user1@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/library/items", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let titles: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"]["fa"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["ثبت‌شده", "عمومی"]);

    // Privileged (library.manage): all published, including scholars_only.
    let (_, admin_token) = admin_user(&env, "admin1@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/library/items", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let titles: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"]["fa"].as_str().unwrap())
        .collect();
    assert_eq!(titles, vec!["علما", "ثبت‌شده", "عمومی"]);
}

#[tokio::test]
async fn public_list_filters_and_pagination() {
    let env = Env::sqlite().await;
    seed_item(
        &env,
        ItemSeed {
            kind: "video",
            title_fa: "درس ماتریدی",
            topic_tags: vec!["ماتریدی", "درس"],
            created_secs_ago: 30,
            ..Default::default()
        },
    )
    .await;
    seed_item(
        &env,
        ItemSeed {
            kind: "audio",
            title_fa: "صوت فقه",
            topic_tags: vec!["فقه"],
            created_secs_ago: 20,
            ..Default::default()
        },
    )
    .await;
    seed_item(
        &env,
        ItemSeed {
            kind: "pdf",
            title_fa: "جزوه ماتریدی",
            topic_tags: vec!["ماتریدی"],
            created_secs_ago: 10,
            ..Default::default()
        },
    )
    .await;

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?kind=audio", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["title"]["fa"], json!("صوت فقه"));

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?tag=ماتریدی", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?q=ماتریدی", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // Pagination: limit=1 walks newest-first.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?limit=1", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["title"]["fa"], json!("جزوه ماتریدی"));
    let cursor = body["next_cursor"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/library/items?limit=1&after={cursor}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["title"]["fa"], json!("صوت فقه"));
    assert!(body["next_cursor"].is_string());

    // Bad cursor → 400; bad kind → 422; bad limit → 422.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?after=!!!", None))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_cursor");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?kind=book", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items?limit=0", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn public_detail_and_search() {
    let env = Env::sqlite().await;
    let published = seed_item(
        &env,
        ItemSeed {
            title_fa: "درس ماتریدی",
            topic_tags: vec!["ماتریدی"],
            transcript: Some("متن رونویسی درباره ماتریدیه و اصول اعتقاد"),
            ..Default::default()
        },
    )
    .await;
    let draft = seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            ..Default::default()
        },
    )
    .await;
    let scholars = seed_item(
        &env,
        ItemSeed {
            title_fa: "علما",
            visibility: "scholars_only",
            ..Default::default()
        },
    )
    .await;

    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/library/items/{published}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["fa"], json!("درس ماتریدی"));
    assert_eq!(body["has_transcript"], json!(true));
    assert!(body["media_url"].is_string());
    assert!(
        body.get("transcript").is_none(),
        "public shape has no transcript"
    );

    for hidden in [draft, scholars] {
        let (status, body) = env
            .call(req(
                Method::GET,
                &format!("/api/v1/library/items/{hidden}"),
                None,
            ))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(err_code(&body), "item_not_found");
    }
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items/not-a-uuid", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Lexical search: title hit scores 3.0, matched_in sorted.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/search?q=ماتریدی", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["query"], json!("ماتریدی"));
    assert_eq!(body["hits"].as_array().unwrap().len(), 1);
    assert_eq!(body["hits"][0]["score"], json!(6.0));
    assert_eq!(
        body["hits"][0]["matched_in"],
        json!(["tags", "title", "transcript"])
    );
    // Too short → 422; missing → 422.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/search?q=م", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/search", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn stream_redirect_and_audit() {
    let env = Env::sqlite().await;
    let published = seed_item(&env, ItemSeed::default()).await;
    let draft = seed_item(
        &env,
        ItemSeed {
            workflow_status: "draft",
            ..Default::default()
        },
    )
    .await;
    let external = seed_item(
        &env,
        ItemSeed {
            kind: "video",
            media_key: None,
            external_url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            ..Default::default()
        },
    )
    .await;

    let (status, headers, _) = env
        .call_raw(req(
            Method::GET,
            &format!("/api/v1/library/items/{published}/stream"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::FOUND);
    let location = headers
        .get("location")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        location.contains("/api/v1/library/download-by-ref?key=library/pdf/abc123.pdf"),
        "{location}"
    );
    let row = env
        .db
        .fetch_optional(
            "SELECT id FROM audit_logs WHERE action = 'library.item.stream'",
            &[],
        )
        .await
        .unwrap();
    assert!(row.is_some(), "stream is audited");

    for missing in [draft, external] {
        let (status, body) = env
            .call(req(
                Method::GET,
                &format!("/api/v1/library/items/{missing}/stream"),
                None,
            ))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(err_code(&body), "item_not_found");
    }
}

#[tokio::test]
async fn download_by_ref_roundtrip() {
    let env = Env::sqlite().await;
    let key = "library/pdf/abc123.pdf";
    let data = pdf_bytes();
    let dir = format!("{}/library/pdf", env.state.config.storage_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/abc123.pdf"), &data).unwrap();
    let _item = seed_item(
        &env,
        ItemSeed {
            mime_type: Some("application/pdf"),
            ..Default::default()
        },
    )
    .await;

    let url = presigned_url(
        &env.state.config,
        key,
        300,
        "/api/v1/library/download-by-ref",
        "library",
    )
    .unwrap();
    let path = url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, headers, body) = env.call_raw(req(Method::GET, &path, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "application/pdf"
    );
    assert_eq!(body, data);

    // Wrong purpose → 403; expired → 403; unknown key → 404.
    let url = presigned_url(
        &env.state.config,
        key,
        300,
        "/api/v1/library/download-by-ref",
        "lms-certificate",
    )
    .unwrap();
    let path = url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, body) = env.call(req(Method::GET, &path, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "invalid_signature");

    let url = presigned_url(
        &env.state.config,
        key,
        -300,
        "/api/v1/library/download-by-ref",
        "library",
    )
    .unwrap();
    let path = url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, body) = env.call(req(Method::GET, &path, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let url = presigned_url(
        &env.state.config,
        "library/pdf/nope.pdf",
        300,
        "/api/v1/library/download-by-ref",
        "library",
    )
    .unwrap();
    let path = url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, body) = env.call(req(Method::GET, &path, None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "key_not_found");

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/download-by-ref", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn admin_list_requires_permission_and_filters() {
    let env = Env::sqlite().await;
    seed_item(
        &env,
        ItemSeed {
            title_fa: "منتشر",
            ..Default::default()
        },
    )
    .await;
    seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            ..Default::default()
        },
    )
    .await;

    // Guest → 401; authed without permission → 403.
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/admin/library/items", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (_, user_token, _) = register_user(&env, "plain@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/library/items", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let (_, admin_token) = admin_user(&env, "admin2@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/library/items", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    // Admin list: signed URLs are null, admin fields present.
    assert_eq!(body["items"][0]["media_url"], json!(null));
    assert!(body["items"][0].get("transcript").is_some());

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/library/items?status=draft",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["title"]["fa"], json!("پیش‌نویس"));

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/library/items?q=منتشر", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);

    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/library/items?status=bogus",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

#[tokio::test]
async fn admin_create_upload_happy_and_validation() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin3@example.com").await;

    let item_id = create_pdf_via_api(&env, &admin_token, "جزوهٔ تازه").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/library/items/{item_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("draft"));
    assert_eq!(body["version"], json!(1));
    assert_eq!(body["file_size_bytes"], json!(pdf_bytes().len()));
    assert_eq!(body["mime_type"], json!("application/pdf"));
    assert_eq!(body["source_type"], json!("upload"));
    assert!(body["media_url"].is_string());
    assert_eq!(body["transcript_status"], json!("pending"));
    // The file really landed in local storage.
    let stored = env
        .db
        .fetch_optional(
            "SELECT media_key FROM library_items WHERE id = ?",
            &[item_id.into_arg()],
        )
        .await
        .unwrap()
        .unwrap();
    let media_key: String = stored.try_get("media_key").unwrap();
    let path = format!("{}/{}", env.state.config.storage_dir, media_key);
    assert_eq!(std::fs::read(path).unwrap(), pdf_bytes());

    // Bad magic → 422 file_content_mismatch.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({"kind": "pdf", "title": title_json("بد")})),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes: b"NOT A PDF AT ALL".to_vec(),
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "file_content_mismatch");

    // Wrong extension for the kind → 422 invalid_file_extension.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({"kind": "pdf", "title": title_json("بد")})),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.mp4".to_string(),
                            content_type: "video/mp4".to_string(),
                            bytes: mp4_bytes(),
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_file_extension");

    // PDF with embedded JavaScript → 422 pdf_active_content.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({"kind": "pdf", "title": title_json("بد")})),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes: pdf_js_bytes(),
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "pdf_active_content");

    // No file and no external URL → 422 file_required.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({"kind": "pdf", "title": title_json("بد") })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "file_required");

    // Bad metadata JSON → 422 invalid_metadata; bad kind → same.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[("metadata", Part::Text("{bad json".to_string()))],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_metadata");
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({"kind": "book", "title": title_json("بد")})),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_metadata");
    // Missing metadata field → 422 validation_error.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "file",
                    Part::File {
                        filename: "test.pdf".to_string(),
                        content_type: "application/pdf".to_string(),
                        bytes: pdf_bytes(),
                    },
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

#[tokio::test]
async fn admin_create_upload_scanner_rejects_eicar_and_blocked_hash() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin4@example.com").await;

    // EICAR test signature → 422 file_infected + audit row.
    let eicar = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*".to_vec();
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({"kind": "pdf", "title": title_json("آلوده")})),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes: eicar,
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "file_infected");
    assert_eq!(
        body["error"]["details"]["signature"],
        json!("Eicar-Test-Signature")
    );
    let row = env
        .db
        .fetch_optional(
            "SELECT id FROM audit_logs WHERE action = 'library.upload_rejected_infected'",
            &[],
        )
        .await
        .unwrap();
    assert!(row.is_some(), "infected rejection is audited");
    // Nothing was stored.
    let count: i64 = env
        .db
        .fetch_one("SELECT COUNT(*) AS c FROM library_items", &[])
        .await
        .unwrap()
        .try_get("c")
        .unwrap();
    assert_eq!(count, 0);

    // Blocked SHA-256 hash → same rejection.
    let bytes = pdf_bytes();
    let hex = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    env.db
        .execute(
            "INSERT INTO platform_settings (key, value, encryption_key_version) VALUES (?, ?, 1)",
            &[
                ("library.blocked_hashes").into_arg(),
                encrypt_value(
                    &env.state.config,
                    &serde_json::to_string(&vec![hex]).unwrap(),
                )
                .into_arg(),
            ],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({"kind": "pdf", "title": title_json("مسدود")})),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes,
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "file_infected");
    assert_eq!(body["error"]["details"]["signature"], json!("blocked-hash"));
}

#[tokio::test]
async fn admin_create_external_videos() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin5@example.com").await;

    // YouTube with a manual title → 201, canonical watch URL, no media.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({
                        "kind": "video",
                        "title": title_json("ویدیوی یوتیوب"),
                        "external_url": "https://youtu.be/dQw4w9WgXcQ",
                        "visibility": "public",
                    })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["external_url"],
        json!("https://www.youtube.com/watch?v=dQw4w9WgXcQ")
    );
    assert_eq!(body["media_url"], json!(null));
    assert_eq!(body["file_size_bytes"], json!(null));
    assert_eq!(body["mime_type"], json!(null));

    // Direct https video-file link → kept verbatim.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({
                        "kind": "video",
                        "title": title_json("فایل مستقیم"),
                        "external_url": "https://cdn.example.com/lesson.mp4?x=1",
                    })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        body["external_url"],
        json!("https://cdn.example.com/lesson.mp4?x=1")
    );

    // External link on a non-video kind → 422 external_url_not_allowed.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({
                        "kind": "pdf",
                        "title": title_json("بد"),
                        "external_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                    })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "external_url_not_allowed");

    // Unparseable link → 422 invalid_youtube_url.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({
                        "kind": "video",
                        "title": title_json("بد"),
                        "external_url": "https://example.com/not-a-video",
                    })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_youtube_url");

    // YouTube without a manual title and with an unresolvable video id
    // (oEmbed 404s) → 422 title_required.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[(
                    "metadata",
                    meta_json(json!({
                        "kind": "video",
                        "external_url": "https://www.youtube.com/watch?v=AAAAAAAAAAA",
                    })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "title_required");

    // File wins over the link: an upload with an external_url ignores it.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::POST,
                "/api/v1/admin/library/items",
                &[
                    (
                        "metadata",
                        meta_json(json!({
                            "kind": "pdf",
                            "title": title_json("فایل برنده"),
                            "external_url": "https://example.com/ignored",
                        })),
                    ),
                    (
                        "file",
                        Part::File {
                            filename: "test.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes: pdf_bytes(),
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["external_url"], json!(null));
    assert!(body["media_url"].is_string());
}

#[tokio::test]
async fn admin_update_versions_and_revert() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin6@example.com").await;
    let item_id = create_pdf_via_api(&env, &admin_token, "عنوان اول").await;

    // Patch title → v2; patch visibility → v3.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::PUT,
                &format!("/api/v1/admin/library/items/{item_id}"),
                &[(
                    "metadata",
                    meta_json(json!({"title": title_json("عنوان دوم"), "note": "اصلاح"})),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(2));
    assert_eq!(body["title"]["fa"], json!("عنوان دوم"));
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::PUT,
                &format!("/api/v1/admin/library/items/{item_id}"),
                &[("metadata", meta_json(json!({"visibility": "public"})))],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(3));
    assert_eq!(body["visibility"], json!("public"));

    // Versions: newest first (v2, v1); v2 snapshot holds the v1 title.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/library/items/{item_id}/versions"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let versions = body.as_array().unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0]["version"], json!(2));
    assert_eq!(versions[1]["version"], json!(1));
    assert_eq!(versions[1]["snapshot"]["title"]["fa"], json!("عنوان اول"));
    assert_eq!(versions[1]["note"], json!("اصلاح"));

    // Revert to v1 → title restored, version 4.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/versions/1/revert"),
                Some(json!({})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(4));
    assert_eq!(body["title"]["fa"], json!("عنوان اول"));

    // Unknown version → 404 version_not_found.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/versions/99/revert"),
                Some(json!({})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "version_not_found");
}

#[tokio::test]
async fn admin_update_file_replacement_and_external_rules() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin7@example.com").await;
    // Seed a published pdf with a transcript so replacement resets it.
    let item_id = seed_item(
        &env,
        ItemSeed {
            title_fa: "جزوه",
            transcript: Some("متن قدیمی"),
            ai_scopes: vec!["fatwa"],
            include_in_fatwa_rag: true,
            ..Default::default()
        },
    )
    .await;
    env.db
        .execute(
            "UPDATE library_items SET ai_indexed = TRUE WHERE id = ?",
            &[item_id.into_arg()],
        )
        .await
        .unwrap();

    // Replace the file: new media key, transcript/AI reset, version bump.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::PUT,
                &format!("/api/v1/admin/library/items/{item_id}"),
                &[
                    ("metadata", meta_json(json!({}))),
                    (
                        "file",
                        Part::File {
                            filename: "new.pdf".to_string(),
                            content_type: "application/pdf".to_string(),
                            bytes: pdf_bytes(),
                        },
                    ),
                ],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(2));
    assert_eq!(body["has_transcript"], json!(false));
    assert_eq!(body["ai_indexed"], json!(false));
    assert_eq!(body["transcript_status"], json!("pending"));

    // External link on a file-backed item → 422 external_url_not_allowed.
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::PUT,
                &format!("/api/v1/admin/library/items/{item_id}"),
                &[(
                    "metadata",
                    meta_json(json!({
                        "external_url": "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
                    })),
                )],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "external_url_not_allowed");

    // Clear a nullable field explicitly (description → null is allowed;
    // here scholar_id is cleared the same way).
    let (status, body) = env
        .call(authed(
            multipart_req(
                Method::PUT,
                &format!("/api/v1/admin/library/items/{item_id}"),
                &[("metadata", meta_json(json!({"scholar_id": null})))],
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["scholar_id"], json!(null));
}

#[tokio::test]
async fn workflow_transitions_and_archive() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin8@example.com").await;
    let item_id = create_pdf_via_api(&env, &admin_token, "گردش کار").await;

    // draft → published is not an edge → 422 invalid_transition.
    let (status, body) = transition_via_api(&env, &admin_token, item_id, "published").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");
    // Unknown target → 422 unknown_status.
    let (status, body) = transition_via_api(&env, &admin_token, item_id, "bogus").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "unknown_status");

    for to in ["in_review", "approved", "published"] {
        let (status, body) = transition_via_api(&env, &admin_token, item_id, to).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["workflow_status"], json!(to));
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/library/items/{item_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["published_at"].is_string());

    // Published items can only go to archived.
    let (status, body) = transition_via_api(&env, &admin_token, item_id, "draft").await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // DELETE = archive; a second archive is an invalid transition.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/library/items/{item_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["workflow_status"], json!("archived"));
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/library/items/{item_id}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "invalid_transition");

    // Missing item → 404.
    let (status, body) = transition_via_api(&env, &admin_token, Uuid::new_v4(), "draft").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn reprocess_and_import() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin9@example.com").await;
    let item_id = create_pdf_via_api(&env, &admin_token, "پردازش مجدد").await;
    env.db
        .execute(
            "UPDATE library_items SET processing_error = 'boom' WHERE id = ?",
            &[item_id.into_arg()],
        )
        .await
        .unwrap();

    // transcribe job → pending, error cleared; the response is the item
    // (the honest `enqueued: false` lives in the audit log, not the body).
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/reprocess"),
                Some(json!({"job": "transcribe"})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["transcript_status"], json!("pending"));
    assert_eq!(body["processing_error"], json!(null));
    let row = env
        .db
        .fetch_optional(
            "SELECT new_values AS v FROM audit_logs WHERE action = 'library.item.reprocess'",
            &[],
        )
        .await
        .unwrap();
    let audit = row.expect("reprocess is audited");
    let raw: String = audit.try_get("v").unwrap();
    let new_values: Value = serde_json::from_str(raw.as_str()).unwrap();
    assert_eq!(new_values["enqueued"], json!(false));

    // ocr job → same shape for the ocr side.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/reprocess"),
                Some(json!({"job": "ocr"})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["ocr_status"], json!("pending"));

    // Bad job → 422 bad_job; missing item → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/reprocess"),
                Some(json!({"job": "summarize"})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_job");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{}/reprocess", Uuid::new_v4()),
                Some(json!({"job": "transcribe"})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    // Import: JSON body → 201, in_review, telegram_import, pending key.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/library/import",
                Some(json!({
                    "kind": "audio",
                    "title": title_json("واردات تلگرام"),
                    "source_ref": {"chat_id": 42, "message_id": 7},
                })),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["workflow_status"], json!("in_review"));
    assert_eq!(body["source_type"], json!("telegram_import"));
    // The pending:telegram media key has no signed URL yet.
    assert_eq!(body["media_url"], json!(null));
    // Missing source_ref → 422 validation_error.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/library/import",
                Some(json!({"kind": "audio", "title": title_json("بد")})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");
}

#[tokio::test]
async fn index_ai_gates_and_chunks() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin10@example.com").await;
    let (_, approver_token) = approver_user(&env, "approver@example.com").await;

    // Draft item → 422 item_not_indexable.
    let draft_id = create_pdf_via_api(&env, &admin_token, "پیش‌نویس ایندکس").await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{draft_id}/index-ai"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "item_not_indexable");

    // Approved item without a fatwa scope → 422 no_ai_scopes.
    let transcript = "متن آزمون ایندکس کتابخانه ماتریدی برای جستجوی معنایی و تقسیم بندی پاراگراف ها است. متن آزمون ایندکس کتابخانه ماتریدی برای جستجوی معنایی و تقسیم بندی پاراگراف ها است. ";
    let item_id = seed_item(
        &env,
        ItemSeed {
            title_fa: "جزوهٔ ایندکس",
            workflow_status: "approved",
            transcript: Some(transcript),
            ..Default::default()
        },
    )
    .await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/index-ai"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "no_ai_scopes");

    // Approved + scope fatwa: without fatwa.approve the chunks stay draft
    // and ai_indexed stays false; with it they go live and ai_indexed=true.
    env.db
        .execute(
            "UPDATE library_items SET ai_scopes = ? WHERE id = ?",
            &[json!(["fatwa"]).into_arg(), item_id.into_arg()],
        )
        .await
        .unwrap();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/index-ai"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["chunks_created"], json!(1));
    assert_eq!(body["ai_indexed"], json!(false));
    assert_eq!(body["chunks_approved"], json!(0));
    // Re-indexing does not delete the old chunks: pin the row by the
    // chunk id the response returns.
    let chunk_id = as_uuid(body["chunk_ids"][0].as_str().unwrap());
    let row = env
        .db
        .fetch_one(
            "SELECT workflow_status AS s FROM fiqh_knowledge_chunks WHERE id = ?",
            &[chunk_id.into_arg()],
        )
        .await
        .unwrap();
    let chunk_status: String = row.try_get("s").unwrap();
    assert_eq!(chunk_status, "draft");

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/library/items/{item_id}/index-ai"),
                None,
            ),
            &approver_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["chunks_created"], json!(1));
    assert_eq!(body["ai_indexed"], json!(true));
    assert_eq!(body["chunks_approved"], json!(1));
    let chunk_id = as_uuid(body["chunk_ids"][0].as_str().unwrap());
    let row = env
        .db
        .fetch_one(
            "SELECT workflow_status AS s FROM fiqh_knowledge_chunks WHERE id = ?",
            &[chunk_id.into_arg()],
        )
        .await
        .unwrap();
    let chunk_status: String = row.try_get("s").unwrap();
    assert_eq!(chunk_status, "approved");
    let row = env
        .db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM ingestion_runs WHERE source_type = 'library_item'",
            &[],
        )
        .await
        .unwrap();
    let runs: i64 = row.try_get("c").unwrap();
    assert_eq!(runs, 2);
}

// ============================ Postgres smoke ==============================

/// Critical path on Postgres: create (multipart PDF) → publish →
/// public list → signed download-by-ref. Gated on `WAVE2_PG_URL`.
async fn wave6_pg_smoke_library() {
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
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };
    let (_, admin_token) = admin_user(&env, "pg-admin@example.com").await;

    let item_id = create_pdf_via_api(&env, &admin_token, "دود دودی").await;
    for to in ["in_review", "approved", "published"] {
        let (status, body) = transition_via_api(&env, &admin_token, item_id, to).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/library/items", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["title"]["fa"], json!("دود دودی"));

    let media_url = body["items"][0]["media_url"].as_str().unwrap().to_string();
    let path = media_url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, _, bytes) = env.call_raw(req(Method::GET, &path, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, pdf_bytes());
}

// ============================ LMS =====================================

fn title(fa: &str) -> Value {
    json!({"fa": fa, "ar": fa, "en": fa})
}

async fn create_curriculum(env: &Env, token: &str, fa: &str) -> String {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/curricula",
                Some(json!({"title": title(fa), "sort_order": 0})),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create curriculum: {body}");
    body["id"].as_str().unwrap().to_string()
}

async fn create_course(
    env: &Env,
    token: &str,
    curriculum_id: Option<&str>,
    fa: &str,
    level: &str,
    visibility: &str,
) -> String {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/courses",
                Some(json!({
                    "curriculum_id": curriculum_id,
                    "title": title(fa),
                    "description": title("توضیح"),
                    "level": level,
                    "visibility": visibility,
                    "sort_order": 0,
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create course {fa}: {body}");
    assert_eq!(body["lesson_count"], json!(0));
    assert_eq!(body["progress_pct"], Value::Null);
    body["id"].as_str().unwrap().to_string()
}

async fn create_lesson(env: &Env, token: &str, course_id: &str, fa: &str, sort: i64) -> String {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/lessons",
                Some(json!({
                    "course_id": course_id,
                    "title": title(fa),
                    "body": [],
                    "duration_minutes": 5,
                    "sort_order": sort,
                    "is_free_preview": sort == 0,
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create lesson {fa}: {body}");
    body["id"].as_str().unwrap().to_string()
}

async fn create_course_with_lessons(
    env: &Env,
    token: &str,
    curriculum_id: Option<&str>,
    fa: &str,
    level: &str,
    n_lessons: usize,
) -> (String, Vec<String>) {
    let course = create_course(env, token, curriculum_id, fa, level, "public").await;
    let mut lessons = Vec::new();
    for i in 0..n_lessons {
        lessons.push(create_lesson(env, token, &course, &format!("درس {}", i + 1), i as i64).await);
    }
    (course, lessons)
}

async fn create_cohort(
    env: &Env,
    token: &str,
    course_id: &str,
    max_seats: i64,
    starts_in_secs: i64,
    ends_in_secs: i64,
) -> String {
    let now = Utc::now();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/cohorts",
                Some(json!({
                    "course_id": course_id,
                    "title": title("هم‌گروهی تست"),
                    "starts_at": (now + Duration::seconds(starts_in_secs)).to_rfc3339(),
                    "ends_at": (now + Duration::seconds(ends_in_secs)).to_rfc3339(),
                    "max_seats": max_seats,
                    "is_open": true,
                })),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "create cohort: {body}");
    assert_eq!(body["enrolled_count"], json!(0));
    assert_eq!(body["seats_left"], json!(max_seats));
    body["id"].as_str().unwrap().to_string()
}

async fn complete_lesson(env: &Env, token: &str, lesson_id: &str) -> Value {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/lessons/{lesson_id}/progress"),
                Some(json!({"status": "completed", "position_seconds": 0})),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "complete lesson: {body}");
    body
}

/// Seed one approved quiz question backed by an approved chunk (the
/// Phase-6 engine only draws from approved/published pairs).
async fn seed_quiz_question(env: &Env, correct_option: i64) -> Uuid {
    let now = Utc::now();
    let chunk_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO fiqh_knowledge_chunks (id, chunk_text, embedding, embedding_provider, \
             embedding_model, embedding_projection, source_type, source_ref, source_library_item_ref, \
             source_citation, topic_category, madhab_scope, workflow_status, approved_by, \
             ai_scopes, created_at, updated_at) \
             VALUES (?, ?, NULL, NULL, NULL, NULL, 'manual', NULL, NULL, ?, NULL, NULL, \
             'approved', NULL, '[]', ?, ?)",
            &[
                chunk_id.into_arg(),
                "متن دانش تست".into_arg(),
                json!({"ref": "test"}).into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    let qid = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO quiz_questions (id, question_text, options, correct_option, \
             source_chunk_id, difficulty, workflow_status, created_at) \
             VALUES (?, ?, ?, ?, ?, 'easy', 'approved', ?)",
            &[
                qid.into_arg(),
                json!({"fa": "سؤال تست؟"}).into_arg(),
                json!([{"fa": "الف"}, {"fa": "ب"}]).into_arg(),
                correct_option.into_arg(),
                chunk_id.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    qid
}

// ============================ admin: curricula ===========================

#[tokio::test]
async fn admin_curriculum_crud() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user_lms(&env, "admin1@example.com").await;
    let (_, plain_token, _) = register_user(&env, "plain1@example.com").await;

    // No permission → 403; no token → 401.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/curricula",
                Some(json!({"title": title("الف")})),
            ),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = env
        .call(req(
            Method::POST,
            "/api/v1/admin/lms/curricula",
            Some(json!({"title": title("الف")})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Bad title type → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/curricula",
                Some(json!({"title": "not-an-object"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "validation_error");

    let cur = create_curriculum(&env, &token, "برنامهٔ تست").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/curricula", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["title"]["fa"], json!("برنامهٔ تست"));

    // PATCH: only the provided field changes; explicit null is a no-op.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/lms/curricula/{cur}"),
                Some(json!({"title": title("برنامهٔ نو"), "description": null})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["fa"], json!("برنامهٔ نو"));
    assert_eq!(body["description"], Value::Null);

    // Unknown id → 404; malformed id → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/lms/curricula/{}", Uuid::new_v4()),
                Some(json!({"title": title("x")})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "curriculum_not_found");
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                "/api/v1/admin/lms/curricula/not-a-uuid",
                Some(json!({"title": title("x")})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Delete → 204; deleting again → 404.
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/curricula/{cur}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/curricula/{cur}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

// ============================ admin: courses =============================

#[tokio::test]
async fn admin_course_crud_and_validation() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user_lms(&env, "admin2@example.com").await;
    let cur = create_curriculum(&env, &token, "برنامه").await;

    // Unknown curriculum → 404 curriculum_not_found.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/courses",
                Some(json!({
                    "curriculum_id": Uuid::new_v4().to_string(),
                    "title": title("دوره"),
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "curriculum_not_found");

    // Bad level / visibility → 422.
    for (field, value) in [("level", "expert"), ("visibility", "secret")] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::POST,
                    "/api/v1/admin/lms/courses",
                    Some(json!({"title": title("دوره"), field: value})),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{field}: {body}");
    }

    let course = create_course(&env, &token, Some(&cur), "دورهٔ تست", "beginner", "public").await;
    // PATCH level + visibility.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/lms/courses/{course}"),
                Some(json!({"level": "advanced"})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["level"], json!("advanced"));
    // Admin list shows all courses (lesson_count hardcoded 0).
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/courses", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["lesson_count"], json!(0));

    // Deleting the curriculum orphans the course (curriculum_id NULL).
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/curricula/{cur}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/courses", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["curriculum_id"], Value::Null);
}

// ============================ public: courses ============================

#[tokio::test]
async fn public_course_list_detail_and_private() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin3@example.com").await;
    let (_, user_token, _) = register_user(&env, "user3@example.com").await;
    let cur = create_curriculum(&env, &admin_token, "برنامه").await;
    let (pub_course, pub_lessons) =
        create_course_with_lessons(&env, &admin_token, Some(&cur), "دورهٔ عمومی", "beginner", 2)
            .await;
    let priv_course = create_course(
        &env,
        &admin_token,
        Some(&cur),
        "دورهٔ خصوصی",
        "beginner",
        "private",
    )
    .await;
    let _ = create_lesson(&env, &admin_token, &priv_course, "درس خصوصی", 0).await;

    // Guest list: private course hidden.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/lms/courses", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], json!(pub_course));
    assert_eq!(body["items"][0]["lesson_count"], json!(2));
    assert_eq!(body["items"][0]["progress_pct"], Value::Null);
    assert_eq!(body["next_cursor"], Value::Null);

    // Level filter + bad level → 422.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/lms/courses?level=advanced", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/lms/courses?level=nope", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // Manager sees the private course too.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/courses", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);

    // Private detail → 404 for guest and plain user, 200 for manager.
    for tok in [None, Some(user_token.as_str())] {
        let r = req(
            Method::GET,
            &format!("/api/v1/lms/courses/{priv_course}"),
            None,
        );
        let (status, body) = match tok {
            Some(t) => env.call(authed(r, t)).await,
            None => env.call(r).await,
        };
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(err_code(&body), "course_not_found");
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/courses/{priv_course}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Public detail: lessons, zero progress, no certificate.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/courses/{pub_course}"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_lessons"], json!(2));
    assert_eq!(body["completed_lessons"], json!(0));
    assert_eq!(body["progress_pct"], json!(0.0));
    assert_eq!(body["has_certificate"], json!(false));
    assert_eq!(body["lessons"].as_array().unwrap().len(), 2);
    assert_eq!(body["lessons"][0]["status"], Value::Null);

    // Lesson detail with prev/next navigation.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/lessons/{}", pub_lessons[0]),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["prev_lesson_id"], Value::Null);
    assert_eq!(body["next_lesson_id"], json!(pub_lessons[1]));
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/lessons/{}", pub_lessons[1]),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["prev_lesson_id"], json!(pub_lessons[0]));
    assert_eq!(body["next_lesson_id"], Value::Null);

    // Unknown course/lesson → 404; malformed id → 422.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/lms/courses/{}", Uuid::new_v4()),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/lms/courses/nope", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn course_list_cursor_pagination() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user_lms(&env, "admin4@example.com").await;
    for i in 0..3 {
        create_course(
            &env,
            &token,
            None,
            &format!("دوره {i}"),
            "beginner",
            "public",
        )
        .await;
    }
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/lms/courses?limit=2", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let cursor = body["next_cursor"].as_str().expect("cursor").to_string();
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/lms/courses?limit=2&after={cursor}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["next_cursor"], Value::Null);
    // Bad cursor → 400 invalid_cursor; bad limit → 422.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/lms/courses?after=!!!", None))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_cursor");
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/lms/courses?limit=0", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ============================ progress + certificates ======================

#[tokio::test]
async fn progress_flow_issues_certificate() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin5@example.com").await;
    let (uid, user_token, _) = register_user(&env, "user5@example.com").await;
    let (course, lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ گواهی", "beginner", 2).await;

    // No certificate yet.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/mine?course_id={course}"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, Value::Null);

    // Bad status → 422; unknown lesson → 404; guest → 401.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/lessons/{}/progress", lessons[0]),
                Some(json!({"status": "done"})),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/lessons/{}/progress", Uuid::new_v4()),
                Some(json!({"status": "completed"})),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "lesson_not_found");
    let (status, _) = env
        .call(req(
            Method::POST,
            &format!("/api/v1/lms/lessons/{}/progress", lessons[0]),
            Some(json!({"status": "completed"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // In-progress first lesson: no completion yet, progress 50%.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/lessons/{}/progress", lessons[0]),
                Some(json!({"status": "in_progress", "position_seconds": 42})),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["course_completed"], json!(false));
    assert_eq!(body["certificate_code"], Value::Null);
    assert_eq!(body["completed_at"], Value::Null);
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/lms/courses/{course}"), None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["progress_pct"], json!(0.0));

    // Complete lesson 1, then lesson 2 → certificate issued.
    let body = complete_lesson(&env, &user_token, &lessons[0]).await;
    assert_eq!(body["course_completed"], json!(false));
    assert!(body["completed_at"].is_string());
    let body = complete_lesson(&env, &user_token, &lessons[1]).await;
    assert_eq!(body["course_completed"], json!(true));
    let code = body["certificate_code"]
        .as_str()
        .expect("certificate code")
        .to_string();
    assert!(code.starts_with("MA-"), "{code}");

    // Completing again is idempotent: same code, no second certificate.
    let body = complete_lesson(&env, &user_token, &lessons[1]).await;
    assert_eq!(body["course_completed"], json!(false));
    assert_eq!(body["certificate_code"], Value::Null);

    // Mine returns the certificate; detail shows has_certificate.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/mine?course_id={course}"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["code"], json!(code));
    assert_eq!(
        body["download_url"],
        json!(format!("/api/v1/lms/certificates/{code}/download"))
    );
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/lms/courses/{course}"), None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["progress_pct"], json!(100.0));
    assert_eq!(body["has_certificate"], json!(true));

    // Public verify (both routes) — valid, holder + course title.
    for path in [
        format!("/api/v1/certificates/{code}/verify"),
        format!("/api/v1/lms/certificates/{code}/verify"),
    ] {
        let (status, body) = env.call(req(Method::GET, &path, None)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["valid"], json!(true));
        assert_eq!(body["code"], json!(code));
        assert_eq!(body["holder_name"], json!("Test User"));
        assert_eq!(body["course_title"]["fa"], json!("دورهٔ گواهی"));
        assert_eq!(body["locale"], json!("fa"));
    }
    // Unknown code verifies as invalid (200, not 404); case-insensitive.
    let (status, body) = env
        .call(req(
            Method::GET,
            "/api/v1/certificates/MA-2099-XXXX/verify",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, json!({"valid": false, "code": "MA-2099-XXXX"}));
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/certificates/{}/verify", code.to_lowercase()),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], json!(true));

    // Download: PDF not rendered by the sidecar yet → 404 (never faked).
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/{code}/download"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "certificate_not_found");

    // Admin list shows the certificate.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/certificates", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["code"], json!(code));
    assert_eq!(body[0]["user_id"], json!(uid));
    assert_eq!(body[0]["user_display"], json!("Test User"));

    // Admin user-progress dashboard read.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/lms/users/{uid}/progress"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["courses"].as_array().unwrap().len(), 1);
    assert_eq!(body["courses"][0]["progress_pct"], json!(100.0));
    assert_eq!(body["courses"][0]["lessons"].as_array().unwrap().len(), 2);
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/lms/users/{}/progress", Uuid::new_v4()),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "user_not_found");
}

#[tokio::test]
async fn certificate_download_serves_stored_bytes() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin6@example.com").await;
    let (_uid, user_token, _) = register_user(&env, "user6@example.com").await;
    let (other_uid, other_token, _) = register_user(&env, "user6b@example.com").await;
    let _ = other_uid;
    let (course, lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ دانلود", "beginner", 1).await;
    complete_lesson(&env, &user_token, &lessons[0]).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/mine?course_id={course}"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let code = body["code"].as_str().unwrap().to_string();

    // Simulate the sidecar: write the PDF bytes under the storage dir
    // and point the certificate row at them.
    let key = format!("certificates/{code}.pdf");
    let dir = format!("{}/certificates", env.state.config.storage_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(format!("{dir}/{code}.pdf"), b"%PDF-1.4 fake-bytes-for-test").unwrap();
    env.db
        .execute(
            "UPDATE certificates SET pdf_storage_ref = ? WHERE code = ?",
            &[key.clone().into_arg(), code.clone().into_arg()],
        )
        .await
        .unwrap();

    // Owner download → 200 PDF + disposition.
    let (status, headers, bytes) = env
        .call_raw(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/{code}/download"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "application/pdf"
    );
    assert_eq!(
        headers
            .get("content-disposition")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("inline; filename=\"certificate-{code}.pdf\"")
    );
    assert_eq!(bytes, b"%PDF-1.4 fake-bytes-for-test");

    // Another user → 403; manager → 200.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/{code}/download"),
                None,
            ),
            &other_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
    let (status, _, _) = env
        .call_raw(authed(
            req(
                Method::GET,
                &format!("/api/v1/lms/certificates/{code}/download"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);

    // Signed download-by-ref with purpose lms-certificate → 200.
    let url = presigned_url(
        &env.state.config,
        &key,
        600,
        "/api/v1/lms/certificates/download-by-ref",
        "lms-certificate",
    )
    .expect("presign");
    let path = url
        .split_once("http://localhost:8000")
        .unwrap()
        .1
        .to_string();
    let (status, _, bytes) = env.call_raw(req(Method::GET, &path, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(bytes, b"%PDF-1.4 fake-bytes-for-test");

    // Wrong purpose → 403 invalid_signature; bad sig → 403.
    let bad_purpose = path.replace("purpose=lms-certificate", "purpose=library");
    let (status, body) = env.call(req(Method::GET, &bad_purpose, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "invalid_signature");
    let bad_sig = format!("{path}X");
    let (status, body) = env.call(req(Method::GET, &bad_sig, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    // A signed key that is NOT a certificate's pdf_storage_ref → 404.
    let other_key = "certificates/MA-2099-NOSUCH.pdf";
    let other_url = presigned_url(
        &env.state.config,
        other_key,
        600,
        "/api/v1/lms/certificates/download-by-ref",
        "lms-certificate",
    )
    .expect("presign");
    let other_path = other_url
        .split_once("http://localhost:8000")
        .unwrap()
        .1
        .to_string();
    let (status, body) = env.call(req(Method::GET, &other_path, None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "certificate_not_found");

    // Missing query params → 422.
    let (status, _) = env
        .call(req(
            Method::GET,
            "/api/v1/lms/certificates/download-by-ref",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
}

// ============================ lesson quiz ================================

#[tokio::test]
async fn lesson_quiz_generate_and_submit() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin7@example.com").await;
    let (_, user_token, _) = register_user(&env, "user7@example.com").await;
    let (course, lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ آزمون", "beginner", 1).await;
    let _ = course;
    seed_quiz_question(&env, 1).await;

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/quiz/{}", lessons[0]),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let attempt_id = body["attempt_id"].as_str().unwrap().to_string();
    assert_eq!(body["questions"].as_array().unwrap().len(), 1);
    let qid = body["questions"][0]["id"].as_str().unwrap().to_string();

    // Correct answer → 100; wrong answer on a fresh attempt → 0.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/lms/quiz/submit",
                Some(json!({"attempt_id": attempt_id, "answers": {qid: 1}})),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["score"], json!(100.0));
    assert_eq!(body["correct"], json!(1));

    // An attempt with no lesson (plain fatwa quiz) → 404 lms_attempt_not_found.
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/fatwa/quiz/generate", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let plain_attempt = body["attempt_id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/lms/quiz/submit",
                Some(json!({"attempt_id": plain_attempt, "answers": {}})),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "lms_attempt_not_found");

    // Unknown lesson → 404; unknown attempt → 404 lms_attempt_not_found.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/quiz/{}", Uuid::new_v4()),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "lesson_not_found");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/lms/quiz/submit",
                Some(json!({"attempt_id": Uuid::new_v4().to_string(), "answers": {}})),
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "lms_attempt_not_found");

    // No approved questions at all → 404 no_quiz_questions (fresh DB).
    let env2 = Env::sqlite().await;
    let (_, admin2) = admin_user_lms(&env2, "admin7b@example.com").await;
    let (_, user2, _) = register_user(&env2, "user7b@example.com").await;
    let (_, lessons2) =
        create_course_with_lessons(&env2, &admin2, None, "دورهٔ خالی", "beginner", 1).await;
    let (status, body) = env2
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/quiz/{}", lessons2[0]),
                None,
            ),
            &user2,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "no_quiz_questions");
}

// ============================ report + recommendations ===================

#[tokio::test]
async fn study_report_and_recommendations() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin8@example.com").await;
    let (_, user_token, _) = register_user(&env, "user8@example.com").await;
    let (course, lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ گزارش", "beginner", 3).await;
    let (adv_course, adv_lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ پیشرفته", "advanced", 1).await;
    let _ = adv_course;
    seed_quiz_question(&env, 0).await;

    // Empty report first.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/reports/me", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_lessons"], json!(0));
    assert_eq!(body["streak_days"], json!(0));

    complete_lesson(&env, &user_token, &lessons[0]).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/reports/me", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_completed_lessons"], json!(1));
    assert_eq!(body["total_lessons"], json!(3));
    assert_eq!(body["overall_progress_pct"], json!(33.3));
    assert_eq!(body["streak_days"], json!(1));
    assert_eq!(body["courses"][0]["course_id"], json!(course));
    assert_eq!(body["courses"][0]["progress_pct"], json!(33.3));

    // Recommendations: next unfinished lesson of the same level first.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/recommendations", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body[0]["lesson"]["id"], json!(lessons[1]));
    assert_eq!(body[0]["reason"], json!("level_next_incomplete"));
    assert_eq!(body[0]["course_title"]["fa"], json!("دورهٔ گزارش"));
    // The advanced lesson comes after the beginner ones (next_level).
    let reasons: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        vec![
            "level_next_incomplete",
            "level_next_incomplete",
            "next_level"
        ]
    );
    assert_eq!(body[2]["lesson"]["id"], json!(adv_lessons[0]));

    // Guest → 401 on both.
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/lms/reports/me", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/lms/recommendations", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

// ============================ cohorts + enrollments ======================

#[tokio::test]
async fn cohort_enroll_flow() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin9@example.com").await;
    let (_, user_a, _) = register_user(&env, "user9a@example.com").await;
    let (_, user_b, _) = register_user(&env, "user9b@example.com").await;
    let (course, _) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ گروهی", "beginner", 1).await;
    let cohort = create_cohort(&env, &admin_token, &course, 1, -3600, 86400).await;

    // Enroll A: seats_left drops to 0.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{cohort}/enroll"),
                None,
            ),
            &user_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("active"));
    assert_eq!(body["seats_left"], json!(0));

    // A again → 409 already_enrolled; B → 409 cohort_full.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{cohort}/enroll"),
                None,
            ),
            &user_a,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "already_enrolled");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{cohort}/enroll"),
                None,
            ),
            &user_b,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "cohort_full");

    // Public list shows enrolled_count + user_enrolled for A.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/cohorts", None),
            &user_a,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["enrolled_count"], json!(1));
    assert_eq!(body["items"][0]["seats_left"], json!(0));
    assert_eq!(body["items"][0]["user_enrolled"], json!(true));

    // Admin enrollments read shows A's display name.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/lms/cohorts/{cohort}/enrollments"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["user_display"], json!("Test User"));
    assert_eq!(body[0]["status"], json!("active"));

    // Unknown cohort → 404; guest enroll → 401.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{}/enroll", Uuid::new_v4()),
                None,
            ),
            &user_a,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "cohort_not_found");
    let (status, _) = env
        .call(req(
            Method::POST,
            &format!("/api/v1/lms/cohorts/{cohort}/enroll"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cohort_closed_ended_and_prerequisite() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin10@example.com").await;
    let (_, user_token, _) = register_user(&env, "user10@example.com").await;
    let (course_a, lessons_a) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ الف", "beginner", 1).await;
    let (course_b, _) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ ب", "intermediate", 1).await;
    let _ = lessons_a;

    // B requires A: enroll in B's cohort → 409 prerequisite_not_met.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/prerequisites",
                Some(json!({"course_id": course_b, "requires_course_id": course_a})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let cohort_b = create_cohort(&env, &admin_token, &course_b, 10, -3600, 86400).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{cohort_b}/enroll"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "prerequisite_not_met");
    assert_eq!(
        body["error"]["details"]["missing_course_ids"],
        json!([course_a])
    );

    // Complete A → certificate → enroll in B now works.
    complete_lesson(&env, &user_token, &lessons_a[0]).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{cohort_b}/enroll"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Closed cohort → 409 cohort_closed; ended cohort → 409 cohort_ended.
    let closed = create_cohort(&env, &admin_token, &course_a, 10, -3600, 86400).await;
    let (status, _) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/lms/cohorts/{closed}"),
                Some(json!({"is_open": false})),
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{closed}/enroll"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "cohort_closed");
    let ended = create_cohort(&env, &admin_token, &course_a, 10, -7200, -3600).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{ended}/enroll"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "cohort_ended");
}

#[tokio::test]
async fn cohort_admin_validation() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user_lms(&env, "admin11@example.com").await;
    let (course, _) =
        create_course_with_lessons(&env, &token, None, "دورهٔ پنجره", "beginner", 1).await;
    let now = Utc::now();
    // ends <= starts → 422 bad_cohort_window.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/cohorts",
                Some(json!({
                    "course_id": course,
                    "title": title("x"),
                    "starts_at": now.to_rfc3339(),
                    "ends_at": now.to_rfc3339(),
                    "max_seats": 5,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_cohort_window");
    // max_seats 0 → 422.
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/cohorts",
                Some(json!({
                    "course_id": course,
                    "title": title("x"),
                    "starts_at": now.to_rfc3339(),
                    "ends_at": (now + Duration::seconds(60)).to_rfc3339(),
                    "max_seats": 0,
                })),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    // PATCH to a bad window → 422, row unchanged.
    let cohort = create_cohort(&env, &token, &course, 5, -3600, 86400).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/lms/cohorts/{cohort}"),
                Some(json!({"ends_at": (now - Duration::seconds(7200)).to_rfc3339()})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "bad_cohort_window");
    // Admin list + delete.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/cohorts", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/cohorts/{cohort}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

// ============================ prerequisites ==============================

#[tokio::test]
async fn prerequisites_self_cycle_duplicate_delete() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user_lms(&env, "admin12@example.com").await;
    let (course_a, _) =
        create_course_with_lessons(&env, &token, None, "دورهٔ الف", "beginner", 1).await;
    let (course_b, _) =
        create_course_with_lessons(&env, &token, None, "دورهٔ ب", "beginner", 1).await;
    let (course_c, _) =
        create_course_with_lessons(&env, &token, None, "دورهٔ ج", "beginner", 1).await;

    // Self → 422 self_prerequisite.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/prerequisites",
                Some(json!({"course_id": course_a, "requires_course_id": course_a})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "self_prerequisite");
    // Unknown required course → 404 with Python's literal code.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/prerequisites",
                Some(json!({"course_id": course_a, "requires_course_id": Uuid::new_v4()})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "required course_not_found");

    // A ← B, B ← C, then C ← A would close a cycle → 422.
    for (c, r) in [(&course_a, &course_b), (&course_b, &course_c)] {
        let (status, body) = env
            .call(authed(
                req(
                    Method::POST,
                    "/api/v1/admin/lms/prerequisites",
                    Some(json!({"course_id": c, "requires_course_id": r})),
                ),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/prerequisites",
                Some(json!({"course_id": course_c, "requires_course_id": course_a})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "prerequisite_cycle");
    // Duplicate → 409 prerequisite_exists.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/prerequisites",
                Some(json!({"course_id": course_a, "requires_course_id": course_b})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "prerequisite_exists");

    // List (filtered + unfiltered), then delete → 204, again → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/lms/prerequisites?course_id={course_a}"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/prerequisites", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 2);
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!(
                    "/api/v1/admin/lms/prerequisites?course_id={course_a}&requires_course_id={course_b}"
                ),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!(
                    "/api/v1/admin/lms/prerequisites?course_id={course_a}&requires_course_id={course_b}"
                ),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "prerequisite_not_found");
}

// ============================ admin deletes ==============================

#[tokio::test]
async fn admin_deletes() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user_lms(&env, "admin13@example.com").await;
    let (_, user_token, _) = register_user(&env, "user13@example.com").await;
    let (course, lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ حذف", "beginner", 2).await;
    complete_lesson(&env, &user_token, &lessons[0]).await;
    complete_lesson(&env, &user_token, &lessons[1]).await;

    // Course with an issued certificate cannot be deleted → 409.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/courses/{course}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "course_has_certificates");

    // Lesson delete removes its progress rows (separate course: its
    // only lesson is completed first, which issues its certificate —
    // so this course is never deleted below).
    let (course3, lessons3) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ حذف ۳", "beginner", 1).await;
    complete_lesson(&env, &user_token, &lessons3[0]).await;
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/lessons/{}", lessons3[0]),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/lms/courses/{course3}"), None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["total_lessons"], json!(0));

    // Course without certificates deletes with its lessons/cohorts.
    let (course2, _) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ حذف ۲", "beginner", 1).await;
    let cohort = create_cohort(&env, &admin_token, &course2, 5, -3600, 86400).await;
    let (status, _) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/admin/lms/courses/{course2}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/courses", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // The certified course and the lesson-deleted course remain.
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/lms/cohorts", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
    let _ = cohort;
}

#[tokio::test]
async fn admin_lessons_list_and_lesson_update() {
    let env = Env::sqlite().await;
    let (_, token) = admin_user_lms(&env, "admin14@example.com").await;
    let (course, lessons) =
        create_course_with_lessons(&env, &token, None, "دورهٔ درس‌ها", "beginner", 2).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/lms/courses/{course}/lessons"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 2);
    assert_eq!(body[0]["status"], Value::Null);
    // Unknown course → 404 course_not_found.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/lms/courses/{}/lessons", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "course_not_found");
    // PATCH lesson title + duration.
    let (status, body) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/lms/lessons/{}", lessons[0]),
                Some(json!({"title": title("درس نو"), "duration_minutes": 30})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["fa"], json!("درس نو"));
    assert_eq!(body["duration_minutes"], json!(30));
    // Lesson create against an unknown course → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/lessons",
                Some(json!({"course_id": Uuid::new_v4().to_string(), "title": title("x")})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "course_not_found");
}

// ============================ Postgres smoke =============================

/// Run with WAVE2_PG_URL set; skipped otherwise (like the wave2/3/4/5 smokes).
/// Binaries must run sequentially — the smokes share one database.
async fn wave6_pg_smoke_lms() {
    let pg_url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("wave6_pg_smoke: WAVE2_PG_URL unset, skipping");
            return;
        }
    };
    let db = Db::connect(&pg_url).await.expect("pg connect");
    // Drop every table `TEST_SCHEMA` creates first, so leftovers
    // from another smoke run cannot fail the schema apply.
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
    init_test_schema(&db).await.expect("pg test schema");
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };

    // Critical path: create course → enroll → progress → certificate.
    let (_, admin_token) = admin_user_lms(&env, "pg-admin@example.com").await;
    let (_, user_token, _) = register_user(&env, "pg-user@example.com").await;
    let (course, lessons) =
        create_course_with_lessons(&env, &admin_token, None, "دورهٔ PG", "beginner", 2).await;
    let cohort = create_cohort(&env, &admin_token, &course, 10, -3600, 86400).await;
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/lms/cohorts/{cohort}/enroll"),
                None,
            ),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["seats_left"], json!(9));
    let body = complete_lesson(&env, &user_token, &lessons[0]).await;
    assert_eq!(body["course_completed"], json!(false));
    let body = complete_lesson(&env, &user_token, &lessons[1]).await;
    assert_eq!(body["course_completed"], json!(true));
    let code = body["certificate_code"].as_str().unwrap().to_string();
    assert!(code.starts_with("MA-"), "{code}");
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/certificates/{code}/verify"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["valid"], json!(true));
    assert_eq!(body["course_title"]["fa"], json!("دورهٔ PG"));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/lms/reports/me", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["overall_progress_pct"], json!(100.0));
    assert_eq!(body["streak_days"], json!(1));
}

// ============================ calculators =============================

fn synthetic_rule() -> Value {
    json!({
        "note": "SYNTHETIC TEST FIXTURE — not fiqh, engine math only",
        "shares": {
            "spouse_t": {"share": "1/4"},
            "mother_t": {"share": "1/6"},
        },
        "awl": {"enabled": true},
        "radd": {"enabled": true, "excluded": ["spouse_t"]},
        "asaba_priority": ["child_t"],
    })
}

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

fn zakat_source_rule_data() -> Value {
    json!({
        "format": "source_extended_v1",
        "money": {
            "nisab_coins": "10.4545454545",
            "zakat_at_nisab_coins": "0.2613636364",
            "step_coins": "2.0909090909",
            "step_zakat_coins": "0.0727272727",
        },
        "gold": {
            "nisab_grams": "92",
            "zakat_at_nisab_grams": "2.3",
            "step_grams": "18.4",
            "step_zakat_grams": "0.46",
        },
        "silver": {
            "nisab_grams": "644",
            "zakat_at_nisab_grams": "16.1",
            "step_grams": "128.8",
            "step_zakat_grams": "3.22",
            "input_round_grams": "0.1",
        },
        "sheep": {
            "nisab": 40,
            "table": [[120, 1], [200, 2], [399, 3]],
            "per_hundred": {"from": 400, "divisor": 100},
        },
        "camel": {
            "table": [
                [4, []], [9, [["sheep", null, 1]]], [14, [["sheep", null, 2]]],
                [19, [["sheep", null, 3]]], [24, [["sheep", null, 4]]],
                [35, [["camel_female", 1, 1]]], [45, [["camel_female", 2, 1]]],
                [60, [["camel_female", 3, 1]]], [75, [["camel_female", 4, 1]]],
                [90, [["camel_female", 2, 2]]], [124, [["camel_female", 3, 2]]],
                [149, [["camel_female", 3, 2], ["camel_female", 1, 1]]],
                [150, [["camel_female", 3, 3]]],
            ],
            "over": {
                "threshold": 150,
                "base_per": 50,
                "base_due": ["camel_female", 3, 1],
                "remainder_table": [
                    [4, []], [9, [["sheep", null, 1]]], [14, [["sheep", null, 2]]],
                    [19, [["sheep", null, 3]]], [24, [["sheep", null, 4]]],
                    [35, [["camel_female", 1, 1]]], [45, [["camel_female", 2, 1]]],
                    [49, [["camel_female", 3, 1]]],
                ],
            },
        },
        "cow": {
            "tabii_from": 30,
            "musinna_from": 40,
            "double_tabii_from": 60,
            "combo_from": 70,
            "combo_round_to": 10,
            "combo_units": [30, 40],
            "musinna_share_denominator": 40,
        },
        "horse": {"per_head_dinar": 1, "value_rate": "1/40"},
        "oshr": {"rates": {"rain_fed": "1/10", "irrigated": "1/20"}},
        "khoms": {"rate": "0.2"},
    })
}

fn miras_source_rule_data() -> Value {
    json!({
        "format": "miras_v1",
        "shares": {
            "husband": {"with_descendants": "1/4", "without_descendants": "1/2"},
            "wives": {"with_descendants": "1/8", "without_descendants": "1/4"},
            "mother": {"reduced": "1/6", "full": "1/3", "sibling_threshold": 2},
            "grandmothers": "1/6",
            "father_fixed": "1/6",
            "grandfather_fixed": "1/6",
            "uterine": {"single": "1/6", "multiple": "1/3"},
            "daughters": {"single": "1/2", "multiple": "2/3"},
            "son_daughters": {"single": "1/2", "multiple": "2/3", "completion": "1/6"},
            "full_sisters": {"single": "1/2", "multiple": "2/3"},
            "paternal_sisters": {"completion": "1/6", "group": "2/3"},
        },
        "umariyya": {"mother_fraction_of_remainder": "1/3"},
        "radd": {"excluded_keys": ["husband", "wives"]},
        "asaba_priority": [
            "sons", "sonSons", "father", "grandfather", "brothers",
            "sistersFemaleDesc", "pBrothers", "pSistersFemaleDesc",
            "fullBrotherSons", "paternalBrotherSons", "fullUncles",
            "paternalUncles", "fullUncleSons", "paternalUncleSons",
        ],
        "will": {"non_heir_cap": "1/3"},
    })
}

/// Create a rule via the admin API. Returns (status, body).
async fn create_rule(
    env: &Env,
    token: &str,
    kind: &str,
    madhab: Option<&str>,
    version: i64,
    rule_data: Value,
    asset_categories: Option<Value>,
) -> (StatusCode, Value) {
    let mut body = json!({
        "kind": kind,
        "source": format!("test-{kind}-v{version}"),
        "version": version,
        "effective_from": "2020-01-01T00:00:00Z",
        "rule_data": rule_data,
    });
    if let Some(m) = madhab {
        body["madhab"] = json!(m);
    }
    if let Some(cats) = asset_categories {
        body["asset_categories"] = cats;
    }
    env.call(authed(
        req(Method::POST, "/api/v1/admin/calculators/rules", Some(body)),
        token,
    ))
    .await
}

/// Create + submit + approve a rule; returns its id.
async fn approve_rule(
    env: &Env,
    token: &str,
    kind: &str,
    madhab: Option<&str>,
    version: i64,
    rule_data: Value,
    asset_categories: Option<Value>,
) -> String {
    let (status, body) = create_rule(
        env,
        token,
        kind,
        madhab,
        version,
        rule_data,
        asset_categories,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create {kind} v{version}: {body}"
    );
    let id = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{id}/submit?kind={kind}"),
                None,
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "submit {kind} v{version}: {body}");
    assert_eq!(body["status"], "pending_review");
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{id}/review?kind={kind}"),
                Some(json!({"decision": "approve", "note": "تأیید تست"})),
            ),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "review {kind} v{version}: {body}");
    assert_eq!(body["status"], "approved");
    id
}

/// Seed a fresh `nisab_price_cache` row (source "default") so zakat
/// calculations have gold/silver prices without a live fetch.
async fn seed_price_cache(env: &Env, age: Duration, gold: &str, silver: &str) {
    let fetched_at = Utc::now() - age;
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

fn inheritance_body(estate: i64, heirs: Value) -> Value {
    json!({"madhab": "hanafi", "estate_value": estate, "heirs": heirs})
}

fn share<'a>(result: &'a Value, heir_type: &str) -> &'a Value {
    result["shares"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["heir_type"] == heir_type)
        .unwrap_or_else(|| panic!("no share for {heir_type} in {result}"))
}

// ============================ inheritance ===========================

#[tokio::test]
async fn inheritance_gate_then_approved_calc() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    let body = inheritance_body(
        1200,
        json!([
            {"type": "spouse_t", "count": 1},
            {"type": "mother_t", "count": 1},
            {"type": "child_t", "count": 2},
        ]),
    );

    // No rule at all: the public gate answers 422 rules_not_approved.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "rules_not_approved");
    assert_eq!(out["error"]["details"]["kind"], "inheritance");

    // Draft rule is not enough either.
    let (status, created) = create_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    let rule_id = created["id"].as_str().unwrap().to_string();
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "rules_not_approved");

    // Submit + approve, then the same request succeeds with the
    // Python phase-7 values (1200 -> 300/200/700, total 1/1).
    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/submit?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/review?kind=inheritance"),
                Some(json!({"decision": "approve", "note": "تأیید تست"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "approved");

    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["estate_value"], "1200.0000");
    assert_eq!(out["result"]["total_allocated_fraction"], "1/1");
    assert_eq!(share(&out["result"], "spouse_t")["amount"], "300.0000");
    assert_eq!(share(&out["result"], "spouse_t")["share_fraction"], "1/4");
    assert_eq!(share(&out["result"], "mother_t")["amount"], "200.0000");
    assert_eq!(share(&out["result"], "child_t")["amount"], "700.0000");
    assert_eq!(
        share(&out["result"], "child_t")["per_person_amount"],
        "350.0000"
    );
    assert_eq!(out["result"]["applied_rule_version"], 1);
}

#[tokio::test]
async fn inheritance_awl_and_radd() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;

    // Radd case from test_calculators_phase7.py: mother alone
    // (600 -> 600.0000, total 1/1).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(inheritance_body(
                600,
                json!([{"type": "mother_t", "count": 1}]),
            )),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(share(&out["result"], "mother_t")["amount"], "600.0000");
    assert_eq!(out["result"]["total_allocated_fraction"], "1/1");
    assert!(out["result"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n == "radd_applied"));

    // Awl case from test_calculators_phase7.py uses its own rule
    // (shares 1/2 + 1/3 + 1/4 = 13/12 > 1): approve it as v2, since
    // the gate always picks the highest approved version.
    approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        2,
        json!({
            "shares": {
                "a_t": {"share": "1/2"},
                "b_t": {"share": "1/3"},
                "c_t": {"share": "1/4"},
            },
            "awl": {"enabled": true},
        }),
        None,
    )
    .await;
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(inheritance_body(
                1300,
                json!([
                    {"type": "a_t", "count": 1},
                    {"type": "b_t", "count": 1},
                    {"type": "c_t", "count": 1},
                ]),
            )),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(share(&out["result"], "a_t")["amount"], "600.0000");
    assert_eq!(share(&out["result"], "b_t")["amount"], "400.0000");
    assert_eq!(share(&out["result"], "c_t")["amount"], "300.0000");
    assert!(out["result"]["notes"]
        .as_array()
        .unwrap()
        .iter()
        .any(|n| n == "awl_applied"));
}

#[tokio::test]
async fn inheritance_idempotency_and_cases_list() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    let (uid, token, _) = register_user(&env, "user@example.com").await;
    approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;
    let body = inheritance_body(
        1200,
        json!([
            {"type": "spouse_t", "count": 1},
            {"type": "mother_t", "count": 1},
            {"type": "child_t", "count": 2},
        ]),
    );

    // Same idempotency key twice: the second call replays the first row.
    let (status, first) = env
        .call(authed(
            req_idem(
                Method::POST,
                "/api/v1/calculators/inheritance",
                Some(body.clone()),
                "key-1",
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = env
        .call(authed(
            req_idem(
                Method::POST,
                "/api/v1/calculators/inheritance",
                Some(body.clone()),
                "key-1",
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(first["id"], second["id"]);

    // A different key creates a second case; the list shows both,
    // newest first, with cursor pagination.
    let (status, _) = env
        .call(authed(
            req_idem(
                Method::POST,
                "/api/v1/calculators/inheritance",
                Some(inheritance_body(
                    600,
                    json!([{"type": "mother_t", "count": 1}]),
                )),
                "key-2",
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, list) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/calculators/inheritance/cases?limit=1",
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    // The cursor is present (non-null). Its content is NOT asserted
    // page-by-page: Python filters `id < UUID(cursor)` while sorting by
    // `created_at DESC`, so page 2 is UUID-ordered, not time-ordered —
    // ported faithfully (see the ADR draft in the final report). The
    // full list is deterministic: both cases, newest first.
    let _cursor = list["next_cursor"].as_str().unwrap().to_string();
    let (status, full) = env
        .call(authed(
            req(Method::GET, "/api/v1/calculators/inheritance/cases", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{full}");
    assert_eq!(full["items"].as_array().unwrap().len(), 2);
    assert_eq!(full["next_cursor"], Value::Null);

    // The cases list is per-user: another user sees nothing.
    let (_, other_token, _) = register_user(&env, "other@example.com").await;
    let (status, list) = env
        .call(authed(
            req(Method::GET, "/api/v1/calculators/inheritance/cases", None),
            &other_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 0);
    // Guest: 401.
    let (status, _) = env
        .call(req(
            Method::GET,
            "/api/v1/calculators/inheritance/cases",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let _ = uid;
}

#[tokio::test]
async fn inheritance_validation_errors() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;

    // Unknown madhab: bad_madhab (raised before any rule lookup).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(
                json!({"madhab": "hanbali2", "estate_value": 100, "heirs": [{"type": "spouse_t"}]}),
            ),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "bad_madhab");

    // Non-positive estate.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(json!({"madhab": "hanafi", "estate_value": 0, "heirs": [{"type": "spouse_t"}]})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "validation_error");

    // Empty heirs.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(json!({"madhab": "hanafi", "estate_value": 100, "heirs": []})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "validation_error");

    // Unknown heir type (not in the approved rule_data).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(json!({"madhab": "hanafi", "estate_value": 100, "heirs": [{"type": "nope_t"}]})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "unknown_heir");

    // The /inheritance/cases alias takes the same handler.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/cases",
            Some(inheritance_body(
                600,
                json!([{"type": "mother_t", "count": 1}]),
            )),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(share(&out["result"], "mother_t")["amount"], "600.0000");
}

// ============================ zakat ===========================

fn zakat_body(assets: Value, debts: i64) -> Value {
    json!({"assets": assets, "debts": debts})
}

#[tokio::test]
async fn zakat_gate_calc_and_below_nisab() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    let body = zakat_body(json!({"cash": 500, "gold_grams": 60}), 100);

    // No rule: 422 rules_not_approved.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "rules_not_approved");

    approve_rule(
        &env,
        &admin,
        "zakat",
        None,
        1,
        synthetic_zakat_rule(),
        Some(json!({
            "cash": {"type": "value"},
            "trade_goods": {"type": "value"},
            "gold_grams": {"type": "grams", "price": "gold_per_gram"},
        })),
    )
    .await;

    // No price cache and no source URL: the engine reports
    // price_unavailable (gold_grams cannot be priced).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["result"]["status"], "price_unavailable");
    assert_eq!(out["result"]["notes"][0], "missing_price:gold_per_gram");

    // Fresh cache (gold 10, silver 1, TST): Python phase-7 values —
    // total 1100.0000, net 1000.0000, nisab 1000.0000, due 100.0000.
    seed_price_cache(&env, Duration::hours(1), "10", "1").await;
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(body.clone()),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let r = &out["result"];
    assert_eq!(r["status"], "ok");
    assert_eq!(r["total_assets"], "1100.0000");
    assert_eq!(r["debts"], "100.0000");
    assert_eq!(r["net_assets"], "1000.0000");
    assert_eq!(r["nisab_basis"], "gold");
    assert_eq!(r["nisab_grams"], "100");
    assert_eq!(r["nisab_value"], "1000.0000");
    assert_eq!(r["nisab_met"], true);
    assert_eq!(r["zakat_rate"], "0.1");
    assert_eq!(r["zakat_due"], "100.0000");
    assert_eq!(r["currency"], "TST");
    assert_eq!(r["price_stale"], false);
    assert_eq!(out["nisab_snapshot"]["status"], "ok");
    assert_eq!(out["nisab_snapshot"]["source"], "cache");
    assert_eq!(out["nisab_snapshot"]["gold_per_gram"], "10.000000");

    // Below nisab: cash 100 only -> nisab_met false, due 0.0000.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(zakat_body(json!({"cash": 100}), 0)),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["result"]["nisab_met"], false);
    assert_eq!(out["result"]["zakat_due"], "0.0000");
    assert_eq!(out["result"]["total_assets"], "100.0000");

    // Unknown asset category: 422 unknown_asset_category.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(zakat_body(json!({"nope_cat": 5}), 0)),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "unknown_asset_category");

    // Idempotency: same key replays the first row.
    let (status, first) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(zakat_body(json!({"cash": 100}), 0)),
            "z-key-1",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    let (status, second) = env
        .call(req_idem(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(zakat_body(json!({"cash": 100}), 0)),
            "z-key-1",
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(first["id"], second["id"]);
}

#[tokio::test]
async fn nisab_rate_states() {
    let env = Env::sqlite().await;

    // Nothing cached, no source URL configured: price_unavailable.
    let (status, out) = env
        .call(req(
            Method::GET,
            "/api/v1/calculators/zakat/nisab-rate",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "price_unavailable");
    assert_eq!(out["gold_per_gram"], Value::Null);
    assert_eq!(out["silver_per_gram"], Value::Null);
    assert_eq!(out["source"], "unconfigured");
    assert_eq!(out["stale"], false);

    // Fresh cache: ok, source cache, 6-decimal prices.
    seed_price_cache(&env, Duration::hours(1), "10", "1").await;
    let (status, out) = env
        .call(req(
            Method::GET,
            "/api/v1/calculators/zakat/nisab-rate",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "ok");
    assert_eq!(out["gold_per_gram"], "10.000000");
    assert_eq!(out["silver_per_gram"], "1.000000");
    assert_eq!(out["currency"], "TST");
    assert_eq!(out["source"], "cache");
    assert_eq!(out["stale"], false);
    assert!(out["fetched_at"].is_string());

    // Stale cache (older than the 6h TTL): stale, still served.
    env.db
        .execute(
            "UPDATE nisab_price_cache SET fetched_at = ? WHERE source = 'default'",
            &[(Utc::now() - Duration::days(2)).into_arg()],
        )
        .await
        .unwrap();
    let (status, out) = env
        .call(req(
            Method::GET,
            "/api/v1/calculators/zakat/nisab-rate",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "stale");
    assert_eq!(out["stale"], true);
    assert_eq!(out["source"], "cache");
    assert_eq!(out["gold_per_gram"], "10.000000");
}

// ============================ extended zakat ===========================

#[tokio::test]
async fn zakat_extended_golden() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;

    // Gate first: no rule -> 422.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"money": {"amount": 200000000, "coin_price": 10000000}})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "rules_not_approved");

    approve_rule(
        &env,
        &admin,
        "zakat",
        None,
        95,
        json!({"extended": zakat_source_rule_data()}),
        None,
    )
    .await;

    // Money (zakat golden fixture): 200M @ 10M/coin.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"money": {"amount": 200000000, "coin_price": 10000000}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["applied_rule_version"], 95);
    assert_eq!(out["sections"]["money"]["zakat_coins"], "0.5522727272");
    assert_eq!(out["sections"]["money"]["zakat_amount"], "5522727");
    assert_eq!(out["sections"]["money"]["nisab_met"], true);

    // Gold 100g: nisab met, 2.30g zakat.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"gold_grams": 100})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["sections"]["gold"]["nisab_met"], true);
    assert_eq!(out["sections"]["gold"]["zakat_grams"], "2.30");

    // Silver 643.94 -> not met; 643.95 -> met, 16.10g.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"silver_grams": 643.94})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["sections"]["silver"]["nisab_met"], false);
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"silver_grams": 643.95})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["sections"]["silver"]["nisab_met"], true);
    assert_eq!(out["sections"]["silver"]["zakat_grams"], "16.10");

    // Sheep 45 -> 1 sheep; camels 30 -> 1 female age 1;
    // camels 200 -> 4 females age 3.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"sheep": 45})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["sections"]["sheep"]["due"],
        json!([{"kind": "sheep", "count": 1}])
    );
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"camels": 30})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["sections"]["camels"]["due"],
        json!([{"kind": "camel_female", "age_years": 1, "count": 1}])
    );
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"camels": 200})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["sections"]["camels"]["due"],
        json!([{"kind": "camel_female", "age_years": 3, "count": 4}])
    );

    // Cows 45 @ 40M musinna: 1 musinna + 5 fortieths = 5_000_000.
    // Cows 70 -> 1 tabii + 1 musinna; cows 100 -> 2 tabii + 1 musinna.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"cows": {"count": 45, "musinna_price": 40000000}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["sections"]["cows"]["due"],
        json!([{"kind": "cow_musinna", "count": 1}])
    );
    assert_eq!(out["sections"]["cows"]["musinna_fortieths"], 5);
    assert_eq!(out["sections"]["cows"]["extra_money"], "5000000");
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"cows": {"count": 70}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["sections"]["cows"]["due"],
        json!([{"kind": "cow_tabii", "count": 1}, {"kind": "cow_musinna", "count": 1}])
    );
    assert_eq!(out["sections"]["cows"]["musinna_fortieths"], 0);
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"cows": {"count": 100}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(
        out["sections"]["cows"]["due"],
        json!([{"kind": "cow_tabii", "count": 2}, {"kind": "cow_musinna", "count": 1}])
    );
    assert_eq!(out["sections"]["cows"]["musinna_fortieths"], 0);

    // Oshr 1000kg rain-fed -> 100kg; khoms 1000 -> 200.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"oshr": {"amount_kg": 1000, "irrigation": "rain_fed"}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["sections"]["oshr"]["oshr_kg"], "100");
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({"khoms": {"amount": 1000}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["sections"]["khoms"]["khoms"], "200");

    // No sections at all: 422 no_sections.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat/extended",
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "no_sections");
}

// ============================ miras ===========================

fn miras_row<'a>(out: &'a Value, key: &str) -> &'a Value {
    out["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["key"] == key)
        .unwrap_or_else(|| panic!("no miras row for {key} in {out}"))
}

#[tokio::test]
async fn miras_golden_cases() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;

    // Gate first: no rule -> 422.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "male", "wives": 1, "sons": 1, "daughters": 1, "estate_value": 90000})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "rules_not_approved");

    approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        95,
        json!({"miras": miras_source_rule_data()}),
        None,
    )
    .await;

    // Miras API test from test_calculators_source_golden.py:
    // male, 1 wife, 1 son, 1 daughter, estate 90000.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "male", "wives": 1, "sons": 1, "daughters": 1, "estate_value": 90000})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "computed");
    assert_eq!(out["applied_rule_version"], 95);
    assert_eq!(miras_row(&out, "wives")["share"], "1/8");
    assert_eq!(miras_row(&out, "sons")["share"], "7/12");
    assert_eq!(miras_row(&out, "daughtersAsabah")["share"], "7/24");

    // Golden: wife/mother/father/2 sons/1 daughter, estate 120000.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "male", "wives": 1, "mother": true, "father": true, "sons": 2, "daughters": 1, "estate_value": 120000})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(miras_row(&out, "wives")["share"], "1/8");
    assert_eq!(miras_row(&out, "mother")["share"], "1/6");
    assert_eq!(miras_row(&out, "father")["share"], "1/6");
    assert_eq!(miras_row(&out, "sons")["share"], "13/30");
    assert_eq!(miras_row(&out, "daughtersAsabah")["share"], "13/120");

    // Umariyya golden: female, husband+father+mother, 60000.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "female", "husband": true, "father": true, "mother": true, "estate_value": 60000})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(miras_row(&out, "husband")["share"], "1/2");
    assert_eq!(miras_row(&out, "mother")["share"], "1/6");
    assert_eq!(miras_row(&out, "father")["share"], "1/3");

    // Awl golden: female, husband+mother+2 sisters+1 uterine brother
    // +1 uterine sister, 100000.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "female", "husband": true, "mother": true, "sisters": 2, "uterine_brothers": 1, "uterine_sisters": 1, "estate_value": 100000})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["awl"], true);
    assert_eq!(miras_row(&out, "husband")["share"], "3/10");
    assert_eq!(miras_row(&out, "mother")["share"], "1/10");
    assert_eq!(miras_row(&out, "uterine")["share"], "1/5");
    assert_eq!(miras_row(&out, "fullSisters")["share"], "2/5");

    // Daughter-only radd: male, 1 daughter, 50000.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "male", "daughters": 1, "estate_value": 50000})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["radd"], true);
    assert_eq!(miras_row(&out, "daughters")["share"], "1/1");

    // Impediment: mixed religion -> stopped, no rows.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "male", "wives": 1, "sons": 1, "estate_value": 90000, "impediments": {"religion_mixed": true}})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "stopped");
    assert_eq!(out["stop_code"], "impediment_religion");
    assert_eq!(out["applied_rule_version"], 95);

    // Miras is stateless: no inheritance_cases row is written.
    let row = env
        .db
        .fetch_optional("SELECT id FROM inheritance_cases", &[])
        .await
        .unwrap();
    assert!(row.is_none());

    // Invalid gender: 422 validation_error — Python validates the
    // gender literal in Pydantic before the engine ever runs, so the
    // engine's own `invalid_gender` code is unreachable via HTTP.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "other", "estate_value": 100})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "validation_error");
}

// ============================ mosaref ===========================

fn table<'a>(out: &'a Value, key: &str, col: &str) -> &'a Value {
    let t = out["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["key"] == key)
        .unwrap_or_else(|| panic!("no table {key} in {out}"));
    &t["columns"][col]
}

#[tokio::test]
async fn mosaref_golden_and_errors() {
    let env = Env::sqlite().await;

    // Golden: كتب / faala / mutaadi (mosaref_golden.json).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/mosaref",
            Some(json!({"root": "كتب", "bab": "faala", "transitivity": "mutaadi"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["root"], "كتب");
    assert_eq!(out["bab"], "faala");
    assert_eq!(out["verb_class"], "sound");
    assert_eq!(table(&out, "past", "active")[0], "كَتَبَ");
    assert_eq!(table(&out, "present", "active")[0], "يَكْتُبُ");
    assert_eq!(out["tables"].as_array().unwrap().len(), 10);
    let ism = out["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["key"] == "ism_fail_maful")
        .unwrap();
    assert_eq!(ism["columns"]["ism_fail"][0], "كَاتِب");
    assert_eq!(ism["columns"]["ism_maful"][0], "مَكْتُوب");
    let zaman = out["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["key"] == "zaman_makan")
        .unwrap();
    assert_eq!(zaman["columns"]["form"][0], "مَكْتَب");
    let tafdil = out["tables"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["key"] == "tafdil")
        .unwrap();
    assert_eq!(tafdil["columns"]["form"][0], "أَكْتَب");

    // Lazim verb gets the harf-jar table (خرج / faala / laazim /
    // ila — fixture sound_lazim_ila).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/mosaref",
            Some(
                json!({"root": "خرج", "bab": "faala", "transitivity": "laazim", "harf_jar": "ila"}),
            ),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["verb_class"], "sound");
    assert_eq!(
        table(&out, "ism_maful_harf_jar", "ism_maful")[0],
        "مَخْرُوج إِلَيْهِ"
    );

    // Rubaai root is valid (4 letters).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/mosaref",
            Some(json!({"root": "زلزل", "bab": "rubaai_mujarrad", "transitivity": "mutaadi"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(table(&out, "past", "active")[0], "زَلْزَلَ");

    // Engine error codes (422, from the golden error cases).
    for (root, bab, code) in [
        ("اب", "faala", "root_too_short"),
        ("دعا", "faala", "alif_in_root"),
        ("أوي", "faala", "mahmuz_weak_mix"),
        ("كتب", "if'alla", "bab_unsupported"),
        ("مدد", "afala", "mudgham_augmented_unsupported"),
    ] {
        let (status, out) = env
            .call(req(
                Method::POST,
                "/api/v1/calculators/mosaref",
                Some(json!({"root": root, "bab": bab, "transitivity": "mutaadi"})),
            ))
            .await;
        assert_eq!(
            status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{root}/{bab}: {out}"
        );
        assert_eq!(err_code(&out), code, "{root}/{bab}: {out}");
    }

    // Pydantic-level validation: bad transitivity / harf_jar.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/mosaref",
            Some(json!({"root": "كتب", "bab": "faala", "transitivity": "weird"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "validation_error");
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/mosaref",
            Some(json!({"root": "كتب", "bab": "faala", "transitivity": "mutaadi", "harf_jar": "weird"})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "validation_error");
}

// ============================ rule admin lifecycle ===========================

#[tokio::test]
async fn rule_lifecycle_audit_and_permissions() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    let (_, plain_token, _) = register_user(&env, "plain@example.com").await;

    // Non-admin cannot list rules (403) and cannot create (403).
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/calculators/rules?kind=inheritance",
                None,
            ),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{out}");
    let (status, _) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/calculators/rules",
                Some(json!({"kind": "inheritance", "madhab": "hanafi", "source": "x", "version": 1, "effective_from": "2020-01-01T00:00:00Z", "rule_data": {}})),
            ),
            &plain_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // kind is OPTIONAL on the list endpoint (Python lists both
    // kinds merged when it is omitted); only an unknown kind is 422.
    let (status, out) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/calculators/rules", None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["items"].as_array().unwrap().len(), 0);
    // Unknown kind: bad_rule_kind.
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/calculators/rules?kind=nope",
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "bad_rule_kind");

    // Create v1 (draft, 201) -> update change_reason -> submit ->
    // review reject -> back to draft on update -> submit -> approve.
    let (status, created) = create_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{created}");
    assert_eq!(created["status"], "draft");
    assert_eq!(created["kind"], "inheritance");
    assert_eq!(created["madhab"], "hanafi");
    assert_eq!(created["version"], 1);
    assert_eq!(created["asset_categories"], json!({}));
    let rule_id = created["id"].as_str().unwrap().to_string();

    let (status, out) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/calculators/rules/{rule_id}?kind=inheritance"),
                Some(json!({"change_reason": "بازبینی اولیه"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["change_reason"], "بازبینی اولیه");

    // Duplicate version while v1 exists: 409 rule_version_exists.
    let (status, out) = create_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{out}");
    assert_eq!(err_code(&out), "rule_version_exists");

    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/submit?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "pending_review");

    // Submit again: 422 bad_rule_transition.
    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/submit?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "bad_rule_transition");

    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/review?kind=inheritance"),
                Some(json!({"decision": "reject", "note": "نیاز به بازبینی"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "rejected");

    // Updating a rejected rule returns it to draft.
    let (status, out) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/calculators/rules/{rule_id}?kind=inheritance"),
                Some(json!({"change_reason": "اصلاح شد"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "draft");

    // Approve via the helper path, then the rule is locked.
    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/submit?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let (status, out) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/review?kind=inheritance"),
                Some(json!({"decision": "approve", "note": "تأیید"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["status"], "approved");
    assert!(out["approved_by"].is_string());
    assert!(out["approved_at"].is_string());

    let (status, out) = env
        .call(authed(
            req(
                Method::PATCH,
                &format!("/api/v1/admin/calculators/rules/{rule_id}?kind=inheritance"),
                Some(json!({"change_reason": "تلاش برای تغییر"})),
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{out}");
    assert_eq!(err_code(&out), "rule_locked");

    // Get by id, list, history.
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/calculators/rules/{rule_id}?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["id"], rule_id);
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/calculators/rules?kind=inheritance",
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["items"].as_array().unwrap().len(), 1);
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/calculators/rules/{rule_id}/history?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["items"].as_array().unwrap().len(), 1);
    assert_eq!(out["next_cursor"], Value::Null);

    // Wrong kind for this id: 404 rule_not_found.
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/calculators/rules/{rule_id}?kind=zakat"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{out}");
    assert_eq!(err_code(&out), "rule_not_found");

    // Audit log contains the lifecycle actions.
    let (status, out) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/calculators/audit", None),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let actions: Vec<&str> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["action"].as_str().unwrap())
        .collect();
    for want in [
        "calculator.rule_created",
        "calculator.rule_updated",
        "calculator.rule_submitted",
        "calculator.rule_rejected",
        "calculator.rule_approved",
    ] {
        assert!(actions.contains(&want), "missing {want} in {actions:?}");
    }
    assert!(out["items"]
        .as_array()
        .unwrap()
        .iter()
        .all(|i| i["entity_type"] == "inheritance_rule_config"));
}

#[tokio::test]
async fn rule_history_versioning_picks_newest_approved() {
    let env = Env::sqlite().await;
    let (_, admin) = admin_user_calc(&env, "admin@example.com").await;
    let v1 = approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        1,
        synthetic_rule(),
        None,
    )
    .await;
    let v2 = approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        2,
        synthetic_rule(),
        None,
    )
    .await;

    // History for v2 lists both versions, newest first.
    let (status, out) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/calculators/rules/{v2}/history?kind=inheritance"),
                None,
            ),
            &admin,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    let versions: Vec<i64> = out["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["version"].as_i64().unwrap())
        .collect();
    assert_eq!(versions, vec![2, 1]);
    let _ = v1;

    // The public gate uses the highest approved version.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance",
            Some(inheritance_body(
                600,
                json!([{"type": "mother_t", "count": 1}]),
            )),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["result"]["applied_rule_version"], 2);
}

// ============================ Postgres smoke ===========================

/// Critical-path replay against a local Postgres test DB.
/// Run with WAVE2_PG_URL set; skipped otherwise (like the wave2/3/4/5 smokes).
/// Binaries must run sequentially — the smokes share one database.
async fn wave6_pg_smoke_calc() {
    let pg_url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("wave6_pg_smoke: WAVE2_PG_URL unset, skipping");
            return;
        }
    };
    let db = Db::connect(&pg_url).await.expect("pg connect");
    // Drop every table TEST_SCHEMA creates first: the DDL is
    // plain `CREATE TABLE`, so a leftover table from another smoke run
    // would fail the schema apply. The table list is derived from the
    // schemas themselves, so it cannot go stale.
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

    let (_, admin) = admin_user_calc(&env, "pg-admin@example.com").await;
    approve_rule(
        &env,
        &admin,
        "zakat",
        None,
        1,
        synthetic_zakat_rule(),
        Some(json!({
            "cash": {"type": "value"},
            "trade_goods": {"type": "value"},
            "gold_grams": {"type": "grams", "price": "gold_per_gram"},
        })),
    )
    .await;
    approve_rule(
        &env,
        &admin,
        "inheritance",
        Some("hanafi"),
        95,
        json!({"miras": miras_source_rule_data()}),
        None,
    )
    .await;
    seed_price_cache(&env, Duration::hours(1), "10", "1").await;

    // Zakat calc on Postgres (NUMERIC columns round-trip exactly).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/zakat",
            Some(zakat_body(json!({"cash": 500, "gold_grams": 60}), 100)),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(out["result"]["total_assets"], "1100.0000");
    assert_eq!(out["result"]["zakat_due"], "100.0000");
    assert_eq!(out["nisab_snapshot"]["gold_per_gram"], "10.000000");

    // Miras calc on Postgres.
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/inheritance/miras",
            Some(json!({"gender": "male", "wives": 1, "sons": 1, "daughters": 1, "estate_value": 90000})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(miras_row(&out, "wives")["share"], "1/8");
    assert_eq!(miras_row(&out, "sons")["share"], "7/12");
    assert_eq!(miras_row(&out, "daughtersAsabah")["share"], "7/24");

    // Mosaref calc on Postgres (stateless, but the route must serve).
    let (status, out) = env
        .call(req(
            Method::POST,
            "/api/v1/calculators/mosaref",
            Some(json!({"root": "كتب", "bab": "faala", "transitivity": "mutaadi"})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{out}");
    assert_eq!(table(&out, "past", "active")[0], "كَتَبَ");
    assert_eq!(table(&out, "present", "active")[0], "يَكْتُبُ");
}

// ============================ combined Postgres smoke =================

/// Combined Wave 6 Postgres smoke: library + LMS + calculators critical
/// paths, sequentially, against one shared database. Gated on
/// `WAVE2_PG_URL`; each inner smoke re-initialises the schema, so they
/// can run back-to-back in one test.
#[tokio::test]
async fn wave6_pg_smoke() {
    if std::env::var("WAVE2_PG_URL")
        .ok()
        .filter(|u| !u.is_empty())
        .is_none()
    {
        eprintln!("wave6_pg_smoke: WAVE2_PG_URL unset, skipping");
        return;
    }
    wave6_pg_smoke_library().await;
    wave6_pg_smoke_lms().await;
    wave6_pg_smoke_calc().await;
}
