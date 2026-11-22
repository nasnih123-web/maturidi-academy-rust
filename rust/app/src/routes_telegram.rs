//! Telegram routes — mirrors `app/modules/telegram/router.py`,
//! `connect_service.py`, `distribute_service.py` and `bot_service.py`
//! (Wave 7).
//!
//! What is native here:
//! - `/webhook` — the Telegram secret is verified exactly like
//!   Python (`X-Telegram-Bot-Api-Secret-Token` against the
//!   `telegram.webhook_secret` setting, constant-time compare) before
//!   anything else happens; non-`tg:` updates are ignored with
//!   `{"ok": true, "ignored": true}`; `tg:` callbacks run the full
//!   one-shot / expiry / HMAC checks and the fatwa-submission
//!   transition through the authorised path.
//! - `/status/public`, `/connect/status`, `/connect/disconnect`,
//!   `/distribute/policies`, `/distribute/log` — pure settings/DB
//!   reads and writes, fully native.
//! - `/distribute/publish`, `/distribute/retry`, `/import` — rows
//!   and audits are written exactly like Python, but there is no ARQ
//!   worker in the Rust binary, so nothing is ever enqueued
//!   (`enqueued: false`, `error: "worker unavailable — retry later"`)
//!   — the honest Redis-down path Python itself takes.
//! - `/distribute/approve` runs `send_pending`; `/distribute/revoke`
//!   runs the delete path. Both degrade honestly when the bot cannot
//!   send (see below).
//!
//! What is NOT native (see SYN-02 / SYN-03 in the wave report):
//! - The Telethon MTProto client (connect code request/confirm) and
//!   the python-telegram-bot runtime (sending, deleting, answering
//!   callback queries) do not exist in the Rust binary. Code
//!   request/confirm validate everything locally, then answer an
//!   honest 503 `telegram_unavailable`; sends fail exactly the way
//!   Python fails them with no token (`failed` / `bot unavailable
//!   or send failed`), never with a fabricated message id.
//! - The Wave 7 admin control-centre distribution gate
//!   (`_telegram_allowed`) is not ported; Python fails open on a
//!   gate error, and Rust fails open the same way.

use std::net::SocketAddr;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission},
    crypto::hmac_sha256_hex,
    db::{utc_now, Arg, Db, IntoArg},
    errors::{ApiError, ApiResult},
    settings::{get_setting, get_typed_setting},
    validate::{
        as_bool, as_int, opt_dict, opt_str_list, opt_str_raw, opt_str_raw_max, parse_body,
        parse_datetime_str, parse_path_uuid, parse_query, query_int, req_str,
    },
    workflow::validate_transition,
    AppState,
};

const POLICIES: [&str; 4] = ["never", "manual_approval", "auto_publish", "scheduled"];

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Constant-time string equality (like `hmac.compare_digest`).
fn const_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Python truthiness of a JSON value (for the `title.get(...) or ...`
/// locale fallback in `fetch_content`).
fn py_truthy_str(value: &Value) -> Option<String> {
    match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        Value::Bool(true) => Some("True".to_string()),
        _ => None,
    }
}

fn first_locale_text(v: &Value) -> String {
    let obj = match v.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    for loc in ["fa", "ar", "en"] {
        if let Some(s) = obj.get(loc).and_then(py_truthy_str) {
            return s;
        }
    }
    String::new()
}

/// Authenticate + require `telegram.manage` (the `_admin` dependency).
async fn admin_ctx(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::authn::AuthCtx, ApiError> {
    let ctx = authenticate(state, headers, "/api/v1/telegram").await?;
    require_permission(state, &ctx, "telegram.manage").await?;
    Ok(ctx)
}

fn audit_ip_ua(
    state: &AppState,
    headers: &HeaderMap,
    peer: SocketAddr,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

// --- bot service (graceful degradation) --------------------------------------

/// `bot_enabled` — `telegram.enabled` (typed) AND a non-empty
/// `telegram.bot_token`, exactly like Python.
async fn bot_enabled(db: &Db, config: &crate::Config) -> Result<bool, ApiError> {
    let enabled = get_typed_setting(db, config, "telegram.enabled", json!(false)).await?;
    if !as_bool(&enabled).unwrap_or(false) {
        return Ok(false);
    }
    let token = get_setting(db, config, "telegram.bot_token").await?;
    Ok(token.is_some_and(|t| !t.trim().is_empty()))
}

async fn default_channel(db: &Db, config: &crate::Config) -> Result<Option<String>, ApiError> {
    let channel = get_setting(db, config, "telegram.channel").await?;
    Ok(channel.and_then(|c| {
        let c = c.trim().to_string();
        if c.is_empty() {
            None
        } else {
            Some(c)
        }
    }))
}

/// Sending a message needs the python-telegram-bot runtime, which is
/// not in the Rust binary (SYN-03): always `None`, exactly like
/// Python with no token configured.
fn send_message_unavailable() -> Option<i64> {
    None
}

/// Deleting a message needs the bot runtime: always `false`.
fn delete_message_unavailable() -> bool {
    false
}

// --- webhook -----------------------------------------------------------------

/// `verify_webhook_secret` — the expected secret comes from the
/// `telegram.webhook_secret` setting; missing on either side fails.
async fn verify_webhook_secret(
    db: &Db,
    config: &crate::Config,
    provided: Option<&str>,
) -> Result<bool, ApiError> {
    let expected = get_setting(db, config, "telegram.webhook_secret").await?;
    let Some(expected) = expected else {
        return Ok(false);
    };
    let Some(provided) = provided else {
        return Ok(false);
    };
    if expected.is_empty() || provided.is_empty() {
        return Ok(false);
    }
    Ok(const_time_eq(provided, &expected))
}

/// `POST /api/v1/telegram/webhook` — no user auth; the Telegram
/// secret header is the only credential. Invalid JSON → 400
/// `bad_update`; anything without a `tg:` callback is ignored.
pub async fn telegram_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let secret = headers
        .get("x-telegram-bot-api-secret-token")
        .and_then(|v| v.to_str().ok());
    if !verify_webhook_secret(db, &state.config, secret).await? {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "bad_webhook_secret",
            "امضای وبهوک نامعتبر است",
        ));
    }
    let update: Value = serde_json::from_slice(&body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_update",
            "بدنهٔ وبهوک نامعتبر است",
        )
    })?;
    let callback = update.get("callback_query").cloned().unwrap_or(Value::Null);
    let data = callback.get("data").and_then(|v| v.as_str()).unwrap_or("");
    if let Some(nonce) = data.strip_prefix("tg:") {
        let callback_query_id = callback
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let result = apply_callback(db, &state.config, nonce, callback_query_id.as_deref()).await?;
        return Ok(Json(json!({ "ok": true, "result": result })));
    }
    Ok(Json(json!({ "ok": true, "ignored": true })))
}

/// `GET /api/v1/telegram/status/public` — bot status without
/// leaking any secret.
pub async fn bot_status(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    Ok(Json(
        json!({ "enabled": bot_enabled(db, &state.config).await? }),
    ))
}

// --- callbacks ---------------------------------------------------------------

fn sign_nonce(config: &crate::Config, nonce: &str, op: &str, content_id: &str) -> String {
    hmac_sha256_hex(
        config.encryption_key.as_bytes(),
        format!("{nonce}:{op}:{content_id}").as_bytes(),
    )
}

/// `apply_callback` — one-shot, expiring, signed inline-button
/// callbacks. The fatwa-submission transition runs through the
/// authorised path (`is_approver` on the bound actor), exactly like
/// Python's `submissions_service.transition`.
async fn apply_callback(
    db: &Db,
    config: &crate::Config,
    nonce: &str,
    _callback_query_id: Option<&str>,
) -> Result<Value, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT nonce, source_content_type, source_content_id, op, actor_user_id, \
             signature, expires_at, used_at FROM telegram_callbacks WHERE nonce = ?",
            &[(nonce).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "callback_not_found",
            "دکمه نامعتبر است",
        ));
    };
    let used_at: Option<DateTime<Utc>> = row.try_get("used_at")?;
    if used_at.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "callback_used",
            "این دکمه قبلاً استفاده شده است",
        ));
    }
    let expires_at: DateTime<Utc> = row.try_get("expires_at")?;
    if expires_at < utc_now() {
        return Err(ApiError::new(
            StatusCode::GONE,
            "callback_expired",
            "این دکمه منقضی شده است",
        ));
    }
    let source_content_type: String = row.try_get("source_content_type")?;
    let source_content_id: String = row.try_get("source_content_id")?;
    let op: String = row.try_get("op")?;
    let actor_user_id: Uuid = row.try_get("actor_user_id")?;
    let signature: String = row.try_get("signature")?;
    let expected_sig = sign_nonce(config, nonce, &op, &source_content_id);
    if !const_time_eq(&expected_sig, &signature) {
        return Err(ApiError::forbidden(
            "callback_tampered",
            "امضای دکمه نامعتبر است",
        ));
    }
    let actor = db
        .fetch_optional(
            "SELECT id FROM users WHERE id = ?",
            &[(actor_user_id).into_arg()],
        )
        .await?;
    if actor.is_none() {
        return Err(ApiError::not_found(
            "actor_not_found",
            "کاربر اجراکننده یافت نشد",
        ));
    }
    let result: Value = if source_content_type == "fatwa_submission" {
        let target = match op.as_str() {
            "approve" => "approved",
            "reject" => "rejected",
            _ => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "bad_callback_op",
                    "عملیات نامعتبر است",
                ));
            }
        };
        transition_submission_for_callback(db, &source_content_id, target, actor_user_id).await?;
        json!({
            "content_type": source_content_type,
            "content_id": source_content_id,
            "op": op,
            "to": target,
        })
    } else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "callback_unsupported",
            format!("نوع محتوای {source_content_type} پشتیبانی نمی‌شود"),
        ));
    };
    db.execute(
        "UPDATE telegram_callbacks SET used_at = ? WHERE nonce = ?",
        &[(utc_now()).into_arg(), (nonce).into_arg()],
    )
    .await?;
    log_audit(
        db,
        Some(actor_user_id),
        "telegram",
        "telegram.callback.apply",
        "telegram_callback",
        Some(nonce),
        None,
        Some(result.clone()),
        None,
        None,
    )
    .await?;
    // `answer_callback_query` needs the bot runtime; the answer is
    // best-effort and its result is ignored in Python too.
    Ok(result)
}

/// The submission transition for a webhook callback: same rules as
/// `routes_fatwa::submission_transition` (workflow validation first,
/// then the approver check), but acting as the callback-bound actor
/// rather than the HTTP caller.
async fn transition_submission_for_callback(
    db: &Db,
    content_id: &str,
    target: &str,
    actor_id: Uuid,
) -> Result<(), ApiError> {
    let id = content_id
        .parse::<Uuid>()
        .map_err(|_| ApiError::internal())?;
    let row = db
        .fetch_optional(
            "SELECT id, workflow_status, version, author_id FROM fatwa_submissions WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "submission_not_found",
            "ثبت فتوا یافت نشد",
        ));
    };
    let current: String = row.try_get("workflow_status")?;
    validate_transition(&current, target)?;
    let approver = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
             JOIN role_permissions rp ON rp.role_id = ur.role_id \
             JOIN permissions p ON p.id = rp.permission_id \
             WHERE ur.user_id = ? AND p.key = 'fatwa.approve' LIMIT 1",
            &[(actor_id).into_arg()],
        )
        .await?
        .is_some();
    if !approver {
        return Err(ApiError::forbidden(
            "forbidden",
            "فقط تأییدکنندهٔ فتوا مجاز است",
        ));
    }
    let old_version: i32 = row.try_get("version")?;
    let now = utc_now();
    let mut sets = vec![
        "workflow_status = ?".to_string(),
        "updated_at = ?".to_string(),
    ];
    let mut args: Vec<Arg> = vec![(target).into_arg(), (now).into_arg()];
    if target == "approved" {
        sets.push("reviewed_by = ?".to_string());
        sets.push("reviewed_at = ?".to_string());
        args.push((actor_id).into_arg());
        args.push((now).into_arg());
    }
    if target == "published" {
        sets.push("published_at = ?".to_string());
        sets.push("version = ?".to_string());
        args.push((now).into_arg());
        args.push((old_version + 1).into_arg());
    }
    args.push((id).into_arg());
    let sql = format!(
        "UPDATE fatwa_submissions SET {} WHERE id = ?",
        sets.join(", ")
    );
    db.execute(&sql, &args).await?;
    let event_type = match target {
        "approved" => "fatwa.submission_approved",
        "rejected" => "fatwa.submission_rejected",
        _ => "fatwa.submission_updated",
    };
    emit_event(
        db,
        "fatwa_submission",
        &id.to_string(),
        event_type,
        json!({ "actor_id": actor_id.to_string(), "to": target }),
    )
    .await?;
    Ok(())
}

// --- connect (Telethon) ------------------------------------------------------

/// `connect_service._mask_phone` — kept for the day the MTProto
/// sidecar lands (request-code answers 503 until then, SYN-02).
#[allow(dead_code)]
fn mask_phone(phone: &str) -> String {
    let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() <= 4 {
        return "****".to_string();
    }
    let (head, tail) = digits.split_at(digits.len() - 4);
    format!("+{}{}", "*".repeat(head.len()), tail)
}

/// `^\+?[0-9]{5,20}$` on the space-stripped number.
fn validate_phone(phone: &str) -> Result<String, ApiError> {
    let cleaned: String = phone.trim().chars().filter(|c| *c != ' ').collect();
    let digits = cleaned.strip_prefix('+').unwrap_or(&cleaned);
    let ok = (5..=20).contains(&digits.len())
        && digits.chars().all(|c| c.is_ascii_digit())
        && cleaned.len() == digits.len() + usize::from(cleaned.starts_with('+'));
    if !ok {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_phone",
            "شمارهٔ تلفن نامعتبر است",
        ));
    }
    Ok(cleaned)
}

async fn telethon_creds(db: &Db, config: &crate::Config) -> Result<(i64, String), ApiError> {
    let api_id_raw = get_setting(db, config, "telegram.api_id").await?;
    let api_hash = get_setting(db, config, "telegram.api_hash").await?;
    if api_id_raw.as_deref().unwrap_or("").is_empty()
        || api_hash.as_deref().unwrap_or("").is_empty()
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "telegram_not_configured",
            "api_id/api_hash تلگرام در تنظیمات پلتفرم ثبت نشده است",
        ));
    }
    let api_id_raw = api_id_raw.unwrap();
    let api_hash = api_hash.unwrap();
    let api_id: i64 = api_id_raw.trim().parse().map_err(|_| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "telegram_bad_api_id",
            "api_id تلگرام نامعتبر است",
        )
    })?;
    Ok((api_id, api_hash.trim().to_string()))
}

struct ConnectStatusOut {
    connected: bool,
    phone_masked: Option<String>,
}

async fn connect_status(db: &Db, config: &crate::Config) -> Result<ConnectStatusOut, ApiError> {
    let session = get_setting(db, config, "telegram.session").await?;
    let phone_masked = get_setting(db, config, "telegram.phone_masked").await?;
    if session.is_some_and(|s| !s.is_empty()) {
        Ok(ConnectStatusOut {
            connected: true,
            phone_masked,
        })
    } else {
        Ok(ConnectStatusOut {
            connected: false,
            phone_masked: None,
        })
    }
}

fn connect_status_json(st: &ConnectStatusOut) -> Value {
    json!({ "connected": st.connected, "phone_masked": st.phone_masked })
}

/// `POST /api/v1/telegram/connect/request-code` — everything up to
/// the Telethon call is validated faithfully; sending the code needs
/// the MTProto client, which is not in the Rust binary (SYN-02), so
/// the honest answer is 503 `telegram_unavailable`.
pub async fn connect_request_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let phone_raw = req_str(&parsed, "phone", 5, 20)?;
    let _phone = validate_phone(&phone_raw)?;
    let (_api_id, _api_hash) = telethon_creds(db, &state.config).await?;
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "telegram_unavailable",
        "اتصال تلگرام (Telethon) در نسخهٔ Rust پیکربندی نشده است",
    ))
}

/// `POST /api/v1/telegram/connect/confirm-code` — the pending-code
/// checks are faithful; verifying the code against Telegram needs
/// the MTProto client (SYN-02), so the honest answer is 503.
pub async fn connect_confirm_code(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let phone_raw = req_str(&parsed, "phone", 5, 20)?;
    let code_raw = req_str(&parsed, "code", 1, 32)?;
    let _password = opt_str_raw_max(&parsed, "password", 256)?;
    let phone = validate_phone(&phone_raw)?;
    let code = code_raw.trim().to_string();
    if code.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_code",
            "کد ورود نامعتبر است",
        ));
    }
    let pending_phone = get_setting(db, &state.config, "telegram.pending_phone").await?;
    let phone_code_hash =
        get_setting(db, &state.config, "telegram.pending_phone_code_hash").await?;
    let expires_raw = get_setting(db, &state.config, "telegram.pending_expires_at").await?;
    let expires_at = expires_raw.as_deref().and_then(parse_datetime_str);
    let expired = match (
        pending_phone.as_deref(),
        phone_code_hash.as_deref(),
        expires_at,
    ) {
        (Some(p), Some(h), Some(exp)) => p != phone || h.is_empty() || exp < utc_now(),
        _ => true,
    };
    if expired {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "code_expired_or_invalid",
            "کد ورود منقضی یا نامعتبر است؛ دوباره درخواست بدهید",
        ));
    }
    let (_api_id, _api_hash) = telethon_creds(db, &state.config).await?;
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "telegram_unavailable",
        "اتصال تلگرام (Telethon) در نسخهٔ Rust پیکربندی نشده است",
    ))
}

/// `GET /api/v1/telegram/connect/status`.
pub async fn connect_status_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let st = connect_status(db, &state.config).await?;
    Ok(Json(connect_status_json(&st)))
}

/// `POST /api/v1/telegram/connect/disconnect` — clears the stored
/// session and all pending state, audits without ever logging the
/// session value itself.
pub async fn connect_disconnect(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    for key in [
        "telegram.session",
        "telegram.phone_masked",
        "telegram.pending_phone",
        "telegram.pending_phone_code_hash",
        "telegram.pending_expires_at",
        "telegram.pending_session",
    ] {
        db.execute(
            "DELETE FROM platform_settings WHERE key = ?",
            &[(key).into_arg()],
        )
        .await?;
    }
    state.bump_settings_version();
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.connect.disconnect",
        "telegram_connection",
        Some("primary"),
        None,
        Some(json!({"field": "telegram_session", "changed": true, "connected": false})),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(
        json!({ "connected": false, "phone_masked": Value::Null }),
    ))
}

// --- import ------------------------------------------------------------------

/// `POST /api/v1/telegram/import` (202) — the channel archive import
/// runs in the ARQ `telegram` queue; there is no ARQ client in the
/// Rust binary, so `enqueued` is honestly `false` (the Redis-down
/// path Python itself takes).
pub async fn import_channel(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let channel = req_str(&parsed, "channel", 1, 128)?;
    let limit = match parsed.get("limit") {
        None | Some(Value::Null) => 100,
        Some(v) => match as_int(v).filter(|n| (1..=5000).contains(n)) {
            Some(n) => n,
            None => return Err(validation_err("limit", "out of range")),
        },
    };
    let st = connect_status(db, &state.config).await?;
    if !st.connected {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "telegram_not_connected",
            "حساب تلگرام متصل نیست؛ اول از /connect وارد شوید",
        ));
    }
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.import.request",
        "telegram_import",
        Some(&channel),
        None,
        Some(json!({"channel": channel, "limit": limit, "enqueued": false})),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "enqueued": false, "channel": channel, "limit": limit })),
    ))
}

// --- distribution: policies --------------------------------------------------

struct PolicyRow {
    id: Uuid,
    content_type: String,
    policy: String,
    message_template: Option<Value>,
    target_channels: Value,
    spacing_seconds: i32,
    require_approval: bool,
    schedule_config: Option<Value>,
    is_enabled: bool,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn policy_out(p: &PolicyRow) -> Value {
    json!({
        "id": p.id.to_string(),
        "content_type": p.content_type,
        "policy": p.policy,
        "message_template": p.message_template,
        "target_channels": p.target_channels,
        "spacing_seconds": p.spacing_seconds,
        "require_approval": p.require_approval,
        "schedule_config": p.schedule_config,
        "is_enabled": p.is_enabled,
        "created_at": p.created_at.to_rfc3339(),
        "updated_at": p.updated_at.to_rfc3339(),
    })
}

fn policy_row_from(row: &crate::db::DynRow) -> Result<PolicyRow, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let content_type: String = row.try_get("content_type")?;
    let policy: String = row.try_get("policy")?;
    let message_template: Option<Value> = row.try_get("message_template")?;
    let target_channels: Option<Value> = row.try_get("target_channels")?;
    let spacing_seconds: i32 = row.try_get("spacing_seconds")?;
    let require_approval: bool = row.try_get("require_approval")?;
    let schedule_config: Option<Value> = row.try_get("schedule_config")?;
    let is_enabled: bool = row.try_get("is_enabled")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
    Ok(PolicyRow {
        id,
        content_type,
        policy,
        message_template,
        target_channels: target_channels.unwrap_or_else(|| json!([])),
        spacing_seconds,
        require_approval,
        schedule_config,
        is_enabled,
        created_at,
        updated_at,
    })
}

async fn fetch_policy(db: &Db, content_type: &str) -> Result<Option<PolicyRow>, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT id, content_type, policy, message_template, target_channels, \
             spacing_seconds, require_approval, schedule_config, is_enabled, created_at, \
             updated_at FROM telegram_distribution_policies WHERE content_type = ?",
            &[(content_type).into_arg()],
        )
        .await?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(policy_row_from(&r)?)),
    }
}

async fn get_or_create_policy(db: &Db, content_type: &str) -> Result<PolicyRow, ApiError> {
    if let Some(p) = fetch_policy(db, content_type).await? {
        return Ok(p);
    }
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO telegram_distribution_policies (id, content_type, policy, \
         target_channels, spacing_seconds, require_approval, is_enabled, created_at, updated_at) \
         VALUES (?, ?, 'never', '[]', 0, TRUE, TRUE, ?, ?)",
        &[
            (id).into_arg(),
            (content_type).into_arg(),
            (now).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    fetch_policy(db, content_type)
        .await?
        .ok_or_else(ApiError::internal)
}

/// `GET /api/v1/telegram/distribute/policies` — all policies, by
/// content type.
pub async fn list_policies(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, content_type, policy, message_template, target_channels, \
             spacing_seconds, require_approval, schedule_config, is_enabled, created_at, \
             updated_at FROM telegram_distribution_policies ORDER BY content_type ASC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        out.push(policy_out(&policy_row_from(row)?));
    }
    Ok(Json(json!(out)))
}

/// `PUT /api/v1/telegram/distribute/policies/{content_type}` —
/// get-or-create, then apply only the fields the client sent
/// (`exclude_unset` semantics).
pub async fn set_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(content_type): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    // Schema-level validation first (Pydantic runs before the service).
    let policy_name = match parsed.get("policy") {
        None | Some(Value::Null) => "never".to_string(),
        Some(v) => match v.as_str() {
            Some(s) => s.to_string(),
            None => return Err(validation_err("policy", "must be a string")),
        },
    };
    let message_template = opt_dict(&parsed, "message_template")?;
    let target_channels: Option<Vec<String>> = match parsed.get("target_channels") {
        None => None,
        Some(Value::Null) => Some(Vec::new()),
        Some(_) => opt_str_list(&parsed, "target_channels")?,
    };
    // `spacing_seconds`: Pydantic `ge=0`. An explicit null reaches the
    // service, where Python's `int(None)` raises → 500; that check runs
    // after the policy-name check, so it is deferred below.
    let spacing_seconds: Option<Option<i64>> = match parsed.get("spacing_seconds") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => match as_int(v).filter(|n| *n >= 0) {
            Some(n) => Some(Some(n)),
            None => return Err(validation_err("spacing_seconds", "out of range")),
        },
    };
    let require_approval = match parsed.get("require_approval") {
        None => None,
        Some(Value::Null) => Some(false),
        Some(v) => match as_bool(v) {
            Some(b) => Some(b),
            None => return Err(validation_err("require_approval", "must be a boolean")),
        },
    };
    let schedule_config = opt_dict(&parsed, "schedule_config")?;
    let is_enabled = match parsed.get("is_enabled") {
        None => None,
        Some(Value::Null) => Some(false),
        Some(v) => match as_bool(v) {
            Some(b) => Some(b),
            None => return Err(validation_err("is_enabled", "must be a boolean")),
        },
    };
    if !POLICIES.contains(&policy_name.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_policy",
            format!("سیاست نامعتبر: {policy_name}"),
        ));
    }
    // Explicit-null spacing_seconds → Python's `int(None)` → 500.
    let spacing_seconds: Option<i64> = match spacing_seconds {
        None => None,
        Some(None) => return Err(ApiError::internal()),
        Some(Some(n)) => Some(n),
    };
    let policy = get_or_create_policy(db, &content_type).await?;
    let old_policy = policy.policy.clone();
    let mut new_policy = policy;
    new_policy.policy = policy_name.clone();
    if message_template.is_some() {
        new_policy.message_template = message_template;
    }
    if let Some(channels) = target_channels {
        new_policy.target_channels = json!(channels);
    }
    if let Some(n) = spacing_seconds {
        new_policy.spacing_seconds = n as i32;
    }
    if let Some(b) = require_approval {
        new_policy.require_approval = b;
    }
    if schedule_config.is_some() {
        new_policy.schedule_config = schedule_config;
    }
    if let Some(b) = is_enabled {
        new_policy.is_enabled = b;
    }
    new_policy.updated_at = utc_now();
    db.execute(
        "UPDATE telegram_distribution_policies SET policy = ?, message_template = ?, \
         target_channels = ?, spacing_seconds = ?, require_approval = ?, schedule_config = ?, \
         is_enabled = ?, updated_at = ? WHERE id = ?",
        &[
            (new_policy.policy.clone()).into_arg(),
            (new_policy.message_template.clone()).into_arg(),
            (new_policy.target_channels.clone()).into_arg(),
            (i64::from(new_policy.spacing_seconds)).into_arg(),
            (new_policy.require_approval).into_arg(),
            (new_policy.schedule_config.clone()).into_arg(),
            (new_policy.is_enabled).into_arg(),
            (new_policy.updated_at).into_arg(),
            (new_policy.id).into_arg(),
        ],
    )
    .await?;
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.policy.set",
        "telegram_distribution_policy",
        Some(&new_policy.id.to_string()),
        Some(json!({"policy": old_policy})),
        Some(json!({"policy": new_policy.policy, "content_type": content_type})),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(policy_out(&new_policy)))
}

// --- distribution: content + rendering ---------------------------------------

struct Content {
    title: String,
    excerpt: String,
    url: String,
}

/// `fetch_content` — `{title, excerpt, url}` or `None` when the
/// content is not found (or the id is not a UUID, or the type is
/// unknown).
async fn fetch_content(
    db: &Db,
    content_type: &str,
    content_id: &str,
) -> Result<Option<Content>, ApiError> {
    match content_type {
        "fatwa_submission" => {
            let Ok(id) = content_id.parse::<Uuid>() else {
                return Ok(None);
            };
            let row = db
                .fetch_optional(
                    "SELECT title, body FROM fatwa_submissions WHERE id = ?",
                    &[(id).into_arg()],
                )
                .await?;
            let Some(row) = row else { return Ok(None) };
            let title: Value = row.try_get("title")?;
            let body: String = row.try_get("body")?;
            Ok(Some(Content {
                title: first_locale_text(&title),
                excerpt: body.chars().take(500).collect(),
                url: format!("/fatwa/{content_id}"),
            }))
        }
        "religious_place" => {
            let Ok(id) = content_id.parse::<Uuid>() else {
                return Ok(None);
            };
            let row = db
                .fetch_optional(
                    "SELECT name, description FROM religious_places WHERE id = ?",
                    &[(id).into_arg()],
                )
                .await?;
            let Some(row) = row else { return Ok(None) };
            let name: Value = row.try_get("name")?;
            let desc: Option<Value> = row.try_get("description")?;
            let desc = desc.unwrap_or_else(|| json!({}));
            Ok(Some(Content {
                title: first_locale_text(&name),
                excerpt: first_locale_text(&desc).chars().take(300).collect(),
                url: format!("/places/{content_id}"),
            }))
        }
        "library_item" => {
            let Ok(id) = content_id.parse::<Uuid>() else {
                return Ok(None);
            };
            let row = db
                .fetch_optional(
                    "SELECT title FROM library_items WHERE id = ?",
                    &[(id).into_arg()],
                )
                .await?;
            let Some(row) = row else { return Ok(None) };
            let title: Value = row.try_get("title")?;
            Ok(Some(Content {
                title: first_locale_text(&title),
                excerpt: String::new(),
                url: format!("/library/{content_id}"),
            }))
        }
        _ => Ok(None),
    }
}

/// `render_message` — the policy template for `fa`, with
/// `{title}`/`{excerpt}`/`{url}` substituted; an unknown placeholder
/// (or a malformed template) returns the template unchanged, like
/// Python's `str.format` raising `KeyError`/`ValueError`.
fn render_message(template: Option<&Value>, content: &Content) -> String {
    let tpl = template.and_then(|t| t.get("fa")).and_then(|v| v.as_str());
    let Some(tpl) = tpl else {
        return format!("📢 <b>{}</b>\n\n{}", content.title, content.excerpt)
            .trim()
            .to_string();
    };
    // Scan placeholders: `{{`/`}}` are escapes; any other `{...}`
    // must be title/excerpt/url or the template is returned as-is.
    let mut rest = tpl;
    while let Some(open) = rest.find('{') {
        if rest[open..].starts_with("{{") {
            rest = &rest[open + 2..];
            continue;
        }
        let Some(close) = rest[open..].find('}') else {
            return tpl.to_string();
        };
        let name = &rest[open + 1..open + close];
        if name != "title" && name != "excerpt" && name != "url" {
            return tpl.to_string();
        }
        rest = &rest[open + close + 1..];
    }
    tpl.replace("{title}", &content.title)
        .replace("{excerpt}", &content.excerpt)
        .replace("{url}", &content.url)
}

// --- distribution: publish log -------------------------------------------------

struct PublishedRow {
    id: Uuid,
    source_content_type: String,
    source_content_id: String,
    distribution_target: String,
    published_message_id: Option<i64>,
    idempotency_key: String,
    status: String,
    retry_count: i32,
    error: Option<String>,
    created_at: DateTime<Utc>,
    sent_at: Option<DateTime<Utc>>,
}

fn published_out(p: &PublishedRow) -> Value {
    json!({
        "id": p.id.to_string(),
        "source_content_type": p.source_content_type,
        "source_content_id": p.source_content_id,
        "distribution_target": p.distribution_target,
        "published_message_id": p.published_message_id,
        "idempotency_key": p.idempotency_key,
        "status": p.status,
        "retry_count": p.retry_count,
        "error": p.error,
        "created_at": p.created_at.to_rfc3339(),
        "sent_at": p.sent_at.map(|t| t.to_rfc3339()),
    })
}

fn published_row_from(row: &crate::db::DynRow) -> Result<PublishedRow, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let source_content_type: String = row.try_get("source_content_type")?;
    let source_content_id: String = row.try_get("source_content_id")?;
    let distribution_target: String = row.try_get("distribution_target")?;
    let published_message_id: Option<i64> = row.try_get("published_message_id")?;
    let idempotency_key: String = row.try_get("idempotency_key")?;
    let status: String = row.try_get("status")?;
    let retry_count: i32 = row.try_get("retry_count")?;
    let error: Option<String> = row.try_get("error")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let sent_at: Option<DateTime<Utc>> = row.try_get("sent_at")?;
    Ok(PublishedRow {
        id,
        source_content_type,
        source_content_id,
        distribution_target,
        published_message_id,
        idempotency_key,
        status,
        retry_count,
        error,
        created_at,
        sent_at,
    })
}

const PUBLISHED_COLS: &str = "id, source_content_type, source_content_id, distribution_target, \
     published_message_id, idempotency_key, status, retry_count, error, created_at, sent_at";

async fn fetch_published(db: &Db, id: Uuid) -> Result<Option<PublishedRow>, ApiError> {
    let row = db
        .fetch_optional(
            &format!("SELECT {PUBLISHED_COLS} FROM telegram_published WHERE id = ?"),
            &[(id).into_arg()],
        )
        .await?;
    match row {
        None => Ok(None),
        Some(r) => Ok(Some(published_row_from(&r)?)),
    }
}

/// `POST /api/v1/telegram/distribute/publish` — manual publish,
/// idempotent on `idempotency_key` (`{type}:{id}:{target}` by
/// default). Rows are created `pending`; the ARQ job is never
/// enqueued in Rust, so `error` records that honestly.
pub async fn request_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let content_type = req_str(&parsed, "content_type", 1, 64)?;
    let content_id = req_str(&parsed, "content_id", 1, 64)?;
    let targets = opt_str_list(&parsed, "targets")?;
    let idempotency_key = opt_str_raw(&parsed, "idempotency_key", 128)?;
    let policy = fetch_policy(db, &content_type).await?;
    let mut channel_targets: Vec<String> = targets.unwrap_or_else(|| {
        policy
            .as_ref()
            .and_then(|p| {
                p.target_channels.as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
            })
            .unwrap_or_default()
    });
    if channel_targets.is_empty() {
        if let Some(default) = default_channel(db, &state.config).await? {
            channel_targets = vec![default];
        }
    }
    if channel_targets.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_target_channel",
            "کانال مقصد مشخص نشده است",
        ));
    }
    let content = fetch_content(db, &content_type, &content_id).await?;
    if content.is_none() {
        return Err(ApiError::not_found("content_not_found", "محتوا یافت نشد"));
    }
    // Python creates the policy row (get-or-create) before the loop;
    // create it now that the request is known to succeed.
    let _ = get_or_create_policy(db, &content_type).await?;
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    let mut results: Vec<Value> = Vec::new();
    for target in &channel_targets {
        let key = idempotency_key
            .clone()
            .unwrap_or_else(|| format!("{content_type}:{content_id}:{target}"));
        if let Some(existing) = db
            .fetch_optional(
                &format!(
                    "SELECT {PUBLISHED_COLS} FROM telegram_published WHERE idempotency_key = ?"
                ),
                &[(key.clone()).into_arg()],
            )
            .await?
        {
            results.push(published_out(&published_row_from(&existing)?));
            continue;
        }
        let id = Uuid::new_v4();
        let now = utc_now();
        db.execute(
            "INSERT INTO telegram_published (id, source_content_type, source_content_id, \
             distribution_target, idempotency_key, status, retry_count, created_at) \
             VALUES (?, ?, ?, ?, ?, 'pending', 0, ?)",
            &[
                (id).into_arg(),
                (content_type.clone()).into_arg(),
                (content_id.clone()).into_arg(),
                (target.clone()).into_arg(),
                (key.clone()).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "admin",
            "telegram.publish.request",
            "telegram_published",
            Some(&id.to_string()),
            None,
            Some(json!({
                "content_type": content_type,
                "content_id": content_id,
                "target": target,
            })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
        // No ARQ worker: the row stays pending with an honest error.
        db.execute(
            "UPDATE telegram_published SET error = 'worker unavailable — retry later' WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
        let row = fetch_published(db, id)
            .await?
            .ok_or_else(ApiError::internal)?;
        results.push(published_out(&row));
    }
    Ok(Json(json!(results)))
}

/// `GET /api/v1/telegram/distribute/log` — publish log, newest first.
pub async fn publish_log(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let params = parse_query(&uri);
    let content_type = params
        .get("content_type")
        .filter(|s| !s.is_empty())
        .cloned();
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let mut count_sql = "SELECT COUNT(*) AS total FROM telegram_published".to_string();
    let mut list_sql = format!("SELECT {PUBLISHED_COLS} FROM telegram_published");
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(ct) = &content_type {
        conds.push("source_content_type = ?".to_string());
        args.push(ct.into_arg());
    }
    if let Some(st) = &status {
        conds.push("status = ?".to_string());
        args.push(st.into_arg());
    }
    if !conds.is_empty() {
        let where_sql = format!(" WHERE {}", conds.join(" AND "));
        count_sql.push_str(&where_sql);
        list_sql.push_str(&where_sql);
    }
    let total: i64 = db.fetch_one(&count_sql, &args).await?.try_get("total")?;
    list_sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    let mut list_args = args.clone();
    list_args.push((limit).into_arg());
    let rows = db.fetch_all(&list_sql, &list_args).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| published_row_from(r).map(|p| published_out(&p)))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(json!({ "items": items, "total": total })))
}

/// `send_pending` — the ARQ job body: send one `pending` row.
/// Idempotent on `sent`. The distribution gate fails open (the admin
/// control-centre module is not ported — SYN-04); the bot send
/// degrades honestly to `failed` without a bot runtime (SYN-03).
async fn send_pending(
    db: &Db,
    _config: &crate::Config,
    published_id: Uuid,
) -> Result<Value, ApiError> {
    let Some(mut row) = fetch_published(db, published_id).await? else {
        return Err(ApiError::not_found(
            "publish_not_found",
            "رکورد انتشار یافت نشد",
        ));
    };
    if row.status == "sent" {
        return Ok(published_out(&row));
    }
    let policy = fetch_policy(db, &row.source_content_type).await?;
    let template = policy.as_ref().and_then(|p| p.message_template.clone());
    let content = fetch_content(db, &row.source_content_type, &row.source_content_id).await?;
    let Some(content) = content else {
        db.execute(
            "UPDATE telegram_published SET status = 'failed', error = 'content not found' WHERE id = ?",
            &[(published_id).into_arg()],
        )
        .await?;
        row.status = "failed".to_string();
        row.error = Some("content not found".to_string());
        return Ok(published_out(&row));
    };
    let _text = render_message(template.as_ref(), &content);
    let message_id = send_message_unavailable();
    if let Some(mid) = message_id {
        row.status = "sent".to_string();
        row.published_message_id = Some(mid);
        row.sent_at = Some(utc_now());
        row.error = None;
        db.execute(
            "UPDATE telegram_published SET status = 'sent', published_message_id = ?, \
             sent_at = ?, error = NULL WHERE id = ?",
            &[
                (mid).into_arg(),
                (row.sent_at.unwrap()).into_arg(),
                (published_id).into_arg(),
            ],
        )
        .await?;
    } else {
        row.status = "failed".to_string();
        row.retry_count += 1;
        row.error = Some("bot unavailable or send failed".to_string());
        db.execute(
            "UPDATE telegram_published SET status = 'failed', retry_count = ?, \
             error = 'bot unavailable or send failed' WHERE id = ?",
            &[
                (i64::from(row.retry_count)).into_arg(),
                (published_id).into_arg(),
            ],
        )
        .await?;
    }
    emit_event(
        db,
        "telegram_published",
        &published_id.to_string(),
        &format!("telegram.published.{}", row.status),
        json!({
            "content_type": row.source_content_type,
            "content_id": row.source_content_id,
            "target": row.distribution_target,
        }),
    )
    .await?;
    log_audit(
        db,
        None,
        "system",
        "telegram.publish.send",
        "telegram_published",
        Some(&published_id.to_string()),
        None,
        Some(json!({"status": row.status, "message_id": row.published_message_id})),
        None,
        None,
    )
    .await?;
    Ok(published_out(&row))
}

/// `POST /api/v1/telegram/distribute/retry/{published_id}` — back
/// to `pending`; the re-enqueue honestly fails without a worker.
pub async fn retry_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(published_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&published_id, "published_id")?;
    let db = state.db_or_500()?;
    let Some(row) = fetch_published(db, id).await? else {
        return Err(ApiError::not_found(
            "publish_not_found",
            "رکورد انتشار یافت نشد",
        ));
    };
    if row.status == "sent" {
        return Ok(Json(published_out(&row)));
    }
    db.execute(
        "UPDATE telegram_published SET status = 'pending', error = NULL WHERE id = ?",
        &[(id).into_arg()],
    )
    .await?;
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.publish.retry",
        "telegram_published",
        Some(&id.to_string()),
        None,
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    db.execute(
        "UPDATE telegram_published SET error = 'worker unavailable — retry later' WHERE id = ?",
        &[(id).into_arg()],
    )
    .await?;
    let row = fetch_published(db, id)
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(published_out(&row)))
}

/// `POST /api/v1/telegram/distribute/revoke/{published_id}` — delete
/// the Telegram message when possible; without a bot runtime the
/// delete fails and the status is left as-is.
pub async fn revoke_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(published_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&published_id, "published_id")?;
    let db = state.db_or_500()?;
    let Some(mut row) = fetch_published(db, id).await? else {
        return Err(ApiError::not_found(
            "publish_not_found",
            "رکورد انتشار یافت نشد",
        ));
    };
    let mut deleted = false;
    if row.published_message_id.is_some() {
        deleted = delete_message_unavailable();
    }
    if deleted {
        row.status = "deleted".to_string();
        db.execute(
            "UPDATE telegram_published SET status = 'deleted' WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    }
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.publish.revoke",
        "telegram_published",
        Some(&id.to_string()),
        None,
        Some(json!({"deleted": deleted, "status": row.status})),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(published_out(&row)))
}

/// `POST /api/v1/telegram/distribute/approve/{published_id}` —
/// manual approval (idempotent): only `pending`/`failed` rows can be
/// approved; the send then runs through `send_pending`.
pub async fn approve_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(published_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&published_id, "published_id")?;
    let db = state.db_or_500()?;
    let Some(row) = fetch_published(db, id).await? else {
        return Err(ApiError::not_found(
            "publish_not_found",
            "رکورد انتشار یافت نشد",
        ));
    };
    if row.status != "pending" && row.status != "failed" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_status",
            format!("وضعیت {} قابل تأیید نیست", row.status),
        ));
    }
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.publish.approve",
        "telegram_published",
        Some(&id.to_string()),
        None,
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let out = send_pending(db, &state.config, id).await?;
    Ok(Json(out))
}

/// `POST /api/v1/telegram/distribute/reject/{published_id}` —
/// manual rejection of a `pending`/`failed` row.
pub async fn reject_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(published_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&published_id, "published_id")?;
    let db = state.db_or_500()?;
    let Some(mut row) = fetch_published(db, id).await? else {
        return Err(ApiError::not_found(
            "publish_not_found",
            "رکورد انتشار یافت نشد",
        ));
    };
    if row.status != "pending" && row.status != "failed" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_status",
            format!("وضعیت {} قابل رد نیست", row.status),
        ));
    }
    row.status = "rejected".to_string();
    db.execute(
        "UPDATE telegram_published SET status = 'rejected' WHERE id = ?",
        &[(id).into_arg()],
    )
    .await?;
    let (ip, ua) = audit_ip_ua(&state, &headers, peer.0);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "telegram.publish.reject",
        "telegram_published",
        Some(&id.to_string()),
        None,
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(published_out(&row)))
}
