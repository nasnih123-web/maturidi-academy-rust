//! Wave 9 PR-3 tests: real email/SMS, the hardened sidecar
//! forwarder, S3 storage (SigV4), and the upload-limit fix.
//!
//! * Mail/SMS (ADR-158): mock Resend/Kavenegar servers (in-process,
//!   never a real network call) prove the request shape, the error
//!   paths, and honest failure when unset. No live key exists here;
//!   a real delivery is a PR-4 staging proof.
//! * Sidecar (ADR-159): a mock sidecar proves forward-on-success for
//!   all 8 routes, honest 503s, per-route timeouts and body caps, and
//!   SSE streaming (first event arrives while the mock is still
//!   holding the stream open).
//! * Storage (ADR-162): SigV4 signatures are proven against an
//!   independent Python reference implementation (stdlib
//!   `hashlib`/`hmac` — the expected values are hardcoded from it),
//!   and the S3 wire shape against a mock S3 server.
//! * Uploads (ADR-161): a 3 MB avatar upload reaches its handler.
//!
//! Each test builds a fresh in-memory sqlite DB (`TEST_SCHEMA`) and a
//! fresh `AppState`, then drives the real Axum router with `oneshot`
//! (mock servers on bound `127.0.0.1:0` listeners for the HTTP
//! doubles). No Redis is needed: the test configs point at the dead
//! port 6399, like wave8_lift.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{Method, Request, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use maturidi_api_rs::{
    build_router, init_test_schema,
    mail::{MailError, Mailer},
    mail_templates::{
        public_url, render_password_reset_email, render_verification_email, resolve_email_locale,
    },
    sigv4::{presigned_url as sigv4_presigned_url, sign_request},
    storage::validate_storage_config,
    AppState, Config, Db,
};
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::net::TcpListener;
use tower::ServiceExt;

fn test_config() -> Config {
    Config {
        app_env: "test".to_string(),
        database_url: Some("sqlite://:memory:".to_string()),
        redis_url: "redis://127.0.0.1:6399/0".to_string(),
        jwt_secret: "test-jwt-secret-wave9-pr3".to_string(),
        encryption_key: "test-encryption-key-wave9-pr3".to_string(),
        captcha_enabled: false,
        storage_dir: format!(
            "{}/wave9-pr3-storage-{}",
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
        let state = AppState::with_db(config, Some(db.clone()));
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

// ============================ mail templates ============================

#[tokio::test]
async fn mail_templates_locale() {
    // Phone country decides: IR -> fa, Arab -> ar, anything else -> en.
    assert_eq!(resolve_email_locale(Some("+989123456789"), None), "fa");
    assert_eq!(resolve_email_locale(Some("0989123456789"), None), "fa");
    assert_eq!(resolve_email_locale(Some("+966512345678"), None), "ar");
    assert_eq!(resolve_email_locale(Some("+971501234567"), None), "ar");
    assert_eq!(resolve_email_locale(Some("+20 101 234 5678"), None), "ar");
    assert_eq!(resolve_email_locale(Some("+14155552671"), None), "en");
    assert_eq!(resolve_email_locale(Some("+44 20 7946 0958"), None), "en");
    // A resolved country never falls through to the stored locale.
    assert_eq!(resolve_email_locale(Some("+14155552671"), Some("fa")), "en");
    // No phone: stored locale when fa/ar/en, else en.
    assert_eq!(resolve_email_locale(None, Some("ar")), "ar");
    assert_eq!(resolve_email_locale(None, Some("fa")), "fa");
    assert_eq!(resolve_email_locale(None, Some("ur")), "en");
    assert_eq!(resolve_email_locale(None, None), "en");
}

#[tokio::test]
async fn mail_templates_verify_render() {
    let url = "https://academy.example.com/fa/verify-email?token=tok123";
    let rendered = render_verification_email("fa", url, "tok123", None, Some("حسین"));
    assert_eq!(rendered.subject, "تأیید ایمیل — آکادمی ماتریدی");
    assert!(rendered.text.contains(url));
    assert!(rendered.text.contains("verify_token=tok123"));
    assert!(rendered.text.contains("سلام حسین،"));
    assert!(rendered.html.contains("تأیید ایمیل"));
    assert!(rendered.html.contains(url));
    assert!(rendered.html.contains("#07231c"));
    // Unknown locale falls back to English; ar has its own subject.
    let en = render_verification_email("xx", url, "tok123", None, None);
    assert_eq!(en.subject, "Verify your email — Maturidi Academy");
    let ar = render_verification_email("ar", url, "tok123", None, None);
    assert_eq!(ar.subject, "تأكيد البريد الإلكتروني — أكاديمية ماتريدي");
    // Reset uses its own copy, not the verify copy.
    let reset = render_password_reset_email("fa", url, "tok123", None, None);
    assert_eq!(reset.subject, "بازنشانی رمز عبور — آکادمی ماتریدی");
    assert!(reset.text.contains("reset_token=tok123"));
}

#[tokio::test]
async fn mail_public_url_validation() {
    assert_eq!(
        public_url("https://academy.example.com", "/fa/verify-email?token=x"),
        Some("https://academy.example.com/fa/verify-email?token=x".to_string())
    );
    assert_eq!(
        public_url("https://academy.example.com/", "fa/verify-email"),
        Some("https://academy.example.com/fa/verify-email".to_string())
    );
    assert_eq!(public_url("ftp://example.com", "/x"), None);
    assert_eq!(public_url("https://user:pass@example.com", "/x"), None);
    assert_eq!(public_url("https:///x", "/x"), None);
    assert_eq!(public_url("", "/x"), None);
}

// ============================ mail transport ============================

/// Captured mock-Resend request.
#[derive(Debug, Default)]
struct CapturedResend {
    auth: Option<String>,
    body: Option<Value>,
}

fn mailer_with_mocks(resend_url: &str, kavenegar_url: &str) -> (Mailer, Config) {
    let mut config = test_config();
    config.resend_api_key = Some("test-resend-key".to_string());
    config.smtp_from = "noreply@matoridi.ir".to_string();
    config.sms_api_key = Some("test-kavenegar-key".to_string());
    config.sms_sender = "1000".to_string();
    let mailer = Mailer::with_endpoints(&config, resend_url.to_string(), kavenegar_url.to_string());
    (mailer, config)
}

async fn mock_resend(captured: Arc<Mutex<CapturedResend>>, status: StatusCode) -> SocketAddr {
    let router = Router::new().route(
        "/emails",
        post(move |req: Request<Body>| {
            let captured = captured.clone();
            async move {
                let (parts, body) = req.into_parts();
                let auth = parts
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let value: Value = serde_json::from_slice(&bytes).unwrap();
                *captured.lock().unwrap() = CapturedResend {
                    auth,
                    body: Some(value),
                };
                (status, Json(json!({"id": "mock-id"}))).into_response()
            }
        }),
    );
    serve(router).await
}

#[tokio::test]
async fn mail_resend_success_shape() {
    let captured = Arc::new(Mutex::new(CapturedResend::default()));
    let addr = mock_resend(captured.clone(), StatusCode::OK).await;
    let (mailer, _config) =
        mailer_with_mocks(&format!("http://{addr}/emails"), "http://127.0.0.1:9/x");
    mailer
        .send_email(
            "user@example.com",
            "تأیید ایمیل",
            "hello text",
            "<p>hello</p>",
        )
        .await
        .expect("resend ok");
    // Drop the guard before the next awaits: holding a std
    // `MutexGuard` across `.await` trips clippy's await_holding_lock.
    let (auth, body) = {
        let cap = captured.lock().unwrap();
        (cap.auth.clone(), cap.body.clone().expect("captured body"))
    };
    assert_eq!(auth.as_deref(), Some("Bearer test-resend-key"));
    assert_eq!(body["from"], json!("noreply@matoridi.ir"));
    assert_eq!(body["to"], json!(["user@example.com"]));
    assert_eq!(body["subject"], json!("تأیید ایمیل"));
    assert_eq!(body["text"], json!("hello text"));
    assert_eq!(body["html"], json!("<p>hello</p>"));
    // Empty html is omitted from the payload, like Python's `if body_html`.
    let captured2 = Arc::new(Mutex::new(CapturedResend::default()));
    let addr2 = mock_resend(captured2.clone(), StatusCode::OK).await;
    let (mailer2, _) = mailer_with_mocks(&format!("http://{addr2}/emails"), "http://127.0.0.1:9/x");
    mailer2
        .send_email("user@example.com", "s", "t", "")
        .await
        .expect("resend ok without html");
    let body2 = captured2.lock().unwrap().body.clone().unwrap();
    assert!(body2.get("html").is_none(), "empty html omitted: {body2}");
}

#[tokio::test]
async fn mail_resend_non2xx_is_error_without_secret() {
    let captured = Arc::new(Mutex::new(CapturedResend::default()));
    let addr = mock_resend(captured.clone(), StatusCode::INTERNAL_SERVER_ERROR).await;
    let (mailer, _) = mailer_with_mocks(&format!("http://{addr}/emails"), "http://127.0.0.1:9/x");
    let err = mailer
        .send_email("user@example.com", "s", "t", "")
        .await
        .expect_err("500 must be an error, never a fake success");
    let msg = err.to_string();
    assert!(msg.contains("http=500"), "status only: {msg}");
    assert!(
        !msg.contains("test-resend-key"),
        "key never in error: {msg}"
    );
    assert!(matches!(err, MailError::Delivery(_)));
}

#[tokio::test]
async fn mail_unset_is_not_configured() {
    let config = test_config();
    assert!(config.resend_api_key.is_none());
    assert!(config.smtp_host.is_empty());
    assert!(config.sms_api_key.is_none());
    let mailer = Mailer::from_config(&config);
    assert!(!mailer.email_configured());
    assert!(!mailer.sms_configured());
    let err = mailer
        .send_email("user@example.com", "s", "t", "")
        .await
        .expect_err("unset email must fail honestly");
    assert!(matches!(err, MailError::NotConfigured(_)));
    let err = mailer
        .send_sms("+989123456789", "code")
        .await
        .expect_err("unset sms must fail honestly");
    assert!(matches!(err, MailError::NotConfigured(_)));
}

#[tokio::test]
async fn mail_smtp_unreachable_is_error() {
    // SMTP is configured but the host is dead: the send must attempt a
    // real connection and fail honestly — never log-and-pretend.
    let mut config = test_config();
    config.smtp_host = "127.0.0.1".to_string();
    config.smtp_port = 9;
    config.smtp_from = "noreply@matoridi.ir".to_string();
    let mailer = Mailer::from_config(&config);
    assert!(mailer.email_configured());
    let err = mailer
        .send_email("user@example.com", "s", "t", "")
        .await
        .expect_err("dead SMTP host must be an error");
    assert!(matches!(err, MailError::Delivery(_)));
}

/// Captured mock-Kavenegar form fields.
#[derive(Debug, Default)]
struct CapturedSms {
    fields: Vec<(String, String)>,
}

async fn mock_kavenegar(
    captured: Arc<Mutex<CapturedSms>>,
    http_status: StatusCode,
    api_status: i64,
) -> SocketAddr {
    let router = Router::new().route(
        "/v1/{key}/sms/send.json",
        post(move |req: Request<Body>| {
            let captured = captured.clone();
            async move {
                let (_parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let text = String::from_utf8_lossy(&bytes).to_string();
                let mut fields = Vec::new();
                for pair in text.split('&') {
                    if let Some((k, v)) = pair.split_once('=') {
                        // Minimal form decoding: reqwest percent-encodes
                        // a literal `+` as `%2B` (a bare `+` means
                        // space), so `%2B` must decode last, after the
                        // `+` → space replacement.
                        let decode = |s: &str| {
                            s.replace('+', " ")
                                .replace("%3A", ":")
                                .replace("%20", " ")
                                .replace("%2B", "+")
                        };
                        fields.push((k.to_string(), decode(v)));
                    }
                }
                *captured.lock().unwrap() = CapturedSms { fields };
                (
                    http_status,
                    Json(json!({"return": {"status": api_status, "message": "ok"}})),
                )
                    .into_response()
            }
        }),
    );
    serve(router).await
}

#[tokio::test]
async fn mail_kavenegar_success_and_bad_status() {
    let captured = Arc::new(Mutex::new(CapturedSms::default()));
    let addr = mock_kavenegar(captured.clone(), StatusCode::OK, 200).await;
    let (mailer, _) = mailer_with_mocks(
        "http://127.0.0.1:9/x",
        &format!("http://{addr}/v1/{{api_key}}/sms/send.json"),
    );
    mailer
        .send_sms("+989123456789", "کد تأیید آکادمی ماتریدی: 123456")
        .await
        .expect("kavenegar 200/200 ok");
    let fields = captured.lock().unwrap().fields.clone();
    let get = |k: &str| {
        fields
            .iter()
            .find(|(n, _)| n == k)
            .map(|(_, v)| v.clone())
            .unwrap_or_default()
    };
    assert_eq!(get("receptor"), "+989123456789");
    assert_eq!(get("sender"), "1000");
    assert!(get("message").contains("123456"), "message: {fields:?}");

    // HTTP 200 with a non-200 return.status is a delivery failure —
    // the fake-success guard from Python's sms.py, ported.
    let captured2 = Arc::new(Mutex::new(CapturedSms::default()));
    let addr2 = mock_kavenegar(captured2.clone(), StatusCode::OK, 411).await;
    let (mailer2, _) = mailer_with_mocks(
        "http://127.0.0.1:9/x",
        &format!("http://{addr2}/v1/{{api_key}}/sms/send.json"),
    );
    let err = mailer2
        .send_sms("+989123456789", "code")
        .await
        .expect_err("api_status 411 must be an error");
    assert!(err.to_string().contains("411"), "err: {err}");

    // HTTP 500 is an error too.
    let captured3 = Arc::new(Mutex::new(CapturedSms::default()));
    let addr3 = mock_kavenegar(captured3.clone(), StatusCode::INTERNAL_SERVER_ERROR, 200).await;
    let (mailer3, _) = mailer_with_mocks(
        "http://127.0.0.1:9/x",
        &format!("http://{addr3}/v1/{{api_key}}/sms/send.json"),
    );
    mailer3
        .send_sms("+989123456789", "code")
        .await
        .expect_err("http 500 must be an error");
}

#[tokio::test]
async fn mail_redact_secrets() {
    use maturidi_api_rs::mail::redact_secrets;
    assert_eq!(
        redact_secrets("reset: https://x/fa/reset-password?token=abc123def"),
        "reset: https://x/fa/reset-password?token=[REDACTED]"
    );
    assert_eq!(redact_secrets("کد تأیید: 123456"), "کد تأیید: [CODE]");
    assert_eq!(
        redact_secrets("plain text, no secrets"),
        "plain text, no secrets"
    );
}

// ============================ sidecar ============================

const SIDECAR_ROUTES: &[(&str, &str, &str)] = &[
    ("POST", "/api/v1/fatwa/ask", "fatwa_ask_unavailable"),
    ("GET", "/api/v1/finance/receipts/11111111-1111-1111-1111-111111111111", "receipt_pdf_unavailable"),
    ("POST", "/api/v1/quran/hifz/attempts/11111111-1111-1111-1111-111111111111/grade", "hifz_asr_unavailable"),
    ("POST", "/api/v1/quran/hifz/attempts/11111111-1111-1111-1111-111111111111/grade-word", "hifz_asr_unavailable"),
    ("POST", "/api/v1/quran/hifz/tests/11111111-1111-1111-1111-111111111111/questions/11111111-1111-1111-1111-111111111111/answer", "hifz_asr_unavailable"),
    ("POST", "/api/v1/quran/hifz/voice-search", "hifz_asr_unavailable"),
    ("POST", "/api/v1/voice/transcribe", "voice_stt_unavailable"),
    ("POST", "/api/v1/voice/ask", "voice_stt_unavailable"),
];

fn sidecar_req(method: &str, uri: &str) -> Request<Body> {
    let m = Method::from_bytes(method.as_bytes()).unwrap();
    let builder = Request::builder()
        .method(m)
        .uri(uri)
        .header("content-type", "application/json")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))));
    if method == "GET" {
        builder.body(Body::empty()).unwrap()
    } else {
        builder
            .body(Body::from(
                serde_json::to_string(&json!({"probe": true})).unwrap(),
            ))
            .unwrap()
    }
}

async fn mock_sidecar_ok() -> (SocketAddr, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new();
    for (method, path, _) in SIDECAR_ROUTES.iter().copied() {
        let hits = hits.clone();
        let path = path.to_string();
        let handler_path = path.clone();
        let handler = move |req: Request<Body>| {
            let hits = hits.clone();
            let path = handler_path.clone();
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                let (_parts, body) = req.into_parts();
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                (
                    StatusCode::OK,
                    Json(json!({"route": path, "bytes": bytes.len()})),
                )
                    .into_response()
            }
        };
        router = match method {
            "GET" => router.route(&path, get(handler)),
            _ => router.route(&path, post(handler)),
        };
    }
    let addr = serve(router).await;
    (addr, hits)
}

#[tokio::test]
async fn sidecar_forwards_all_eight() {
    let (addr, hits) = mock_sidecar_ok().await;
    let mut config = test_config();
    config.sidecar_url = Some(format!("http://{addr}"));
    let env = Env::sqlite_with(config).await;
    for (method, path, _) in SIDECAR_ROUTES {
        let (status, body) = env.call(sidecar_req(method, path)).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {body}");
        assert_eq!(body["route"], json!(*path));
    }
    assert_eq!(hits.load(Ordering::SeqCst), 8);
}

#[tokio::test]
async fn sidecar_unset_503_all_routes() {
    let env = Env::sqlite().await;
    for (method, path, code) in SIDECAR_ROUTES {
        let (status, body) = env.call(sidecar_req(method, path)).await;
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{method} {path}: {body}"
        );
        assert_eq!(err_code(&body), *code);
    }
}

#[tokio::test]
async fn sidecar_unreachable_503() {
    let mut config = test_config();
    config.sidecar_url = Some("http://127.0.0.1:9".to_string());
    let env = Env::sqlite_with(config).await;
    let (status, body) = env.call(sidecar_req("POST", "/api/v1/fatwa/ask")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "fatwa_ask_unavailable");
}

#[tokio::test]
async fn sidecar_timeout_503() {
    // Mock sleeps 3 s; the route timeout is overridden to 1 s, so the
    // forwarder must 503 instead of waiting for the sidecar.
    let router = Router::new().route(
        "/api/v1/fatwa/ask",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(3)).await;
            Json(json!({"answer": "too late"})).into_response()
        }),
    );
    let addr = serve(router).await;
    let mut config = test_config();
    config.sidecar_url = Some(format!("http://{addr}"));
    config.sidecar_timeout_secs = 1;
    let env = Env::sqlite_with(config).await;
    let (status, body) = env.call(sidecar_req("POST", "/api/v1/fatwa/ask")).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(err_code(&body), "fatwa_ask_unavailable");
}

#[tokio::test]
async fn sidecar_fatwa_body_cap() {
    // 2 MB JSON to fatwa/ask: the forwarder cap is 1 MB, so this 413s
    // before any upstream call — the mock must never be hit.
    let (addr, hits) = mock_sidecar_ok().await;
    let mut config = test_config();
    config.sidecar_url = Some(format!("http://{addr}"));
    let env = Env::sqlite_with(config).await;
    let big = "x".repeat(2 * 1024 * 1024);
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/fatwa/ask")
        .header("content-type", "application/json")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(format!("{{\"question\": \"{big}\"}}")))
        .unwrap();
    let (status, body) = env.call(request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(err_code(&body), "request_too_large");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "over-cap never forwarded");
}

#[tokio::test]
async fn sidecar_audio_body_cap() {
    // 33 MB to a hifz grade route: over the 32 MB audio cap, 413 with
    // the Python-equivalent code — no sidecar needed, the cap is
    // enforced by the forwarder.
    let env = Env::sqlite().await;
    let big = vec![0u8; 33 * 1024 * 1024];
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/quran/hifz/attempts/11111111-1111-1111-1111-111111111111/grade")
        .header("content-type", "application/octet-stream")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(big))
        .unwrap();
    let (status, body) = env.call(request).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    assert_eq!(err_code(&body), "hifz_audio_too_large");
}

#[tokio::test]
async fn sidecar_audio_3mb_reaches_sidecar() {
    // 3 MB to a hifz grade route with a live mock: before PR-3 the
    // axum router's 2 MB default body limit 413'd this; after PR-3
    // the route's own 32 MB cap governs and the bytes arrive.
    let (addr, _hits) = mock_sidecar_ok().await;
    let mut config = test_config();
    config.sidecar_url = Some(format!("http://{addr}"));
    let env = Env::sqlite_with(config).await;
    let big = vec![7u8; 3 * 1024 * 1024];
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/quran/hifz/attempts/11111111-1111-1111-1111-111111111111/grade")
        .header("content-type", "application/octet-stream")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(big))
        .unwrap();
    let (status, body) = env.call(request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["bytes"], json!(3 * 1024 * 1024));
}

#[tokio::test]
async fn sidecar_sse_streams() {
    // Mock SSE: event 1 immediately, then an 800 ms pause, then
    // event 2 + [DONE]. The first chunk must arrive well before the
    // pause ends — if the forwarder buffered the whole response, it
    // would only arrive after the mock finished.
    use futures_util::StreamExt;
    let sse = Router::new().route(
        "/api/v1/fatwa/ask",
        post(|| async {
            let stream = futures_util::stream::unfold(0u8, |state| async move {
                match state {
                    0 => Some((
                        Ok::<_, std::io::Error>(axum::body::Bytes::from(
                            "data: {\"event\": 1}\n\n",
                        )),
                        1,
                    )),
                    1 => {
                        tokio::time::sleep(Duration::from_millis(800)).await;
                        Some((
                            Ok::<_, std::io::Error>(axum::body::Bytes::from(
                                "data: {\"event\": 2}\n\ndata: [DONE]\n\n",
                            )),
                            2,
                        ))
                    }
                    _ => None,
                }
            });
            (
                StatusCode::OK,
                [("content-type", "text/event-stream")],
                Body::from_stream(stream),
            )
                .into_response()
        }),
    );
    let addr = serve(sse).await;
    let mut config = test_config();
    config.sidecar_url = Some(format!("http://{addr}"));
    let env = Env::sqlite_with(config).await;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/fatwa/ask?stream=true")
        .header("content-type", "application/json")
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from("{\"question\": \"hi\"}"))
        .unwrap();
    let res = env.router().oneshot(request).await.expect("oneshot");
    assert_eq!(res.status(), StatusCode::OK);
    assert!(
        res.headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .contains("text/event-stream"),
        "sse content-type preserved"
    );
    let mut stream = res.into_body().into_data_stream();
    let first = tokio::time::timeout(Duration::from_millis(500), stream.next())
        .await
        .expect("first SSE chunk must arrive before the mock's 800 ms pause")
        .expect("chunk present")
        .expect("chunk ok");
    assert!(
        String::from_utf8_lossy(&first).contains("\"event\": 1"),
        "first chunk: {}",
        String::from_utf8_lossy(&first)
    );
    let mut rest = Vec::new();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("stream must finish")
    {
        rest.extend_from_slice(&chunk.expect("chunk ok"));
    }
    let rest_text = String::from_utf8_lossy(&rest).to_string();
    assert!(rest_text.contains("\"event\": 2"), "rest: {rest_text}");
    assert!(rest_text.contains("[DONE]"), "rest: {rest_text}");
}

// ============================ storage / SigV4 ============================

// AWS's published example keys (not real secrets) — the reference
// signatures below were computed with an independent Python
// implementation (stdlib hashlib/hmac only, /tmp/sigv4-ref.py).
const TEST_AK: &str = "AKIAIOSFODNN7EXAMPLE";
const TEST_SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const TEST_REGION: &str = "us-east-1";
const TEST_AMZ_DATE: &str = "20130524T000000Z";
const TEST_DATE_STAMP: &str = "20130524";

#[tokio::test]
async fn sigv4_header_signing_matches_reference() {
    let (auth, date, payload_hash) = sign_request(
        "GET",
        "http://127.0.0.1:9000",
        "maturidi-media",
        "library/abc.pdf",
        TEST_AK,
        TEST_SK,
        TEST_REGION,
        &[],
        TEST_AMZ_DATE,
        TEST_DATE_STAMP,
    );
    assert_eq!(date, TEST_AMZ_DATE);
    assert_eq!(
        payload_hash,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        auth,
        "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature=758086ebb19e6efeee7bfd484ca6d55323196d2bbcd3166d8b543ee0c7312159"
    );
    let (auth_put, _, _) = sign_request(
        "PUT",
        "http://127.0.0.1:9000",
        "maturidi-media",
        "library/abc.pdf",
        TEST_AK,
        TEST_SK,
        TEST_REGION,
        b"hello",
        TEST_AMZ_DATE,
        TEST_DATE_STAMP,
    );
    assert!(
        auth_put.ends_with(
            "Signature=c95032c584f350e641271fb98100193be472acf1cec63b53940acaddeb35e9eb"
        ),
        "PUT auth: {auth_put}"
    );
}

#[tokio::test]
async fn sigv4_presigned_url_matches_reference() {
    let url = sigv4_presigned_url(
        "https://files.example.com",
        "maturidi-media",
        "library/abc.pdf",
        TEST_AK,
        TEST_SK,
        TEST_REGION,
        300,
        TEST_AMZ_DATE,
        TEST_DATE_STAMP,
    );
    assert_eq!(
        url,
        "https://files.example.com/maturidi-media/library/abc.pdf?X-Amz-Algorithm=AWS4-HMAC-SHA256&X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20130524%2Fus-east-1%2Fs3%2Faws4_request&X-Amz-Date=20130524T000000Z&X-Amz-Expires=300&X-Amz-SignedHeaders=host&X-Amz-Signature=9057abf77c871e9d09685cc937f412777044e2cabfe6249221e27d0825d43ddc"
    );
}

#[tokio::test]
async fn storage_s3_mock_roundtrip() {
    // A mock S3: asserts the SigV4 Authorization header is present
    // and well-formed, then behaves like a bucket.
    use maturidi_api_rs::storage::{delete_object, put_object, read_bytes};
    let seen_auth = Arc::new(Mutex::new(Vec::<String>::new()));
    let store = Arc::new(Mutex::new(
        std::collections::HashMap::<String, Vec<u8>>::new(),
    ));
    let router = Router::new().route(
        "/{bucket}/{*key}",
        get({
            let seen_auth = seen_auth.clone();
            let store = store.clone();
            move |req: Request<Body>| {
                let seen_auth = seen_auth.clone();
                let store = store.clone();
                async move {
                    let (parts, _body) = req.into_parts();
                    let auth = parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen_auth.lock().unwrap().push(auth);
                    let key = parts.uri.path().to_string();
                    match store.lock().unwrap().get(&key) {
                        Some(bytes) => (StatusCode::OK, Body::from(bytes.clone())).into_response(),
                        None => (StatusCode::NOT_FOUND, Body::empty()).into_response(),
                    }
                }
            }
        })
        .put({
            let seen_auth = seen_auth.clone();
            let store = store.clone();
            move |req: Request<Body>| {
                let seen_auth = seen_auth.clone();
                let store = store.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let auth = parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen_auth.lock().unwrap().push(auth);
                    let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                    let key = parts.uri.path().to_string();
                    store.lock().unwrap().insert(key, bytes.to_vec());
                    (StatusCode::OK, Body::empty()).into_response()
                }
            }
        })
        .delete({
            let seen_auth = seen_auth.clone();
            let store = store.clone();
            move |req: Request<Body>| {
                let seen_auth = seen_auth.clone();
                let store = store.clone();
                async move {
                    let (parts, _body) = req.into_parts();
                    let auth = parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    seen_auth.lock().unwrap().push(auth);
                    let key = parts.uri.path().to_string();
                    store.lock().unwrap().remove(&key);
                    (StatusCode::OK, Body::empty()).into_response()
                }
            }
        }),
    );
    let addr = serve(router).await;
    let mut config = test_config();
    config.storage_provider = "s3".to_string();
    config.s3_endpoint = format!("http://{addr}");
    config.s3_endpoint_public = format!("http://{addr}");
    config.s3_access_key = TEST_AK.to_string();
    config.s3_secret_key = TEST_SK.to_string();
    config.s3_bucket = "maturidi-media".to_string();
    config.s3_region = TEST_REGION.to_string();
    put_object(&config, "library/test.pdf", b"pdf-bytes")
        .await
        .expect("s3 put");
    let back = read_bytes(&config, "library/test.pdf")
        .await
        .expect("s3 get");
    assert_eq!(back, b"pdf-bytes");
    delete_object(&config, "library/test.pdf")
        .await
        .expect("s3 delete");
    assert!(read_bytes(&config, "library/test.pdf").await.is_err());
    let auths = seen_auth.lock().unwrap();
    assert_eq!(auths.len(), 4, "one signed call per op: {auths:?}");
    for auth in auths.iter() {
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/"),
            "auth: {auth}"
        );
        assert!(auth.contains("SignedHeaders=host;x-amz-content-sha256;x-amz-date"));
    }
    // Presigned URLs use the PUBLIC endpoint, never the internal one.
    let url = maturidi_api_rs::storage::presigned_url(
        &config,
        "library/test.pdf",
        300,
        "/api/v1/library/download-by-ref",
        "library",
    )
    .expect("presigned");
    assert!(url.starts_with(&format!("http://{addr}/maturidi-media/library/test.pdf?")));
    assert!(url.contains("X-Amz-Algorithm=AWS4-HMAC-SHA256"));
    assert!(url.contains("X-Amz-Signature="));
}

#[tokio::test]
async fn storage_validate_production() {
    let mut config = test_config();
    config.app_env = "production".to_string();
    config.storage_provider = "s3".to_string();
    config.s3_endpoint = "http://minio:9000".to_string();
    config.s3_endpoint_public = String::new();
    config.s3_access_key = "minioadmin".to_string();
    config.s3_secret_key = "minioadmin".to_string();
    config.s3_bucket = "maturidi-media".to_string();
    let problems = validate_storage_config(&config);
    assert!(
        problems.iter().any(|p| p.contains("s3_secret_key")),
        "weak secret flagged: {problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("s3_access_key")),
        "weak access key flagged: {problems:?}"
    );
    assert!(
        problems.iter().any(|p| p.contains("s3_endpoint")),
        "internal presigned host flagged: {problems:?}"
    );
    for p in &problems {
        assert!(!p.contains("minioadmin"), "no secret value in message: {p}");
    }
    // A good production config passes.
    config.s3_endpoint = "https://s3.example.com".to_string();
    config.s3_endpoint_public = "https://files.example.com".to_string();
    config.s3_access_key = "AKIAIOSFODNN7EXAMPLE".to_string();
    config.s3_secret_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".to_string();
    config.storage_signing_secret =
        "a-very-long-random-signing-secret-value-0123456789".to_string();
    assert!(
        validate_storage_config(&config).is_empty(),
        "{:?}",
        validate_storage_config(&config)
    );
    // In dev/test the same bad config is not a boot refusal.
    config.app_env = "dev".to_string();
    config.s3_secret_key = "minioadmin".to_string();
    assert!(validate_storage_config(&config).is_empty());
}

#[tokio::test]
async fn storage_local_still_works() {
    use maturidi_api_rs::storage::{delete_object, put_object, read_bytes};
    let config = test_config();
    put_object(&config, "library/local.pdf", b"local-bytes")
        .await
        .expect("local put");
    let back = read_bytes(&config, "library/local.pdf")
        .await
        .expect("local get");
    assert_eq!(back, b"local-bytes");
    delete_object(&config, "library/local.pdf")
        .await
        .expect("local delete");
    assert!(read_bytes(&config, "library/local.pdf").await.is_err());
}

// ============================ uploads ============================

#[tokio::test]
async fn upload_avatar_3mb_reaches_handler() {
    // 3 MB PNG-magic body: before PR-3 the axum router's 2 MB default
    // body limit 413'd it; after PR-3 the handler's 5 MB cap governs.
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "avatar3mb@example.com").await;
    let mut body = Vec::with_capacity(3 * 1024 * 1024);
    body.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    body.resize(3 * 1024 * 1024, 0);
    let boundary = "----pr3boundary";
    let mut multipart = Vec::new();
    multipart.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    multipart.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n",
    );
    multipart.extend_from_slice(&body);
    multipart.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/users/me/avatar")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {access}"))
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(multipart))
        .unwrap();
    let (status, value) = env.call(request).await;
    assert_eq!(status, StatusCode::OK, "3 MB avatar: {value}");
}

#[tokio::test]
async fn upload_avatar_eicar_rejected() {
    // The ADR-051 scanner applies to avatars too (ADR-164): an EICAR
    // body is 422 file_infected, never stored.
    let env = Env::sqlite().await;
    let (_uid, access, _refresh) = register_user(&env, "avatareicar@example.com").await;
    let eicar = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";
    let mut body = Vec::new();
    body.extend_from_slice(b"\x89PNG\r\n\x1a\n");
    body.extend_from_slice(eicar);
    let boundary = "----pr3boundary2";
    let mut multipart = Vec::new();
    multipart.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    multipart.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"a.png\"\r\nContent-Type: image/png\r\n\r\n",
    );
    multipart.extend_from_slice(&body);
    multipart.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let request = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/users/me/avatar")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {access}"))
        .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 43111))))
        .body(Body::from(multipart))
        .unwrap();
    let (status, value) = env.call(request).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "EICAR avatar: {value}"
    );
    assert_eq!(err_code(&value), "file_infected");
}
