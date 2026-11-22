//! Wave 1 front-door tests: native /health, native /ready, native 404,
//! and the Wave 8 lift surfaces (/metrics, /api/v1/health,
//! /openapi.json, sidecar 503s).
//!
//! The old proxy fallback is gone (ADR-133): there is no origin any
//! more, so the mock-origin forward tests were deleted with it.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    Router,
};
use maturidi_api_rs::{build_router, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        database_url: None,
        redis_url: "redis://127.0.0.1:6399/0".to_string(),
        port: 8000,
        ..Default::default()
    }
}

async fn call(app: Router, req: Request<Body>) -> (StatusCode, Value) {
    let res = app.oneshot(req).await.expect("router call");
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    let value: Value = serde_json::from_slice(&bytes).expect("body is JSON");
    (status, value)
}

async fn call_bytes(
    app: Router,
    req: Request<Body>,
) -> (StatusCode, Vec<(String, String)>, Vec<u8>) {
    let res = app.oneshot(req).await.expect("router call");
    let status = res.status();
    let headers = res
        .headers()
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .expect("read body");
    (status, headers, bytes.to_vec())
}

#[tokio::test]
async fn health_returns_ok_and_env() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"status": "ok", "env": "test"}));
}

#[tokio::test]
async fn ready_with_unreachable_db_returns_503_not_ready() {
    let mut config = test_config();
    // Connection-refused Postgres: DB probe fails fast, never panics.
    config.database_url = Some("postgres://127.0.0.1:1/nope".to_string());
    let app = build_router(AppState::new(config).await);
    let req = Request::builder()
        .uri("/ready")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        body,
        json!({
            "status": "not_ready",
            "env": "test",
            "db": "unavailable",
            "redis": "unavailable",
        })
    );
}

#[tokio::test]
async fn ready_with_sqlite_memory_returns_200_ready() {
    let mut config = test_config();
    config.database_url = Some("sqlite://:memory:".to_string());
    let app = build_router(AppState::new(config).await);
    let req = Request::builder()
        .uri("/ready")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({
            "status": "ready",
            "env": "test",
            "db": "ok",
            "redis": "unavailable",
        })
    );
}

#[tokio::test]
async fn ready_with_no_database_url_returns_503() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .uri("/ready")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], json!("not_ready"));
    assert_eq!(body["db"], json!("unavailable"));
}

/// The lift (ADR-133): an unknown path is a native 404 in Python's
/// error envelope — a `StarletteHTTPException(404, "Not Found")`
/// through the `http_error` handler in `app/core/errors.py`, NOT
/// FastAPI's default `{"detail": "Not Found"}`.
#[tokio::test]
async fn unknown_path_returns_python_404_envelope() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .uri("/no/such/route")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], json!("http_error"));
    assert_eq!(body["error"]["message"], json!("Not Found"));
    assert_eq!(body["error"]["details"], json!({}));
    assert!(body["request_id"].as_str().is_some_and(|s| !s.is_empty()));
}

/// A known path with the wrong method is a native 405 in the same
/// envelope (Python: `StarletteHTTPException(405, "Method Not
/// Allowed")`).
#[tokio::test]
async fn known_path_wrong_method_returns_405_envelope() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .method("DELETE")
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(body["error"]["code"], json!("http_error"));
    assert_eq!(body["error"]["message"], json!("Method Not Allowed"));
}

/// `/docs` is dev-only in Python (`docs_url="/docs" if is_dev else
/// None` in main.py), so in production it 404s — and the Rust binary
/// never serves it, so it 404s here too (ADR-139).
#[tokio::test]
async fn docs_returns_404() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder().uri("/docs").body(Body::empty()).unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], json!("http_error"));
}

#[tokio::test]
async fn metrics_exposition_shape_and_content_type() {
    let app = build_router(AppState::new(test_config()).await);
    // One counted request first, so the counter line exists.
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let (status, _) = call(app.clone(), req).await;
    assert_eq!(status, StatusCode::OK);
    let req = Request::builder()
        .uri("/metrics")
        .body(Body::empty())
        .unwrap();
    let (status, headers, bytes) = call_bytes(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers
        .iter()
        .any(|(k, v)| k == "content-type" && v == "text/plain; version=0.0.4"));
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("# HELP http_requests_total Total HTTP requests"));
    assert!(text.contains("# TYPE http_requests_total counter"));
    assert!(text.contains("http_requests_total{method=\"GET\",path=\"/health\",status=\"200\"} 1"));
    assert!(text.contains("# HELP http_request_duration_ms_avg Average request duration in ms"));
    assert!(text.contains("http_request_duration_ms_avg "));
    assert!(text.contains("# HELP process_uptime_seconds Process uptime in seconds"));
    assert!(text.contains("process_uptime_seconds "));
    assert!(text.contains("# HELP outbox_pending_events Domain events waiting for dispatch"));
    assert!(text.contains("outbox_pending_events 0"));
    assert!(text
        .contains("# HELP outbox_oldest_pending_age_seconds Age of the oldest undispatched event"));
    assert!(text.contains("outbox_oldest_pending_age_seconds 0.0"));
    assert!(text.contains("# HELP outbox_dead_letter_events Events that exhausted retries"));
    assert!(text.contains("outbox_dead_letter_events 0"));
}

#[tokio::test]
async fn health_v1_reports_media_tools_unavailable() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .uri("/api/v1/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], json!("ok"));
    assert_eq!(body["env"], json!("test"));
    assert_eq!(body["api"], json!("v1"));
    // The Rust binary owns no media toolchain: every tool reports
    // unavailable, honestly (ADR-137).
    let tools = &body["media_tools"];
    assert_eq!(
        tools["binaries"],
        json!({"ffmpeg": null, "ffprobe": null, "tesseract": null, "pdftoppm": null})
    );
    assert_eq!(tools["tesseract_langs"], json!(null));
    assert_eq!(tools["faster_whisper"], json!(false));
    assert_eq!(tools["stt_engine"], json!("faster-whisper"));
    assert_eq!(tools["stt_model"], json!("small"));
    assert_eq!(
        tools["missing"],
        json!([
            "ffmpeg",
            "ffprobe",
            "tesseract",
            "pdftoppm",
            "faster-whisper"
        ])
    );
    assert_eq!(tools["ready"], json!(false));
}

#[tokio::test]
async fn openapi_json_lists_registered_routes() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .uri("/openapi.json")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["openapi"], json!("3.0.3"));
    assert_eq!(body["info"]["title"], json!("Maturidi Academy API"));
    let paths = body["paths"].as_object().expect("paths object");
    // Spot-checks across the lift surface and the old modules.
    assert!(paths.contains_key("/health"));
    assert!(paths.contains_key("/metrics"));
    assert!(paths.contains_key("/api/v1/health"));
    assert!(paths.contains_key("/ws/events"));
    assert!(paths.contains_key("/api/v1/fatwa/ask"));
    assert!(paths.contains_key("/api/v1/finance/receipts/{donation_id}"));
    assert!(paths.contains_key("/api/v1/voice/transcribe"));
    assert!(paths.contains_key("/api/v1/quran/hifz/attempts/{attempt_id}/grade"));
    assert!(paths.contains_key("/api/v1/auth/register"));
    assert!(paths.contains_key("/api/v1/articles"));
    // An unknown path is not a route.
    assert!(!paths.contains_key("/no/such/route"));
    // The generated spec covers every registered route+method combo:
    // 440 pre-lift native + 5 new native + 8 sidecar + 18 projects = 471.
    let combos: usize = paths
        .values()
        .map(|v| v.as_object().map(|o| o.len()).unwrap_or(0))
        .sum();
    assert_eq!(combos, 471, "openapi route+method combos");
}

/// With `SIDECAR_URL` unset, each sidecar-contract route answers an
/// honest 503 with its route-specific code (ADR-134) — never the old
/// proxy's 502 `bad_gateway`.
#[tokio::test]
async fn sidecar_routes_answer_503_when_sidecar_unset() {
    let app = build_router(AppState::new(test_config()).await);
    let cases = [
        ("POST", "/api/v1/fatwa/ask", "fatwa_ask_unavailable"),
        (
            "GET",
            "/api/v1/finance/receipts/00000000-0000-0000-0000-000000000000",
            "receipt_pdf_unavailable",
        ),
        (
            "POST",
            "/api/v1/quran/hifz/attempts/00000000-0000-0000-0000-000000000000/grade",
            "hifz_asr_unavailable",
        ),
        (
            "POST",
            "/api/v1/quran/hifz/attempts/00000000-0000-0000-0000-000000000000/grade-word",
            "hifz_asr_unavailable",
        ),
        (
            "POST",
            "/api/v1/quran/hifz/tests/00000000-0000-0000-0000-000000000000/questions/00000000-0000-0000-0000-000000000000/answer",
            "hifz_asr_unavailable",
        ),
        (
            "POST",
            "/api/v1/quran/hifz/voice-search",
            "hifz_asr_unavailable",
        ),
        ("POST", "/api/v1/voice/transcribe", "voice_stt_unavailable"),
        ("POST", "/api/v1/voice/ask", "voice_stt_unavailable"),
    ];
    for (method, uri, code) in cases {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let (status, body) = call(app.clone(), req).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        assert_eq!(body["error"]["code"], json!(code), "{uri}");
    }
}
