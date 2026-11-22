//! Wave 9 PR-1 tests: guard the cutover preflight (`scripts/cutover.sh`)
//! and the live endpoints its Step 7 probes — `/health`, `/ready`,
//! `/api/v1/health`, `/metrics`, `/openapi.json`, `/static` 404,
//! `/docs` 404, unknown-path 404 (Python `http_error` envelope), and
//! the 8 sidecar-contract routes' honest 503s when `SIDECAR_URL` is
//! unset (ADR-134).
//!
//! The script tests run the real script via `bash`: `bash -n`
//! (syntax), `--dry-run` (the plan text — it must name `SIDECAR_URL`
//! and the Step 7 endpoints and must not mention `ORIGIN_URL` or any
//! proxied route anywhere), and the Step 0 live-project guard (it
//! refuses without the operator ack, and refuses a live domain even
//! with the ack). The PG smoke at the end (`wave9_pr1_pg_smoke`,
//! gated on `WAVE2_PG_URL`) replays the endpoint checks on local
//! Postgres.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    Router,
};
use maturidi_api_rs::{build_router, init_test_schema, AppState, Config, Db, TEST_SCHEMA};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::process::Command;
use tower::ServiceExt;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        database_url: None,
        redis_url: "redis://127.0.0.1:6399/0".to_string(),
        jwt_secret: "test-jwt-secret-for-wave9-pr1-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave9-pr1".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave9-pr1-storage-{}",
            std::env::temp_dir().display(),
            std::process::id()
        ),
        ..Default::default()
    }
}

struct Env {
    state: AppState,
}

impl Env {
    async fn sqlite() -> Self {
        Self::sqlite_with(test_config()).await
    }

    async fn sqlite_with(config: Config) -> Self {
        let db = Db::connect("sqlite://:memory:")
            .await
            .expect("sqlite connect");
        init_test_schema(&db).await.expect("test schema");
        let state = AppState::with_db(config, Some(db));
        Self { state }
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

    async fn call_raw(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let res = self.router().oneshot(req).await.expect("oneshot");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, bytes.to_vec())
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

fn err_code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<no error code>")
}

// ============================ cutover.sh ============================

fn cutover_script() -> String {
    format!("{}/../../scripts/cutover.sh", env!("CARGO_MANIFEST_DIR"))
}

/// Run the script with a clean environment (only PATH survives), so no
/// ambient variable from the test runner can leak into the preflight.
/// `bash_args` go to bash itself (e.g. `-n` for a syntax check);
/// `script_args` go to the script (e.g. `--dry-run`).
fn run_script(
    bash_args: &[&str],
    script_args: &[&str],
    extra_env: &[(&str, &str)],
) -> (i32, String) {
    let mut cmd = Command::new("bash");
    for a in bash_args {
        cmd.arg(a);
    }
    cmd.arg(cutover_script());
    for a in script_args {
        cmd.arg(a);
    }
    cmd.env_clear();
    if let Ok(path) = std::env::var("PATH") {
        cmd.env("PATH", path);
    }
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run cutover.sh");
    let code = out.status.code().unwrap_or(-1);
    let mut text = String::from_utf8_lossy(&out.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    (code, text)
}

#[test]
fn cutover_script_syntax_ok() {
    let (code, out) = run_script(&["-n"], &[], &[]);
    assert_eq!(code, 0, "bash -n failed: {out}");
}

#[test]
fn cutover_dry_run_lists_steps_without_origin_url() {
    let (code, out) = run_script(&[], &["--dry-run"], &[]);
    assert_eq!(code, 0, "dry-run exit != 0: {out}");
    // The plan must name the sidecar variable and every Step 7 endpoint.
    for needle in [
        "SIDECAR_URL",
        "RUST_API_URL",
        "Step 7",
        "/health",
        "/ready",
        "/api/v1/health",
        "/metrics",
        "/openapi.json",
        "python-sidecar",
    ] {
        assert!(out.contains(needle), "dry-run missing {needle}:\n{out}");
    }
    // No trace of the removed proxy contract anywhere in the plan.
    for banned in ["ORIGIN_URL", "python-origin", "proxied", "502"] {
        assert!(
            !out.contains(banned),
            "dry-run still mentions {banned}:\n{out}"
        );
    }
}

#[test]
fn cutover_guard_refuses_without_ack() {
    let (code, out) = run_script(&[], &[], &[]);
    assert_eq!(code, 1, "expected NO-GO (exit 1) without the ack: {out}");
    assert!(out.contains("CUTOVER_ACK_NEW_PROJECT"), "{out}");
    assert!(out.contains("NO-GO"), "{out}");
}

#[test]
fn cutover_guard_refuses_live_domain_even_with_ack() {
    let (code, out) = run_script(
        &[],
        &[],
        &[
            ("CUTOVER_ACK_NEW_PROJECT", "yes"),
            ("APP_BASE_URL", "https://api.matoridi.ir"),
        ],
    );
    assert_eq!(code, 1, "expected NO-GO (exit 1) for a live domain: {out}");
    assert!(
        out.contains("points at live domain api.matoridi.ir"),
        "{out}"
    );
}

// ============================ Step 7 endpoints ============================

#[tokio::test]
async fn preflight_health_and_ready() {
    // /ready probes config.database_url itself (lib.rs probe_db), NOT the
    // test pool — so the config must name a reachable DB. A fresh
    // in-memory sqlite pool still answers SELECT 1, so this is 200.
    let mut config = test_config();
    config.database_url = Some("sqlite://:memory:".to_string());
    let env = Env::sqlite_with(config).await;
    let (status, body) = env.call(req(Method::GET, "/health", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("ok"));
    let (status, body) = env.call(req(Method::GET, "/ready", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("ready"));
    assert_eq!(body["db"], json!("ok"));
    // No Redis runs in the test env: reported unavailable, readiness
    // still 200 (Redis never fails readiness — lib.rs:1912).
    assert_eq!(body["redis"], json!("unavailable"));
}

#[tokio::test]
async fn preflight_health_v1_media_tools_honest() {
    let env = Env::sqlite().await;
    let (status, body) = env.call(req(Method::GET, "/api/v1/health", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(body["media_tools"]["ready"], json!(false));
    assert_eq!(
        body["media_tools"]["missing"],
        json!([
            "ffmpeg",
            "ffprobe",
            "tesseract",
            "pdftoppm",
            "faster-whisper"
        ])
    );
}

#[tokio::test]
async fn preflight_metrics_and_openapi() {
    let env = Env::sqlite().await;
    let (status, bytes) = env.call_raw(req(Method::GET, "/metrics", None)).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("http_requests_total"), "{text}");
    let (status, body) = env.call(req(Method::GET, "/openapi.json", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paths = body["paths"].as_object().expect("paths");
    let combos: usize = paths
        .values()
        .map(|v| v.as_object().map(|o| o.len()).unwrap_or(0))
        .sum();
    assert_eq!(combos, 471, "openapi route+method combos");
    assert!(paths["/api/v1/fatwa/ask"].is_object());
    assert!(paths["/ws/events"].is_object());
}

#[tokio::test]
async fn preflight_404s() {
    let env = Env::sqlite().await;
    let (status, _) = env
        .call(req(Method::GET, "/static/__cutover_probe__.woff2", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = env.call(req(Method::GET, "/docs", None)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = env
        .call(req(Method::GET, "/__cutover_unknown_path__", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "http_error");
    assert_eq!(body["error"]["message"], json!("Not Found"));
}

/// All 8 sidecar-contract routes answer 503 with their route-specific
/// code when SIDECAR_URL is unset (the default test config has none).
#[tokio::test]
async fn preflight_sidecar_503s_when_unset() {
    let env = Env::sqlite().await;
    let cases: [(&str, &str, &str, Option<Value>); 8] = [
        ("POST", "/api/v1/fatwa/ask", "fatwa_ask_unavailable", Some(json!({"question": "hi"}))),
        ("GET", "/api/v1/finance/receipts/00000000-0000-0000-0000-000000000000", "receipt_pdf_unavailable", None),
        ("POST", "/api/v1/quran/hifz/attempts/00000000-0000-0000-0000-000000000000/grade", "hifz_asr_unavailable", Some(json!({}))),
        ("POST", "/api/v1/quran/hifz/attempts/00000000-0000-0000-0000-000000000000/grade-word", "hifz_asr_unavailable", Some(json!({}))),
        ("POST", "/api/v1/quran/hifz/tests/00000000-0000-0000-0000-000000000000/questions/00000000-0000-0000-0000-000000000000/answer", "hifz_asr_unavailable", Some(json!({}))),
        ("POST", "/api/v1/quran/hifz/voice-search", "hifz_asr_unavailable", Some(json!({}))),
        ("POST", "/api/v1/voice/transcribe", "voice_stt_unavailable", Some(json!({}))),
        ("POST", "/api/v1/voice/ask", "voice_stt_unavailable", Some(json!({}))),
    ];
    for (method, uri, code, body) in cases {
        let m = Method::from_bytes(method.as_bytes()).unwrap();
        let (status, resp) = env.call(req(m, uri, body)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {uri}: {resp}"
        );
        assert_eq!(err_code(&resp), code, "{method} {uri}: {resp}");
    }
}

/// Critical-path replay against a local Postgres test DB.
/// Run with WAVE2_PG_URL set; skipped otherwise (like the wave2/3/4 smokes).
#[tokio::test]
async fn wave9_pr1_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok() else {
        return;
    };
    let db = Db::connect(&url).await.expect("pg connect");
    // The smokes share one DB: drop every TEST_SCHEMA table first, like
    // the wave2/wave8 smokes.
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
    // /ready probes config.database_url itself, so point it at the same
    // test Postgres (unlike the sqlite tests, where the config names an
    // in-memory sqlite URL).
    let mut config = test_config();
    config.database_url = Some(url.clone());
    let state = AppState::with_db(config, Some(db));
    let env = Env { state };
    let (status, body) = env.call(req(Method::GET, "/health", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = env.call(req(Method::GET, "/ready", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["db"], json!("ok"));
    let (status, body) = env.call(req(Method::GET, "/api/v1/health", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["media_tools"]["ready"], json!(false));
    let (status, bytes) = env.call_raw(req(Method::GET, "/metrics", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&bytes).contains("http_requests_total"));
    let (status, body) = env.call(req(Method::GET, "/openapi.json", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paths = body["paths"].as_object().expect("paths");
    let combos: usize = paths
        .values()
        .map(|v| v.as_object().map(|o| o.len()).unwrap_or(0))
        .sum();
    assert_eq!(combos, 471, "openapi route+method combos on Postgres");
    let (status, body) = env
        .call(req(Method::GET, "/__cutover_unknown_path__", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert_eq!(err_code(&body), "http_error");
    let (status, body) = env
        .call(req(
            Method::POST,
            "/api/v1/fatwa/ask",
            Some(json!({"question": "hi"})),
        ))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "fatwa_ask_unavailable");
}
