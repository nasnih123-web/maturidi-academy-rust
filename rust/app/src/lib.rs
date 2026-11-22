//! Maturidi Academy API — Rust front door (Wave 1) + Wave 2 modules.
//!
//! Natively implements `GET /health` and `GET /ready` (Wave 1) plus the
//! Wave 2 modules — meta, activity, api_keys, users, auth — exactly like
//! the Python FastAPI server (`apps/api/app/main.py`). Every other
//! method/path is proxied to the original server (`ORIGIN_URL`).
//!
//! Kill switch (per module): `RUST_ANYTHING_UNIT_<NAME>=original`
//! (`META`, `ACTIVITY`, `API_KEYS`, `USERS`, `AUTH`) un-registers that
//! module's native routes, so they proxy to the Python server again.

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
    routing::{delete, get, post},
    Router,
};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::ratelimit::RateLimiter;

mod audit;
mod auth_service;
mod authn;
mod captcha;
mod countries;
mod crypto;
mod db;
mod errors;
mod jwt;
mod mail;
mod ratelimit;
mod routes_activity;
mod routes_api_keys;
mod routes_auth;
mod routes_meta;
mod routes_users;
mod settings;
mod storage;
mod totp;
mod validate;

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
