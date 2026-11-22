//! API-keys routes — mirrors `app/modules/api_keys/router.py` and
//! `service.py` (admin CRUD + embed snippets).
//!
//! All seven endpoints require the `api_keys.manage` permission.
//! The raw key (`mk_` + `token_urlsafe(32)`) is returned exactly once
//! at creation; only its SHA-256 hash is stored, with the first 8
//! characters as `key_prefix`. Embed tokens are HMAC-SHA256 signed
//! payloads `{kid, w, exp}` (365 days) under the storage signing key.
//! `validate_key` / `validate_embed_token` are the widget guards reused
//! by Wave 7 — rate limit is enforced per minute, quota against a
//! monthly counter seeded from the stored `used_this_month`.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{Datelike, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::log_audit,
    authn::{authenticate, require_permission, AuthCtx},
    crypto::{hash_token, hmac_sha256_hex, token_urlsafe},
    db::{new_id, utc_now, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    storage::storage_signing_keys,
    validate::{opt_bool, opt_int, opt_str, opt_str_list, parse_body, req_int, req_str},
    AppState,
};

pub const WIDGET_SCOPES: &[&str] = &["prayer-times", "ayah-of-day", "latest-fatwa", "zakat-mini"];
const EMBED_TOKEN_TTL_DAYS: i64 = 365;

const WIDGET_TITLES: &[(&str, &str)] = &[
    ("prayer-times", "اوقات شرعی"),
    ("ayah-of-day", "آیهٔ روز"),
    ("latest-fatwa", "آخرین فتوا"),
    ("zakat-mini", "محاسبهٔ زکات"),
];

pub fn period_now() -> String {
    let now = Utc::now();
    format!("{:04}-{:02}", now.year(), now.month())
}

#[derive(Debug, Clone)]
pub struct ApiKeyRow {
    pub id: Uuid,
    pub name: String,
    pub key_prefix: String,
    pub scopes: Vec<String>,
    pub rate_limit_per_min: i64,
    pub quota_monthly: i64,
    pub used_this_month: i64,
    pub quota_period: String,
    pub is_active: bool,
    pub abuse_flag: bool,
    pub last_used_at: Option<chrono::DateTime<Utc>>,
    pub created_at: chrono::DateTime<Utc>,
    pub revoked_at: Option<chrono::DateTime<Utc>>,
    pub key_hash: String,
}

fn key_from_row(row: &DynRow) -> Result<ApiKeyRow, ApiError> {
    let scopes_raw: Value = row.try_get("scopes")?;
    let scopes = scopes_raw
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    Ok(ApiKeyRow {
        id: row.try_get("id")?,
        name: row.try_get("name")?,
        key_prefix: row.try_get("key_prefix")?,
        scopes,
        // INTEGER columns read as i32 (ADR-028): sqlx cannot decode
        // Postgres int4 as i64, so read i32 and widen here. (Wave 7
        // widgets exposed this: `validate_key` 500'd on Postgres
        // while passing on sqlite.)
        rate_limit_per_min: row.try_get::<i32, _>("rate_limit_per_min")? as i64,
        quota_monthly: row.try_get::<i32, _>("quota_monthly")? as i64,
        used_this_month: row.try_get::<i32, _>("used_this_month")? as i64,
        quota_period: row.try_get("quota_period")?,
        is_active: row.try_get("is_active")?,
        abuse_flag: row.try_get("abuse_flag")?,
        last_used_at: row.try_get("last_used_at")?,
        created_at: row.try_get("created_at")?,
        revoked_at: row.try_get("revoked_at")?,
        key_hash: row.try_get("key_hash")?,
    })
}

fn key_out(k: &ApiKeyRow) -> Value {
    json!({
        "id": k.id.to_string(),
        "name": k.name,
        "key_prefix": k.key_prefix,
        "scopes": k.scopes,
        "rate_limit_per_min": k.rate_limit_per_min,
        "quota_monthly": k.quota_monthly,
        "used_this_month": k.used_this_month,
        "quota_period": k.quota_period,
        "is_active": k.is_active,
        "abuse_flag": k.abuse_flag,
        "last_used_at": k.last_used_at.map(|d| d.to_rfc3339()),
        "created_at": k.created_at.to_rfc3339(),
        "revoked_at": k.revoked_at.map(|d| d.to_rfc3339()),
    })
}

async fn get_key_or_404(db: &Db, key_id: Uuid) -> Result<ApiKeyRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM api_keys WHERE id = ?",
            &[(key_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => key_from_row(&r),
        None => Err(ApiError::not_found("api_key_not_found", "کلید یافت نشد")),
    }
}

fn validate_scopes(scopes: &[String]) -> Result<(), ApiError> {
    let unknown: Vec<&str> = scopes
        .iter()
        .map(String::as_str)
        .filter(|s| !WIDGET_SCOPES.contains(s) && *s != "*")
        .collect();
    if !unknown.is_empty() {
        return Err(ApiError::validation(vec![
            json!({ "field": "scopes", "msg": format!("unknown scopes: {unknown:?}") }),
        ]));
    }
    Ok(())
}

fn client_ip(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string())
}

async fn admin_ctx(state: &AppState, headers: &HeaderMap, path: &str) -> Result<AuthCtx, ApiError> {
    let ctx = authenticate(state, headers, path).await?;
    require_permission(state, &ctx, "api_keys.manage").await?;
    Ok(ctx)
}

pub async fn create_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let name = req_str(&parsed, "name", 1, 128)?;
    let scopes = opt_str_list(&parsed, "scopes")?.unwrap_or_default();
    validate_scopes(&scopes)?;
    let rate_limit_per_min = req_int(&parsed, "rate_limit_per_min", 1, 10000, 60)?;
    let quota_monthly = req_int(&parsed, "quota_monthly", 1, 10_000_000, 10000)?;
    let raw = format!("mk_{}", token_urlsafe(32));
    let key = ApiKeyRow {
        id: new_id(),
        name: name.clone(),
        key_prefix: raw.chars().take(8).collect(),
        scopes: scopes.clone(),
        rate_limit_per_min,
        quota_monthly,
        used_this_month: 0,
        quota_period: period_now(),
        is_active: true,
        abuse_flag: false,
        last_used_at: None,
        created_at: utc_now(),
        revoked_at: None,
        key_hash: hash_token(&raw),
    };
    db.execute(
        "INSERT INTO api_keys (id, key_hash, key_prefix, name, scopes, rate_limit_per_min, \
         quota_monthly, used_this_month, quota_period, is_active, abuse_flag, last_used_at, \
         created_by, created_at, revoked_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            (key.id).into_arg(),
            (&key.key_hash).into_arg(),
            (&key.key_prefix).into_arg(),
            (&key.name).into_arg(),
            (Value::Array(scopes.iter().map(|s| Value::String(s.clone())).collect())).into_arg(),
            (rate_limit_per_min).into_arg(),
            (quota_monthly).into_arg(),
            (0i64).into_arg(),
            (&key.quota_period).into_arg(),
            (true).into_arg(),
            (false).into_arg(),
            (Option::<chrono::DateTime<Utc>>::None).into_arg(),
            (ctx.user.id).into_arg(),
            (key.created_at).into_arg(),
            (Option::<chrono::DateTime<Utc>>::None).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "api_keys.create",
        "api_key",
        Some(&key.id.to_string()),
        None,
        Some(json!({ "name": key.name, "key_prefix": key.key_prefix, "scopes": key.scopes })),
        client_ip(&headers).as_deref(),
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await?;
    let mut out = key_out(&key);
    out.as_object_mut()
        .unwrap()
        .insert("key".to_string(), Value::String(raw));
    // `ApiKeyCreated` field order: id, name, key, key_prefix, scopes, ...
    let mut ordered = serde_json::Map::new();
    ordered.insert("id".into(), out["id"].clone());
    ordered.insert("name".into(), out["name"].clone());
    ordered.insert("key".into(), out["key"].clone());
    ordered.insert("key_prefix".into(), out["key_prefix"].clone());
    ordered.insert("scopes".into(), out["scopes"].clone());
    ordered.insert(
        "rate_limit_per_min".into(),
        out["rate_limit_per_min"].clone(),
    );
    ordered.insert("quota_monthly".into(), out["quota_monthly"].clone());
    Ok((StatusCode::CREATED, Json(Value::Object(ordered))))
}

pub async fn list_keys(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all("SELECT * FROM api_keys ORDER BY created_at DESC", &[])
        .await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(key_out(&key_from_row(&row)?));
    }
    Ok(Json(Value::Array(out)))
}

pub async fn get_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    Ok(Json(key_out(&get_key_or_404(db, key_id).await?)))
}

pub async fn update_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    let mut key = get_key_or_404(db, key_id).await?;
    let parsed = parse_body(&body)?;
    let name = opt_str(&parsed, "name", 1, 128)?;
    let scopes = opt_str_list(&parsed, "scopes")?;
    if let Some(s) = &scopes {
        validate_scopes(s)?;
    }
    let rate_limit_per_min = opt_int(&parsed, "rate_limit_per_min", 1, 10000)?;
    let quota_monthly = opt_int(&parsed, "quota_monthly", 1, 10_000_000)?;
    let is_active = opt_bool(&parsed, "is_active")?;
    let abuse_flag = opt_bool(&parsed, "abuse_flag")?;
    let old = json!({ "is_active": key.is_active, "scopes": key.scopes });
    if let Some(v) = name {
        key.name = v;
    }
    if let Some(v) = scopes {
        key.scopes = v;
    }
    if let Some(v) = rate_limit_per_min {
        key.rate_limit_per_min = v;
    }
    if let Some(v) = quota_monthly {
        key.quota_monthly = v;
    }
    if let Some(v) = is_active {
        key.is_active = v;
    }
    if let Some(v) = abuse_flag {
        key.abuse_flag = v;
    }
    db.execute(
        "UPDATE api_keys SET name = ?, scopes = ?, rate_limit_per_min = ?, quota_monthly = ?, \
         is_active = ?, abuse_flag = ? WHERE id = ?",
        &[
            (&key.name).into_arg(),
            (Value::Array(
                key.scopes
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ))
            .into_arg(),
            (key.rate_limit_per_min).into_arg(),
            (key.quota_monthly).into_arg(),
            (key.is_active).into_arg(),
            (key.abuse_flag).into_arg(),
            (key.id).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "api_keys.update",
        "api_key",
        Some(&key.id.to_string()),
        Some(old),
        Some(json!({ "is_active": key.is_active, "scopes": key.scopes, "abuse_flag": key.abuse_flag })),
        client_ip(&headers).as_deref(),
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await?;
    Ok(Json(key_out(&key)))
}

pub async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    let mut key = get_key_or_404(db, key_id).await?;
    key.is_active = false;
    key.revoked_at = Some(utc_now());
    db.execute(
        "UPDATE api_keys SET is_active = FALSE, revoked_at = ? WHERE id = ?",
        &[(key.revoked_at).into_arg(), (key.id).into_arg()],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "api_keys.revoke",
        "api_key",
        Some(&key.id.to_string()),
        None,
        Some(json!({ "is_active": false, "key_prefix": key.key_prefix })),
        client_ip(&headers).as_deref(),
        headers
            .get(axum::http::header::USER_AGENT)
            .and_then(|v| v.to_str().ok()),
    )
    .await?;
    Ok(Json(key_out(&key)))
}

pub async fn usage_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    let key = get_key_or_404(db, key_id).await?;
    let remaining = (key.quota_monthly - key.used_this_month).max(0);
    Ok(Json(json!({
        "id": key.id.to_string(),
        "name": key.name,
        "key_prefix": key.key_prefix,
        "quota_period": key.quota_period,
        "used_this_month": key.used_this_month,
        "quota_monthly": key.quota_monthly,
        "quota_remaining": remaining,
        "rate_limit_per_min": key.rate_limit_per_min,
        "last_used_at": key.last_used_at.map(|d| d.to_rfc3339()),
        "is_active": key.is_active,
        "abuse_flag": key.abuse_flag,
    })))
}

/// `issue_embed_token(key_id, widget)` — `{kid, w, exp}` HMAC-signed.
pub fn issue_embed_token(config: &crate::Config, key_id: Uuid, widget: &str) -> String {
    let exp = Utc::now().timestamp() + EMBED_TOKEN_TTL_DAYS * 86400;
    let payload = json!({ "kid": key_id.to_string(), "w": widget, "exp": exp });
    let raw = URL_SAFE_NO_PAD.encode(serde_json::to_string(&payload).unwrap());
    let signing = storage_signing_keys(config);
    let sig = hmac_sha256_hex(signing[0].as_bytes(), raw.as_bytes());
    format!("{raw}.{sig}")
}

/// `verify_embed_token(token, widget)` → key id, or `None`.
pub fn verify_embed_token(config: &crate::Config, token: &str, widget: &str) -> Option<Uuid> {
    let (raw, sig) = token.rsplit_once('.')?;
    let keys = storage_signing_keys(config);
    if !keys
        .iter()
        .any(|k| hmac_sha256_hex(k.as_bytes(), raw.as_bytes()) == sig)
    {
        return None;
    }
    // The payload is base64url *without* padding; the NO_PAD engine
    // decodes it as-is. (Padding it with `len % 4` '=' chars was wrong:
    // a payload with `len % 4 == 3` needs 1 pad char, not 3.)
    let decoded = URL_SAFE_NO_PAD.decode(raw).ok()?;
    let payload: Value = serde_json::from_slice(&decoded).ok()?;
    if payload.get("w").and_then(|w| w.as_str()) != Some(widget) {
        return None;
    }
    let exp = payload.get("exp").and_then(|e| e.as_i64()).unwrap_or(0);
    if exp <= Utc::now().timestamp() {
        return None;
    }
    payload
        .get("kid")
        .and_then(|k| k.as_str())
        .and_then(|k| k.parse().ok())
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

fn safe_base_url(base_url: &str) -> Result<String, ApiError> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Ok(String::new());
    }
    let lower = base.to_lowercase();
    let ok = (lower.starts_with("http://") || lower.starts_with("https://"))
        && base
            .split("://")
            .nth(1)
            .map(|rest| !rest.is_empty())
            .unwrap_or(false);
    if !ok {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_base_url",
            "base_url باید یک نشانی معتبر http(s) باشد",
        ));
    }
    Ok(escape_html(base))
}

/// `build_embed_snippet(widget, embed_token, base_url=...)`.
pub fn build_embed_snippet(
    widget: &str,
    embed_token: &str,
    base_url: &str,
) -> Result<String, ApiError> {
    if !WIDGET_SCOPES.contains(&widget) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_widget",
            format!("ویجت ناشناخته: {widget}"),
        ));
    }
    let title = WIDGET_TITLES
        .iter()
        .find(|(k, _)| *k == widget)
        .map(|(_, t)| *t)
        .unwrap();
    let base = safe_base_url(base_url)?;
    let (src, extra) = if widget == "prayer-times" {
        (
            format!("{base}/api/v1/widgets/prayer-times?embed_token={embed_token}&lat={{LAT}}&lng={{LNG}}"),
            "<!-- به‌جای {LAT} و {LNG} مختصات شهر را بگذارید، مثلاً lat=35.72&lng=51.33 -->".to_string(),
        )
    } else {
        (
            format!("{base}/api/v1/widgets/{widget}?embed_token={embed_token}"),
            String::new(),
        )
    };
    Ok(format!(
        "<!-- ویجت آکادمی ماتریدی — {title} -->\n{extra}\n<iframe src=\"{src}\" title=\"{title}\" style=\"border:0;width:100%;max-width:420px;height:320px\" loading=\"lazy\" referrerpolicy=\"no-referrer\"></iframe>"
    ))
}

pub async fn embed_snippet(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key_id): Path<Uuid>,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers, "/api/v1/api-keys").await?;
    let db = state.db_or_500()?;
    get_key_or_404(db, key_id).await?;
    let widget = query.get("widget").cloned().unwrap_or_default();
    let base_url = query.get("base_url").cloned().unwrap_or_default();
    let token = issue_embed_token(&state.config, key_id, &widget);
    let html = build_embed_snippet(&widget, &token, &base_url)?;
    Ok(Json(json!({
        "widget": widget,
        "html": html,
        "note": "snippet با توکن embed امضاشده ساخته شد — کلید خام در آن نیست؛ توکن فقط همین ویجت را باز می‌کند و با revoke کلید از کار می‌افتد.",
    })))
}

// ---------- widget guards (used by Wave 7 widgets) ----------

async fn apply_rate_and_quota(state: &AppState, key: &ApiKeyRow) -> Result<(), ApiError> {
    let rl_key = format!("apikey:{}", key.key_hash);
    if !state
        .limiter
        .is_allowed(
            &rl_key,
            key.rate_limit_per_min as u32,
            60,
            false,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    let period = period_now();
    let seed = if key.quota_period == period {
        key.used_this_month
    } else {
        0
    };
    let total = state.api_key_usage_hit(&key.key_hash, &period, seed).await;
    if total > key.quota_monthly {
        state.api_key_usage_refund(&key.key_hash, &period).await;
        return Err(ApiError::with_details(
            StatusCode::TOO_MANY_REQUESTS,
            "quota_exceeded",
            "سهمیهٔ ماهانهٔ این کلید تمام شده است",
            json!({ "quota_monthly": key.quota_monthly }),
        ));
    }
    Ok(())
}

fn scope_denied(scope: &str) -> ApiError {
    ApiError::with_details(
        StatusCode::FORBIDDEN,
        "api_key_scope_denied",
        format!("این کلید به ویجت «{scope}» دسترسی ندارد"),
        json!({ "scope": scope }),
    )
}

/// `validate_key(db, raw_key, scope=...)` — widget API-key guard.
pub async fn validate_key(
    state: &AppState,
    raw_key: Option<&str>,
    scope: &str,
) -> Result<ApiKeyRow, ApiError> {
    let Some(raw) = raw_key.filter(|s| !s.is_empty()) else {
        return Err(ApiError::unauthorized(
            "api_key_required",
            "کلید API لازم است",
        ));
    };
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM api_keys WHERE key_hash = ?",
            &[(hash_token(raw)).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::unauthorized(
            "api_key_invalid",
            "کلید API نامعتبر است",
        ));
    };
    let key = key_from_row(&row)?;
    check_key_usable(state, &key, scope).await?;
    Ok(key)
}

/// `validate_embed_token(db, token, scope=...)` — embed-token guard.
pub async fn validate_embed_token(
    state: &AppState,
    token: Option<&str>,
    scope: &str,
) -> Result<ApiKeyRow, ApiError> {
    let Some(token) = token.filter(|s| !s.is_empty()) else {
        return Err(ApiError::unauthorized(
            "api_key_required",
            "کلید API لازم است",
        ));
    };
    let key_id = verify_embed_token(&state.config, token, scope)
        .ok_or_else(|| ApiError::unauthorized("api_key_invalid", "کلید API نامعتبر است"))?;
    let db = state.db_or_500()?;
    let key = get_key_or_404(db, key_id)
        .await
        .map_err(|_| ApiError::unauthorized("api_key_invalid", "کلید API نامعتبر است"))?;
    check_key_usable(state, &key, scope).await?;
    Ok(key)
}

async fn check_key_usable(state: &AppState, key: &ApiKeyRow, scope: &str) -> Result<(), ApiError> {
    if !key.is_active || key.revoked_at.is_some() {
        return Err(ApiError::unauthorized(
            "api_key_invalid",
            "کلید API نامعتبر است",
        ));
    }
    if key.abuse_flag {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "api_key_abused",
            "این کلید به‌دلیل سوءاستفاده مسدود شده است",
        ));
    }
    if !key.scopes.iter().any(|s| s == scope || s == "*") {
        return Err(scope_denied(scope));
    }
    apply_rate_and_quota(state, key).await
}
