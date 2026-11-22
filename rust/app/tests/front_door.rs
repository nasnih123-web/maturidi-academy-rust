//! Wave 1 front-door tests: native /health, native /ready, proxy fallback.

use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
    Json, Router,
};
use maturidi_api_rs::{build_router, AppState, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        database_url: None,
        redis_url: "redis://127.0.0.1:6399/0".to_string(),
        // Dead port: proves /health and /ready never touch the origin.
        origin_url: "http://127.0.0.1:9".to_string(),
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

/// A tiny mock origin: echoes method/path, a header, and the request body.
async fn start_mock_origin() -> String {
    let mock = Router::new()
        .route(
            "/api/v1/health",
            get(|| async { Json(json!({"status": "ok", "env": "test", "api": "v1"})) }),
        )
        .route(
            "/echo",
            post(|req: Request<Body>| async move {
                let (parts, body) = req.into_parts();
                let echoed = parts
                    .headers
                    .get("x-test")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let text = String::from_utf8_lossy(&bytes).to_string();
                (
                    StatusCode::CREATED,
                    [("x-echoed", echoed)],
                    format!("{}|{}", parts.method, text),
                )
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock origin");
    let addr = listener.local_addr().expect("mock origin addr");
    tokio::spawn(async move {
        axum::serve(listener, mock)
            .await
            .expect("mock origin serve");
    });
    format!("http://{addr}")
}

#[tokio::test]
async fn proxy_forwards_to_origin_and_returns_status_and_body() {
    let origin = start_mock_origin().await;
    let mut config = test_config();
    config.origin_url = origin;
    let app = build_router(AppState::new(config).await);

    let req = Request::builder()
        .uri("/api/v1/health")
        .header("x-test", "hello")
        .body(Body::empty())
        .unwrap();
    let (status, headers, bytes) = call_bytes(app, req).await;
    assert_eq!(status, StatusCode::OK);
    let value: Value = serde_json::from_slice(&bytes).expect("proxy body is JSON");
    assert_eq!(value, json!({"status": "ok", "env": "test", "api": "v1"}));
    // The origin's JSON content-type must survive the proxy verbatim.
    assert!(headers
        .iter()
        .any(|(k, v)| k == "content-type" && v.contains("application/json")));
}

#[tokio::test]
async fn proxy_preserves_method_headers_and_body() {
    let origin = start_mock_origin().await;
    let mut config = test_config();
    config.origin_url = origin;
    let app = build_router(AppState::new(config).await);

    let req = Request::builder()
        .method("POST")
        .uri("/echo?x=1")
        .header("x-test", "hello")
        .header("content-type", "text/plain")
        .body(Body::from("payload"))
        .unwrap();
    let (status, headers, bytes) = call_bytes(app, req).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(String::from_utf8_lossy(&bytes), "POST|payload");
    assert!(headers.iter().any(|(k, v)| k == "x-echoed" && v == "hello"));
}

#[tokio::test]
async fn proxy_to_dead_origin_returns_502() {
    let app = build_router(AppState::new(test_config()).await);
    let req = Request::builder()
        .uri("/api/v1/health")
        .body(Body::empty())
        .unwrap();
    let (status, body) = call(app, req).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["status"], json!("bad_gateway"));
}
