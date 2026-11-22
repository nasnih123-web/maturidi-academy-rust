//! Wave 5 integration tests: videos (public list/playlists/watch,
//! saved, progress, subtitles, admin playlists/subtitles/candidates)
//! and voice (admin settings, usage log, signed TTS download).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot` —
//! no live origin, no production DB. The Postgres smoke test at the end
//! (`wave5_pg_smoke`, gated on `WAVE2_PG_URL`) replays the critical
//! path against a local Postgres test database.
//!
//! `POST /voice/transcribe` and `POST /voice/ask` are deliberately
//! NOT native (no STT in the Rust binary — ADR-046): they are
//! sidecar-contract routes (ADR-134), asserted here
//! (`voice_transcribe_sidecar_503`, `voice_ask_sidecar_503`).

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use chrono::{Duration, Utc};
use maturidi_api_rs::{
    build_router, init_test_schema, presigned_url, AppState, Config, Db, IntoArg,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::net::SocketAddr;
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        jwt_secret: "test-jwt-secret-for-wave5-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave5".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave5-storage-{}",
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

    /// Like `call`, but returns the raw body and headers (for the VTT
    /// and TTS-download routes, which answer outside the JSON envelope).
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

fn req_raw(method: Method, uri: &str, content_type: &str, bytes: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", content_type)
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(bytes))
        .unwrap()
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

/// Register a user with `library.manage` + `voice.manage`.
async fn admin_user(env: &Env, email: &str) -> (String, String) {
    let (uid, token, _) = register_user(env, email).await;
    grant(env, &uid, &["library.manage", "voice.manage"]).await;
    (uid, token)
}

// ============================ video seeds ===========================

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
    created_secs_ago: i64,
}

impl Default for ItemSeed {
    fn default() -> Self {
        Self {
            kind: "video",
            title_fa: "ویدیوی تست",
            workflow_status: "published",
            visibility: "public",
            external_url: None,
            media_key: Some("library/video/abc123.mp4"),
            cover_media_key: None,
            duration_seconds: Some(120),
            topic_tags: vec![],
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
            "INSERT INTO library_items (id, kind, title, description, transcript, \
             workflow_status, visibility, external_url, media_key, cover_media_key, \
             scholar_id, topic_tags, duration_seconds, published_at, created_at, updated_at) \
             VALUES (?, ?, ?, NULL, NULL, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                s.kind.into_arg(),
                json!({"fa": s.title_fa, "ar": s.title_fa, "en": s.title_fa}).into_arg(),
                s.workflow_status.into_arg(),
                s.visibility.into_arg(),
                s.external_url.into_arg(),
                s.media_key.into_arg(),
                s.cover_media_key.into_arg(),
                json!(s.topic_tags).into_arg(),
                s.duration_seconds.into_arg(),
                published_at.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_playlist(
    env: &Env,
    title_fa: &str,
    is_published: bool,
    sort_order: i64,
    created_secs_ago: i64,
) -> Uuid {
    let id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(created_secs_ago);
    env.db
        .execute(
            "INSERT INTO video_playlists (id, title, description, cover_media_key, scholar_id, \
             is_published, sort_order, created_at, updated_at) \
             VALUES (?, ?, NULL, NULL, NULL, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                json!({"fa": title_fa, "ar": title_fa, "en": title_fa}).into_arg(),
                is_published.into_arg(),
                sort_order.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

async fn seed_membership(env: &Env, playlist_id: Uuid, item_id: Uuid, position: i64) {
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO video_playlist_items (id, playlist_id, item_id, position, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                playlist_id.into_arg(),
                item_id.into_arg(),
                position.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
}

async fn seed_subtitle(
    env: &Env,
    item_id: Uuid,
    lang: &str,
    status: &str,
    vtt: Option<&str>,
    error: Option<&str>,
) {
    let now = Utc::now();
    env.db
        .execute(
            "INSERT INTO item_subtitles (id, item_id, lang, status, vtt, error, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                item_id.into_arg(),
                lang.into_arg(),
                status.into_arg(),
                vtt.into_arg(),
                error.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
}

async fn seed_usage(
    env: &Env,
    intent: &str,
    lang: &str,
    stt_ms: Option<i32>,
    tts_provider: Option<&str>,
    created_secs_ago: i64,
) -> Uuid {
    let session_id = Uuid::new_v4();
    let now = Utc::now() - Duration::seconds(created_secs_ago);
    env.db
        .execute(
            "INSERT INTO voice_sessions (id, user_id, lang, created_at) VALUES (?, NULL, ?, ?)",
            &[session_id.into_arg(), lang.into_arg(), now.into_arg()],
        )
        .await
        .unwrap();
    let id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO voice_usage_log (id, session_id, intent, lang, stt_ms, tts_provider, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                session_id.into_arg(),
                intent.into_arg(),
                lang.into_arg(),
                stt_ms.into_arg(),
                tts_provider.into_arg(),
                now.into_arg(),
            ],
        )
        .await
        .unwrap();
    id
}

// ============================ videos: public list ===========================

#[tokio::test]
async fn videos_list_empty_and_visibility() {
    let env = Env::sqlite().await;
    let (status, body) = env.call(req(Method::GET, "/api/v1/videos", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"], json!([]));
    assert_eq!(body["next_cursor"], Value::Null);

    let pub_video = seed_item(
        &env,
        ItemSeed {
            title_fa: "عمومی",
            created_secs_ago: 30,
            ..Default::default()
        },
    )
    .await;
    let reg_video = seed_item(
        &env,
        ItemSeed {
            title_fa: "ثبت‌نامی",
            visibility: "registered",
            created_secs_ago: 20,
            ..Default::default()
        },
    )
    .await;
    let scholars_video = seed_item(
        &env,
        ItemSeed {
            title_fa: "علما",
            visibility: "scholars_only",
            created_secs_ago: 10,
            ..Default::default()
        },
    )
    .await;
    seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            created_secs_ago: 5,
            ..Default::default()
        },
    )
    .await;
    seed_item(
        &env,
        ItemSeed {
            kind: "pdf",
            title_fa: "کتاب",
            created_secs_ago: 1,
            ..Default::default()
        },
    )
    .await;

    // Guest: only the public video.
    let (status, body) = env.call(req(Method::GET, "/api/v1/videos", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![pub_video.to_string()]);

    // Logged-in user: public + registered.
    let (_, token, _) = register_user(&env, "viewer@example.com").await;
    let (status, body) = env
        .call(authed(req(Method::GET, "/api/v1/videos", None), &token))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![reg_video.to_string(), pub_video.to_string()],
        "newest first: {body}"
    );

    // Privileged (library.manage): every published video, incl. scholars_only.
    let (_, admin_token) = admin_user(&env, "admin@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/videos", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec![
            scholars_video.to_string(),
            reg_video.to_string(),
            pub_video.to_string()
        ],
        "{body}"
    );
}

#[tokio::test]
async fn videos_list_cursor_pagination_and_bad_cursor() {
    let env = Env::sqlite().await;
    for i in 0..3 {
        seed_item(
            &env,
            ItemSeed {
                title_fa: match i {
                    0 => "اول",
                    1 => "دوم",
                    _ => "سوم",
                },
                created_secs_ago: (3 - i) * 10,
                ..Default::default()
            },
        )
        .await;
    }
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/videos?limit=2", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["items"][0]["title"]["fa"], json!("سوم"));
    assert_eq!(body["items"][1]["title"]["fa"], json!("دوم"));
    let cursor = body["next_cursor"].as_str().expect("cursor").to_string();
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos?limit=2&after={cursor}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["title"]["fa"], json!("اول"));
    assert_eq!(body["next_cursor"], Value::Null);

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/videos?after=garbage", None))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_cursor");
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/videos?limit=0", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
}

// ============================ videos: playlists ===========================

#[tokio::test]
async fn videos_playlists_public() {
    let env = Env::sqlite().await;
    let mut item_ids = Vec::new();
    for i in 0..5 {
        item_ids.push(
            seed_item(
                &env,
                ItemSeed {
                    title_fa: match i {
                        0 => "یک",
                        1 => "دو",
                        2 => "سه",
                        3 => "چهار",
                        _ => "پنج",
                    },
                    duration_seconds: Some(10 * (i + 1)),
                    created_secs_ago: 50 - i * 10,
                    ..Default::default()
                },
            )
            .await,
        );
    }
    // One draft item in the playlist: invisible, not counted.
    let draft = seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            duration_seconds: Some(999),
            ..Default::default()
        },
    )
    .await;
    let playlist = seed_playlist(&env, "مکتوبات", true, 1, 100).await;
    for (i, id) in item_ids.iter().enumerate() {
        seed_membership(&env, playlist, *id, i as i64).await;
    }
    seed_membership(&env, playlist, draft, 5).await;
    let hidden = seed_playlist(&env, "پنهان", false, 0, 90).await;

    let (status, body) = env
        .call(req(Method::GET, "/api/v1/videos/playlists", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let lists = body.as_array().unwrap();
    assert_eq!(lists.len(), 1, "{body}");
    assert_eq!(lists[0]["title"]["fa"], json!("مکتوبات"));
    assert_eq!(lists[0]["item_count"], json!(5));
    assert_eq!(lists[0]["total_duration_seconds"], json!(150));
    assert_eq!(
        lists[0]["items"].as_array().unwrap().len(),
        4,
        "max 4 in list"
    );

    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos/playlists/{playlist}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 5);
    assert_eq!(body["items"][0]["title"]["fa"], json!("یک"));

    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos/playlists/{hidden}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "playlist_not_found");
}

#[tokio::test]
async fn videos_watch_and_related() {
    let env = Env::sqlite().await;
    let target = seed_item(
        &env,
        ItemSeed {
            title_fa: "هدف",
            topic_tags: vec!["a", "b"],
            created_secs_ago: 40,
            ..Default::default()
        },
    )
    .await;
    let two_shared = seed_item(
        &env,
        ItemSeed {
            title_fa: "دو مشترک",
            topic_tags: vec!["a", "b"],
            created_secs_ago: 30,
            ..Default::default()
        },
    )
    .await;
    let one_shared = seed_item(
        &env,
        ItemSeed {
            title_fa: "یک مشترک",
            topic_tags: vec!["a", "c"],
            created_secs_ago: 20,
            ..Default::default()
        },
    )
    .await;
    let none_shared = seed_item(
        &env,
        ItemSeed {
            title_fa: "بی‌ربط",
            topic_tags: vec!["z"],
            created_secs_ago: 10,
            ..Default::default()
        },
    )
    .await;
    let playlist = seed_playlist(&env, "لیست", true, 0, 100).await;
    seed_membership(&env, playlist, target, 0).await;
    seed_membership(&env, playlist, two_shared, 1).await;
    seed_membership(&env, playlist, one_shared, 2).await;
    seed_subtitle(
        &env,
        target,
        "fa",
        "ready",
        Some("WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nسلام\n"),
        None,
    )
    .await;

    let (status, body) = env
        .call(req(Method::GET, &format!("/api/v1/videos/{target}"), None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["item"]["id"], json!(target.to_string()));
    let stream = body["stream_url"].as_str().expect("stream_url");
    assert!(
        stream.contains("/api/v1/library/download-by-ref"),
        "{stream}"
    );
    assert!(stream.contains("purpose=library"), "{stream}");
    assert!(stream.contains("library/video/abc123.mp4"), "{stream}");
    let related: Vec<&str> = body["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["title"]["fa"].as_str().unwrap())
        .collect();
    assert_eq!(related, vec!["دو مشترک", "یک مشترک", "بی‌ربط"], "{body}");
    assert_eq!(
        body["subtitles"],
        json!([{"lang": "fa", "url": format!("/api/v1/videos/{target}/subtitles/fa.vtt")}])
    );
    assert_eq!(body["playlist"]["position"], json!(1));
    assert_eq!(body["playlist"]["total"], json!(3));
    assert_eq!(body["playlist"]["prev_item_id"], Value::Null);
    assert_eq!(
        body["playlist"]["next_item_id"],
        json!(two_shared.to_string())
    );

    // Playlist context with ?playlist= — position 2 for the middle item.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos/{two_shared}?playlist={playlist}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["playlist"]["position"], json!(2));
    assert_eq!(body["playlist"]["prev_item_id"], json!(target.to_string()));
    assert_eq!(
        body["playlist"]["next_item_id"],
        json!(one_shared.to_string())
    );

    // Draft item → 404; youtube item → embed, no stream, no subtitles.
    let draft = seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            ..Default::default()
        },
    )
    .await;
    let (status, body) = env
        .call(req(Method::GET, &format!("/api/v1/videos/{draft}"), None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "item_not_found");

    let yt = seed_item(
        &env,
        ItemSeed {
            title_fa: "یوتیوب",
            external_url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            media_key: None,
            ..Default::default()
        },
    )
    .await;
    seed_subtitle(&env, yt, "fa", "ready", Some("WEBVTT\n"), None).await;
    let (status, body) = env
        .call(req(Method::GET, &format!("/api/v1/videos/{yt}"), None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["stream_url"], Value::Null);
    assert_eq!(
        body["item"]["embed_url"],
        json!("https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ")
    );
    assert_eq!(
        body["item"]["cover_url"],
        json!("https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg")
    );
    assert_eq!(body["subtitles"], json!([]), "embed items have no tracks");
    let _ = none_shared;
}

#[tokio::test]
async fn videos_saved_and_progress() {
    let env = Env::sqlite().await;
    let video = seed_item(&env, ItemSeed::default()).await;
    let pdf = seed_item(
        &env,
        ItemSeed {
            kind: "pdf",
            title_fa: "کتاب",
            ..Default::default()
        },
    )
    .await;
    let yt = seed_item(
        &env,
        ItemSeed {
            title_fa: "یوتیوب",
            external_url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            media_key: None,
            ..Default::default()
        },
    )
    .await;

    // Guest saved-state is false, never an error.
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos/{video}/saved"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(false));

    // Unauthenticated save → 401; route order: /saved is not an item id.
    let (status, _) = env
        .call(req(
            Method::POST,
            &format!("/api/v1/videos/{video}/saved"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/videos/saved", None))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (_, token, _) = register_user(&env, "viewer@example.com").await;
    // Save the video (idempotent) and the pdf (any kind may be saved).
    for id in [video, pdf] {
        let (status, body) = env
            .call(authed(
                req(Method::POST, &format!("/api/v1/videos/{id}/saved"), None),
                &token,
            ))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["saved"], json!(true));
    }
    let (status, body) = env
        .call(authed(
            req(Method::POST, &format!("/api/v1/videos/{video}/saved"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/videos/{video}/saved"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(true));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/videos/saved", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2);
    assert_eq!(body["next_cursor"], Value::Null);
    // Unsave is idempotent too.
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/videos/{video}/saved"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["saved"], json!(false));
    let (status, body) = env
        .call(authed(
            req(
                Method::DELETE,
                &format!("/api/v1/videos/{video}/saved"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Saving a missing item → 404.
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                &format!("/api/v1/videos/{}/saved", Uuid::new_v4()),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "item_not_found");

    // Progress: no row → nulls; PUT → stored; PUT without duration keeps it.
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/videos/{video}/progress"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["position_seconds"], Value::Null);
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/videos/{video}/progress"),
                Some(json!({"position_seconds": 42.5, "duration_seconds": 120.0})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["position_seconds"], json!(42.5));
    assert_eq!(body["duration_seconds"], json!(120.0));
    assert!(body["updated_at"].is_string());
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/videos/{video}/progress"),
                Some(json!({"position_seconds": 50.0})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["position_seconds"], json!(50.0));
    assert_eq!(
        body["duration_seconds"],
        json!(120.0),
        "duration kept: {body}"
    );
    // Negative position → 422; pdf → 422 not_a_video; youtube GET → nulls, PUT → 422.
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/videos/{video}/progress"),
                Some(json!({"position_seconds": -1})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/videos/{pdf}/progress"),
                Some(json!({"position_seconds": 1})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "not_a_video");
    assert_eq!(body["error"]["details"]["kind"], json!("pdf"));
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/videos/{yt}/progress"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["position_seconds"], Value::Null);
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/videos/{yt}/progress"),
                Some(json!({"position_seconds": 1})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "external_video");
}

#[tokio::test]
async fn videos_subtitle_vtt() {
    let env = Env::sqlite().await;
    let video = seed_item(&env, ItemSeed::default()).await;
    let vtt = "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nسلام\n";
    seed_subtitle(&env, video, "fa", "ready", Some(vtt), None).await;
    seed_subtitle(&env, video, "ar", "pending", None, None).await;
    let (status, headers, bytes) = env
        .call_raw(req(
            Method::GET,
            &format!("/api/v1/videos/{video}/subtitles/fa.vtt"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "text/vtt"
    );
    assert_eq!(String::from_utf8(bytes).unwrap(), vtt);
    // Pending track → 404; unknown lang → 404 (no allow-list here).
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos/{video}/subtitles/ar.vtt"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "subtitle_not_found");
    let (status, body) = env
        .call(req(
            Method::GET,
            &format!("/api/v1/videos/{video}/subtitles/xx.vtt"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

// ============================ videos: admin ===========================

#[tokio::test]
async fn videos_admin_playlists_crud() {
    let env = Env::sqlite().await;
    let v1 = seed_item(
        &env,
        ItemSeed {
            title_fa: "یک",
            ..Default::default()
        },
    )
    .await;
    let v2 = seed_item(
        &env,
        ItemSeed {
            title_fa: "دو",
            ..Default::default()
        },
    )
    .await;
    let pdf = seed_item(
        &env,
        ItemSeed {
            kind: "pdf",
            title_fa: "کتاب",
            ..Default::default()
        },
    )
    .await;

    // Without library.manage → 403.
    let (_, token, _) = register_user(&env, "viewer@example.com").await;
    let (status, _) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/videos/playlists", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (_, admin_token) = admin_user(&env, "admin@example.com").await;
    let call_admin = |method: Method, uri: String, body: Option<Value>| {
        authed(req(method, &uri, body), &admin_token)
    };

    // Create: defaults is_published=false, sort_order=0.
    let (status, body) = env
        .call(call_admin(
            Method::POST,
            "/api/v1/admin/videos/playlists".to_string(),
            Some(json!({"title": {"fa": "مکتوبات", "ar": "مکتوبات", "en": "Maktubat"}})),
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let playlist = body["id"].as_str().unwrap().to_string();
    assert_eq!(body["is_published"], json!(false));
    assert_eq!(body["sort_order"], json!(0));

    // Add items: append; the response is the playlist (AdminPlaylistOut).
    for (i, id) in [v1, v2].iter().enumerate() {
        let (status, body) = env
            .call(call_admin(
                Method::POST,
                format!("/api/v1/admin/videos/playlists/{playlist}/items"),
                Some(json!({"item_id": id.to_string()})),
            ))
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["items"].as_array().unwrap().len(), i + 1);
        assert_eq!(body["items"][i]["item_id"], json!(id.to_string()));
        assert_eq!(body["items"][i]["position"], json!(i));
    }
    let (status, body) = env
        .call(call_admin(
            Method::POST,
            format!("/api/v1/admin/videos/playlists/{playlist}/items"),
            Some(json!({"item_id": v1.to_string()})),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "already_in_playlist");
    let (status, body) = env
        .call(call_admin(
            Method::POST,
            format!("/api/v1/admin/videos/playlists/{playlist}/items"),
            Some(json!({"item_id": pdf.to_string()})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "not_a_video");

    // Reorder: exact set required.
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/playlists/{playlist}/items/order"),
            Some(json!({"item_ids": [v1.to_string()]})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "order_mismatch");
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/playlists/{playlist}/items/order"),
            Some(json!({"item_ids": [v2.to_string(), v1.to_string()]})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"][0]["item_id"], json!(v2.to_string()));
    assert_eq!(body["items"][1]["item_id"], json!(v1.to_string()));

    // PATCH: publish + rename.
    let (status, body) = env
        .call(call_admin(
            Method::PATCH,
            format!("/api/v1/admin/videos/playlists/{playlist}"),
            Some(json!({"title": {"fa": "مکتوبات جدید", "ar": "مکتوبات جدید", "en": "Maktubat"}, "is_published": true})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["fa"], json!("مکتوبات جدید"));
    assert_eq!(body["is_published"], json!(true));
    // PATCH with no fields → 200 no-op (Python applies whatever is present).
    let (status, body) = env
        .call(call_admin(
            Method::PATCH,
            format!("/api/v1/admin/videos/playlists/{playlist}"),
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["title"]["fa"], json!("مکتوبات جدید"));

    // Remove one item, then delete the playlist (returns the snapshot).
    let (status, body) = env
        .call(call_admin(
            Method::DELETE,
            format!("/api/v1/admin/videos/playlists/{playlist}/items/{v2}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["item_id"], json!(v1.to_string()));
    assert_eq!(body["items"][0]["position"], json!(0));
    let (status, body) = env
        .call(call_admin(
            Method::DELETE,
            format!("/api/v1/admin/videos/playlists/{playlist}"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], json!(playlist));
    let (status, body) = env
        .call(call_admin(
            Method::GET,
            "/api/v1/admin/videos/playlists".to_string(),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn videos_admin_subtitles() {
    let env = Env::sqlite().await;
    let upload = seed_item(
        &env,
        ItemSeed {
            title_fa: "آپلودی",
            created_secs_ago: 30,
            ..Default::default()
        },
    )
    .await;
    let direct = seed_item(
        &env,
        ItemSeed {
            title_fa: "مستقیم",
            external_url: Some("https://example.com/v.mp4"),
            media_key: None,
            created_secs_ago: 20,
            ..Default::default()
        },
    )
    .await;
    let yt = seed_item(
        &env,
        ItemSeed {
            title_fa: "یوتیوب",
            external_url: Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            media_key: None,
            created_secs_ago: 10,
            ..Default::default()
        },
    )
    .await;
    let draft = seed_item(
        &env,
        ItemSeed {
            title_fa: "پیش‌نویس",
            workflow_status: "draft",
            created_secs_ago: 5,
            ..Default::default()
        },
    )
    .await;
    seed_subtitle(&env, upload, "fa", "failed", None, Some("ترجمه انجام نشد")).await;
    let (_, admin_token) = admin_user(&env, "admin@example.com").await;
    let call_admin = |method: Method, uri: String, body: Option<Value>| {
        authed(req(method, &uri, body), &admin_token)
    };

    // Status list: upload + direct + draft (no workflow filter), no youtube.
    let (status, body) = env
        .call(call_admin(
            Method::GET,
            "/api/v1/admin/videos/subtitles".to_string(),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 3, "{body}");
    assert_eq!(items[0]["id"], json!(draft.to_string()), "newest first");
    let up = items
        .iter()
        .find(|i| i["id"] == json!(upload.to_string()))
        .unwrap();
    assert_eq!(up["subtitles"]["fa"], json!("failed"));
    assert_eq!(up["subtitles"]["ar"], json!("missing"));
    assert_eq!(up["subtitles"]["dari"], json!("missing"));
    assert_eq!(up["errors"]["fa"], json!("ترجمه انجام نشد"));

    // Generate: youtube → 422 external_video; no-media → 422 no_media_file.
    let (status, body) = env
        .call(call_admin(
            Method::POST,
            format!("/api/v1/admin/videos/{yt}/subtitles/generate"),
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "external_video");
    let no_media = seed_item(
        &env,
        ItemSeed {
            title_fa: "بی‌فایل",
            media_key: None,
            ..Default::default()
        },
    )
    .await;
    let (status, body) = env
        .call(call_admin(
            Method::POST,
            format!("/api/v1/admin/videos/{no_media}/subtitles/generate"),
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "no_media_file");
    // Generate on the upload video: 202, pending rows, enqueued=false.
    let (status, body) = env
        .call(call_admin(
            Method::POST,
            format!("/api/v1/admin/videos/{direct}/subtitles/generate"),
            Some(json!({})),
        ))
        .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    assert_eq!(body["enqueued"], json!(false));
    assert_eq!(body["statuses"]["fa"], json!("pending"));
    assert_eq!(body["statuses"]["dari"], json!("pending"));

    // Cues: unknown lang is checked BEFORE the item exists.
    let (status, body) = env
        .call(call_admin(
            Method::GET,
            format!("/api/v1/admin/videos/{}/subtitles/xx", Uuid::new_v4()),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "subtitle_lang_unknown");
    assert_eq!(body["error"]["details"]["lang"], json!("xx"));
    // Missing row → status missing, empty cues.
    let (status, body) = env
        .call(call_admin(
            Method::GET,
            format!("/api/v1/admin/videos/{upload}/subtitles/ar"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("missing"));
    assert_eq!(body["cues"], json!([]));
    assert_eq!(body["preview_url"], Value::Null, "upload item: no preview");
    let (status, body) = env
        .call(call_admin(
            Method::GET,
            format!("/api/v1/admin/videos/{direct}/subtitles/ar"),
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("pending"));
    assert_eq!(
        body["preview_url"],
        json!("https://example.com/v.mp4"),
        "direct-file item: preview_url is the file URL"
    );
    // PUT while pending → 409.
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/{direct}/subtitles/ar"),
            Some(json!({"cues": [{"start": 0.0, "end": 1.0, "text": "سلام"}]})),
        ))
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(err_code(&body), "subtitle_generation_in_progress");
    // PUT cues on the upload video (failed row): sorts, stores VTT, ready.
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/{upload}/subtitles/fa"),
            Some(json!({"cues": [
                {"start": 2.0, "end": 3.0, "text": "دوم"},
                {"start": 0.0, "end": 1.0, "text": "اول"},
            ]})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["cues"][0]["text"], json!("اول"));
    assert_eq!(body["cues"][1]["text"], json!("دوم"));
    // Validation: empty cues, empty text, bad range.
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/{upload}/subtitles/ar"),
            Some(json!({"cues": []})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "subtitle_cues_empty");
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/{upload}/subtitles/ar"),
            Some(json!({"cues": [{"start": 0.0, "end": 1.0, "text": "  "}]})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "subtitle_cue_empty_text");
    assert_eq!(body["error"]["details"]["index"], json!(0));
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            format!("/api/v1/admin/videos/{upload}/subtitles/ar"),
            Some(json!({"cues": [{"start": 2.0, "end": 1.0, "text": "x"}]})),
        ))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(err_code(&body), "subtitle_cue_range");
}

#[tokio::test]
async fn videos_candidates_search() {
    let env = Env::sqlite().await;
    let v1 = seed_item(
        &env,
        ItemSeed {
            title_fa: "مکتوبات امام ربانی",
            created_secs_ago: 30,
            ..Default::default()
        },
    )
    .await;
    seed_item(
        &env,
        ItemSeed {
            title_fa: "طریقه محمدیه",
            created_secs_ago: 20,
            ..Default::default()
        },
    )
    .await;
    let draft = seed_item(
        &env,
        ItemSeed {
            title_fa: "مکتوبات پیش‌نویس",
            workflow_status: "draft",
            created_secs_ago: 10,
            ..Default::default()
        },
    )
    .await;
    let (_, admin_token) = admin_user(&env, "admin@example.com").await;
    // q=mکتوبات matches the published one AND the draft (all statuses).
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                "/api/v1/admin/videos/candidates?q=%D9%85%DA%A9%D8%AA%D9%88%D8%A8%D8%A7%D8%AA",
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![draft.to_string(), v1.to_string()], "{body}");
    // No q → all, newest first, limit applies.
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/videos/candidates?limit=2", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body.as_array().unwrap().len(), 2);
}

// ============================ voice: settings ===========================

#[tokio::test]
async fn voice_settings_get_put() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "viewer@example.com").await;
    let (status, _) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, admin_token) = admin_user(&env, "admin@example.com").await;
    let call_admin = |method: Method, uri: &str, body: Option<Value>| {
        authed(req(method, uri, body), &admin_token)
    };

    let (status, body) = env
        .call(call_admin(
            Method::GET,
            "/api/v1/admin/voice/settings",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["settings"]["voice.stt_provider"], json!("auto"));
    assert_eq!(body["settings"]["voice.max_audio_seconds"], json!(120));
    assert_eq!(body["settings"]["voice.tts_cloud_endpoint"], json!(""));
    // (The route returns only `settings` — no `defaults` object.)

    // Unknown key → 400; bad provider → 400 with allowed list; int out of range → 400.
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            "/api/v1/admin/voice/settings",
            Some(json!({"settings": {"voice.bogus": "x"}})),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "unknown_setting");
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            "/api/v1/admin/voice/settings",
            Some(json!({"settings": {"voice.stt_provider": "whisper"}})),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_setting_value");
    assert_eq!(
        body["error"]["details"]["allowed"],
        json!(["auto", "faster_whisper", "none"])
    );
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            "/api/v1/admin/voice/settings",
            Some(json!({"settings": {"voice.max_audio_seconds": 0}})),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_setting_value");

    // Valid patch: stored raw — the int comes back as the string "60"
    // (voice keys are not in Python's SETTING_DEFS, ADR-049).
    let (status, body) = env
        .call(call_admin(
            Method::PUT,
            "/api/v1/admin/voice/settings",
            Some(json!({"settings": {
                "voice.stt_provider": "faster_whisper",
                "voice.max_audio_seconds": 60,
            }})),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["updated"],
        json!(["voice.stt_provider", "voice.max_audio_seconds"])
    );
    let (status, body) = env
        .call(call_admin(
            Method::GET,
            "/api/v1/admin/voice/settings",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["settings"]["voice.stt_provider"],
        json!("faster_whisper")
    );
    assert_eq!(body["settings"]["voice.max_audio_seconds"], json!("60"));
    // The settings version bumps with the update.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/settings/public", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["version"], json!(1));
}

#[tokio::test]
async fn voice_usage_log_and_invalid_intent() {
    let env = Env::sqlite().await;
    let (_, admin_token) = admin_user(&env, "admin@example.com").await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/usage", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"], json!([]));
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/usage?intent=bogus", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(err_code(&body), "invalid_intent");

    seed_usage(&env, "quran", "fa", Some(1234), Some("cloud"), 30).await;
    seed_usage(&env, "fatwa", "ar", None, None, 20).await;
    seed_usage(&env, "general", "fa", Some(500), Some("coqui"), 10).await;
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/usage?limit=2", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["intent"], json!("general"));
    assert_eq!(items[1]["intent"], json!("fatwa"));
    assert_eq!(items[1]["stt_ms"], json!(Value::Null));
    assert!(items[0]["created_at"].as_str().unwrap().contains('T'));
    let cursor = body["next_cursor"].as_str().expect("cursor").to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/admin/voice/usage?limit=2&cursor={cursor}"),
                None,
            ),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["intent"], json!("quran"));
    assert_eq!(body["next_cursor"], Value::Null);
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/usage?intent=quran", None),
            &admin_token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
}

// ============================ voice: TTS download ===========================

#[tokio::test]
async fn voice_tts_audio_by_ref() {
    let env = Env::sqlite().await;
    let session_id = Uuid::new_v4();
    let key = format!("voice/tts/{session_id}.mp3");
    let cfg = test_config();
    // Write the file where the app's local storage reads it.
    let path = format!("{}/{}", cfg.storage_dir, key);
    std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
    std::fs::write(&path, b"fake-mp3-bytes").unwrap();

    let url = presigned_url(
        &cfg,
        &key,
        300,
        "/api/v1/voice/tts-audio-by-ref",
        "voice-tts",
    )
    .expect("sign");
    // Use only the path+query part of the absolute URL.
    let uri = url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, headers, bytes) = env.call_raw(req(Method::GET, &uri, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "audio/mpeg"
    );
    assert_eq!(bytes, b"fake-mp3-bytes");

    // Wrong purpose → 403; bad key format → 403; missing file → 404.
    let bad_purpose =
        presigned_url(&cfg, &key, 300, "/api/v1/voice/tts-audio-by-ref", "library").expect("sign");
    let uri = bad_purpose.strip_prefix("http://localhost:8000").unwrap();
    let (status, body) = env.call(req(Method::GET, uri, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(err_code(&body), "invalid_signature");
    let bad_key = presigned_url(
        &cfg,
        "voice/tts/not-a-uuid.mp3",
        300,
        "/api/v1/voice/tts-audio-by-ref",
        "voice-tts",
    )
    .expect("sign");
    let uri = bad_key.strip_prefix("http://localhost:8000").unwrap();
    let (status, body) = env.call(req(Method::GET, uri, None)).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let missing = presigned_url(
        &cfg,
        &format!("voice/tts/{}.mp3", Uuid::new_v4()),
        300,
        "/api/v1/voice/tts-audio-by-ref",
        "voice-tts",
    )
    .expect("sign");
    let uri = missing.strip_prefix("http://localhost:8000").unwrap();
    let (status, body) = env.call(req(Method::GET, uri, None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    // Missing query params → 422.
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/voice/tts-audio-by-ref", None))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // .bin file → application/octet-stream.
    let bin_key = format!("voice/tts/{}.bin", Uuid::new_v4());
    let bin_path = format!("{}/{}", cfg.storage_dir, bin_key);
    std::fs::write(&bin_path, b"bin").unwrap();
    let bin_url = presigned_url(
        &cfg,
        &bin_key,
        300,
        "/api/v1/voice/tts-audio-by-ref",
        "voice-tts",
    )
    .expect("sign");
    let uri = bin_url
        .strip_prefix("http://localhost:8000")
        .unwrap()
        .to_string();
    let (status, headers, _) = env.call_raw(req(Method::GET, &uri, None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers.get("content-type").unwrap().to_str().unwrap(),
        "application/octet-stream"
    );
}

// ============================ voice: transcribe/ask proxied ===========================

fn multipart_audio() -> (String, Vec<u8>) {
    let boundary = "----wave5testboundary";
    let mut bytes = Vec::new();
    bytes.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    bytes.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.webm\"\r\n",
    );
    bytes.extend_from_slice(b"Content-Type: audio/webm\r\n\r\n");
    bytes.extend_from_slice(b"fake-audio-bytes");
    bytes.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), bytes)
}

#[tokio::test]
async fn voice_transcribe_sidecar_503() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "asker@example.com").await;
    let (ct, body) = multipart_audio();
    // Transcribe is NOT native (no STT in the Rust binary, ADR-046):
    // it is a sidecar-contract route (ADR-134), and with SIDECAR_URL
    // unset the sidecar answers 503 voice_stt_unavailable.
    let (status, resp) = env
        .call(authed(
            req_raw(Method::POST, "/api/v1/voice/transcribe", &ct, body),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{resp}");
    assert_eq!(err_code(&resp), "voice_stt_unavailable");
}

#[tokio::test]
async fn voice_ask_sidecar_503() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "asker@example.com").await;
    let (ct, body) = multipart_audio();
    let (status, resp) = env
        .call(authed(
            req_raw(Method::POST, "/api/v1/voice/ask", &ct, body),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{resp}");
    assert_eq!(err_code(&resp), "voice_stt_unavailable");
}

// ============================ Postgres smoke ===========================

/// Critical-path replay against a local Postgres test DB.
/// Run with WAVE2_PG_URL set; skipped otherwise (like the wave2/3/4 smokes).
/// Binaries must run sequentially — the smokes share one database.
#[tokio::test]
async fn wave5_pg_smoke() {
    let pg_url = match std::env::var("WAVE2_PG_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("wave5_pg_smoke: WAVE2_PG_URL unset, skipping");
            return;
        }
    };
    let db = Db::connect(&pg_url).await.expect("pg connect");
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
    init_test_schema(&db).await.expect("pg test schema");
    let state = AppState::with_db(test_config(), Some(db.clone()));
    let env = Env { state, db };

    let (uid, token, _) = register_user(&env, "pg@example.com").await;
    grant(&env, &uid, &["library.manage", "voice.manage"]).await;
    let video = seed_item(
        &env,
        ItemSeed {
            title_fa: "ویدیوی PG",
            topic_tags: vec!["a"],
            ..Default::default()
        },
    )
    .await;
    let playlist = seed_playlist(&env, "لیست PG", true, 0, 0).await;
    seed_membership(&env, playlist, video, 0).await;

    let (status, body) = env.call(req(Method::GET, "/api/v1/videos", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    let (status, body) = env
        .call(authed(
            req(Method::GET, &format!("/api/v1/videos/{video}"), None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["item"]["title"]["fa"], json!("ویدیوی PG"));
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                &format!("/api/v1/videos/{video}/progress"),
                Some(json!({"position_seconds": 7.5})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["position_seconds"], json!(7.5));
    let (status, body) = env
        .call(authed(
            req(
                Method::GET,
                &format!("/api/v1/videos/{video}/progress"),
                None,
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["position_seconds"], json!(7.5));

    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                "/api/v1/admin/videos/playlists",
                Some(json!({"title": {"fa": "جدید", "ar": "جدید", "en": "New"}, "is_published": true})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let new_playlist = body["id"].as_str().unwrap().to_string();
    let (status, body) = env
        .call(authed(
            req(
                Method::POST,
                format!("/api/v1/admin/videos/playlists/{new_playlist}/items").as_str(),
                Some(json!({"item_id": video.to_string()})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/settings", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["settings"]["voice.max_audio_seconds"], json!(120));
    let (status, body) = env
        .call(authed(
            req(
                Method::PUT,
                "/api/v1/admin/voice/settings",
                Some(json!({"settings": {"voice.max_audio_seconds": 90}})),
            ),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env
        .call(authed(
            req(Method::GET, "/api/v1/admin/voice/usage", None),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["items"], json!([]));
    // Transcribe is a sidecar-contract route on Postgres too:
    // SIDECAR_URL unset → 503 voice_stt_unavailable.
    let (ct, body_bytes) = multipart_audio();
    let (status, resp) = env
        .call(authed(
            req_raw(Method::POST, "/api/v1/voice/transcribe", &ct, body_bytes),
            &token,
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{resp}");
    assert_eq!(err_code(&resp), "voice_stt_unavailable");
}
