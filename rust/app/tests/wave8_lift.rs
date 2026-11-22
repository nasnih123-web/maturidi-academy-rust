//! Wave 8 lift tests: the proxy is gone, so these pin the native
//! replacements — the Python 404 envelope, `/metrics`, `/api/v1/health`,
//! `/openapi.json`, `/static/*`, the sidecar contract (503s + live
//! forwarding to a mock sidecar), the retired kill switches, and the
//! native `/ws/events` gateway (handshake auth, subscriptions,
//! heartbeat, resume replay) over a real TCP socket with a minimal
//! hand-rolled WebSocket client (no new test-only crate).
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router — `oneshot` for
//! HTTP, a bound `127.0.0.1:0` listener for WebSocket. The Postgres
//! smoke at the end (`wave8_pg_smoke`, gated on `WAVE2_PG_URL`)
//! replays metrics + health + openapi + a live WS event on local
//! Postgres.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    routing::post,
    Json, Router,
};
use chrono::{DateTime, Utc};
use maturidi_api_rs::{build_router, init_test_schema, AppState, Config, Db, IntoArg, TEST_SCHEMA};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tower::ServiceExt;
use uuid::Uuid;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        database_url: None,
        redis_url: "redis://127.0.0.1:6399/0".to_string(),
        jwt_secret: "test-jwt-secret-for-wave8-tests-only".to_string(),
        encryption_key: "test-encryption-key-wave8".to_string(),
        captcha_enabled: false,
        // Fast realtime for the WS tests: 50 ms outbox poll, 1 s
        // heartbeat interval, 2 s pong timeout.
        ws_heartbeat_interval: 1,
        ws_heartbeat_timeout: 2,
        dispatcher_poll_interval: 0.05,
        storage_dir: format!(
            "{}/wave8-storage-{}",
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
        Self::sqlite_with(test_config()).await
    }

    async fn sqlite_with(config: Config) -> Self {
        let db = Db::connect("sqlite://:memory:")
            .await
            .expect("sqlite connect");
        init_test_schema(&db).await.expect("test schema");
        let state = AppState::with_db(config, Some(db.clone()));
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

    async fn call_raw(&self, req: Request<Body>) -> (StatusCode, Vec<u8>) {
        let res = self.router().oneshot(req).await.expect("oneshot");
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .expect("read body");
        (status, bytes.to_vec())
    }

    async fn call_raw_full(
        &self,
        req: Request<Body>,
    ) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
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
    (
        user_id,
        body["access_token"].as_str().unwrap().to_string(),
        body["refresh_token"].as_str().unwrap().to_string(),
    )
}

/// Grant one permission to a user via a fresh non-privileged role
/// (a role named `admin` would force 2FA enrolment and hand out a
/// limited-scope token, which the WS gateway rejects).
async fn grant_permission(env: &Env, user_id: &str, key: &str) {
    let now = Utc::now().to_rfc3339();
    let role_id = Uuid::new_v4();
    let perm_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO roles (id, name, created_at) VALUES (?, ?, ?)",
            &[
                role_id.into_arg(),
                format!("ws-test-{key}").into_arg(),
                now.clone().into_arg(),
            ],
        )
        .await
        .unwrap();
    env.db
        .execute(
            "INSERT INTO permissions (id, key, created_at) VALUES (?, ?, ?)",
            &[perm_id.into_arg(), key.into_arg(), now.into_arg()],
        )
        .await
        .unwrap();
    env.db
        .execute(
            "INSERT INTO role_permissions (role_id, permission_id) VALUES (?, ?)",
            &[role_id.into_arg(), perm_id.into_arg()],
        )
        .await
        .unwrap();
    env.db
        .execute(
            "INSERT INTO user_roles (user_id, role_id) VALUES (?, ?)",
            &[as_uuid(user_id).into_arg(), role_id.into_arg()],
        )
        .await
        .unwrap();
}

/// Mint a one-time WS ticket through the real endpoint.
async fn ws_ticket(env: &Env, token: &str) -> String {
    let (status, body) = env
        .call(authed(
            req(Method::POST, "/api/v1/auth/ws-ticket", None),
            token,
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "ws-ticket: {body}");
    body["ticket"].as_str().unwrap().to_string()
}

/// Insert one `domain_events` row directly (the outbox writer).
async fn insert_event(
    env: &Env,
    event_type: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    created_at: DateTime<Utc>,
) -> Uuid {
    let event_id = Uuid::new_v4();
    env.db
        .execute(
            "INSERT INTO domain_events (event_id, idempotency_key, aggregate_type, aggregate_id, \
             event_type, payload, schema_version, source_client, created_at, retry_count) \
             VALUES (?, NULL, ?, ?, ?, ?, 1, 'api', ?, 0)",
            &[
                event_id.into_arg(),
                aggregate_type.into_arg(),
                aggregate_id.into_arg(),
                event_type.into_arg(),
                json!({"probe": true}).into_arg(),
                created_at.into_arg(),
            ],
        )
        .await
        .expect("insert domain event");
    event_id
}

async fn serve(router: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("server addr");
    tokio::spawn(async move {
        axum::serve(listener, router).await.expect("serve");
    });
    addr
}

// ============================ minimal WS client ============================

enum Frame {
    Text(String),
    Close(u16),
    Ping(Vec<u8>),
    Other,
}

struct WsClient {
    stream: TcpStream,
    /// Bytes read past the end of the handshake response (the server
    /// can send the 101 and the first frames in one segment).
    buf: Vec<u8>,
}

impl WsClient {
    async fn connect(addr: SocketAddr, target: &str, headers: &[(&str, &str)]) -> Self {
        let mut stream = TcpStream::connect(addr).await.expect("tcp connect");
        let mut req = format!(
            "GET {target} HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n"
        );
        for (k, v) in headers {
            req.push_str(&format!("{k}: {v}\r\n"));
        }
        req.push_str("\r\n");
        stream
            .write_all(req.as_bytes())
            .await
            .expect("write upgrade");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        let end = loop {
            let n = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
                .await
                .expect("handshake timeout")
                .expect("handshake read");
            assert!(n > 0, "eof during handshake");
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..end]).to_string();
        assert!(head.starts_with("HTTP/1.1 101"), "handshake: {head}");
        Self {
            stream,
            buf: buf[end..].to_vec(),
        }
    }

    /// `read_exact`, served from the leftover handshake bytes first.
    async fn read_exact_buf(&mut self, mut out: &mut [u8]) -> std::io::Result<()> {
        while !out.is_empty() {
            if !self.buf.is_empty() {
                let take = std::cmp::min(self.buf.len(), out.len());
                out[..take].copy_from_slice(&self.buf[..take]);
                self.buf.drain(..take);
                out = &mut out[take..];
            } else {
                let mut chunk = [0u8; 4096];
                let n = self.stream.read(&mut chunk).await?;
                if n == 0 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "eof",
                    ));
                }
                self.buf.extend_from_slice(&chunk[..n]);
            }
        }
        Ok(())
    }

    async fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        assert!(payload.len() < 65536, "test frame too long");
        let mask = [0x12, 0x34, 0x56, 0x78];
        let mut frame = vec![0x81u8];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            frame.push(b ^ mask[i % 4]);
        }
        self.stream.write_all(&frame).await.expect("write frame");
    }

    async fn read_frame(&mut self) -> Option<Frame> {
        let mut hdr = [0u8; 2];
        self.read_exact_buf(&mut hdr).await.ok()?;
        let opcode = hdr[0] & 0x0f;
        let mut len = (hdr[1] & 0x7f) as u64;
        if len == 126 {
            let mut b = [0u8; 2];
            self.read_exact_buf(&mut b).await.ok()?;
            len = u16::from_be_bytes(b) as u64;
        } else if len == 127 {
            let mut b = [0u8; 8];
            self.read_exact_buf(&mut b).await.ok()?;
            len = u64::from_be_bytes(b);
        }
        let mut payload = vec![0u8; len as usize];
        self.read_exact_buf(&mut payload).await.ok()?;
        match opcode {
            1 => Some(Frame::Text(String::from_utf8_lossy(&payload).to_string())),
            8 => {
                let code = if payload.len() >= 2 {
                    u16::from_be_bytes([payload[0], payload[1]])
                } else {
                    1005
                };
                Some(Frame::Close(code))
            }
            9 => Some(Frame::Ping(payload)),
            _ => Some(Frame::Other),
        }
    }

    /// Next frame, auto-replying to WS-level pings. `None` on timeout.
    async fn next_frame(&mut self, timeout: Duration) -> Option<Frame> {
        loop {
            let frame = tokio::time::timeout(timeout, self.read_frame())
                .await
                .ok()??;
            match frame {
                Frame::Ping(data) => {
                    // Client frames must be masked.
                    let mask = [0x12, 0x34, 0x56, 0x78];
                    let mut pong = vec![0x8au8, 0x80 | (data.len() as u8)];
                    pong.extend_from_slice(&mask);
                    for (i, b) in data.iter().enumerate() {
                        pong.push(b ^ mask[i % 4]);
                    }
                    let _ = self.stream.write_all(&pong).await;
                }
                other => return Some(other),
            }
        }
    }

    /// Next text message (skipping pings). `None` on timeout or close.
    async fn next_text(&mut self, timeout: Duration) -> Option<String> {
        loop {
            match self.next_frame(timeout).await? {
                Frame::Text(t) => return Some(t),
                Frame::Close(_) => return None,
                _ => {}
            }
        }
    }

    /// Wait for a close frame; `None` on timeout.
    async fn wait_close(&mut self, timeout: Duration) -> Option<u16> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            match self.next_frame(left).await? {
                Frame::Close(code) => return Some(code),
                Frame::Text(_) => {}
                _ => {}
            }
        }
    }
}

/// Read text messages until one parses as JSON with `"type": "event"`
/// and the expected `event_type` — server `{"type":"ping"}` frames
/// are answered with `{"type":"pong"}` on the way.
async fn wait_event(client: &mut WsClient, event_type: &str, timeout: Duration) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return None;
        }
        let text = client.next_text(left).await?;
        let value: Value = serde_json::from_str(&text).ok()?;
        if value["type"] == "ping" {
            client.send_text(r#"{"type":"pong"}"#).await;
            continue;
        }
        if value["type"] == "event" && value["event_type"] == event_type {
            return Some(value);
        }
    }
}

// ============================ lift surfaces ============================

#[tokio::test]
async fn unknown_path_404_envelope() {
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/definitely/not/a/route", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err_code(&body), "http_error");
    assert_eq!(body["error"]["message"], json!("Not Found"));
    assert!(body["request_id"].as_str().is_some());
}

#[tokio::test]
async fn metrics_counts_requests_and_outbox_gauges() {
    let env = Env::sqlite().await;
    // One pending event (2 min old) + one dead-letter event.
    let old = Utc::now() - chrono::Duration::seconds(120);
    insert_event(&env, "probe.pending", "probe", "p1", old).await;
    let dead_id = insert_event(&env, "probe.dead", "probe", "p2", Utc::now()).await;
    env.db
        .execute(
            "UPDATE domain_events SET failed_at = ? WHERE event_id = ?",
            &[Utc::now().into_arg(), dead_id.into_arg()],
        )
        .await
        .unwrap();
    // Two counted requests first.
    for _ in 0..2 {
        let (status, _) = env.call(req(Method::GET, "/health", None)).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, bytes) = env.call_raw(req(Method::GET, "/metrics", None)).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert!(text.contains("http_requests_total{method=\"GET\",path=\"/health\",status=\"200\"} 2"));
    assert!(text.contains("outbox_pending_events 1"));
    assert!(text.contains("outbox_dead_letter_events 1"));
    let age_line = text
        .lines()
        .find(|l| l.starts_with("outbox_oldest_pending_age_seconds "))
        .expect("age gauge");
    let age: f64 = age_line.split_whitespace().nth(1).unwrap().parse().unwrap();
    assert!((115.0..600.0).contains(&age), "age {age}");
}

#[tokio::test]
async fn openapi_spec_counts_and_static_route() {
    let env = Env::sqlite().await;
    let (status, body) = env.call(req(Method::GET, "/openapi.json", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let paths = body["paths"].as_object().expect("paths");
    assert_eq!(
        paths["/static/{path}"]["get"]["responses"]["200"]["description"],
        json!("OK")
    );
    assert_eq!(
        paths["/ws/events"]["get"]["responses"]["200"]["description"],
        json!("OK")
    );
    let combos: usize = paths
        .values()
        .map(|v| v.as_object().map(|o| o.len()).unwrap_or(0))
        .sum();
    assert_eq!(combos, 471, "openapi route+method combos");
}

#[tokio::test]
async fn static_font_served_and_missing_404s() {
    let env = Env::sqlite().await;
    let name = format!("lift-test-{}.woff2", Uuid::new_v4().simple());
    let dir = format!("{}/static/fonts/fa", test_config().storage_dir);
    tokio::fs::create_dir_all(&dir).await.unwrap();
    let bytes = b"wOF2fake-font-bytes";
    tokio::fs::write(format!("{dir}/{name}"), bytes)
        .await
        .unwrap();
    let (status, got) = env
        .call_raw(req(Method::GET, &format!("/static/fonts/fa/{name}"), None))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, bytes);
    let (status, body) = env
        .call(req(
            Method::GET,
            "/static/fonts/fa/does-not-exist.woff2",
            None,
        ))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err_code(&body), "http_error");
    // Traversal never escapes the static dir.
    let (status, _) = env
        .call_raw(req(Method::GET, "/static/%2E%2E/%2E%2E/secret", None))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn kill_switches_are_ignored_routes_stay_native() {
    // SAFETY: env vars are process-global, but the retired switches
    // change nothing any more — every concurrent test builds the same
    // native router with or without them.
    std::env::set_var("RUST_ANYTHING_UNIT_META", "original");
    std::env::set_var("RUST_ANYTHING_UNIT_VOICE", "original");
    let env = Env::sqlite().await;
    let (status, body) = env
        .call(req(Method::GET, "/api/v1/themes/active", None))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = env
        .call(req(Method::GET, "/api/v1/admin/voice/settings", None))
        .await;
    // Admin voice settings needs auth, but the route must exist: 401,
    // not 404.
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    std::env::remove_var("RUST_ANYTHING_UNIT_META");
    std::env::remove_var("RUST_ANYTHING_UNIT_VOICE");
}

/// The sidecar contract forwards method, path, query, headers and
/// body verbatim when `SIDECAR_URL` is set.
#[tokio::test]
async fn sidecar_forwards_to_live_sidecar() {
    let mock = Router::new().route(
        "/api/v1/fatwa/ask",
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
                Json(json!({"method": parts.method.to_string(), "query": parts.uri.query().unwrap_or("").to_string(), "body": text})),
            )
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });
    let mut config = test_config();
    config.sidecar_url = Some(format!("http://{addr}"));
    let env = Env::sqlite_with(config).await;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/fatwa/ask?debug=1")
        .header("content-type", "application/json")
        .header("x-test", "hello")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(
            serde_json::to_string(&json!({"question": "hi"})).unwrap(),
        ))
        .unwrap();
    let (status, headers, bytes) = env.call_raw_full(request).await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(
        headers.get("x-echoed").and_then(|v| v.to_str().ok()),
        Some("hello")
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["method"], json!("POST"));
    assert_eq!(body["query"], json!("debug=1"));
    assert!(body["body"].as_str().unwrap().contains("hi"));
}

#[tokio::test]
async fn sidecar_unreachable_answers_503() {
    let mut config = test_config();
    config.sidecar_url = Some("http://127.0.0.1:9".to_string());
    let env = Env::sqlite_with(config).await;
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

// ============================ WebSocket ============================

#[tokio::test]
async fn ws_no_auth_rejected_4401() {
    let env = Env::sqlite().await;
    let addr = serve(env.router()).await;
    let mut client = WsClient::connect(addr, "/ws/events", &[]).await;
    assert_eq!(client.wait_close(Duration::from_secs(5)).await, Some(4401));
}

#[tokio::test]
async fn ws_token_query_rejected() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "tokq@example.com").await;
    let addr = serve(env.router()).await;
    // `?token=<valid JWT>` is never read — the handshake must fail.
    let target = format!("/ws/events?token={token}");
    let mut client = WsClient::connect(addr, &target, &[]).await;
    assert_eq!(client.wait_close(Duration::from_secs(5)).await, Some(4401));
}

#[tokio::test]
async fn ws_foreign_origin_rejected() {
    let mut config = test_config();
    config.cors_allow_origins = "https://app.example".to_string();
    let env = Env::sqlite_with(config).await;
    let (_, token, _) = register_user(&env, "origin@example.com").await;
    let ticket = ws_ticket(&env, &token).await;
    let addr = serve(env.router()).await;
    let target = format!("/ws/events?ticket={ticket}");
    let mut client = WsClient::connect(addr, &target, &[("Origin", "https://evil.example")]).await;
    assert_eq!(client.wait_close(Duration::from_secs(5)).await, Some(4401));
}

#[tokio::test]
async fn ws_ticket_subscribe_and_live_event() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "ws1@example.com").await;
    let ticket = ws_ticket(&env, &token).await;
    let addr = serve(env.router()).await;
    let target = format!("/ws/events?ticket={ticket}");
    let mut client = WsClient::connect(addr, &target, &[]).await;
    client
        .send_text(r#"{"type":"subscribe","channel":"user.*"}"#)
        .await;
    let first = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("subscribed");
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["type"],
        json!("subscribed")
    );
    // An event for this user is delivered live via the outbox poller.
    let event_id = insert_event(&env, "user.login", "user", &uid, Utc::now()).await;
    let got = wait_event(&mut client, "user.login", Duration::from_secs(5))
        .await
        .expect("live event");
    assert_eq!(got["event_id"], json!(event_id.to_string()));
    assert_eq!(got["aggregate_type"], json!("user"));
    // Another user's event is NOT delivered (privacy rule), even
    // under the matching `user.*` subscription.
    let other = Uuid::new_v4();
    insert_event(&env, "user.login", "user", &other.to_string(), Utc::now()).await;
    assert!(
        wait_event(&mut client, "user.login", Duration::from_secs(2))
            .await
            .is_none(),
        "another user's event must not be delivered"
    );
}

#[tokio::test]
async fn ws_bearer_auth_and_admin_channel_forbidden() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "ws2@example.com").await;
    let addr = serve(env.router()).await;
    let mut client = WsClient::connect(
        addr,
        "/ws/events",
        &[("Authorization", &format!("Bearer {token}"))],
    )
    .await;
    client
        .send_text(r#"{"type":"subscribe","channel":"admin.stats"}"#)
        .await;
    let first = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("reply");
    let value: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(value["type"], json!("error"));
    assert_eq!(value["code"], json!("forbidden"));
    // A wildcard subscription is fine without the admin permission.
    client
        .send_text(r#"{"type":"subscribe","channel":"*"}"#)
        .await;
    let second = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("reply");
    assert_eq!(
        serde_json::from_str::<Value>(&second).unwrap()["type"],
        json!("subscribed")
    );
}

#[tokio::test]
async fn ws_admin_can_subscribe_admin_channel() {
    let env = Env::sqlite().await;
    let (uid, token, _) = register_user(&env, "ws3@example.com").await;
    grant_permission(&env, &uid, "realtime.admin").await;
    // Permissions are loaded from the DB at WS handshake, so the
    // pre-grant token works.
    let ticket = ws_ticket(&env, &token).await;
    let addr = serve(env.router()).await;
    let target = format!("/ws/events?ticket={ticket}");
    let mut client = WsClient::connect(addr, &target, &[]).await;
    client
        .send_text(r#"{"type":"subscribe","channel":"admin.*"}"#)
        .await;
    let first = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("reply");
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["type"],
        json!("subscribed")
    );
}

#[tokio::test]
async fn ws_heartbeat_timeout_closes_4400() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "ws4@example.com").await;
    let ticket = ws_ticket(&env, &token).await;
    let addr = serve(env.router()).await;
    let target = format!("/ws/events?ticket={ticket}");
    let mut client = WsClient::connect(addr, &target, &[]).await;
    // Never answer the server's {"type":"ping"} — the connection must
    // close with 4400 (interval 1 s, timeout 2 s in test config).
    let mut saw_ping = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        assert!(!left.is_zero(), "no 4400 close within 10 s");
        match client.next_frame(left).await {
            Some(Frame::Text(t)) => {
                let value: Value = serde_json::from_str(&t).unwrap();
                if value["type"] == "ping" {
                    saw_ping = true;
                }
            }
            Some(Frame::Close(code)) => {
                assert!(saw_ping, "server must ping before closing");
                assert_eq!(code, 4400);
                return;
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn ws_resume_replay_from_query_and_live_message() {
    let env = Env::sqlite().await;
    let (_, token, _) = register_user(&env, "ws5@example.com").await;
    let base = Utc::now();
    let first = insert_event(&env, "probe.one", "probe", "p", base).await;
    let second = insert_event(
        &env,
        "probe.two",
        "probe",
        "p",
        base + chrono::Duration::seconds(1),
    )
    .await;
    let third = insert_event(
        &env,
        "probe.three",
        "probe",
        "p",
        base + chrono::Duration::seconds(2),
    )
    .await;
    // Handshake replay: ?last_event_id=<first> + channels=probe.*
    // replays two and three, in order, without repeating one. The
    // first frame is the {"type":"subscribed"} ack for the handshake
    // channel, then the replayed events follow.
    let ticket = ws_ticket(&env, &token).await;
    let addr = serve(env.router()).await;
    let target = format!("/ws/events?ticket={ticket}&channels=probe.*&last_event_id={first}");
    let mut client = WsClient::connect(addr, &target, &[]).await;
    let ack = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("subscribed ack");
    assert_eq!(
        serde_json::from_str::<Value>(&ack).unwrap()["type"],
        json!("subscribed")
    );
    let replayed = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("replay 1");
    let value: Value = serde_json::from_str(&replayed).unwrap();
    assert_eq!(value["type"], json!("event"));
    assert_eq!(value["event_id"], json!(second.to_string()));
    let replayed = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("replay 2");
    let value: Value = serde_json::from_str(&replayed).unwrap();
    assert_eq!(value["event_id"], json!(third.to_string()));
    // Live resume: replays nothing new (cursor is already at the
    // newest event) and answers {"type":"resumed"}.
    client
        .send_text(&format!(r#"{{"type":"resume","last_event_id":"{third}"}}"#))
        .await;
    let resumed = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("resumed");
    let value: Value = serde_json::from_str(&resumed).unwrap();
    assert_eq!(value["type"], json!("resumed"));
    assert_eq!(value["last_event_id"], json!(third.to_string()));
}

// ============================ Postgres smoke ============================

/// Read a `name <int>` gauge out of a Prometheus exposition body.
fn gauge(text: &str, name: &str) -> i64 {
    text.lines()
        .find(|l| l.starts_with(name) && l.as_bytes().get(name.len()) == Some(&b' '))
        .and_then(|l| l[name.len() + 1..].trim().parse::<i64>().ok())
        .unwrap_or(-1)
}

/// Critical-path replay against a local Postgres test DB.
/// Run with WAVE2_PG_URL set; skipped otherwise (like the wave2/3/4 smokes).
#[tokio::test]
async fn wave8_pg_smoke() {
    let Some(url) = std::env::var("WAVE2_PG_URL").ok() else {
        return;
    };
    let db = Db::connect(&url).await.expect("pg connect");
    // The smokes share one DB: drop every TEST_SCHEMA table first, like
    // the wave2/wave8/wave8_projects smokes — plain CREATE TABLE in
    // init_test_schema fails on existing tables, and the CREATE INDEX
    // for ix_tasks_related_entity has no IF NOT EXISTS.
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
    // Metrics gauges read real outbox rows on Postgres. The smokes share
    // one DB, so earlier binaries may have left pending rows behind:
    // assert the gauge grows by exactly one, not an absolute value.
    let (_, before_bytes) = env.call_raw(req(Method::GET, "/metrics", None)).await;
    let before = gauge(
        &String::from_utf8_lossy(&before_bytes),
        "outbox_pending_events",
    );
    insert_event(&env, "probe.pg", "probe", "pg", Utc::now()).await;
    let (status, bytes) = env.call_raw(req(Method::GET, "/metrics", None)).await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8_lossy(&bytes).to_string();
    assert_eq!(gauge(&text, "outbox_pending_events"), before + 1, "{text}");
    // Health + openapi on the PG-backed router.
    let (status, body) = env.call(req(Method::GET, "/api/v1/health", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["media_tools"]["ready"], json!(false));
    let (status, body) = env.call(req(Method::GET, "/openapi.json", None)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["paths"]["/ws/events"]["get"].is_object());
    // Live WS event on Postgres: register, ticket, subscribe, emit.
    let (uid, token, _) = register_user(&env, "wspg@example.com").await;
    let ticket = ws_ticket(&env, &token).await;
    let addr = serve(env.router()).await;
    let target = format!("/ws/events?ticket={ticket}");
    let mut client = WsClient::connect(addr, &target, &[]).await;
    client
        .send_text(r#"{"type":"subscribe","channel":"user.*"}"#)
        .await;
    let first = client
        .next_text(Duration::from_secs(5))
        .await
        .expect("subscribed");
    assert_eq!(
        serde_json::from_str::<Value>(&first).unwrap()["type"],
        json!("subscribed")
    );
    let event_id = insert_event(&env, "user.login", "user", &uid, Utc::now()).await;
    let got = wait_event(&mut client, "user.login", Duration::from_secs(5))
        .await
        .expect("live event on pg");
    assert_eq!(got["event_id"], json!(event_id.to_string()));
}
