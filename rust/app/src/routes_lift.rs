//! Wave 8 lift — everything the old proxy fallback used to answer.
//!
//! The catch-all proxy is gone (ADR-133): unknown paths get Python's
//! native 404 envelope, and the surfaces that used to fall through to
//! the origin are now served here:
//!
//! - `GET /metrics` — native Prometheus exposition (ADR-136,
//!   supersedes ADR-127), with real request counters and outbox gauges
//!   read from `domain_events`.
//! - `GET /api/v1/health` — native, with an honest `media_tools`
//!   report: the Rust binary owns no media toolchain, so every tool
//!   reports unavailable (ADR-137, supersedes ADR-128).
//! - `GET /openapi.json` — minimal spec generated at startup from the
//!   built router's `Debug` form (ADR-139); `/docs` is not served, so
//!   it 404s exactly like Python in production (docs only in dev).
//! - `GET /static/{*path}` — uploaded custom fonts from the local
//!   static dir (`{storage_dir}/static`, where `upload_font` writes
//!   them); 404 when absent (ADR-140).
//! - `GET /ws/events` — native realtime WebSocket (ADR-135, supersedes
//!   ADR-126): ticket/JWT handshake auth, Origin check, channel
//!   subscriptions, heartbeat, resume replay, and a poller that fans
//!   out `domain_events` outbox rows to subscribers.
//! - The 8 sidecar-contract routes (ADR-134): explicit per-route
//!   forwarding to `SIDECAR_URL`, honest 503 with a route-specific
//!   code when the sidecar is unset or unreachable.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{header, HeaderMap, Request, StatusCode, Uri},
    middleware::Next,
    response::{IntoResponse, Json, Response},
    Router,
};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::{
    crypto::hash_token,
    db::{utc_now, Db, IntoArg},
    errors::ApiError,
    jwt::{decode_access_token, SCOPE_FULL},
    AppState, Config,
};

// ===========================================================================
// 404 / 405 — Python's error envelope (app/core/errors.py)
// ===========================================================================

/// The envelope Python returns for an unknown path: a
/// `StarletteHTTPException(404, "Not Found")` goes through the
/// `http_error` handler, so the body is
/// `{"error": {"code": "http_error", "message": "Not Found",
/// "details": {}}, "request_id": "<uuid>"}` — NOT FastAPI's default
/// `{"detail": "Not Found"}`. `request_id` is fresh per response,
/// like Python's when no request-id middleware set one.
fn http_error_envelope(status: StatusCode, message: &str) -> Response {
    (
        status,
        Json(json!({
            "error": {"code": "http_error", "message": message, "details": {}},
            "request_id": Uuid::new_v4().to_string(),
        })),
    )
        .into_response()
}

/// Native fallback for every path no route matches (replaces the old
/// `router.fallback(proxy)`).
pub async fn not_found() -> Response {
    http_error_envelope(StatusCode::NOT_FOUND, "Not Found")
}

/// A known path with the wrong method: Python answers 405 with the
/// same envelope (`"Method Not Allowed"`); axum's default is an empty
/// 405 body, so this keeps the parity.
pub async fn method_not_allowed() -> Response {
    http_error_envelope(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed")
}

// ===========================================================================
// Metrics — mirrors app/core/metrics.py exactly
// ===========================================================================

/// In-memory request counters + total duration, exactly like Python's
/// `_Metrics` (counters keyed by method + normalised path + status).
#[derive(Debug)]
pub struct Metrics {
    started_at: Instant,
    counts: Mutex<HashMap<(String, String, u16), u64>>,
    total_duration_ms: Mutex<f64>,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            counts: Mutex::new(HashMap::new()),
            total_duration_ms: Mutex::new(0.0),
        }
    }

    pub fn record(&self, method: &str, path: &str, status: u16, duration_ms: f64) {
        if let Ok(mut counts) = self.counts.lock() {
            *counts
                .entry((method.to_string(), normalize_path(path), status))
                .or_insert(0) += 1;
        }
        if let Ok(mut total) = self.total_duration_ms.lock() {
            *total += duration_ms;
        }
    }

    /// Python's `exposition()`, byte-for-byte in shape: same HELP/TYPE
    /// lines, same metric names, same float formatting (Python
    /// `str(round(x, n))` — Rust's `{:?}` on the rounded f64 prints
    /// the same shortest form, `0.0` included).
    pub fn exposition(&self, pending: i64, oldest_age_s: f64, dead: i64) -> String {
        let (entries, total, total_ms) = {
            let counts = self.counts.lock().unwrap_or_else(|e| e.into_inner());
            let mut entries: Vec<((String, String, u16), u64)> =
                counts.iter().map(|(k, v)| (k.clone(), *v)).collect();
            entries.sort();
            let total: u64 = counts.values().sum();
            let total_ms = *self
                .total_duration_ms
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (entries, total, total_ms)
        };
        let mut lines = vec![
            "# HELP http_requests_total Total HTTP requests".to_string(),
            "# TYPE http_requests_total counter".to_string(),
        ];
        for ((method, path, status), count) in entries {
            lines.push(format!(
                "http_requests_total{{method=\"{method}\",path=\"{path}\",status=\"{status}\"}} {count}"
            ));
        }
        let avg = if total > 0 {
            total_ms / total as f64
        } else {
            0.0
        };
        lines
            .push("# HELP http_request_duration_ms_avg Average request duration in ms".to_string());
        lines.push("# TYPE http_request_duration_ms_avg gauge".to_string());
        lines.push(format!("http_request_duration_ms_avg {}", py_float(avg, 2)));
        lines.push("# HELP process_uptime_seconds Process uptime in seconds".to_string());
        lines.push("# TYPE process_uptime_seconds gauge".to_string());
        lines.push(format!(
            "process_uptime_seconds {}",
            py_float(self.started_at.elapsed().as_secs_f64(), 1)
        ));
        lines.push("# HELP outbox_pending_events Domain events waiting for dispatch".to_string());
        lines.push("# TYPE outbox_pending_events gauge".to_string());
        lines.push(format!("outbox_pending_events {pending}"));
        lines.push(
            "# HELP outbox_oldest_pending_age_seconds Age of the oldest undispatched event"
                .to_string(),
        );
        lines.push("# TYPE outbox_oldest_pending_age_seconds gauge".to_string());
        lines.push(format!(
            "outbox_oldest_pending_age_seconds {}",
            py_float(oldest_age_s, 1)
        ));
        lines.push("# HELP outbox_dead_letter_events Events that exhausted retries".to_string());
        lines.push("# TYPE outbox_dead_letter_events gauge".to_string());
        lines.push(format!("outbox_dead_letter_events {dead}"));
        lines.join("\n") + "\n"
    }
}

/// Python `str(round(v, decimals))` for a float gauge.
fn py_float(v: f64, decimals: u32) -> String {
    let m = 10f64.powi(decimals as i32);
    let r = (v * m).round() / m;
    if r == 0.0 {
        return "0.0".to_string();
    }
    format!("{r:?}")
}

/// `normalize_path` from `app/core/metrics.py`: UUID and numeric
/// segments become `{id}`, so label cardinality stays bounded.
/// Python's numeric rule (`/\d+(?=/|$)`) is exactly "a segment that
/// is all digits", implemented here without a look-around (the Rust
/// `regex` crate has no look-around support).
pub fn normalize_path(path: &str) -> String {
    static UUID_RE: OnceLock<Regex> = OnceLock::new();
    let uuid = UUID_RE.get_or_init(|| {
        Regex::new(r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}")
            .expect("uuid regex")
    });
    let replaced = uuid.replace_all(path, "{id}");
    replaced
        .split('/')
        .map(|seg| {
            if !seg.is_empty() && seg.bytes().all(|b| b.is_ascii_digit()) {
                "{id}"
            } else {
                seg
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

/// Access-log middleware: records every request (method, raw path,
/// status, duration) into the shared counters, like Python's
/// `AccessLogMiddleware` + `record_request`. Applied to the whole
/// router, so the fallback 404s are counted too.
pub async fn metrics_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let method = req.method().to_string();
    let path = req.uri().path().to_string();
    let start = Instant::now();
    let res = next.run(req).await;
    state.metrics.record(
        &method,
        &path,
        res.status().as_u16(),
        start.elapsed().as_secs_f64() * 1000.0,
    );
    res
}

/// Outbox gauges, mirroring `dispatcher._refresh_outbox_metrics`:
/// pending = `processed_at IS NULL AND failed_at IS NULL`, dead =
/// `failed_at IS NOT NULL`, oldest age from the oldest pending row.
/// No DB (or any query error) → all zeros, like Python's gauges
/// before the first dispatcher refresh — `/metrics` never 500s.
async fn outbox_gauges(state: &AppState) -> (i64, f64, i64) {
    let Some(db) = &state.db else {
        return (0, 0.0, 0);
    };
    let pending = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM domain_events WHERE processed_at IS NULL AND failed_at IS NULL",
            &[],
        )
        .await
        .ok()
        .and_then(|r| r.try_get::<i64, _>("n").ok())
        .unwrap_or(0);
    let dead = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM domain_events WHERE failed_at IS NOT NULL",
            &[],
        )
        .await
        .ok()
        .and_then(|r| r.try_get::<i64, _>("n").ok())
        .unwrap_or(0);
    let age = db
        .fetch_optional(
            "SELECT created_at FROM domain_events WHERE processed_at IS NULL AND failed_at IS NULL ORDER BY created_at ASC, event_id ASC LIMIT 1",
            &[],
        )
        .await
        .ok()
        .flatten()
        .and_then(|r| r.try_get::<DateTime<Utc>, _>("created_at").ok())
        .map(|dt| {
            (Utc::now() - dt).num_milliseconds() as f64 / 1000.0
        })
        .unwrap_or(0.0)
        .max(0.0);
    (pending, age, dead)
}

/// `GET /metrics` — `text/plain; version=0.0.4`, exactly like Python.
pub async fn metrics_handler(State(state): State<AppState>) -> Response {
    let (pending, age, dead) = outbox_gauges(&state).await;
    let body = state.metrics.exposition(pending, age, dead);
    Response::builder()
        .header(header::CONTENT_TYPE, "text/plain; version=0.0.4")
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"code": "internal_error", "message": "خطای داخلی سرور", "details": {}}})),
            )
                .into_response()
        })
}

// ===========================================================================
// /api/v1/health — mirrors main.py's health_v1
// ===========================================================================

/// `GET /api/v1/health` — `{status, env, api: "v1", media_tools}`.
/// The `media_tools` shape mirrors `media_tools_status()` exactly
/// (same keys, same `stt_engine`/`stt_model` defaults), but the values
/// are honest for this binary: it owns no media toolchain, so every
/// binary reports `null`, `faster_whisper` is `false`, `missing` lists
/// all five tools and `ready` is `false`. No tool is ever claimed
/// present.
pub async fn health_v1(State(state): State<AppState>) -> Json<Value> {
    let stt_model = std::env::var("FASTER_WHISPER_MODEL")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "small".to_string());
    Json(json!({
        "status": "ok",
        "env": state.config.app_env,
        "api": "v1",
        "media_tools": {
            "binaries": {
                "ffmpeg": Value::Null,
                "ffprobe": Value::Null,
                "tesseract": Value::Null,
                "pdftoppm": Value::Null,
            },
            "tesseract_langs": Value::Null,
            "faster_whisper": false,
            "stt_engine": "faster-whisper",
            "stt_model": stt_model,
            "missing": ["ffmpeg", "ffprobe", "tesseract", "pdftoppm", "faster-whisper"],
            "ready": false,
        },
    }))
}

// ===========================================================================
// /openapi.json — generated at startup from the built router
// ===========================================================================

/// Build the minimal spec from the router itself: `format!("{:?}",
/// router)` on axum 0.8 lists every registered route as
/// `RouteId(N): MethodRouter(MethodRouter { get: BoxedHandler, ... })`
/// plus `Node { paths: {RouteId(N): "/path"} }`, so the paths and
/// methods are read off the live router — never hand-written. The
/// fallback is not registered yet when this runs, so it is excluded.
/// If parsing ever yields nothing (axum internals changed), the spec
/// still serves with empty paths rather than failing the boot.
pub fn openapi_spec<S>(router: &Router<S>) -> String
where
    S: Clone + Send + Sync + 'static,
{
    let debug = format!("{router:?}");
    let mut entries = parse_router_debug(&debug);
    entries.sort();
    let mut paths = serde_json::Map::new();
    for (path, methods) in entries {
        // axum's `{*name}` wildcard has no OpenAPI equivalent; the
        // static mount is the only one, and `{path}` says the same.
        let openapi_path = path.replace("{*", "{");
        let mut methods_obj = serde_json::Map::new();
        for method in methods {
            methods_obj.insert(
                method.to_string(),
                json!({"responses": {"200": {"description": "OK"}}}),
            );
        }
        paths.insert(openapi_path, Value::Object(methods_obj));
    }
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Maturidi Academy API", "version": "0.2.0"},
        "paths": paths,
    })
    .to_string()
}

const OPENAPI_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "options", "trace", "connect",
];

/// Parse the `Router` Debug form described in `openapi_spec`.
/// Returns `(path, methods)` pairs; `head` is skipped (axum serves it
/// implicitly for every GET route, as FastAPI does).
fn parse_router_debug(debug: &str) -> Vec<(String, Vec<&'static str>)> {
    let Some(start) = debug.find("path_router: PathRouter {") else {
        return Vec::new();
    };
    let rest = &debug[start..];
    let Some(open) = rest.find('{') else {
        return Vec::new();
    };
    let Some(section) = extract_braced(rest, open) else {
        return Vec::new();
    };
    let Some(routes_open) = section.find("routes: {") else {
        return Vec::new();
    };
    let Some(routes_brace) = section[routes_open..].find('{') else {
        return Vec::new();
    };
    let Some(routes_map) = extract_braced(&section[routes_open..], routes_brace) else {
        return Vec::new();
    };
    let Some(paths_open) = section.find("paths: {") else {
        return Vec::new();
    };
    let Some(paths_brace) = section[paths_open..].find('{') else {
        return Vec::new();
    };
    let Some(paths_map) = extract_braced(&section[paths_open..], paths_brace) else {
        return Vec::new();
    };
    let method_re = static_method_re();
    let path_re = static_path_re();
    let mut methods_by_id: HashMap<u32, Vec<&'static str>> = HashMap::new();
    for cap in method_re.captures_iter(routes_map) {
        let Ok(id) = cap[1].parse::<u32>() else {
            continue;
        };
        let fields = &cap[2];
        let mut methods = Vec::new();
        for method in OPENAPI_METHODS {
            // A registered method shows as `BoxedHandler` (or
            // `Route(...)`); an unregistered one shows as `None`.
            if !fields.contains(&format!("{method}: None")) {
                methods.push(*method);
            }
        }
        methods_by_id.insert(id, methods);
    }
    let mut out = Vec::new();
    for cap in path_re.captures_iter(paths_map) {
        let Ok(id) = cap[1].parse::<u32>() else {
            continue;
        };
        if let Some(methods) = methods_by_id.remove(&id) {
            if !methods.is_empty() {
                out.push((cap[2].to_string(), methods));
            }
        }
    }
    out
}

fn static_method_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"RouteId\((\d+)\): MethodRouter\(MethodRouter \{([^}]*)\}\)")
            .expect("method router regex")
    })
}

fn static_path_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r#"RouteId\((\d+)\): "((?:[^"\\]|\\.)*)""#).expect("path regex"))
}

/// Return the `{...}` substring starting at `open_idx`, skipping
/// quoted strings (which may contain braces, e.g. `"{id}"`).
fn extract_braced(s: &str, open_idx: usize) -> Option<&str> {
    let bytes = s.as_bytes();
    if bytes.get(open_idx) != Some(&b'{') {
        return None;
    }
    let mut depth: i32 = 0;
    let mut in_str = false;
    let mut escaped = false;
    let mut i = open_idx;
    while i < bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else if c == b'"' {
            in_str = true;
        } else if c == b'{' {
            depth += 1;
        } else if c == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(&s[open_idx..=i]);
            }
        }
        i += 1;
    }
    None
}

/// `GET /openapi.json` — the generated spec. Python serves this in
/// every environment (`openapi_url` is never overridden, so it keeps
/// FastAPI's `/openapi.json` default); only `/docs` is dev-gated.
pub async fn openapi_handler(State(state): State<AppState>) -> Response {
    let body = state.openapi_json.as_ref().clone();
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": {"code": "internal_error", "message": "خطای داخلی سرور", "details": {}}})),
            )
                .into_response()
        })
}

// ===========================================================================
// /static — uploaded custom fonts (mirrors the StaticFiles mount)
// ===========================================================================

/// `GET /static/{*path}` — serves the files Python's
/// `app.mount("/static", StaticFiles(...))` serves: uploaded custom
/// fonts land under `{storage_dir}/static/fonts/<locale>/` (that is
/// where the Rust `upload_font` writes them via `put_object`, key
/// `static/fonts/...`). Anything else in the static dir serves the
/// same way; a missing file (or a traversal attempt) is a 404 in
/// Python's envelope.
pub async fn static_file(State(state): State<AppState>, Path(path): Path<String>) -> Response {
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return not_found().await;
        }
    }
    // Uploaded fonts live under `static/` in the configured storage
    // (local disk or S3, ADR-162) — read through the storage
    // abstraction, never straight from disk.
    let bytes = match crate::storage::read_bytes(&state.config, &format!("static/{path}")).await {
        Ok(b) => b,
        Err(_) => return not_found().await,
    };
    let content_type = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_lowercase)
        .as_deref()
        .map(|ext| match ext {
            "woff2" => "font/woff2",
            "woff" => "font/woff",
            "ttf" => "font/ttf",
            "otf" => "font/otf",
            "eot" => "application/vnd.ms-fontobject",
            "css" => "text/css",
            "js" => "text/javascript",
            "json" => "application/json",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "svg" => "image/svg+xml",
            "html" => "text/html",
            "txt" => "text/plain",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        })
        .unwrap_or("application/octet-stream");
    Response::builder()
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(bytes))
        .unwrap_or_else(|_| not_found_response())
}

fn not_found_response() -> Response {
    http_error_envelope(StatusCode::NOT_FOUND, "Not Found")
}

// ===========================================================================
// Sidecar contract — the 8 routes that need the Python toolchain
// (ADR-134, hardened in ADR-159; full contract in
// `rust/app-cases/wave9/sidecar-contract.md`)
// ===========================================================================

/// One sidecar route's contract: the 503 code when the sidecar is
/// unset/unreachable/timed-out, the timeout bounding the whole
/// upstream call, the request-body cap, and the error when the cap is
/// exceeded (the Python-equivalent code, answered before any byte
/// goes upstream).
struct SidecarRoute {
    code: &'static str,
    timeout_secs: u64,
    body_cap: usize,
    over_cap_status: StatusCode,
    over_cap_code: &'static str,
    over_cap_message: &'static str,
}

const AUDIO_CAP: usize = 32 * 1024 * 1024; // Python MAX_AUDIO_BYTES

const SIDECAR_FATWA_ASK: SidecarRoute = SidecarRoute {
    code: "fatwa_ask_unavailable",
    timeout_secs: 300,
    body_cap: 1024 * 1024,
    over_cap_status: StatusCode::PAYLOAD_TOO_LARGE,
    over_cap_code: "request_too_large",
    over_cap_message: "درخواست بیش از حد بزرگ است",
};

const SIDECAR_RECEIPT_PDF: SidecarRoute = SidecarRoute {
    code: "receipt_pdf_unavailable",
    timeout_secs: 60,
    body_cap: 1024,
    over_cap_status: StatusCode::PAYLOAD_TOO_LARGE,
    over_cap_code: "request_too_large",
    over_cap_message: "درخواست بیش از حد بزرگ است",
};

const SIDECAR_HIFZ: SidecarRoute = SidecarRoute {
    code: "hifz_asr_unavailable",
    timeout_secs: 300,
    body_cap: AUDIO_CAP,
    over_cap_status: StatusCode::PAYLOAD_TOO_LARGE,
    over_cap_code: "hifz_audio_too_large",
    over_cap_message: "فایل صوتی بیش از حد بزرگ است",
};

const SIDECAR_VOICE: SidecarRoute = SidecarRoute {
    code: "voice_stt_unavailable",
    timeout_secs: 180,
    body_cap: AUDIO_CAP,
    over_cap_status: StatusCode::UNPROCESSABLE_ENTITY,
    over_cap_code: "audio_too_large",
    over_cap_message: "فایل صوتی بیش از حد بزرگ است",
};

/// The effective timeout for a route: `SIDECAR_TIMEOUT_SECS` (env)
/// overrides every route at once when set — an operations knob and
/// the knob the tests use to prove the timeout path without waiting
/// minutes. `0` (default) means the per-route value.
fn sidecar_timeout(config: &Config, route: &SidecarRoute) -> Duration {
    if config.sidecar_timeout_secs > 0 {
        Duration::from_secs(config.sidecar_timeout_secs)
    } else {
        Duration::from_secs(route.timeout_secs)
    }
}

/// Forward one request to the sidecar (`SIDECAR_URL` + the same
/// method/path/query/headers/body) and return its status, headers and
/// body — the old proxy's forwarding logic, renamed, applied ONLY to
/// these routes. Rules (ADR-159):
/// - the request body is read capped: over-cap answers with the
///   route's over-cap error before any upstream call;
/// - unset or unreachable sidecar, or a timeout, → honest 503 with
///   the route's code (never a 502, never a faked success);
/// - no retries: none of these POSTs is idempotent;
/// - the upstream body streams (`Body::from_stream`) instead of
///   being buffered, so SSE (`fatwa/ask?stream=true`) reaches the
///   client event-by-event.
async fn forward_to_sidecar(
    state: &AppState,
    req: Request<Body>,
    route: &SidecarRoute,
) -> Response<Body> {
    let (parts, body) = req.into_parts();
    let bytes = match read_capped(body, route.body_cap).await {
        Ok(b) => b,
        Err(CappedRead::OverCap) => {
            return ApiError::new(
                route.over_cap_status,
                route.over_cap_code,
                route.over_cap_message,
            )
            .into_response();
        }
        Err(CappedRead::ReadFailed) => return sidecar_unavailable(route.code),
    };
    let Some(base) = state.config.sidecar_url.as_deref() else {
        return sidecar_unavailable(route.code);
    };
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{path_and_query}", base.trim_end_matches('/'));
    let mut request = state
        .client
        .request(parts.method.clone(), &url)
        .timeout(sidecar_timeout(&state.config, route));
    for (name, value) in parts.headers.iter() {
        if name == header::HOST {
            continue;
        }
        request = request.header(name, value);
    }
    let upstream = match request.body(bytes).send().await {
        Ok(r) => r,
        Err(_) => return sidecar_unavailable(route.code),
    };
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut builder = Response::builder().status(status);
    for (name, value) in headers.iter() {
        if name == header::CONTENT_LENGTH
            || name == header::TRANSFER_ENCODING
            || name == header::CONNECTION
        {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
        .body(Body::from_stream(upstream.bytes_stream()))
        .unwrap_or_else(|_| sidecar_unavailable(route.code))
}

enum CappedRead {
    OverCap,
    ReadFailed,
}

/// Read a request body up to `cap` bytes; stop as soon as the cap is
/// exceeded instead of buffering an unbounded body.
async fn read_capped(body: Body, cap: usize) -> Result<bytes::Bytes, CappedRead> {
    use futures_util::StreamExt;
    let mut stream = body.into_data_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| CappedRead::ReadFailed)?;
        buf.extend_from_slice(&chunk);
        if buf.len() > cap {
            return Err(CappedRead::OverCap);
        }
    }
    Ok(bytes::Bytes::from(buf))
}

fn sidecar_unavailable(code: &'static str) -> Response<Body> {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        code,
        "سرویس جانبی در دسترس نیست",
    )
    .into_response()
}

pub async fn sidecar_fatwa_ask(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_FATWA_ASK).await
}

pub async fn sidecar_receipt_pdf(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_RECEIPT_PDF).await
}

pub async fn sidecar_hifz_grade(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_HIFZ).await
}

pub async fn sidecar_hifz_grade_word(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_HIFZ).await
}

pub async fn sidecar_hifz_test_answer(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_HIFZ).await
}

pub async fn sidecar_hifz_voice_search(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_HIFZ).await
}

pub async fn sidecar_voice_transcribe(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_VOICE).await
}

pub async fn sidecar_voice_ask(
    State(state): State<AppState>,
    req: Request<Body>,
) -> Response<Body> {
    forward_to_sidecar(&state, req, &SIDECAR_VOICE).await
}

// ===========================================================================
// /ws/events — native realtime WebSocket (mirrors app/core/realtime.py)
// ===========================================================================

/// One `domain_events` row, as delivered to subscribers.
#[derive(Debug, Clone, Serialize)]
pub struct WsEvent {
    pub event_id: Uuid,
    pub event_type: String,
    pub aggregate_type: String,
    pub aggregate_id: String,
    pub payload: Value,
    pub schema_version: i64,
    pub created_at: String,
}

/// A `domain_events` row with its parsed timestamp (for cursor
/// ordering); `event` is the wire form.
#[derive(Debug, Clone)]
struct StoredEvent {
    event: WsEvent,
    created_at: DateTime<Utc>,
}

/// Shared realtime hub: one broadcast channel fed by the outbox
/// poller; every live connection subscribes and filters locally.
#[derive(Debug)]
pub struct WsHub {
    pub sender: broadcast::Sender<WsEvent>,
    pub poller_started: AtomicBool,
}

impl WsHub {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(512);
        Self {
            sender,
            poller_started: AtomicBool::new(false),
        }
    }
}

const CLOSE_UNAUTHORIZED: u16 = 4401;
const CLOSE_HEARTBEAT_TIMEOUT: u16 = 4400;

/// Fetch the newest `limit` events, oldest-first. Reading them in one
/// `ORDER BY created_at DESC, event_id DESC` query and reversing keeps
/// the two-part cursor order `(created_at, event_id)` on both backends
/// without SQL-side timestamp comparison (sqlite stores the timestamps
/// as text).
async fn fetch_recent_events(db: &Db, limit: usize) -> Vec<StoredEvent> {
    let sql = format!(
        "SELECT event_id, event_type, aggregate_type, aggregate_id, payload, schema_version, created_at \
         FROM domain_events ORDER BY created_at DESC, event_id DESC LIMIT {limit}"
    );
    let rows = match db.fetch_all(&sql, &[]).await {
        Ok(r) => r,
        Err(e) => {
            // TEMPORARY diagnostic logging; revert after the CI log is read.
            eprintln!("[poller-diag] fetch_recent_events ERROR limit={limit}: {e:?}");
            return Vec::new();
        }
    };
    let row_count = rows.len();
    let mut events = Vec::new();
    let mut skipped = 0usize;
    for row in rows {
        let Ok(event_id) = row.try_get::<Uuid, _>("event_id") else {
            skipped += 1;
            continue;
        };
        let Ok(event_type) = row.try_get::<String, _>("event_type") else {
            skipped += 1;
            continue;
        };
        let Ok(aggregate_type) = row.try_get::<String, _>("aggregate_type") else {
            skipped += 1;
            continue;
        };
        let Ok(aggregate_id) = row.try_get::<String, _>("aggregate_id") else {
            skipped += 1;
            continue;
        };
        let Ok(created_at) = row.try_get::<DateTime<Utc>, _>("created_at") else {
            skipped += 1;
            continue;
        };
        let schema_version = row.try_get::<i32, _>("schema_version").unwrap_or(1) as i64;
        let payload = row.try_get::<Value, _>("payload").unwrap_or(Value::Null);
        events.push(StoredEvent {
            event: WsEvent {
                event_id,
                event_type,
                aggregate_type,
                aggregate_id,
                payload,
                schema_version,
                created_at: created_at.to_rfc3339(),
            },
            created_at,
        });
    }
    events.reverse();
    // TEMPORARY diagnostic logging; revert after the CI log is read.
    eprintln!(
        "[poller-diag] fetch_recent_events limit={limit} rows={row_count} parsed={} skipped={skipped}",
        events.len()
    );
    events
}

/// The outbox poller: one per `AppState` (guarded by
/// `poller_started`, so the test harness's repeated `build_router`
/// calls never fan an event out twice). It starts its watermark at
/// the newest row present at spawn, then polls every
/// `dispatcher_poll_interval` seconds for newer rows and broadcasts
/// them. `emit_event` is the only writer, so polling the outbox is
/// the whole delivery path — exactly the role Python's dispatcher
/// bus plays for `broadcast_event`.
pub fn spawn_realtime_poller(state: &AppState) {
    let Some(db) = state.db.clone() else {
        return;
    };
    if state.ws_hub.poller_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let hub = state.ws_hub.clone();
    let interval = Duration::from_secs_f64(state.config.dispatcher_poll_interval.max(0.05));
    tokio::spawn(async move {
        // TEMPORARY diagnostic logging; revert after the CI log is read.
        // All [poller-diag] lines in this task are temporary.
        let spawned_at = tokio::time::Instant::now();
        let mut watermark: Option<(DateTime<Utc>, Uuid)> = None;
        if let Some(first) = fetch_recent_events(&db, 1).await.into_iter().last() {
            watermark = Some((first.created_at, first.event.event_id));
        }
        eprintln!(
            "[poller-diag] first run after {:?}: watermark={watermark:?}",
            spawned_at.elapsed()
        );
        loop {
            tokio::time::sleep(interval).await;
            let fetched = fetch_recent_events(&db, 200).await;
            let fetched_count = fetched.len();
            let mut sent = 0usize;
            let mut skipped = 0usize;
            for stored in fetched {
                let key = (stored.created_at, stored.event.event_id);
                if let Some(wm) = watermark {
                    if key <= wm {
                        skipped += 1;
                        continue;
                    }
                }
                watermark = Some(key);
                let _ = hub.sender.send(stored.event);
                sent += 1;
            }
            eprintln!(
                "[poller-diag] tick: fetched={fetched_count} sent={sent} skipped={skipped} watermark={watermark:?}"
            );
        }
    });
}

/// Channel matching: exact, `prefix.*`, or `*` — verbatim from
/// `realtime.channel_matches`.
pub fn channel_matches(subscription: &str, event_type: &str) -> bool {
    if subscription == "*" || subscription == event_type {
        return true;
    }
    if subscription.ends_with(".*") {
        return event_type.starts_with(&subscription[..subscription.len() - 1]);
    }
    false
}

/// Delivery privacy: `user`-aggregate events go only to the owner or
/// a `users.manage` holder, even under a matching `user.*` / `*`
/// subscription — verbatim from `realtime._event_visible_to`.
fn event_visible(event: &WsEvent, user_id: Uuid, permissions: &HashSet<String>) -> bool {
    if event.aggregate_type == "user" && event.aggregate_id != user_id.to_string() {
        return permissions.contains("users.manage");
    }
    true
}

fn can_subscribe(identity: &WsIdentity, channel: &str) -> bool {
    if channel.starts_with("admin.") {
        return identity.permissions.contains("realtime.admin");
    }
    true
}

/// Origin check, verbatim from `realtime.check_ws_origin`: no Origin
/// (non-browser client) or `null` → allowed; empty CORS list →
/// allowed; otherwise only listed origins.
fn origin_allowed(config: &Config, headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    if origin.is_empty() || origin == "null" {
        return true;
    }
    let allowed: Vec<&str> = config
        .cors_allow_origins
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if allowed.is_empty() {
        return true;
    }
    allowed.contains(&origin)
}

#[derive(Debug, Clone)]
struct WsIdentity {
    user_id: Uuid,
    #[allow(dead_code)]
    session_id: Option<Uuid>,
    permissions: HashSet<String>,
}

/// Atomic one-time ticket redeem — verbatim from Python's
/// `redeem_ws_ticket_with_session`: unknown / used / expired ticket,
/// revoked or mismatched session, or inactive user all reject; the
/// redeem itself is a conditional `UPDATE ... WHERE used_at IS NULL`
/// and must affect exactly one row.
async fn redeem_ticket(db: &Db, ticket: &str) -> Option<(Uuid, Option<Uuid>)> {
    let row = db
        .fetch_optional(
            "SELECT id, user_id, session_id, expires_at, used_at FROM ws_tickets WHERE ticket_hash = ?",
            &[hash_token(ticket).into_arg()],
        )
        .await
        .ok()??;
    let id: Uuid = row.try_get("id").ok()?;
    let user_id: Uuid = row.try_get("user_id").ok()?;
    let session_id: Option<Uuid> = row.try_get("session_id").ok()?;
    let expires_at: DateTime<Utc> = row.try_get("expires_at").ok()?;
    let used_at: Option<DateTime<Utc>> = row.try_get("used_at").ok()?;
    if used_at.is_some() || expires_at <= Utc::now() {
        return None;
    }
    if let Some(sid) = session_id {
        let srow = db
            .fetch_optional(
                "SELECT user_id, revoked FROM sessions WHERE id = ?",
                &[(sid).into_arg()],
            )
            .await
            .ok()??;
        let s_user: Uuid = srow.try_get("user_id").ok()?;
        let revoked: bool = srow.try_get("revoked").ok()?;
        if revoked || s_user != user_id {
            return None;
        }
    }
    let changed = db
        .execute(
            "UPDATE ws_tickets SET used_at = ? WHERE id = ? AND used_at IS NULL",
            &[utc_now().into_arg(), id.into_arg()],
        )
        .await
        .ok()?;
    if changed != 1 {
        return None;
    }
    let urow = db
        .fetch_optional(
            "SELECT is_active FROM users WHERE id = ?",
            &[(user_id).into_arg()],
        )
        .await
        .ok()??;
    if !urow.try_get::<bool, _>("is_active").ok()? {
        return None;
    }
    Some((user_id, session_id))
}

/// JWT-bearer fallback — verbatim from realtime.py: header only
/// (`?token=` is never read), `scope` must be `full` (a `2fa_setup`
/// token is rejected), and a bound session must exist, be unrevoked,
/// and belong to the user.
async fn authenticate_jwt(db: &Db, jwt_secret: &str, token: &str) -> Option<(Uuid, Option<Uuid>)> {
    let claims = decode_access_token(jwt_secret, token).ok()?;
    if claims.scope != SCOPE_FULL {
        return None;
    }
    let user_id: Uuid = claims.sub.parse().ok()?;
    let urow = db
        .fetch_optional(
            "SELECT is_active FROM users WHERE id = ?",
            &[(user_id).into_arg()],
        )
        .await
        .ok()??;
    if !urow.try_get::<bool, _>("is_active").ok()? {
        return None;
    }
    let mut session_id = None;
    if let Some(sid) = claims.session_id() {
        let srow = db
            .fetch_optional(
                "SELECT user_id, revoked FROM sessions WHERE id = ?",
                &[(sid).into_arg()],
            )
            .await
            .ok()??;
        let s_user: Uuid = srow.try_get("user_id").ok()?;
        let revoked: bool = srow.try_get("revoked").ok()?;
        if revoked || s_user != user_id {
            return None;
        }
        session_id = Some(sid);
    }
    Some((user_id, session_id))
}

/// The same permission join as `require_permission`:
/// `user_roles → role_permissions → permissions`.
async fn load_permissions(db: &Db, user_id: Uuid) -> HashSet<String> {
    let rows = db
        .fetch_all(
            "SELECT p.key FROM permissions p \
             JOIN role_permissions rp ON rp.permission_id = p.id \
             JOIN user_roles ur ON ur.role_id = rp.role_id \
             WHERE ur.user_id = ?",
            &[(user_id).into_arg()],
        )
        .await
        .unwrap_or_default();
    rows.iter()
        .filter_map(|r| r.try_get::<String, _>("key").ok())
        .collect()
}

fn query_params(uri: &Uri) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(q) = uri.query() {
        for pair in q.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            map.insert(percent_decode(k), percent_decode(v));
        }
    }
    map
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                if let Ok(n) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(n);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

async fn send_json(socket: &mut WebSocket, value: Value) {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    let _ = socket.send(Message::Text(text.into())).await;
}

async fn send_close(socket: &mut WebSocket, code: u16, reason: &str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.to_string().into(),
        })))
        .await;
}

fn event_message(event: &WsEvent) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), json!("event"));
    if let Value::Object(fields) = serde_json::to_value(event).unwrap_or(Value::Null) {
        for (k, v) in fields {
            obj.insert(k, v);
        }
    }
    serde_json::to_string(&Value::Object(obj)).unwrap_or_else(|_| "{}".to_string())
}

/// Resume replay from the DB: events after the reference, in
/// `(created_at, event_id)` order, filtered by the client's current
/// subscriptions and the `user`-aggregate privacy rule. An unknown
/// reference id replays nothing (Python returns `[]` for it). The
/// window is the newest 1000 rows — older history is not replayed.
async fn replay_missed(
    socket: &mut WebSocket,
    subs: &HashSet<String>,
    identity: &WsIdentity,
    db: &Db,
    last_event_id: &str,
) {
    if subs.is_empty() {
        return;
    }
    let Ok(ref_id) = last_event_id.parse::<Uuid>() else {
        return;
    };
    let events = fetch_recent_events(db, 1000).await;
    let Some(pos) = events.iter().position(|e| e.event.event_id == ref_id) else {
        return;
    };
    for stored in &events[pos + 1..] {
        let ev = &stored.event;
        if !subs.iter().any(|s| channel_matches(s, &ev.event_type)) {
            continue;
        }
        if !event_visible(ev, identity.user_id, &identity.permissions) {
            continue;
        }
        if socket
            .send(Message::Text(event_message(ev).into()))
            .await
            .is_err()
        {
            break;
        }
    }
}

async fn handle_client_message(
    socket: &mut WebSocket,
    subs: &mut HashSet<String>,
    identity: &WsIdentity,
    msg: &serde_json::Map<String, Value>,
    db: Option<&Db>,
    last_pong: &mut Instant,
) {
    match msg.get("type").and_then(|v| v.as_str()) {
        Some("pong") => {
            *last_pong = Instant::now();
        }
        Some("subscribe") => {
            let channel = msg
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if channel.is_empty() {
                send_json(
                    socket,
                    json!({"type": "error", "code": "bad_channel", "message": "channel خالی است"}),
                )
                .await;
            } else if can_subscribe(identity, &channel) {
                subs.insert(channel.clone());
                send_json(socket, json!({"type": "subscribed", "channel": channel})).await;
            } else {
                send_json(
                    socket,
                    json!({"type": "error", "code": "forbidden", "message": "مجوز اشتراك این کانال را ندارید"}),
                )
                .await;
            }
        }
        Some("unsubscribe") => {
            let channel = msg
                .get("channel")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            subs.remove(&channel);
            send_json(
                socket,
                json!({"type": "unsubscribed", "channel": msg.get("channel")}),
            )
            .await;
        }
        Some("resume") => {
            if let Some(last_id) = msg.get("last_event_id").and_then(|v| v.as_str()) {
                if let Some(db) = db {
                    replay_missed(socket, subs, identity, db, last_id).await;
                }
                send_json(socket, json!({"type": "resumed", "last_event_id": last_id})).await;
            } else {
                send_json(
                    socket,
                    json!({"type": "error", "code": "bad_cursor", "message": "last_event_id لازم است"}),
                )
                .await;
            }
        }
        _ => {
            send_json(
                socket,
                json!({"type": "error", "code": "unknown_message", "message": "پیام ناشناخته"}),
            )
            .await;
        }
    }
}

async fn run_ws_client(
    mut socket: WebSocket,
    identity: WsIdentity,
    initial_channels: Vec<String>,
    last_event_id: Option<String>,
    config: Config,
    hub: Arc<WsHub>,
    db: Option<Db>,
) {
    let mut subs: HashSet<String> = HashSet::new();
    let mut rx = hub.sender.subscribe();
    for channel in &initial_channels {
        if can_subscribe(&identity, channel) {
            subs.insert(channel.clone());
            send_json(
                &mut socket,
                json!({"type": "subscribed", "channel": channel}),
            )
            .await;
        } else {
            send_json(
                &mut socket,
                json!({"type": "error", "code": "forbidden", "message": format!("مجوز اشتراک کانال {channel} را ندارید")}),
            )
            .await;
        }
    }
    if let (Some(last_id), Some(db)) = (&last_event_id, &db) {
        replay_missed(&mut socket, &subs, &identity, db, last_id).await;
    }
    let interval = Duration::from_secs(config.ws_heartbeat_interval.max(1));
    let timeout = Duration::from_secs(config.ws_heartbeat_timeout.max(1));
    let mut last_pong = Instant::now();
    loop {
        if last_pong.elapsed() > timeout {
            send_close(&mut socket, CLOSE_HEARTBEAT_TIMEOUT, "heartbeat timeout").await;
            break;
        }
        tokio::select! {
            msg = socket.recv() => {
                let Some(msg) = msg else { break };
                let Ok(msg) = msg else { break };
                match msg {
                    Message::Text(text) => {
                        match serde_json::from_str::<Value>(&text) {
                            Ok(Value::Object(map)) => {
                                handle_client_message(&mut socket, &mut subs, &identity, &map, db.as_ref(), &mut last_pong).await;
                            }
                            Ok(_) => { /* non-object JSON is ignored, like Python */ }
                            Err(_) => {
                                send_json(&mut socket, json!({"type": "error", "code": "bad_json", "message": "پیام باید JSON معتبر باشد"})).await;
                            }
                        }
                    }
                    Message::Ping(data) => {
                        let _ = socket.send(Message::Pong(data)).await;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            () = tokio::time::sleep(interval) => {
                let ts = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs_f64())
                    .unwrap_or(0.0);
                let ping = json!({"type": "ping", "ts": ts}).to_string();
                if socket.send(Message::Text(ping.into())).await.is_err() {
                    break;
                }
                if last_pong.elapsed() > timeout {
                    send_close(&mut socket, CLOSE_HEARTBEAT_TIMEOUT, "heartbeat timeout").await;
                    break;
                }
            }
            event = rx.recv() => {
                match event {
                    Ok(ev)
                        if subs.iter().any(|s| channel_matches(s, &ev.event_type))
                            && event_visible(&ev, identity.user_id, &identity.permissions) =>
                    {
                        if socket
                            .send(Message::Text(event_message(&ev).into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// `GET /ws/events` — the realtime gateway. Authentication happens
/// before the upgrade response is built, but a rejection is delivered
/// as a WebSocket close frame (4401) after accepting, exactly like
/// Python (`websocket.close(code=4401)`): the client sees `101`
/// followed by the close, not an HTTP 403. `?token=` is never read —
/// a JWT there alone is rejected, like Python.
pub async fn ws_events(
    ws: WebSocketUpgrade,
    uri: Uri,
    headers: HeaderMap,
    State(state): State<AppState>,
) -> Response {
    let params = query_params(&uri);
    let auth: Result<WsIdentity, &'static str> = async {
        if !origin_allowed(&state.config, &headers) {
            return Err("foreign origin");
        }
        let Some(db) = &state.db else {
            return Err("missing token");
        };
        if let Some(ticket) = params.get("ticket") {
            return match redeem_ticket(db, ticket).await {
                Some((user_id, session_id)) => Ok(WsIdentity {
                    user_id,
                    session_id,
                    permissions: load_permissions(db, user_id).await,
                }),
                None => Err("invalid ticket"),
            };
        }
        let auth = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let token = if auth.len() > 7 && auth[..7].eq_ignore_ascii_case("bearer ") {
            auth[7..].trim()
        } else {
            ""
        };
        if token.is_empty() {
            return Err("missing token");
        }
        match authenticate_jwt(db, &state.config.jwt_secret, token).await {
            Some((user_id, session_id)) => Ok(WsIdentity {
                user_id,
                session_id,
                permissions: load_permissions(db, user_id).await,
            }),
            None => Err("invalid token"),
        }
    }
    .await;
    let initial_channels: Vec<String> = params
        .get("channels")
        .map(|c| {
            c.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let last_event_id = params.get("last_event_id").cloned();
    let config = state.config.clone();
    let hub = state.ws_hub.clone();
    let db = state.db.clone();
    ws.on_upgrade(move |socket| async move {
        let mut socket = socket;
        match auth {
            Err(reason) => {
                send_close(&mut socket, CLOSE_UNAUTHORIZED, reason).await;
            }
            Ok(identity) => {
                run_ws_client(
                    socket,
                    identity,
                    initial_channels,
                    last_event_id,
                    config,
                    hub,
                    db,
                )
                .await;
            }
        }
    })
}
