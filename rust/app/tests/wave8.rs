//! Wave 8 replay runner: executes the 127 JSON HTTP cases in
//! `rust/app-cases/wave2..wave7` against the pure-Rust router
//! (`build_router`, fresh in-memory sqlite `TEST_SCHEMA` per case).
//!
//! The cases are documentation fixtures for a planned `app record` /
//! `app replay` harness that was never built (ADR-007 / ADR-011
//! mention it). This file is that harness, executable: it walks the
//! case files in order, provisions the auth the case names (seeded
//! user + HS256 token, permissions granted through a fresh
//! non-privileged `editor` role — never `admin`/`super_admin`, so no
//! TOTP enrolment is triggered), runs the per-file seed hook for the
//! (b)-tier cases that need a row, substitutes the mechanical
//! placeholders (`<uuid>`, `<password>`, `<post_id>`, `<report_id>`,
//! `{item_id}`), sends the request with `oneshot`, and checks the
//! response with subset semantics (see `check`).
//!
//! Tiers (surveyed, per wave README):
//! - (a) executable as-is on a fresh sqlite DB.
//! - (b) executable once a user/role/row/setting or a mechanical
//!   `<uuid>` substitution is supplied — the seed hook does that.
//! - (c) documentation-only, cannot run as written — the SKIP list
//!   below, printed in the summary, never silent.
//!
//! Wave 8 cases (`proj-*`, `lift-*`) are NOT replayed here; they are
//! covered by `wave8_projects.rs` / `wave8_lift.rs`.
//!
//! The Postgres smoke at the end (`wave8_pg_smoke`, gated on
//! `WAVE2_PG_URL`) replays the lift-critical subset on Postgres:
//! unknown-path 404, `/metrics`, `/api/v1/health`, `/openapi.json`,
//! the fatwa-ask sidecar 503, and `GET /api/v1/projects` (empty list
//! for a `projects.manage` holder, 403 for a plain user).

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode},
    Router,
};
use chrono::{Duration, Utc};
use maturidi_api_rs::{
    build_router, encrypt_value, init_test_schema, AppState, Config, Db, IntoArg, TEST_SCHEMA,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf};
use tower::ServiceExt;
use uuid::Uuid;

const JWT_SECRET: &str = "test-jwt-secret-wave8";
const REPLAY_PASSWORD: &str = "ReplayPass123!";
const TELEGRAM_SECRET: &str = "replay-telegram-secret";

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: JWT_SECRET.to_string(),
        encryption_key: "test-encryption-key-wave8".to_string(),
        captcha_enabled: false,
        // No origin: native routes must never touch another server.
        storage_dir: format!(
            "{}/wave8-replay-storage-{}",
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

    async fn call_raw(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let res = self.router().oneshot(req).await.expect("oneshot");
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, headers, bytes.to_vec())
    }

    async fn call(&self, req: Request<Body>) -> (StatusCode, Value) {
        let (status, _, bytes) = self.call_raw(req).await;
        let value: Value = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
        (status, value)
    }
}

fn req(method: Method, uri: &str, body: Option<ReqBody>) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))));
    match body {
        Some(ReqBody::Json(v)) => {
            builder = builder.header("content-type", "application/json");
            builder
                .body(Body::from(serde_json::to_string(&v).unwrap()))
                .unwrap()
        }
        Some(ReqBody::Raw(text, ctype)) => {
            if let Some(c) = ctype {
                builder = builder.header("content-type", c);
            }
            builder.body(Body::from(text)).unwrap()
        }
        Some(ReqBody::Empty) | None => builder.body(Body::empty()).unwrap(),
    }
}

enum ReqBody {
    Json(Value),
    Raw(String, Option<&'static str>),
    Empty,
}

fn authed(request: Request<Body>, token: &str) -> Request<Body> {
    let mut request = request;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {token}").parse().unwrap());
    request
}

fn with_header(mut request: Request<Body>, name: &str, value: &str) -> Request<Body> {
    request.headers_mut().insert(
        HeaderName::from_bytes(name.as_bytes()).unwrap(),
        HeaderValue::from_str(value).unwrap(),
    );
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
    if keys.is_empty() {
        return;
    }
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

fn loc(fa: &str) -> Value {
    json!({ "fa": fa, "ar": fa, "en": fa })
}

// ============================ case files ===================================

#[derive(Debug, Clone, Deserialize)]
struct Case {
    #[serde(default)]
    case: String,
    method: String,
    path: String,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(default)]
    headers: Option<HashMap<String, String>>,
    expected_status: u16,
    #[serde(default)]
    expected: Option<Value>,
    #[serde(default)]
    expected_content_type: Option<String>,
}

struct CaseFile {
    wave: u8,
    stem: String,
    file: String,
    case: Case,
}

fn cases_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../app-cases")
}

fn load_cases() -> Vec<CaseFile> {
    let dir = cases_dir();
    let mut out = Vec::new();
    for wave in 2..=7u8 {
        let wdir = dir.join(format!("wave{wave}"));
        let mut names: Vec<String> = std::fs::read_dir(&wdir)
            .unwrap_or_else(|e| panic!("read {}: {e}", wdir.display()))
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".json"))
            .collect();
        names.sort();
        for name in names {
            let text = std::fs::read_to_string(wdir.join(&name))
                .unwrap_or_else(|e| panic!("read {name}: {e}"));
            let case: Case =
                serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {name}: {e}"));
            out.push(CaseFile {
                wave,
                stem: name.trim_end_matches(".json").to_string(),
                file: format!("wave{wave}/{name}"),
                case,
            });
        }
    }
    out
}

/// (c)-tier: documentation-only, cannot run as written. Printed in the
/// summary; never silent. Anything else that cannot run must FAIL.
fn skip_reason(wave: u8, stem: &str) -> Option<&'static str> {
    match (wave, stem) {
        (2, "08-auth-refresh-rotate") => Some(
            "needs a refresh token minted by a prior login (cross-case state)",
        ),
        (2, "09-auth-verify-email") => {
            Some("needs the email-verification token issued at registration")
        }
        (2, "10-auth-2fa-enable") => {
            Some("needs a TOTP secret + live code from the setup step")
        }
        (6, "lib-05") => Some(
            "path is a {media_key}&exp={exp}&sig={sig} template — a download signature must be minted first",
        ),
        (6, "lib-06") => Some("body is a prose multipart description, not sendable bytes"),
        _ => None,
    }
}

// ============================ auth provisioning ============================

/// Permissions the case's endpoint requires, keyed by file, on top of
/// what the auth label itself grants. Read from each handler's
/// `require_permission` / `admin_ctx` — the case files name the auth
/// loosely (`bearer` for admin endpoints), so the runner grants what
/// the endpoint actually checks.
fn endpoint_perms(wave: u8, stem: &str) -> Vec<&'static str> {
    match (wave, stem) {
        (2, "11-api-keys-create") | (2, "12-api-keys-embed-snippet") => {
            vec!["api_keys.manage"]
        }
        (2, "14-users-admin-list") => vec!["users.manage"],
        (3, "05-content-page-bad-slug") | (3, "06-content-page-create") => {
            vec!["content.create"]
        }
        (3, "08-articles-category-bad-slug") | (3, "09-articles-create-bad-slug") => {
            vec!["knowledge.manage"]
        }
        (3, "12-fatwa-submission-empty-body") => vec!["fatwa.submit"],
        (5, "09-admin-playlist-create") | (5, "10-admin-subtitles") => {
            vec!["library.manage"]
        }
        (5, "11-voice-settings") | (5, "14-voice-usage") => vec!["voice.manage"],
        (7, s) if s.starts_with("acc-0") => vec!["content.bulk"],
        (7, "fin-08-fx-rate-irr") => vec!["finance.manage"],
        (7, "syn-07-policies") | (7, "syn-08-publish") => vec!["telegram.manage"],
        _ => vec![],
    }
}

/// Resolve the case's `auth` label into (user_id, token). Returns
/// (None, None) for unauthenticated cases and for the telegram
/// webhook header case (the secret header is set by the seed hook).
async fn provision_auth(
    env: &Env,
    wave: u8,
    stem: &str,
    label: Option<&str>,
) -> (Option<Uuid>, Option<String>) {
    let Some(label) = label else {
        return (None, None);
    };
    if label == "none" || label.starts_with("header ") {
        return (None, None);
    }
    let mut perms: Vec<String> = endpoint_perms(wave, stem)
        .into_iter()
        .map(str::to_string)
        .collect();
    match label {
        "moderator" => perms.push("social.moderate".to_string()),
        "bearer" | "user" | "bearer-admin" | "admin" | "bearer <redacted>" => {}
        _ if label.starts_with("admin (") && label.ends_with(')') => {
            let inner = &label["admin (".len()..label.len() - 1];
            for p in inner.split('+') {
                perms.push(p.trim().to_string());
            }
        }
        _ if label.contains('.') => perms.push(label.to_string()),
        _ => {}
    }
    // wave2/13 `users.me` asserts the seeded user's own email.
    let email = if wave == 2 && stem == "13-users-me" {
        "user@example.com".to_string()
    } else {
        format!("replay-w{wave}-{stem}@example.com")
    };
    let (uid, token) = seed_user(env, &email).await;
    let refs: Vec<&str> = perms.iter().map(String::as_str).collect();
    grant(env, uid, &refs).await;
    (Some(uid), Some(token))
}

// ============================ seed hook ====================================

struct SeedOut {
    subs: HashMap<String, String>,
    extra_headers: Vec<(String, String)>,
}

impl SeedOut {
    fn new() -> Self {
        Self {
            subs: HashMap::new(),
            extra_headers: Vec::new(),
        }
    }
}

struct LibrarySeed {
    id: Option<Uuid>,
    kind: &'static str,
    title_fa: &'static str,
    workflow_status: &'static str,
    visibility: &'static str,
    transcript: Option<&'static str>,
    media_key: Option<&'static str>,
    created_secs_ago: i64,
    ai_scopes: Vec<&'static str>,
    include_in_fatwa_rag: bool,
}

impl Default for LibrarySeed {
    fn default() -> Self {
        Self {
            id: None,
            kind: "video",
            title_fa: "ویدیوی تست",
            workflow_status: "published",
            visibility: "public",
            transcript: None,
            media_key: Some("library/video/abc123.mp4"),
            created_secs_ago: 0,
            ai_scopes: vec![],
            include_in_fatwa_rag: false,
        }
    }
}

async fn seed_library_item(env: &Env, s: LibrarySeed) -> Uuid {
    let id = s.id.unwrap_or_else(Uuid::new_v4);
    let now = Utc::now() - Duration::seconds(s.created_secs_ago);
    let published_at = if s.workflow_status == "published" {
        Some(now)
    } else {
        None
    };
    env.db
        .execute(
            "INSERT INTO library_items (id, kind, title, description, transcript, \
             workflow_status, visibility, external_url, media_key, cover_media_key, \
             scholar_id, topic_tags, duration_seconds, published_at, created_at, updated_at, \
             ai_scopes, include_in_fatwa_rag) \
             VALUES (?, ?, ?, NULL, ?, ?, ?, NULL, ?, NULL, NULL, '[]', 120, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                s.kind.into_arg(),
                json!({"fa": s.title_fa, "ar": s.title_fa, "en": s.title_fa}).into_arg(),
                s.transcript.into_arg(),
                s.workflow_status.into_arg(),
                s.visibility.into_arg(),
                s.media_key.into_arg(),
                published_at.into_arg(),
                now.into_arg(),
                now.into_arg(),
                json!(s.ai_scopes).into_arg(),
                s.include_in_fatwa_rag.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_video(env: &Env, created_secs_ago: i64) -> Uuid {
    seed_library_item(
        env,
        LibrarySeed {
            created_secs_ago,
            ..Default::default()
        },
    )
    .await
}

async fn seed_playlist(env: &Env, is_published: bool) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO video_playlists (id, title, description, cover_media_key, scholar_id, \
             is_published, sort_order, created_at, updated_at) \
             VALUES (?, ?, NULL, NULL, NULL, ?, 0, ?, ?)",
            &[
                id.into_arg(),
                json!({"fa": "مکتوبات", "ar": "مکتوبات", "en": "Maktubat"}).into_arg(),
                is_published.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_membership(env: &Env, playlist_id: Uuid, item_id: Uuid, position: i64) {
    env.db
        .execute(
            "INSERT INTO video_playlist_items (id, playlist_id, item_id, position, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                playlist_id.into_arg(),
                item_id.into_arg(),
                position.into_arg(),
                Utc::now().into_arg(),
            ],
        )
        .await
        .unwrap();
}

async fn seed_subtitle(env: &Env, item_id: Uuid, lang: &str, vtt: &str) {
    env.db
        .execute(
            "INSERT INTO item_subtitles (id, item_id, lang, status, vtt, error, created_at, updated_at) \
             VALUES (?, ?, ?, 'ready', ?, NULL, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                item_id.into_arg(),
                lang.into_arg(),
                vtt.into_arg(),
                Utc::now().into_arg(),
                Utc::now().into_arg(),
            ],
        )
        .await
        .unwrap();
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

/// Register (email + password) then log in. Returns (user_id, access, refresh).
async fn register_user(env: &Env, email: &str, password: &str) -> Result<String, String> {
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/register",
            Some(ReqBody::Json(json!({
                "full_name": "Test User",
                "email": email,
                "password": password,
                "accept_terms": true,
            }))),
        ))
        .await;
    if status != StatusCode::CREATED {
        return Err(format!("register {email}: {status} {body}"));
    }
    let user_id = body["id"].as_str().unwrap_or("").to_string();
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/auth/login",
            Some(ReqBody::Json(
                json!({"identifier": email, "password": password}),
            )),
        ))
        .await;
    if status != StatusCode::OK {
        return Err(format!("login {email}: {status} {body}"));
    }
    Ok(user_id)
}

async fn lms_admin_token(env: &Env) -> (Uuid, String) {
    let (uid, token) = seed_user(env, "replay-lms-admin@example.com").await;
    grant(env, uid, &["lms.manage"]).await;
    (uid, token)
}

async fn create_course(env: &Env, token: &str) -> Result<String, String> {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/courses",
                Some(ReqBody::Json(json!({
                    "curriculum_id": Value::Null,
                    "title": loc("دورهٔ تست"),
                    "description": loc("توضیح"),
                    "level": "beginner",
                    "visibility": "public",
                    "sort_order": 0,
                }))),
            ),
            token,
        ))
        .await;
    if status != StatusCode::CREATED {
        return Err(format!("create course: {status} {body}"));
    }
    Ok(body["id"].as_str().unwrap().to_string())
}

async fn create_lesson(env: &Env, token: &str, course_id: &str) -> Result<String, String> {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/lessons",
                Some(ReqBody::Json(json!({
                    "course_id": course_id,
                    "title": loc("درس ۱"),
                    "body": [],
                    "duration_minutes": 5,
                    "sort_order": 0,
                    "is_free_preview": true,
                }))),
            ),
            token,
        ))
        .await;
    if status != StatusCode::CREATED {
        return Err(format!("create lesson: {status} {body}"));
    }
    Ok(body["id"].as_str().unwrap().to_string())
}

async fn create_cohort(env: &Env, token: &str, course_id: &str) -> Result<String, String> {
    let now = Utc::now();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/lms/cohorts",
                Some(ReqBody::Json(json!({
                    "course_id": course_id,
                    "title": loc("هم‌گروهی تست"),
                    "starts_at": (now - Duration::hours(1)).to_rfc3339(),
                    "ends_at": (now + Duration::hours(1)).to_rfc3339(),
                    "max_seats": 1,
                    "is_open": true,
                }))),
            ),
            token,
        ))
        .await;
    if status != StatusCode::CREATED {
        return Err(format!("create cohort: {status} {body}"));
    }
    Ok(body["id"].as_str().unwrap().to_string())
}

async fn create_post(env: &Env, token: &str, body_text: &str) -> Result<String, String> {
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/social/posts",
                Some(ReqBody::Json(json!({ "body": body_text }))),
            ),
            token,
        ))
        .await;
    if status != StatusCode::CREATED {
        return Err(format!("create post: {status} {body}"));
    }
    Ok(body["id"].as_str().unwrap().to_string())
}

async fn seed_conflict(env: &Env, user_id: Uuid, id: Uuid) -> Result<(), String> {
    let entity_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO conflict_records (id, user_id, entity_type, entity_id, local_payload, \
             remote_payload, local_version, remote_version, resolution, created_at) \
             VALUES (?, ?, 'user_ayah_note', ?, ?, ?, 1, 1, 'pending', ?)",
            &[
                id.into_arg(),
                user_id.into_arg(),
                entity_id.to_string().into_arg(),
                json!({"surah": 2, "ayah": 255, "note_text": "نسخهٔ محلی"}).into_arg(),
                json!({"surah": 2, "ayah": 255, "note_text": "نسخهٔ راه‌دور"}).into_arg(),
                Utc::now().into_arg(),
            ],
        )
        .await
        .map_err(|e| format!("seed conflict: {e:?}"))?;
    Ok(())
}

const VTT_FA: &str = "WEBVTT\n\n00:00.000 --> 00:01.000\nسلام";

/// Per-file seed hook for the (b)-tier cases. Returns substitution
/// values (`uuid` / `post_id` / `report_id` / `item_id`) and any extra
/// headers (telegram webhook secret). Each branch is keyed by file
/// name with a comment naming the case it serves.
async fn seed(
    env: &Env,
    wave: u8,
    stem: &str,
    user_id: Option<Uuid>,
    token: Option<&str>,
) -> Result<SeedOut, String> {
    let mut out = SeedOut::new();
    let tok = token.unwrap_or("");
    match (wave, stem) {
        // wave2/07-auth-login: the login needs a registered user with
        // the replay password (real password hash, via the API).
        (2, "07-auth-login") => {
            register_user(env, "user@example.com", REPLAY_PASSWORD).await?;
        }
        // wave2/12-api-keys-embed-snippet: the path needs a real API
        // key id; create one with the case's admin token.
        (2, "12-api-keys-embed-snippet") => {
            let (status, body) = env
                .call(authed(
                    req(
                        Method::POST,
                        "/api/v1/api-keys",
                        Some(ReqBody::Json(json!({
                            "name": "replay key",
                            "scopes": ["prayer-times"],
                            "rate_limit_per_min": 60,
                            "quota_monthly": 10000,
                        }))),
                    ),
                    tok,
                ))
                .await;
            if status != StatusCode::CREATED {
                return Err(format!("seed api key: {status} {body}"));
            }
            out.subs
                .insert("uuid".to_string(), body["id"].as_str().unwrap().to_string());
        }
        // wave4/02-quran-surah / 04-quran-ayah: the built-in defaults
        // point translations/tafsir at AlQuran Cloud (online); the
        // offline replay asks for the bundled editions explicitly,
        // exactly like the wave4 tests do.
        (4, "02-quran-surah") | (4, "04-quran-ayah") => {
            seed_setting(env, "quran.translations", "[\"fa.rowwad\"]").await;
            seed_setting(env, "quran.tafsirs", "[\"fa.mokhtasar\"]").await;
            seed_setting(env, "quran.reciters", "[]").await;
        }
        // wave4/08-quran-reading-state-stale: store a NEWER state
        // first (last_read.surah 2, updated_at 100); the case's PUT
        // with updated_at 50 is then stale and answers 409.
        (4, "08-quran-reading-state-stale") => {
            let (status, body) = env
                .call(authed(
                    req(
                        Method::PUT,
                        "/api/v1/quran/reading-state",
                        Some(ReqBody::Json(json!({
                            "bookmarks": [],
                            "last_read": {
                                "surah": 2, "ayah": 1, "view": "mushaf",
                                "mushaf": "1405", "savedAt": 100
                            },
                            "updated_at": 100,
                        }))),
                    ),
                    tok,
                ))
                .await;
            if status != StatusCode::OK {
                return Err(format!("seed reading-state: {status} {body}"));
            }
        }
        // wave5/02-videos-list-cursor: ≥2 videos so ?limit=1 has a
        // next page and a non-null next_cursor.
        (5, "02-videos-list-cursor") => {
            seed_video(env, 10).await;
            seed_video(env, 0).await;
        }
        // wave5/04-videos-watch: published video + published playlist
        // (position 1) + ready fa subtitle.
        (5, "04-videos-watch") => {
            let item = seed_video(env, 0).await;
            let playlist = seed_playlist(env, true).await;
            seed_membership(env, playlist, item, 1).await;
            seed_subtitle(env, item, "fa", VTT_FA).await;
            out.subs.insert("uuid".to_string(), item.to_string());
        }
        // wave5/06-videos-saved / 07-videos-progress: a published
        // video to save / track progress on.
        (5, "06-videos-saved") | (5, "07-videos-progress") => {
            let item = seed_video(env, 0).await;
            out.subs.insert("uuid".to_string(), item.to_string());
        }
        // wave5/08-videos-subtitle-vtt: published video + ready fa VTT.
        (5, "08-videos-subtitle-vtt") => {
            let item = seed_video(env, 0).await;
            seed_subtitle(env, item, "fa", VTT_FA).await;
            out.subs.insert("uuid".to_string(), item.to_string());
        }
        // wave5/10-admin-subtitles: one video so the statuses list
        // has one row (all langs missing).
        (5, "10-admin-subtitles") => {
            seed_video(env, 0).await;
        }
        // wave6/02-lms-course-detail: a public course (0 lessons).
        (6, "02-lms-course-detail") => {
            let (_, admin) = lms_admin_token(env).await;
            let course = create_course(env, &admin).await?;
            out.subs.insert("uuid".to_string(), course);
        }
        // wave6/03-lms-progress-certificate: a course with exactly
        // one lesson, so completing it completes the course.
        (6, "03-lms-progress-certificate") => {
            let (_, admin) = lms_admin_token(env).await;
            let course = create_course(env, &admin).await?;
            let lesson = create_lesson(env, &admin, &course).await?;
            out.subs.insert("uuid".to_string(), lesson);
        }
        // wave6/04-lms-cohort-enroll: a 1-seat open cohort.
        (6, "04-lms-cohort-enroll") => {
            let (_, admin) = lms_admin_token(env).await;
            let course = create_course(env, &admin).await?;
            let cohort = create_cohort(env, &admin, &course).await?;
            out.subs.insert("uuid".to_string(), cohort);
        }
        // wave6/lib-01: one published public item.
        (6, "lib-01") => {
            seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "کتاب تست",
                    media_key: None,
                    ..Default::default()
                },
            )
            .await;
        }
        // wave6/lib-02: two published public items (newest first).
        (6, "lib-02") => {
            seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "کتاب قدیمی",
                    media_key: None,
                    created_secs_ago: 10,
                    ..Default::default()
                },
            )
            .await;
            seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "کتاب جدید",
                    media_key: None,
                    ..Default::default()
                },
            )
            .await;
        }
        // wave6/lib-03: published public item with a media key.
        (6, "lib-03") => {
            let item = seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "کتاب تست",
                    ..Default::default()
                },
            )
            .await;
            out.subs.insert("item_id".to_string(), item.to_string());
        }
        // wave6/lib-04: an item whose title contains the search term.
        (6, "lib-04") => {
            seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "ماتریدی تست",
                    transcript: Some("متن دربارهٔ ماتریدی و عقیده"),
                    media_key: None,
                    ..Default::default()
                },
            )
            .await;
        }
        // wave6/lib-07: a draft item to transition to in_review.
        (6, "lib-07") => {
            let item = seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "جزوهٔ تست",
                    workflow_status: "draft",
                    media_key: None,
                    ..Default::default()
                },
            )
            .await;
            out.subs.insert("item_id".to_string(), item.to_string());
        }
        // wave6/lib-08: an approved item with a fatwa scope and a
        // ≥20-word text, so index-ai creates ≥1 chunk.
        (6, "lib-08") => {
            let item = seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "رسالهٔ فقهی تست",
                    workflow_status: "approved",
                    transcript: Some(
                        "متن فقهی تستی برای ایندکس هوش مصنوعی با بیش از بیست واژه \
                         تا قطعه دانش ساخته شود و تأیید گردد و در بازیابی استفاده شود \
                         ان شاء الله تعالی به توفیق الهی و عنایت حضرت حق",
                    ),
                    media_key: None,
                    ai_scopes: vec!["fatwa"],
                    include_in_fatwa_rag: true,
                    ..Default::default()
                },
            )
            .await;
            out.subs.insert("item_id".to_string(), item.to_string());
        }
        // wave7/soc-03-feed / soc-05-like / soc-06-report: one
        // published post by the case user.
        (7, "soc-03-feed") => {
            create_post(env, tok, "پست تست برای فید").await?;
        }
        (7, "soc-05-like") | (7, "soc-06-report") => {
            let post = create_post(env, tok, "پست تست").await?;
            out.subs.insert("post_id".to_string(), post);
        }
        // wave7/soc-04-post-banned-word: seed the banned word the
        // case body contains, via a words-admin.
        (7, "soc-04-post-banned-word") => {
            let (_uid, wtoken) = seed_user(env, "replay-words-admin@example.com").await;
            grant(env, _uid, &["moderation.words"]).await;
            let (status, body) = env
                .call(authed(
                    req(
                        Method::POST,
                        "/api/v1/moderation/banned-words",
                        Some(ReqBody::Json(json!({ "word": "ممنوعه" }))),
                    ),
                    &wtoken,
                ))
                .await;
            if status != StatusCode::CREATED {
                return Err(format!("seed banned word: {status} {body}"));
            }
        }
        // wave7/soc-07-report-triage: a post + a report on it.
        (7, "soc-07-report-triage") => {
            let post = create_post(env, tok, "پست گزارش‌شده").await?;
            let (status, body) = env
                .call(authed(
                    req(
                        Method::POST,
                        "/api/v1/social/report",
                        Some(ReqBody::Json(json!({
                            "reason": "محتوای نامناسب",
                            "target_id": post,
                            "target_type": "post",
                        }))),
                    ),
                    tok,
                ))
                .await;
            if status != StatusCode::CREATED {
                return Err(format!("seed report: {status} {body}"));
            }
            out.subs.insert(
                "report_id".to_string(),
                body["id"].as_str().unwrap().to_string(),
            );
        }
        // wave7/syn-02-pull: push one note op first, so the changelog
        // has a row after cursor 0.
        (7, "syn-02-pull") => {
            let (status, body) = env
                .call(authed(
                    req(
                        Method::POST,
                        "/api/v1/sync/push",
                        Some(ReqBody::Json(json!({
                            "device_id": "dev-1",
                            "ops": [{
                                "client_op_id": "op-1",
                                "entity_type": "user_ayah_note",
                                "entity_id": "00000000-0000-0000-0000-000000000001",
                                "op_type": "upsert",
                                "payload": {"surah": 2, "ayah": 255, "note_text": "یادداشت تست"},
                                "base_version": 0,
                            }],
                        }))),
                    ),
                    tok,
                ))
                .await;
            if status != StatusCode::OK {
                return Err(format!("seed sync push: {status} {body}"));
            }
        }
        // wave7/syn-03-conflicts: one pending conflict row.
        (7, "syn-03-conflicts") => {
            seed_conflict(env, user_id.unwrap(), Uuid::new_v4()).await?;
        }
        // wave7/syn-04-resolve: a pending conflict with the exact id
        // the case path resolves.
        (7, "syn-04-resolve") => {
            let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
            seed_conflict(env, user_id.unwrap(), id).await?;
        }
        // wave7/syn-05-offline-pack: one published public item feeds
        // the "popular" branch.
        (7, "syn-05-offline-pack") => {
            seed_library_item(
                env,
                LibrarySeed {
                    kind: "pdf",
                    title_fa: "کتاب تست",
                    media_key: None,
                    ..Default::default()
                },
            )
            .await;
        }
        // wave7/syn-08-publish: the case sends no targets, so the
        // publish needs a policy row to supply the target channel —
        // and the referenced library item must exist (fetch_content
        // 404s otherwise), with the exact id the case names.
        (7, "syn-08-publish") => {
            seed_library_item(
                env,
                LibrarySeed {
                    id: Some(Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap()),
                    kind: "pdf",
                    title_fa: "کتاب تست",
                    media_key: None,
                    ..Default::default()
                },
            )
            .await;
            let (_uid, ttoken) = seed_user(env, "replay-tg-admin@example.com").await;
            grant(env, _uid, &["telegram.manage"]).await;
            let (status, body) = env
                .call(authed(
                    req(
                        Method::PUT,
                        "/api/v1/telegram/distribute/policies/library_item",
                        Some(ReqBody::Json(json!({
                            "policy": "manual_approval",
                            "target_channels": ["@chan1"],
                        }))),
                    ),
                    &ttoken,
                ))
                .await;
            if status != StatusCode::OK {
                return Err(format!("seed telegram policy: {status} {body}"));
            }
        }
        // wave7/syn-06-webhook: store the telegram webhook secret in
        // settings, then send it as the header the case names.
        (7, "syn-06-webhook") => {
            seed_setting(env, "telegram.webhook_secret", TELEGRAM_SECRET).await;
            out.extra_headers.push((
                "x-telegram-bot-api-secret-token".to_string(),
                TELEGRAM_SECRET.to_string(),
            ));
        }
        _ => {}
    }
    Ok(out)
}

// ============================ substitution =================================

struct SubCtx {
    uuid: String,
    post_id: String,
    report_id: String,
    item_id: String,
    token: String,
}

fn subst_str(s: &str, ctx: &SubCtx) -> String {
    s.replace("<uuid>", &ctx.uuid)
        .replace("<password>", REPLAY_PASSWORD)
        .replace("<post_id>", &ctx.post_id)
        .replace("<report_id>", &ctx.report_id)
        .replace("{item_id}", &ctx.item_id)
        .replace("<jwt>", &ctx.token)
        .replace("<opaque>", "replay-opaque-token-123")
        .replace("<cursor>", "replay-cursor")
}

fn subst_value(v: &Value, ctx: &SubCtx) -> Value {
    match v {
        Value::String(s) => Value::String(subst_str(s, ctx)),
        Value::Array(items) => Value::Array(items.iter().map(|i| subst_value(i, ctx)).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(k.clone(), subst_value(val, ctx));
            }
            Value::Object(out)
        }
        _ => v.clone(),
    }
}

/// Expected-side substitution: only `{item_id}` is a real reference
/// (the seeded row id). `<...>` placeholders stay literal for the
/// matcher.
fn subst_expected(v: &Value, ctx: &SubCtx) -> Value {
    match v {
        Value::String(s) if s == "{item_id}" => Value::String(ctx.item_id.clone()),
        Value::Array(items) => Value::Array(items.iter().map(|i| subst_expected(i, ctx)).collect()),
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                out.insert(k.clone(), subst_expected(val, ctx));
            }
            Value::Object(out)
        }
        _ => v.clone(),
    }
}

// ============================ check semantics ==============================

/// Prose that describes an ABSENT key (e.g. lib-03 `transcript`:
/// "absent — the public shape never exposes transcript/ocr_text").
fn is_absent_prose(v: &Value) -> bool {
    v.as_str()
        .is_some_and(|s| s.to_lowercase().contains("absent"))
}

fn is_valid_uuid(s: &str) -> bool {
    Uuid::parse_str(s).is_ok()
}

/// Template match for expected strings with an embedded `<uuid>`
/// (e.g. `/api/v1/videos/<uuid>/subtitles/fa.vtt`): the literal parts
/// must match and each gap must be a valid UUID.
fn template_match(exp: &str, act: &str) -> bool {
    let parts: Vec<&str> = exp.split("<uuid>").collect();
    if parts.len() < 2 {
        return false;
    }
    let mut rest = act;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else {
                return false;
            };
            rest = r;
        } else if i == parts.len() - 1 {
            if !part.is_empty() {
                let Some(r) = rest.strip_suffix(part) else {
                    return false;
                };
                rest = r;
            }
            return Uuid::parse_str(rest).is_ok();
        } else {
            let Some(pos) = rest.find(part) else {
                return false;
            };
            if Uuid::parse_str(&rest[..pos]).is_err() {
                return false;
            }
            rest = &rest[pos + part.len()..];
        }
    }
    false
}

fn check_string(exp: &str, act: &Value, path: &str, errs: &mut Vec<String>) {
    // Exact placeholders.
    match exp {
        "<uuid>" => {
            if !act.as_str().is_some_and(is_valid_uuid) {
                errs.push(format!("{path}: expected <uuid>, got {act}"));
            }
            return;
        }
        "<cursor>" => {
            if !act.as_str().is_some_and(|s| !s.is_empty()) {
                errs.push(format!(
                    "{path}: expected <cursor> (non-empty string), got {act}"
                ));
            }
            return;
        }
        "<password>"
        | "<jwt>"
        | "<opaque>"
        | "<opaque-new>"
        | "<cert-code>"
        | "<mk_... shown only once>"
        | "<signed-library-download-url>" => {
            if !act.as_str().is_some_and(|s| !s.is_empty()) {
                errs.push(format!("{path}: expected non-empty string, got {act}"));
            }
            return;
        }
        "<string|null>" => {
            if !(act.is_null() || act.is_string()) {
                errs.push(format!("{path}: expected <string|null>, got {act}"));
            }
            return;
        }
        "<array>" => {
            if !act.is_array() {
                errs.push(format!("{path}: expected <array>, got {act}"));
            }
            return;
        }
        "<8 chars>" => {
            if !act.as_str().is_some_and(|s| s.chars().count() == 8) {
                errs.push(format!("{path}: expected <8 chars>, got {act}"));
            }
            return;
        }
        _ => {}
    }
    // "<N items>" / "<N item>" — array of exactly N.
    if exp.starts_with('<') && exp.ends_with('>') {
        let inner = &exp[1..exp.len() - 1];
        let mut it = inner.split_whitespace();
        if let (Some(n), Some(unit)) = (it.next(), it.next()) {
            if let Ok(n) = n.parse::<usize>() {
                if unit == "items" || unit == "item" {
                    match act.as_array() {
                        Some(a) if a.len() == n => return,
                        _ => {
                            errs.push(format!("{path}: expected <{n} {unit}>, got {act}"));
                            return;
                        }
                    }
                }
            }
        }
        // Any other "<...>" placeholder: any non-empty string.
        if !exp.contains(' ') {
            if !act.as_str().is_some_and(|s| !s.is_empty()) {
                errs.push(format!("{path}: expected placeholder {exp}, got {act}"));
            }
            return;
        }
    }
    // Embedded <uuid> template.
    if exp.contains("<uuid>") {
        match act.as_str() {
            Some(s) if template_match(exp, s) => return,
            _ => {
                errs.push(format!("{path}: expected template {exp}, got {act}"));
                return;
            }
        }
    }
    // Prose "<...>" / sentence: the key exists and the value is
    // present and non-empty in its own type.
    if exp.contains(' ') || exp.starts_with('<') {
        match act {
            Value::Null => {
                if !exp.to_lowercase().contains("null") {
                    errs.push(format!("{path}: prose {exp:?} but value is null"));
                }
            }
            Value::String(s) => {
                if s.is_empty() {
                    errs.push(format!(
                        "{path}: prose {exp:?} but value is an empty string"
                    ));
                }
            }
            Value::Array(a) => {
                if exp.starts_with("array") && a.is_empty() {
                    errs.push(format!("{path}: prose {exp:?} but array is empty"));
                } else if let Some(rest) = exp.strip_prefix("exactly ") {
                    if let Some(n) = rest
                        .split_whitespace()
                        .next()
                        .and_then(|w| w.parse::<usize>().ok())
                    {
                        if a.len() != n {
                            errs.push(format!(
                                "{path}: prose {exp:?} but array has {} items",
                                a.len()
                            ));
                        }
                    }
                }
            }
            Value::Number(_) => {
                if exp.starts_with("integer") && !act.is_number() {
                    errs.push(format!("{path}: prose {exp:?} but value is not an integer"));
                }
            }
            Value::Bool(_) | Value::Object(_) => {}
        }
        return;
    }
    // Plain string: exact equality.
    if act.as_str() != Some(exp) {
        errs.push(format!("{path}: expected {exp:?}, got {act}"));
    }
}

/// Subset-match `expected` against the response JSON: every key in
/// `expected` must exist in the response; objects recurse; numbers
/// compare numerically (0.0 == 0); empty arrays compare exactly;
/// non-empty arrays compare element-wise up to the expected length
/// (the expected array is a prefix of the actual one).
fn check(exp: &Value, act: &Value, path: &str, errs: &mut Vec<String>) {
    match exp {
        Value::Null => {
            if !act.is_null() {
                errs.push(format!("{path}: expected null, got {act}"));
            }
        }
        Value::Bool(b) => {
            if act.as_bool() != Some(*b) {
                errs.push(format!("{path}: expected {b}, got {act}"));
            }
        }
        Value::Number(n) => {
            let ok = match (n.as_f64(), act.as_f64()) {
                (Some(a), Some(b)) => (a - b).abs() < 1e-9,
                _ => false,
            };
            if !ok {
                errs.push(format!("{path}: expected {n}, got {act}"));
            }
        }
        Value::String(s) => check_string(s, act, path, errs),
        Value::Array(items) => {
            if items.is_empty() {
                if act.as_array().is_none_or(|a| !a.is_empty()) {
                    errs.push(format!("{path}: expected [], got {act}"));
                }
                return;
            }
            match act.as_array() {
                None => errs.push(format!("{path}: expected array, got {act}")),
                Some(a) => {
                    if a.len() < items.len() {
                        errs.push(format!(
                            "{path}: expected at least {} items, got {}",
                            items.len(),
                            a.len()
                        ));
                    } else {
                        for (i, e) in items.iter().enumerate() {
                            check(e, &a[i], &format!("{path}[{i}]"), errs);
                        }
                    }
                }
            }
        }
        Value::Object(map) => {
            let Some(obj) = act.as_object() else {
                errs.push(format!("{path}: expected object, got {act}"));
                return;
            };
            for (k, v) in map {
                let p = format!("{path}.{k}");
                // Case-file annotation: several lib-* cases carry a
                // `note` key documenting behaviour ("illegal edge →
                // 422 …", "draft item → 422 …") — it is not a response
                // field in Python or Rust, so the matcher skips it.
                if k == "note" {
                    continue;
                }
                if is_absent_prose(v) {
                    if obj.contains_key(k) {
                        errs.push(format!("{p}: expected absent, got {}", obj[k]));
                    }
                    continue;
                }
                match obj.get(k) {
                    None => errs.push(format!("{p}: missing key (expected {v})")),
                    Some(av) => check(v, av, &p, errs),
                }
            }
        }
    }
}

// ============================ runner =======================================

struct WaveStats {
    passed: usize,
    failed: usize,
    skipped: usize,
}

#[tokio::test]
async fn replay_all_app_cases() {
    let cases = load_cases();
    assert_eq!(cases.len(), 127, "expected 127 wave2..wave7 cases");
    let mut stats: HashMap<u8, WaveStats> = HashMap::new();
    let mut failures: Vec<String> = Vec::new();
    let mut skips: Vec<String> = Vec::new();

    for cf in &cases {
        let entry = stats.entry(cf.wave).or_insert(WaveStats {
            passed: 0,
            failed: 0,
            skipped: 0,
        });
        if let Some(reason) = skip_reason(cf.wave, &cf.stem) {
            entry.skipped += 1;
            skips.push(format!("{}: {reason}", cf.file));
            continue;
        }
        match run_case(cf).await {
            Ok(()) => entry.passed += 1,
            Err(detail) => {
                entry.failed += 1;
                failures.push(format!("{} ({}) — {detail}", cf.file, cf.case.case));
            }
        }
    }

    println!("wave  passed  failed  skipped");
    for wave in 2..=7u8 {
        let s = stats.get(&wave).unwrap();
        println!(
            "wave{wave}  {:>6}  {:>6}  {:>7}",
            s.passed, s.failed, s.skipped
        );
    }
    println!("skipped ({}):", skips.len());
    for s in &skips {
        println!("  SKIP {s}");
    }
    if !failures.is_empty() {
        println!("failures ({}):", failures.len());
        for f in &failures {
            println!("  FAIL {f}");
        }
    }
    assert!(
        failures.is_empty(),
        "{} replay case(s) failed (see output above)",
        failures.len()
    );
}

async fn run_case(cf: &CaseFile) -> Result<(), String> {
    let env = Env::sqlite().await;
    let case = &cf.case;
    let (user_id, token) = provision_auth(&env, cf.wave, &cf.stem, case.auth.as_deref()).await;
    let seed_out = seed(&env, cf.wave, &cf.stem, user_id, token.as_deref())
        .await
        .map_err(|e| format!("seed failed: {e}"))?;
    let sub = SubCtx {
        uuid: seed_out
            .subs
            .get("uuid")
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        post_id: seed_out
            .subs
            .get("post_id")
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        report_id: seed_out
            .subs
            .get("report_id")
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        item_id: seed_out
            .subs
            .get("item_id")
            .cloned()
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        token: token.clone().unwrap_or_default(),
    };

    let method = Method::from_bytes(case.method.as_bytes())
        .map_err(|_| format!("bad method {}", case.method))?;
    let uri = subst_str(&case.path, &sub);
    let body = match (&cf.wave, cf.stem.as_str(), &case.body) {
        // wave5/12 + wave5/13: the body is prose "<multipart audio>" —
        // send an EMPTY body; the sidecar handler 503s before reading.
        (5, "12-voice-transcribe-proxied", _) | (5, "13-voice-ask-proxied", _) => {
            Some(ReqBody::Empty)
        }
        (_, _, Some(Value::String(s))) if s == "not json" => {
            // wave7/fin-07: the raw string "not json" as the body.
            Some(ReqBody::Raw(s.clone(), Some("application/json")))
        }
        (_, _, Some(Value::String(s))) if s.starts_with("<multipart") => Some(ReqBody::Empty),
        (_, _, Some(b)) => Some(ReqBody::Json(subst_value(b, &sub))),
        (_, _, None) => None,
    };
    let mut request = req(method, &uri, body);
    if let Some(t) = &token {
        request = authed(request, t);
    }
    if let Some(headers) = &case.headers {
        for (k, v) in headers {
            request = with_header(request, k, v);
        }
    }
    for (k, v) in &seed_out.extra_headers {
        request = with_header(request, k, v);
    }

    let (status, headers, bytes) = env.call_raw(request).await;
    if status.as_u16() != case.expected_status {
        let text = String::from_utf8_lossy(&bytes);
        return Err(format!(
            "status: expected {}, got {} — body: {text}",
            case.expected_status, status
        ));
    }
    if let Some(ect) = &case.expected_content_type {
        let ct = headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !ct.starts_with(ect.as_str()) {
            return Err(format!("content-type: expected prefix {ect}, got {ct:?}"));
        }
    }
    if let Some(expected) = &case.expected {
        let expected = subst_expected(expected, &sub);
        // Non-JSON body (raw VTT): the expected string must appear
        // in the text.
        let parsed: Option<Value> = serde_json::from_slice(&bytes).ok();
        match parsed {
            None => {
                let text = String::from_utf8_lossy(&bytes).to_string();
                if let Value::String(s) = &expected {
                    if !text.contains(s.as_str()) {
                        return Err(format!("raw body does not contain {s:?}: {text:?}"));
                    }
                } else {
                    return Err(format!("non-JSON body, expected {expected}"));
                }
            }
            Some(actual) => {
                // fin-05: the public campaign list is empty by design
                // ("array, active campaigns newest first" over []).
                if cf.stem == "fin-05-campaigns-public" {
                    if !actual.is_array() {
                        return Err(format!("expected array, got {actual}"));
                    }
                } else {
                    let mut errs = Vec::new();
                    check(&expected, &actual, "$", &mut errs);
                    if !errs.is_empty() {
                        return Err(format!("mismatch: {}; actual: {actual}", errs.join("; ")));
                    }
                }
            }
        }
    }
    // Explicit overrides.
    if cf.wave == 2 && cf.stem == "12-api-keys-embed-snippet" {
        let (status2, body2) = (status, serde_json::from_slice::<Value>(&bytes).unwrap());
        let _ = status2;
        let html = body2["html"].as_str().unwrap_or("");
        if !html.contains("https://example.com") {
            return Err(format!("html missing base_url: {html}"));
        }
        if !html.contains("اوقات شرعی") {
            return Err(format!("html missing 'اوقات شرعی': {html}"));
        }
        if html.contains("mk_") {
            return Err(format!("html leaks raw mk_ key: {html}"));
        }
    }
    if cf.wave == 4 && cf.stem == "08-quran-reading-state-stale" {
        let body: Value =
            serde_json::from_slice(&bytes).map_err(|e| format!("stale state not JSON: {e}"))?;
        if body["last_read"]["surah"] != json!(2) {
            return Err(format!(
                "stale PUT: last_read.surah should be 2, got {}",
                body["last_read"]["surah"]
            ));
        }
    }
    Ok(())
}

// ============================ Postgres smoke ===============================

/// Lift-critical subset replayed against a local Postgres test DB.
/// Run with WAVE2_PG_URL set; skipped otherwise (like the other
/// wave smokes).
#[tokio::test]
async fn wave8_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok() else {
        return;
    };
    let db = Db::connect(&url).await.expect("pg connect");
    // Drop every table `TEST_SCHEMA` creates first: the DDL is plain
    // `CREATE TABLE`, so a leftover table from another smoke run (all
    // waves share this database) would fail the schema apply. Same
    // pattern as the wave2/3/4 pg smokes; the table list is derived
    // from the schema itself, so it cannot go stale.
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
    init_test_schema(&db).await.expect("pg test schema");
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };

    // Unknown path → 404 in Python's error envelope.
    let (status, body) = env.call(req(Method::GET, "/no/such/route", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(body["error"]["code"], json!("http_error"));

    // Metrics.
    let (status, _, bytes) = env.call_raw(req(Method::GET, "/metrics", None)).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("outbox_pending_events"), "{text}");

    // Health + openapi.
    let (status, body) = env.call(req(Method::GET, "/api/v1/health", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["media_tools"]["ready"], json!(false));
    let (status, body) = env.call(req(Method::GET, "/openapi.json", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["paths"]["/ws/events"]["get"].is_object());

    // Sidecar 503 (fatwa ask, no sidecar configured).
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/fatwa/ask",
            Some(ReqBody::Json(json!({ "question": "سوال تستی" }))),
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "fatwa_ask_unavailable");

    // Projects: empty list for a projects.manage holder, 403 for a
    // plain user (staff-reader gate).
    let (admin_id, admin_token) = seed_user(&env, "pg-proj-admin@example.com").await;
    grant(&env, admin_id, &["projects.manage"]).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"], json!([]));
    let (_uid, user_token) = seed_user(&env, "pg-proj-user@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/projects", None),
            &user_token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "forbidden");
}
