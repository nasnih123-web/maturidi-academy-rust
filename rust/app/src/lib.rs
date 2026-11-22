//! Maturidi Academy API — Rust front door (Wave 1) + Wave 2 modules.
//!
//! Natively implements `GET /health` and `GET /ready` (Wave 1) plus the
//! Wave 2 modules — meta, activity, api_keys, users, auth — exactly like
//! the Python FastAPI server (`apps/api/app/main.py`). Every other
//! method/path is proxied to the original server (`ORIGIN_URL`).
//!
//! Kill switch (per module): `RUST_ANYTHING_UNIT_<NAME>=original`
//! (`META`, `ACTIVITY`, `API_KEYS`, `USERS`, `AUTH`, `HOME`, `CONTENT`,
//! `ARTICLES`, `FATWA`, `VIDEOS`, `VOICE`) un-registers that module's native routes, so
//! they proxy to the Python server again.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use axum::{
    body::Body,
    extract::State,
    http::{Request, Response, StatusCode},
    response::{IntoResponse, Json},
    routing::{delete, get, patch, post, put},
    Router,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::ratelimit::RateLimiter;

mod adhan;
mod audit;
mod auth_service;
mod authn;
mod captcha;
mod countries;
mod crypto;
mod db;
mod embeddings;
mod errors;
mod hifz_logic;
mod hifz_mutashabihat;
mod hijri;
mod jwt;
mod library_visibility;
mod localized;
mod mail;
mod quran_data;
mod quran_providers;
mod ratelimit;
mod routes_activity;
mod routes_api_keys;
mod routes_articles;
mod routes_auth;
mod routes_calculators;
mod routes_content;
mod routes_fatwa;
mod routes_hifz;
mod routes_home;
mod routes_library;
mod routes_lms;
mod routes_meta;
mod routes_quran;
mod routes_users;
mod routes_videos;
mod routes_voice;
mod settings;
mod storage;
mod subtitles;
mod sync;
mod totp;
mod validate;
mod video_links;
mod voice_intents;
mod voice_tts;
mod workflow;

pub use db::{init_test_schema, Arg, Db, IntoArg, TEST_SCHEMA};
use errors::ApiError;
/// API-key guards (Wave 7 widget routes) — public so later waves and
/// integration tests can call them directly.
pub use routes_api_keys::{
    issue_embed_token, validate_embed_token, validate_key, verify_embed_token,
};
/// Settings encryption envelope — public so tests (and later admin waves)
/// can seed stored settings exactly as the app writes them.
pub use settings::encrypt_value;
/// Signed-URL builder — public so tests can mint valid download URLs
/// exactly as the app does.
pub use storage::presigned_url;

/// Runtime configuration, read from the environment.
///
/// Mirrors `app/core/config.py` where it applies: `APP_ENV` (default
/// `dev`), `DATABASE_URL` (no default — unset means the DB is
/// unavailable), `REDIS_URL` (default `redis://localhost:6379/0`),
/// `ORIGIN_URL` (default `http://127.0.0.1:8000`), `PORT` (8000), plus
/// the Wave 2 auth settings: `JWT_SECRET`, `ACCESS_TOKEN_EXPIRE_MINUTES`
/// (15), `ENCRYPTION_KEY` / `ENCRYPTION_KEY_VERSION` / `ENCRYPTION_KEYS`,
/// `STORAGE_SIGNING_SECRET` (+ `_PREVIOUS`), `APP_BASE_URL`,
/// `CAPTCHA_ENABLED` (true), `TURNSTILE_SITE_KEY` /
/// `TURNSTILE_SECRET_KEY`, `TRUSTED_PROXIES`, `STORAGE_PROVIDER`
/// (`local`), `STORAGE_LOCAL_DIR` (`./storage`),
/// `CLAMAV_ALLOW_INSECURE_DEV_BYPASS` (false).
#[derive(Debug, Clone)]
pub struct Config {
    pub app_env: String,
    pub database_url: Option<String>,
    pub redis_url: String,
    pub origin_url: String,
    pub port: u16,
    pub jwt_secret: String,
    pub access_token_expire_minutes: i64,
    pub encryption_key: String,
    pub encryption_key_version: i64,
    pub encryption_keys: String,
    pub storage_signing_secret: String,
    pub storage_signing_secret_previous: String,
    pub app_base_url: String,
    pub captcha_enabled: bool,
    pub turnstile_site_key: String,
    pub turnstile_secret_key: String,
    pub trusted_proxies: Vec<String>,
    pub storage_provider: String,
    pub storage_dir: String,
    pub clamav_allow_insecure_dev_bypass: bool,
}

fn env_str(name: &str, default: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "0" | "false" | "no" | "off" => false,
            _ => default,
        },
        Err(_) => default,
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            app_env: "dev".to_string(),
            database_url: None,
            redis_url: "redis://localhost:6379/0".to_string(),
            origin_url: "http://127.0.0.1:8000".to_string(),
            port: 8000,
            jwt_secret: "CHANGE_ME".to_string(),
            access_token_expire_minutes: 15,
            encryption_key: "CHANGE_ME".to_string(),
            encryption_key_version: 1,
            encryption_keys: String::new(),
            storage_signing_secret: String::new(),
            storage_signing_secret_previous: String::new(),
            app_base_url: "http://localhost:8000".to_string(),
            captcha_enabled: true,
            turnstile_site_key: String::new(),
            turnstile_secret_key: String::new(),
            trusted_proxies: Vec::new(),
            storage_provider: "local".to_string(),
            storage_dir: "./storage".to_string(),
            clamav_allow_insecure_dev_bypass: false,
        }
    }
}

impl Config {
    pub fn from_env() -> Self {
        let app_env = env_str("APP_ENV", "dev");
        let database_url = std::env::var("DATABASE_URL")
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let redis_url = env_str("REDIS_URL", "redis://localhost:6379/0");
        let origin_url = env_str("ORIGIN_URL", "http://127.0.0.1:8000");
        let port = std::env::var("PORT")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .unwrap_or(8000);
        let trusted_proxies = std::env::var("TRUSTED_PROXIES")
            .ok()
            .unwrap_or_default()
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect();
        Self {
            app_env,
            database_url,
            redis_url,
            origin_url,
            port,
            jwt_secret: env_str("JWT_SECRET", "CHANGE_ME"),
            access_token_expire_minutes: std::env::var("ACCESS_TOKEN_EXPIRE_MINUTES")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(15),
            encryption_key: env_str("ENCRYPTION_KEY", "CHANGE_ME"),
            encryption_key_version: std::env::var("ENCRYPTION_KEY_VERSION")
                .ok()
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(1),
            encryption_keys: std::env::var("ENCRYPTION_KEYS").ok().unwrap_or_default(),
            storage_signing_secret: std::env::var("STORAGE_SIGNING_SECRET")
                .ok()
                .unwrap_or_default(),
            storage_signing_secret_previous: std::env::var("STORAGE_SIGNING_SECRET_PREVIOUS")
                .ok()
                .unwrap_or_default(),
            app_base_url: env_str("APP_BASE_URL", "http://localhost:8000"),
            captcha_enabled: env_bool("CAPTCHA_ENABLED", true),
            turnstile_site_key: std::env::var("TURNSTILE_SITE_KEY").ok().unwrap_or_default(),
            turnstile_secret_key: std::env::var("TURNSTILE_SECRET_KEY")
                .ok()
                .unwrap_or_default(),
            trusted_proxies,
            storage_provider: env_str("STORAGE_PROVIDER", "local"),
            storage_dir: env_str("STORAGE_LOCAL_DIR", "./storage"),
            clamav_allow_insecure_dev_bypass: env_bool("CLAMAV_ALLOW_INSECURE_DEV_BYPASS", false),
        }
    }
}

/// Shared application state.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Config,
    pub client: reqwest::Client,
    pub db: Option<Db>,
    pub limiter: Arc<RateLimiter>,
    settings_version: Arc<AtomicU64>,
    api_key_usage: Arc<Mutex<HashMap<String, i64>>>,
}

impl AppState {
    /// Build state, connecting to the DB when `DATABASE_URL` is set.
    /// A failed connection is a warning, not a crash — native routes
    /// then answer 500, like Python's `get_db` failing (ADR-013).
    pub async fn new(config: Config) -> Self {
        let db = match &config.database_url {
            Some(url) => match Db::connect(url).await {
                Ok(db) => Some(db),
                Err(err) => {
                    tracing::warn!("database unavailable at startup: {err}");
                    None
                }
            },
            None => None,
        };
        Self::with_db(config, db)
    }

    /// Build state with an explicit DB handle (tests).
    pub fn with_db(config: Config, db: Option<Db>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        let limiter = Arc::new(RateLimiter::new(config.redis_url.clone()));
        Self {
            config,
            client,
            db,
            limiter,
            settings_version: Arc::new(AtomicU64::new(0)),
            api_key_usage: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The DB handle, or 500 when no database is configured/connected.
    pub fn db_or_500(&self) -> Result<&Db, ApiError> {
        self.db.as_ref().ok_or_else(ApiError::internal)
    }

    /// `get_settings_version()` — process-local counter, starts at 0.
    pub fn settings_version(&self) -> u64 {
        self.settings_version.load(Ordering::Relaxed)
    }

    /// `bump_settings_version()` — called after a settings write
    /// (voice settings update, Wave 5); the counter only ever grows.
    pub fn bump_settings_version(&self) {
        self.settings_version.fetch_add(1, Ordering::Relaxed);
    }

    /// Monthly API-key usage counter: `hit` returns the running total
    /// for `(key_hash, period)`, seeded from the stored `used_this_month`
    /// on first use in the period (mirrors `counter.hit`).
    pub async fn api_key_usage_hit(&self, key_hash: &str, period: &str, seed: i64) -> i64 {
        let mut map = self.api_key_usage.lock().await;
        let entry = map.entry(format!("{key_hash}:{period}")).or_insert(0);
        *entry += 1;
        seed + *entry
    }

    pub async fn api_key_usage_refund(&self, key_hash: &str, period: &str) {
        let mut map = self.api_key_usage.lock().await;
        if let Some(e) = map.get_mut(&format!("{key_hash}:{period}")) {
            *e = (*e - 1).max(0);
        }
    }
}

/// Kill switch: `RUST_ANYTHING_UNIT_<NAME>=original` disables the native
/// module (its routes fall through to the proxy).
fn unit_native(name: &str) -> bool {
    std::env::var(format!("RUST_ANYTHING_UNIT_{name}"))
        .map(|v| v.trim().to_lowercase() != "original")
        .unwrap_or(true)
}

/// Build the router: native `/health`, `/ready` and Wave 2 modules,
/// everything else proxied.
pub fn build_router(state: AppState) -> Router {
    let mut router = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready));

    if unit_native("HOME") {
        router = router
            .route("/api/v1/home/sections", get(routes_home::home_sections))
            .route("/api/v1/search", get(routes_home::search))
            .route(
                "/api/v1/admin/home/sections/{key}/data",
                put(routes_home::put_section_data),
            );
    }
    if unit_native("ARTICLES") {
        router = router
            .route("/api/v1/articles", get(routes_articles::public_list))
            .route(
                "/api/v1/articles/categories",
                get(routes_articles::public_categories),
            )
            .route(
                "/api/v1/articles/{slug}/meta",
                get(routes_articles::public_meta),
            )
            .route(
                "/api/v1/articles/{slug}",
                get(routes_articles::public_detail),
            )
            .route(
                "/api/v1/articles/{slug}/share",
                post(routes_articles::public_share),
            )
            .route(
                "/api/v1/admin/articles",
                get(routes_articles::admin_list).post(routes_articles::admin_create),
            )
            .route(
                "/api/v1/admin/articles/{article_id}",
                put(routes_articles::admin_update),
            )
            .route(
                "/api/v1/admin/articles/{article_id}/status",
                patch(routes_articles::admin_set_status),
            )
            .route(
                "/api/v1/admin/articles/categories",
                get(routes_articles::admin_category_list)
                    .post(routes_articles::admin_category_create),
            )
            .route(
                "/api/v1/admin/articles/categories/{category_id}",
                put(routes_articles::admin_category_update)
                    .delete(routes_articles::admin_category_delete),
            );
    }
    if unit_native("FATWA") {
        router = router
            // Public.
            .route("/api/v1/fatwa/categories", get(routes_fatwa::categories))
            .route("/api/v1/fatwa/topics", get(routes_fatwa::public_topic_list))
            .route(
                "/api/v1/fatwa/topics/categories",
                get(routes_fatwa::public_topic_categories),
            )
            .route(
                "/api/v1/fatwa/topics/{topic_id}",
                get(routes_fatwa::public_topic_detail),
            )
            .route(
                "/api/v1/fatwa/topics/{topic_id}/share",
                post(routes_fatwa::public_topic_share),
            )
            .route(
                "/api/v1/fatwa/compare/{topic_id}",
                get(routes_fatwa::compare),
            )
            .route(
                "/api/v1/fatwa/quiz/generate",
                post(routes_fatwa::quiz_generate),
            )
            .route("/api/v1/fatwa/quiz/submit", post(routes_fatwa::quiz_submit))
            .route("/api/v1/verify/check", post(routes_fatwa::verify_check))
            // Admin: chunks + ingestion.
            .route(
                "/api/v1/admin/fatwa/chunks",
                get(routes_fatwa::admin_chunk_list).post(routes_fatwa::admin_chunk_create),
            )
            .route(
                "/api/v1/admin/fatwa/chunks/{chunk_id}/status",
                patch(routes_fatwa::admin_chunk_set_status),
            )
            .route(
                "/api/v1/admin/fatwa/ingest",
                post(routes_fatwa::admin_ingest_trigger),
            )
            .route(
                "/api/v1/admin/fatwa/ingest/{run_id}",
                get(routes_fatwa::admin_ingest_status),
            )
            // Admin: topics + positions.
            .route(
                "/api/v1/admin/fatwa/topics",
                get(routes_fatwa::admin_topic_list).post(routes_fatwa::admin_topic_create),
            )
            .route(
                "/api/v1/admin/fatwa/topics/{topic_id}",
                put(routes_fatwa::admin_topic_update),
            )
            .route(
                "/api/v1/admin/fatwa/topics/{topic_id}/status",
                patch(routes_fatwa::admin_topic_set_status),
            )
            .route(
                "/api/v1/admin/fatwa/topics/{topic_id}/positions",
                post(routes_fatwa::admin_position_create),
            )
            .route(
                "/api/v1/admin/fatwa/positions",
                post(routes_fatwa::admin_position_create_flat),
            )
            .route(
                "/api/v1/admin/fatwa/positions/{position_id}/status",
                patch(routes_fatwa::admin_position_set_status),
            )
            .route(
                "/api/v1/admin/fatwa/quiz/questions",
                post(routes_fatwa::admin_quiz_question_create),
            )
            // Admin: categories.
            .route(
                "/api/v1/admin/fatwa/categories",
                get(routes_fatwa::admin_category_list).post(routes_fatwa::admin_category_create),
            )
            .route(
                "/api/v1/admin/fatwa/categories/{category_id}",
                put(routes_fatwa::admin_category_update)
                    .delete(routes_fatwa::admin_category_delete),
            )
            // Admin: trusted sources + audit.
            .route(
                "/api/v1/admin/fatwa/trusted-sources",
                get(routes_fatwa::admin_trusted_source_list)
                    .post(routes_fatwa::admin_trusted_source_create),
            )
            .route(
                "/api/v1/admin/fatwa/trusted-sources/{source_id}",
                patch(routes_fatwa::admin_trusted_source_update)
                    .delete(routes_fatwa::admin_trusted_source_delete),
            )
            .route(
                "/api/v1/admin/fatwa/trusted-sources/{source_id}/check",
                post(routes_fatwa::admin_trusted_source_check),
            )
            .route(
                "/api/v1/admin/fatwa/audit",
                get(routes_fatwa::admin_fatwa_audit),
            )
            // Submissions (author panel).
            .route(
                "/api/v1/fatwa/submissions",
                get(routes_fatwa::submission_list).post(routes_fatwa::submission_create),
            )
            .route(
                "/api/v1/fatwa/submissions/{submission_id}",
                get(routes_fatwa::submission_get),
            )
            .route(
                "/api/v1/fatwa/submissions/{submission_id}/submit",
                post(routes_fatwa::submission_submit),
            )
            .route(
                "/api/v1/fatwa/submissions/{submission_id}/approve",
                post(routes_fatwa::submission_approve),
            )
            .route(
                "/api/v1/fatwa/submissions/{submission_id}/publish",
                post(routes_fatwa::submission_publish),
            )
            .route(
                "/api/v1/fatwa/submissions/{submission_id}/reject",
                post(routes_fatwa::submission_reject),
            )
            .route(
                "/api/v1/fatwa/submissions/{submission_id}/add-to-rag",
                post(routes_fatwa::submission_add_to_rag),
            )
            // Clusters.
            .route(
                "/api/v1/fatwa/questions",
                post(routes_fatwa::submit_question),
            )
            .route("/api/v1/fatwa/clusters", get(routes_fatwa::cluster_list))
            .route(
                "/api/v1/fatwa/clusters/{cluster_id}",
                get(routes_fatwa::cluster_get),
            )
            .route(
                "/api/v1/fatwa/clusters/{cluster_id}/answer",
                post(routes_fatwa::cluster_answer),
            )
            // Saved fatwas (bookmarks).
            .route("/api/v1/fatwas/saved", get(routes_fatwa::saved_list))
            .route(
                "/api/v1/fatwas/{fatwa_id}/saved",
                get(routes_fatwa::saved_get)
                    .post(routes_fatwa::saved_save)
                    .delete(routes_fatwa::saved_unsave),
            );
    }
    if unit_native("CONTENT") {
        router = router
            // Control center (adapter registry is empty — see routes_content).
            .route("/api/v1/content/items", get(routes_content::content_items))
            .route(
                "/api/v1/content/items/{content_type}/{item_id}/preview",
                get(routes_content::content_preview),
            )
            .route(
                "/api/v1/content/policies/{content_type}/{item_id}",
                get(routes_content::content_policies),
            )
            // CMS admin (generic resource router).
            .route(
                "/api/v1/content/{resource}",
                get(routes_content::cms_list).post(routes_content::cms_create),
            )
            .route(
                "/api/v1/content/{resource}/{item_id}",
                get(routes_content::cms_get)
                    .patch(routes_content::cms_update)
                    .delete(routes_content::cms_delete),
            )
            .route(
                "/api/v1/content/{resource}/{item_id}/transition",
                post(routes_content::cms_transition),
            )
            // CMS public reads.
            .route(
                "/api/v1/content/public/nav",
                get(routes_content::public_nav),
            )
            .route(
                "/api/v1/content/public/banners",
                get(routes_content::public_banners),
            )
            .route(
                "/api/v1/content/public/announcements",
                get(routes_content::public_announcements),
            )
            .route(
                "/api/v1/content/public/pages",
                get(routes_content::public_pages),
            )
            .route(
                "/api/v1/content/public/pages/slug/{slug}",
                get(routes_content::public_page_slug),
            )
            .route(
                "/api/v1/content/public/events",
                get(routes_content::public_events),
            )
            .route(
                "/api/v1/content/public/quotes",
                get(routes_content::public_quotes),
            )
            .route(
                "/api/v1/content/public/calendar/theme",
                get(routes_content::public_calendar_theme),
            );
    }
    if unit_native("QURAN") {
        router = router
            .route("/api/v1/quran/surahs", get(routes_quran::list_surahs))
            .route("/api/v1/quran/surah/{number}", get(routes_quran::get_surah))
            .route(
                "/api/v1/quran/ayah/{surah}/{ayah}",
                get(routes_quran::get_ayah),
            )
            .route(
                "/api/v1/quran/recitation/reciters",
                get(routes_quran::list_recitation_reciters),
            )
            .route(
                "/api/v1/quran/recitation/timing/generated",
                get(routes_quran::get_generated_timing),
            )
            .route(
                "/api/v1/quran/recitation/timing",
                get(routes_quran::get_recitation_timing),
            )
            .route("/api/v1/quran/providers", get(routes_quran::get_providers))
            .route(
                "/api/v1/quran/registry/custom-providers/{provider_id}/key",
                put(routes_quran::set_custom_provider_key),
            )
            .route(
                "/api/v1/quran/munajat/status",
                get(routes_quran::munajat_status),
            )
            .route(
                "/api/v1/quran/notes",
                post(routes_quran::create_note).get(routes_quran::list_notes),
            )
            .route(
                "/api/v1/quran/notes/{note_id}",
                put(routes_quran::update_note).delete(routes_quran::delete_note),
            )
            .route(
                "/api/v1/quran/reading-state",
                get(routes_quran::get_reading_state).put(routes_quran::put_reading_state),
            )
            .route("/api/v1/tasbih/sessions", post(routes_quran::tasbih_start))
            .route(
                "/api/v1/tasbih/sessions/{session_id}/count",
                post(routes_quran::tasbih_count),
            )
            .route("/api/v1/tasbih/today", get(routes_quran::tasbih_today))
            .route("/api/v1/adhan/times", get(routes_quran::adhan_times))
            .route("/api/v1/adhan/range", get(routes_quran::adhan_range))
            .route("/api/v1/adhan/options", get(routes_quran::adhan_options))
            .route("/api/v1/adhan/geocode", get(routes_quran::adhan_geocode))
            .route("/api/v1/adhan/reverse", get(routes_quran::adhan_reverse));
    }
    if unit_native("HIFZ") {
        router = router
            .route("/api/v1/quran/hifz/status", get(routes_hifz::hifz_status))
            .route(
                "/api/v1/quran/hifz/attempts",
                post(routes_hifz::create_attempt).get(routes_hifz::list_attempts),
            )
            .route(
                "/api/v1/quran/hifz/attempts/resume",
                get(routes_hifz::resume_attempt_lookup),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}",
                get(routes_hifz::get_attempt).delete(routes_hifz::delete_attempt),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/complete",
                post(routes_hifz::complete_attempt),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/position",
                patch(routes_hifz::update_position),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/pause",
                post(routes_hifz::pause_attempt),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/resume",
                post(routes_hifz::resume_attempt),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/mistakes",
                get(routes_hifz::attempt_mistakes),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/peek",
                post(routes_hifz::peek_word),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/dispute",
                post(routes_hifz::dispute_word),
            )
            .route(
                "/api/v1/quran/hifz/attempts/{attempt_id}/stream",
                get(routes_hifz::hifz_stream_unavailable),
            )
            .route(
                "/api/v1/quran/hifz/mistakes/{mistake_id}",
                delete(routes_hifz::delete_mistake),
            )
            .route(
                "/api/v1/quran/hifz/settings",
                get(routes_hifz::get_settings).put(routes_hifz::put_settings),
            )
            .route(
                "/api/v1/quran/hifz/listen-position",
                get(routes_hifz::get_listen_position)
                    .put(routes_hifz::put_listen_position)
                    .delete(routes_hifz::delete_listen_position),
            )
            .route("/api/v1/quran/hifz/stats", get(routes_hifz::hifz_stats))
            .route("/api/v1/quran/hifz/review", get(routes_hifz::hifz_review))
            .route(
                "/api/v1/quran/hifz/admin/attempts",
                get(routes_hifz::admin_list_attempts),
            )
            .route(
                "/api/v1/quran/hifz/admin/attempts/{attempt_id}",
                delete(routes_hifz::admin_delete_attempt),
            )
            .route(
                "/api/v1/quran/hifz/reviews",
                post(routes_hifz::record_review),
            )
            .route(
                "/api/v1/quran/hifz/reviews/due",
                get(routes_hifz::due_reviews),
            )
            .route(
                "/api/v1/quran/hifz/plans",
                post(routes_hifz::create_plan).get(routes_hifz::list_plans_handler),
            )
            .route(
                "/api/v1/quran/hifz/plans/{plan_id}",
                get(routes_hifz::get_plan)
                    .patch(routes_hifz::set_plan_status)
                    .delete(routes_hifz::delete_plan),
            )
            .route(
                "/api/v1/quran/hifz/plans/{plan_id}/items/{item_id}",
                patch(routes_hifz::set_plan_item),
            )
            .route("/api/v1/quran/hifz/dashboard", get(routes_hifz::hifz_dashboard))
            .route("/api/v1/quran/hifz/activity", get(routes_hifz::hifz_activity))
            .route("/api/v1/quran/hifz/suggested", get(routes_hifz::hifz_suggested))
            .route(
                "/api/v1/quran/hifz/tests",
                post(routes_hifz::create_test).get(routes_hifz::list_tests),
            )
            .route(
                "/api/v1/quran/hifz/tests/{test_id}",
                get(routes_hifz::get_test).delete(routes_hifz::delete_test),
            )
            .route(
                "/api/v1/quran/hifz/tests/{test_id}/complete",
                post(routes_hifz::complete_test),
            )
            .route(
                "/api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/skip",
                post(routes_hifz::skip_test_question),
            )
            .route(
                "/api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/peek",
                post(routes_hifz::peek_test_question),
            )
            .route(
                "/api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/events/{event_index}/remove",
                post(routes_hifz::remove_test_event),
            )
            .route(
                "/api/v1/quran/hifz/mutashabihat",
                get(routes_hifz::mutashabihat),
            )
            .route(
                "/api/v1/quran/hifz/mutashabihat/phrase",
                get(routes_hifz::mutashabihat_phrase),
            )
            .route(
                "/api/v1/quran/hifz/mutashabihat/count",
                get(routes_hifz::mutashabihat_count),
            )
            .route(
                "/api/v1/quran/hifz/tasmi/sessions",
                post(routes_hifz::create_tasmi).get(routes_hifz::list_tasmi),
            )
            .route(
                "/api/v1/quran/hifz/tasmi/sessions/{session_id}",
                get(routes_hifz::get_tasmi)
                    .patch(routes_hifz::patch_tasmi)
                    .delete(routes_hifz::delete_tasmi),
            )
            .route(
                "/api/v1/quran/hifz/data/preview",
                get(routes_hifz::data_preview),
            )
            .route(
                "/api/v1/quran/hifz/data/purge",
                post(routes_hifz::data_purge),
            );
    }
    if unit_native("LIBRARY") {
        router = router
            .route("/api/v1/library/items", get(routes_library::public_list))
            .route("/api/v1/library/search", get(routes_library::search_items))
            .route(
                "/api/v1/library/items/{item_id}/stream",
                get(routes_library::stream_item),
            )
            .route(
                "/api/v1/library/items/{item_id}",
                get(routes_library::public_get),
            )
            .route(
                "/api/v1/library/download-by-ref",
                get(routes_library::download_by_ref),
            )
            .route(
                "/api/v1/admin/library/items",
                get(routes_library::admin_list).post(routes_library::admin_create),
            )
            .route(
                "/api/v1/admin/library/items/{item_id}",
                get(routes_library::admin_get)
                    .put(routes_library::admin_update)
                    .delete(routes_library::admin_delete),
            )
            .route(
                "/api/v1/admin/library/items/{item_id}/transition",
                post(routes_library::transition_item),
            )
            .route(
                "/api/v1/admin/library/items/{item_id}/versions",
                get(routes_library::list_versions),
            )
            .route(
                "/api/v1/admin/library/items/{item_id}/versions/{version}/revert",
                post(routes_library::revert_version),
            )
            .route(
                "/api/v1/admin/library/import",
                post(routes_library::import_item),
            )
            .route(
                "/api/v1/admin/library/items/{item_id}/reprocess",
                post(routes_library::reprocess_item),
            )
            .route(
                "/api/v1/admin/library/items/{item_id}/index-ai",
                post(routes_library::index_ai),
            );
    }
    if unit_native("VIDEOS") {
        router = router
            .route("/api/v1/videos", get(routes_videos::public_list))
            .route(
                "/api/v1/videos/playlists",
                get(routes_videos::public_playlist_list),
            )
            .route(
                "/api/v1/videos/playlists/{playlist_id}",
                get(routes_videos::public_playlist_get),
            )
            .route("/api/v1/videos/saved", get(routes_videos::saved_list))
            .route(
                "/api/v1/videos/{item_id}/saved",
                get(routes_videos::saved_get)
                    .post(routes_videos::saved_save)
                    .delete(routes_videos::saved_unsave),
            )
            .route("/api/v1/videos/{item_id}", get(routes_videos::watch_get))
            .route(
                // matchit allows only one capture per segment, so the
                // `.vtt` suffix is captured whole (`{lang_vtt}`) and
                // stripped in the handler — 404 for a non-`.vtt` name.
                "/api/v1/videos/{item_id}/subtitles/{lang_vtt}",
                get(routes_videos::subtitle_vtt_get),
            )
            .route(
                "/api/v1/videos/{item_id}/progress",
                get(routes_videos::progress_get).put(routes_videos::progress_put),
            )
            .route(
                "/api/v1/admin/videos/playlists",
                get(routes_videos::admin_playlist_list).post(routes_videos::admin_playlist_create),
            )
            .route(
                "/api/v1/admin/videos/playlists/{playlist_id}",
                patch(routes_videos::admin_playlist_update)
                    .delete(routes_videos::admin_playlist_delete),
            )
            .route(
                "/api/v1/admin/videos/playlists/{playlist_id}/items",
                post(routes_videos::admin_playlist_item_add),
            )
            .route(
                "/api/v1/admin/videos/playlists/{playlist_id}/items/{item_id}",
                delete(routes_videos::admin_playlist_item_remove),
            )
            .route(
                "/api/v1/admin/videos/playlists/{playlist_id}/items/order",
                put(routes_videos::admin_playlist_reorder),
            )
            .route(
                "/api/v1/admin/videos/subtitles",
                get(routes_videos::admin_subtitle_statuses),
            )
            .route(
                "/api/v1/admin/videos/{item_id}/subtitles/generate",
                post(routes_videos::admin_subtitle_generate),
            )
            .route(
                "/api/v1/admin/videos/{item_id}/subtitles/{lang}",
                get(routes_videos::admin_subtitle_cues_get)
                    .put(routes_videos::admin_subtitle_cues_put),
            )
            .route(
                "/api/v1/admin/videos/candidates",
                get(routes_videos::admin_candidates),
            );
    }
    if unit_native("LMS") {
        router = router
            .route("/api/v1/lms/courses", get(routes_lms::list_courses))
            .route(
                "/api/v1/lms/courses/{course_id}",
                get(routes_lms::get_course),
            )
            .route(
                "/api/v1/lms/lessons/{lesson_id}",
                get(routes_lms::get_lesson),
            )
            .route(
                "/api/v1/lms/lessons/{lesson_id}/progress",
                post(routes_lms::update_progress),
            )
            .route(
                "/api/v1/lms/quiz/submit",
                post(routes_lms::submit_lesson_quiz),
            )
            .route(
                "/api/v1/lms/quiz/{lesson_id}",
                post(routes_lms::generate_lesson_quiz),
            )
            .route("/api/v1/lms/reports/me", get(routes_lms::study_report))
            .route(
                "/api/v1/lms/recommendations",
                get(routes_lms::recommendations),
            )
            .route("/api/v1/lms/cohorts", get(routes_lms::list_cohorts))
            .route(
                "/api/v1/lms/cohorts/{cohort_id}/enroll",
                post(routes_lms::enroll_cohort),
            )
            .route(
                "/api/v1/lms/certificates/mine",
                get(routes_lms::my_certificate),
            )
            .route(
                "/api/v1/lms/certificates/{code}/download",
                get(routes_lms::download_certificate),
            )
            .route(
                "/api/v1/lms/certificates/download-by-ref",
                get(routes_lms::download_certificate_by_ref),
            )
            .route(
                "/api/v1/lms/certificates/{code}/verify",
                get(routes_lms::verify_certificate),
            )
            .route(
                "/api/v1/certificates/{code}/verify",
                get(routes_lms::verify_certificate),
            )
            .route(
                "/api/v1/admin/lms/curricula",
                post(routes_lms::admin_create_curriculum).get(routes_lms::admin_list_curricula),
            )
            .route(
                "/api/v1/admin/lms/curricula/{mid}",
                patch(routes_lms::admin_update_curriculum)
                    .delete(routes_lms::admin_delete_curriculum),
            )
            .route(
                "/api/v1/admin/lms/courses",
                post(routes_lms::admin_create_course).get(routes_lms::admin_list_courses),
            )
            .route(
                "/api/v1/admin/lms/courses/{mid}",
                patch(routes_lms::admin_update_course).delete(routes_lms::admin_delete_course),
            )
            .route(
                "/api/v1/admin/lms/courses/{course_id}/lessons",
                get(routes_lms::admin_list_lessons),
            )
            .route(
                "/api/v1/admin/lms/lessons",
                post(routes_lms::admin_create_lesson),
            )
            .route(
                "/api/v1/admin/lms/lessons/{mid}",
                patch(routes_lms::admin_update_lesson).delete(routes_lms::admin_delete_lesson),
            )
            .route(
                "/api/v1/admin/lms/cohorts",
                post(routes_lms::admin_create_cohort).get(routes_lms::admin_list_cohorts),
            )
            .route(
                "/api/v1/admin/lms/cohorts/{mid}",
                patch(routes_lms::admin_update_cohort).delete(routes_lms::admin_delete_cohort),
            )
            .route(
                "/api/v1/admin/lms/cohorts/{cohort_id}/enrollments",
                get(routes_lms::admin_list_cohort_enrollments),
            )
            .route(
                "/api/v1/admin/lms/prerequisites",
                post(routes_lms::admin_add_prerequisite)
                    .get(routes_lms::admin_list_prerequisites)
                    .delete(routes_lms::admin_remove_prerequisite),
            )
            .route(
                "/api/v1/admin/lms/users/{user_id}/progress",
                get(routes_lms::admin_user_progress),
            )
            .route(
                "/api/v1/admin/lms/certificates",
                get(routes_lms::admin_list_certificates),
            );
    }
    if unit_native("VOICE") {
        router = router
            .route(
                "/api/v1/voice/tts-audio-by-ref",
                get(routes_voice::tts_audio_by_ref),
            )
            .route(
                "/api/v1/admin/voice/settings",
                get(routes_voice::get_voice_settings)
                    .put(routes_voice::admin_update_voice_settings),
            )
            .route(
                "/api/v1/admin/voice/usage",
                get(routes_voice::list_voice_usage),
            );
    }
    if unit_native("CALCULATORS") {
        router = router
            // Public.
            .route(
                "/api/v1/calculators/inheritance",
                post(routes_calculators::inheritance_calculate),
            )
            .route(
                "/api/v1/calculators/inheritance/cases",
                post(routes_calculators::inheritance_calculate)
                    .get(routes_calculators::inheritance_cases_list),
            )
            .route(
                "/api/v1/calculators/zakat",
                post(routes_calculators::zakat_calculate),
            )
            .route(
                "/api/v1/calculators/zakat/nisab-rate",
                get(routes_calculators::nisab_rate),
            )
            .route(
                "/api/v1/calculators/zakat/extended",
                post(routes_calculators::zakat_extended),
            )
            .route(
                "/api/v1/calculators/inheritance/miras",
                post(routes_calculators::inheritance_miras),
            )
            .route(
                "/api/v1/calculators/mosaref",
                post(routes_calculators::mosaref_conjugate),
            )
            // Admin: rule configs + audit.
            .route(
                "/api/v1/admin/calculators/rules",
                get(routes_calculators::rule_list).post(routes_calculators::rule_create),
            )
            .route(
                "/api/v1/admin/calculators/rules/{rule_id}",
                get(routes_calculators::rule_get).patch(routes_calculators::rule_update),
            )
            .route(
                "/api/v1/admin/calculators/rules/{rule_id}/submit",
                post(routes_calculators::rule_submit),
            )
            .route(
                "/api/v1/admin/calculators/rules/{rule_id}/review",
                post(routes_calculators::rule_review),
            )
            .route(
                "/api/v1/admin/calculators/rules/{rule_id}/history",
                get(routes_calculators::rule_history),
            )
            .route(
                "/api/v1/admin/calculators/audit",
                get(routes_calculators::calculator_audit),
            );
    }
    if unit_native("META") {
        router = router
            .route("/api/v1/themes/active", get(routes_meta::themes_active))
            .route("/api/v1/settings/public", get(routes_meta::settings_public))
            .route("/api/v1/i18n/detect", get(routes_meta::i18n_detect));
    }
    if unit_native("ACTIVITY") {
        router = router.route(
            "/api/v1/activity/events",
            post(routes_activity::post_activity_event),
        );
    }
    if unit_native("API_KEYS") {
        router = router
            .route(
                "/api/v1/api-keys",
                post(routes_api_keys::create_key).get(routes_api_keys::list_keys),
            )
            .route(
                "/api/v1/api-keys/{key_id}",
                get(routes_api_keys::get_key).patch(routes_api_keys::update_key),
            )
            .route(
                "/api/v1/api-keys/{key_id}/revoke",
                post(routes_api_keys::revoke_key),
            )
            .route(
                "/api/v1/api-keys/{key_id}/usage",
                get(routes_api_keys::usage_stats),
            )
            .route(
                "/api/v1/api-keys/{key_id}/embed-snippet",
                get(routes_api_keys::embed_snippet),
            );
    }
    if unit_native("USERS") {
        router = router
            .route(
                "/api/v1/users/me",
                get(routes_users::get_me)
                    .patch(routes_users::update_me)
                    .delete(routes_users::delete_me),
            )
            .route("/api/v1/users/me/avatar", post(routes_users::upload_avatar))
            .route(
                "/api/v1/users/me/sessions",
                get(routes_users::list_sessions),
            )
            .route(
                "/api/v1/users/me/sessions/{session_id}",
                delete(routes_users::revoke_session),
            )
            .route("/api/v1/users/me/consent", get(routes_users::list_consents))
            .route("/api/v1/users/me/export", post(routes_users::export_data))
            .route("/api/v1/admin/users", get(routes_users::admin_list_users))
            .route(
                "/api/v1/users/me/devices",
                post(routes_users::register_device),
            )
            .route(
                "/api/v1/notifications/web-push/vapid-key",
                get(routes_users::vapid_key),
            )
            .route(
                "/api/v1/users/me/web-push/subscriptions",
                post(routes_users::subscribe_web_push).delete(routes_users::unsubscribe_web_push),
            )
            .route(
                "/api/v1/users/me/notifications",
                get(routes_users::list_my_notifications),
            )
            .route(
                "/api/v1/users/me/notifications/{notification_id}/read",
                post(routes_users::mark_notification_read),
            )
            .route(
                "/api/v1/users/avatar-by-ref",
                get(routes_users::avatar_by_ref),
            );
    }
    if unit_native("AUTH") {
        router = router
            .route("/api/v1/auth/register", post(routes_auth::register))
            .route("/api/v1/auth/login", post(routes_auth::login))
            .route("/api/v1/auth/refresh", post(routes_auth::refresh))
            .route("/api/v1/auth/logout", post(routes_auth::logout))
            .route("/api/v1/auth/logout-all", post(routes_auth::logout_all))
            .route("/api/v1/auth/verify-email", post(routes_auth::verify_email))
            .route(
                "/api/v1/auth/register/verify-email",
                post(routes_auth::register_verify_email),
            )
            .route(
                "/api/v1/auth/register/phone-code/request",
                post(routes_auth::register_phone_code_request),
            )
            .route(
                "/api/v1/auth/register/phone-code/verify",
                post(routes_auth::register_phone_code_verify),
            )
            .route(
                "/api/v1/auth/register/set-password",
                post(routes_auth::register_set_password),
            )
            .route(
                "/api/v1/auth/register/resend",
                post(routes_auth::register_resend),
            )
            .route(
                "/api/v1/auth/verify-phone/request",
                post(routes_auth::verify_phone_request),
            )
            .route("/api/v1/auth/verify-phone", post(routes_auth::verify_phone))
            .route(
                "/api/v1/auth/password-reset/request",
                post(routes_auth::password_reset_request),
            )
            .route(
                "/api/v1/auth/password-reset/confirm",
                post(routes_auth::password_reset_confirm),
            )
            .route("/api/v1/auth/2fa/setup", post(routes_auth::twofa_setup))
            .route("/api/v1/auth/2fa/enable", post(routes_auth::twofa_enable))
            .route("/api/v1/auth/2fa/recover", post(routes_auth::twofa_recover))
            .route("/api/v1/auth/2fa/status", get(routes_auth::twofa_status))
            .route("/api/v1/auth/2fa/disable", post(routes_auth::twofa_disable))
            .route(
                "/api/v1/auth/2fa/recovery/regenerate",
                post(routes_auth::twofa_regenerate),
            )
            .route("/api/v1/auth/ws-ticket", post(routes_auth::ws_ticket));
    }

    router.fallback(proxy).with_state(state)
}

/// Liveness, pure: the process is alive. Exactly `{"status":"ok","env":...}`,
/// like the Python `GET /health`.
async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({"status": "ok", "env": state.config.app_env}))
}

/// Readiness, mirroring the Python `GET /ready` exactly:
/// DB probed with `SELECT 1` and a 3-second timeout; Redis only reported
/// (`ok` / `unavailable`, 2-second PING timeout) and never fails readiness.
/// DB failure → 503 `not_ready`; DB ok → 200 `ready`.
async fn ready(State(state): State<AppState>) -> (StatusCode, Json<Value>) {
    let db_ok = probe_db(state.config.database_url.as_deref()).await;
    let redis_status = probe_redis(&state.config.redis_url).await;
    if !db_ok {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "status": "not_ready",
                "env": state.config.app_env,
                "db": "unavailable",
                "redis": redis_status,
            })),
        );
    }
    (
        StatusCode::OK,
        Json(json!({
            "status": "ready",
            "env": state.config.app_env,
            "db": "ok",
            "redis": redis_status,
        })),
    )
}

/// Probe the database with `SELECT 1` under a 3-second timeout.
/// Postgres via `PgPool`, `sqlite://` (and `sqlite+aiosqlite://`, the
/// Python dialect form) via a sqlite pool. Unset, empty or invalid URLs,
/// and any error or timeout, mean unavailable — never a panic.
async fn probe_db(database_url: Option<&str>) -> bool {
    let Some(url) = database_url.map(str::trim).filter(|u| !u.is_empty()) else {
        return false;
    };
    let check = async {
        if url.starts_with("sqlite") {
            let normalized = normalize_sqlite_url(url);
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&normalized)
                .await
                .ok()?;
            let ok = sqlx::query("SELECT 1").fetch_one(&pool).await.is_ok();
            pool.close().await;
            if ok {
                Some(())
            } else {
                None
            }
        } else if url.starts_with("postgres://") || url.starts_with("postgresql://") {
            let pool = sqlx::postgres::PgPoolOptions::new()
                .max_connections(1)
                .connect(url)
                .await
                .ok()?;
            let ok = sqlx::query("SELECT 1").fetch_one(&pool).await.is_ok();
            pool.close().await;
            if ok {
                Some(())
            } else {
                None
            }
        } else {
            None
        }
    };
    tokio::time::timeout(Duration::from_secs(3), check)
        .await
        .ok()
        .flatten()
        .is_some()
}

/// Normalize sqlite URL forms sqlx understands: `sqlite://:memory:` becomes
/// the canonical `sqlite::memory:`, and the Python SQLAlchemy form
/// `sqlite+aiosqlite://...` becomes plain `sqlite://...`.
fn normalize_sqlite_url(url: &str) -> String {
    if url == "sqlite://:memory:" {
        return "sqlite::memory:".to_string();
    }
    url.replace("sqlite+aiosqlite://", "sqlite://")
}

/// PING Redis under a 2-second timeout. Returns `"ok"` or `"unavailable"`.
async fn probe_redis(redis_url: &str) -> &'static str {
    let check = async {
        let client = redis::Client::open(redis_url).ok()?;
        let mut conn = client.get_multiplexed_async_connection().await.ok()?;
        let pong: String = redis::cmd("PING").query_async(&mut conn).await.ok()?;
        if pong == "PONG" {
            Some(())
        } else {
            None
        }
    };
    match tokio::time::timeout(Duration::from_secs(2), check).await {
        Ok(Some(())) => "ok",
        _ => "unavailable",
    }
}

/// Proxy fallback: forward method, path, query, headers (except `host`) and
/// body to the origin, and return its status, headers and body verbatim.
/// The origin is unreachable → 502 `bad_gateway` (ADR-013).
async fn proxy(State(state): State<AppState>, req: Request<Body>) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let origin = state.config.origin_url.trim_end_matches('/');
    let url = format!("{origin}{path_and_query}");

    let bytes = match axum::body::to_bytes(body, usize::MAX).await {
        Ok(b) => b,
        Err(_) => return bad_gateway(&state.config.app_env),
    };

    let mut request = state.client.request(parts.method.clone(), &url);
    for (name, value) in parts.headers.iter() {
        if name == http::header::HOST {
            continue;
        }
        request = request.header(name, value);
    }

    let upstream = match request.body(bytes).send().await {
        Ok(r) => r,
        Err(_) => return bad_gateway(&state.config.app_env),
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let upstream_bytes = match upstream.bytes().await {
        Ok(b) => b,
        Err(_) => return bad_gateway(&state.config.app_env),
    };

    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        // `content-length` is set by axum from the body; `transfer-encoding`
        // and `connection` are hop-by-hop and must not be forwarded.
        if name == http::header::CONTENT_LENGTH
            || name == http::header::TRANSFER_ENCODING
            || name == http::header::CONNECTION
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from(upstream_bytes))
        .unwrap_or_else(|_| bad_gateway(&state.config.app_env))
}

fn bad_gateway(env: &str) -> Response<Body> {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({"status": "bad_gateway", "env": env})),
    )
        .into_response()
}
