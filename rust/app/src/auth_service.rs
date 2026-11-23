//! Auth service — mirrors `app/modules/auth/service.py`.
//!
//! Register / login / refresh / logout / email+phone verification /
//! password reset / TOTP 2FA / WS tickets. Constants, rate-limit keys,
//! error codes and messages are copied from the original; single-use
//! tokens are consumed with conditional `UPDATE ... WHERE used = FALSE`
//! statements (atomic at the DB, §9), and refresh-token reuse triggers
//! the strict B-02 response (revoke the whole session, emit
//! `token.reuse_detected`, 401 — no grace window).

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{load_user, must_enroll_2fa, UserRow},
    crypto::{hash_token, token_urlsafe, verify_password},
    db::{new_id, utc_now, Arg, Db, IntoArg},
    errors::ApiError,
    jwt::{create_access_token, SCOPE_2FA_SETUP, SCOPE_FULL},
    settings::decrypt_value,
    totp::verify_totp_step,
    AppState,
};

pub const REFRESH_EXPIRE_DAYS: i64 = 30;
pub const EMAIL_VERIFY_EXPIRE_HOURS: i64 = 1;
pub const PHONE_CODE_EXPIRE_MINUTES: i64 = 5;
pub const SETUP_TOKEN_EXPIRE_MINUTES: i64 = 60;
pub const REGISTRATION_MARKER_EXPIRE_DAYS: i64 = 7;
pub const RESET_TOKEN_EXPIRE_HOURS: i64 = 1;
pub const WS_TICKET_EXPIRE_SECONDS: i64 = 60;
pub const LEGAL_TEXT_VERSION: &str = "v1";

pub struct Ctx<'a> {
    pub state: &'a AppState,
    pub db: &'a Db,
}

impl<'a> Ctx<'a> {
    pub fn new(state: &'a AppState, db: &'a Db) -> Self {
        Self { state, db }
    }
}

fn rate_limited() -> ApiError {
    ApiError::new(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "تعداد درخواست بیش از حد مجاز است",
    )
}

async fn check_rl(ctx: &Ctx<'_>, key: &str, limit: u32, window_s: u64) -> Result<(), ApiError> {
    if !ctx
        .state
        .limiter
        .is_allowed(key, limit, window_s, true, &ctx.state.config.app_env)
        .await
    {
        return Err(rate_limited());
    }
    Ok(())
}

pub async fn get_user_by_identifier(
    db: &Db,
    identifier: &str,
) -> Result<Option<UserRow>, ApiError> {
    let identifier = identifier.trim();
    let row = db
        .fetch_optional(
            "SELECT * FROM users WHERE email = ? OR phone = ?",
            &[(identifier).into_arg(), (identifier).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Some(crate::authn::user_from_row(&r)?)),
        None => Ok(None),
    }
}

#[allow(dead_code)]
pub struct TokenRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub expires_at: DateTime<Utc>,
    pub used: bool,
}

pub async fn fetch_token_row(
    db: &Db,
    table: &str,
    hash: &str,
    purpose: Option<&str>,
) -> Result<Option<TokenRow>, ApiError> {
    let sql = match purpose {
        Some(_) => format!("SELECT id, user_id, expires_at, used FROM {table} WHERE token_hash = ? AND purpose = ? AND used = FALSE"),
        None => format!("SELECT id, user_id, expires_at, used FROM {table} WHERE token_hash = ? AND used = FALSE"),
    };
    // Callers pass the raw token; the column holds its SHA-256 hash.
    let mut args: Vec<Arg> = vec![hash_token(hash).into_arg()];
    if let Some(p) = purpose {
        args.push(p.into_arg());
    }
    let row = db.fetch_optional(&sql, &args).await?;
    Ok(row.map(|r| TokenRow {
        id: r.try_get("id").unwrap(),
        user_id: r.try_get("user_id").unwrap(),
        expires_at: r.try_get("expires_at").unwrap(),
        used: r.try_get("used").unwrap(),
    }))
}

/// Atomic single-use consumption: `UPDATE ... WHERE id = ? AND used = FALSE`.
pub async fn consume_token(
    db: &Db,
    table: &str,
    id: Uuid,
    error: ApiError,
) -> Result<(), ApiError> {
    let res = db
        .execute(
            &format!("UPDATE {table} SET used = TRUE WHERE id = ? AND used = FALSE"),
            &[(id).into_arg()],
        )
        .await?;
    if res != 1 {
        return Err(error);
    }
    Ok(())
}

pub async fn pending_setup_row(db: &Db, user_id: Uuid) -> Result<Option<TokenRow>, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT id, user_id, expires_at, used FROM password_reset_tokens \
         WHERE user_id = ? AND purpose = 'password_setup' AND used = FALSE \
         ORDER BY created_at DESC LIMIT 1",
            &[(user_id).into_arg()],
        )
        .await?;
    Ok(row.map(|r| TokenRow {
        id: r.try_get("id").unwrap(),
        user_id: r.try_get("user_id").unwrap(),
        expires_at: r.try_get("expires_at").unwrap(),
        used: r.try_get("used").unwrap(),
    }))
}

pub async fn has_sessions(db: &Db, user_id: Uuid) -> Result<bool, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT id FROM refresh_tokens WHERE user_id = ? LIMIT 1",
            &[(user_id).into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

pub async fn clear_setup_tokens(db: &Db, user_id: Uuid) -> Result<(), ApiError> {
    db.execute("UPDATE password_reset_tokens SET used = TRUE WHERE user_id = ? AND purpose = 'password_setup' AND used = FALSE", &[(user_id).into_arg()])
    .await?;
    Ok(())
}

pub async fn issue_setup_token(db: &Db, user: &UserRow) -> Result<String, ApiError> {
    clear_setup_tokens(db, user.id).await?;
    let raw = token_urlsafe(32);
    db.execute("INSERT INTO password_reset_tokens (id, user_id, token_hash, purpose, expires_at, used, created_at) \
         VALUES (?, ?, ?, 'password_setup', ?, FALSE, ?)", &[(new_id()).into_arg(), (user.id).into_arg(), (hash_token(&raw)).into_arg(), (utc_now() + chrono::Duration::minutes(SETUP_TOKEN_EXPIRE_MINUTES)).into_arg(), (utc_now()).into_arg()])
    .await?;
    Ok(raw)
}

pub fn mask_phone(phone: Option<&str>) -> Option<String> {
    let digits = phone?.trim().to_string();
    if digits.chars().count() <= 4 {
        return Some(digits);
    }
    let chars: Vec<char> = digits.chars().collect();
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 2..].iter().collect();
    Some(format!("{head}****{tail}"))
}

pub async fn issue_verification_email(ctx: &Ctx<'_>, user: &UserRow) -> Result<(), ApiError> {
    let token = token_urlsafe(32);
    ctx.db.execute("INSERT INTO password_reset_tokens (id, user_id, token_hash, purpose, expires_at, used, created_at) \
         VALUES (?, ?, ?, 'email_verify', ?, FALSE, ?)", &[(new_id()).into_arg(), (user.id).into_arg(), (hash_token(&token)).into_arg(), (utc_now() + chrono::Duration::hours(EMAIL_VERIFY_EXPIRE_HOURS)).into_arg(), (utc_now()).into_arg()])
    .await?;
    if let Some(email) = &user.email {
        // Best-effort like Python: delivery failure never breaks
        // signup. The token is already stored, so the user can ask
        // for a resend; a failure is logged (redacted) and the flow
        // continues.
        let locale = crate::mail_templates::resolve_email_locale(
            user.phone.as_deref(),
            Some(user.locale.as_str()),
        );
        let action_url = crate::mail_templates::public_url(
            &ctx.state.config.app_base_url,
            &format!("/{locale}/verify-email?token={token}"),
        );
        let logo_url =
            crate::mail_templates::public_url(&ctx.state.config.app_base_url, "/logo.jpg");
        match action_url {
            Some(action_url) => {
                let rendered = crate::mail_templates::render_verification_email(
                    locale,
                    &action_url,
                    &token,
                    logo_url.as_deref(),
                    Some(user.full_name.as_str()),
                );
                if let Err(err) = crate::mail::send_email(
                    &ctx.state.config,
                    email,
                    &rendered.subject,
                    &rendered.text,
                    &rendered.html,
                )
                .await
                {
                    tracing::warn!(
                        "verification email to {email} failed: {}",
                        crate::mail::redact_secrets(&err.to_string())
                    );
                }
            }
            None => {
                tracing::warn!("verification email to {email} skipped: invalid APP_BASE_URL");
            }
        }
    }
    Ok(())
}

pub async fn revoke_user_tokens(db: &Db, user_id: Uuid) -> Result<(), ApiError> {
    db.execute(
        "UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
        &[(user_id).into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE sessions SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
        &[(user_id).into_arg()],
    )
    .await?;
    Ok(())
}

async fn revoke_session(db: &Db, session_id: Uuid) -> Result<(), ApiError> {
    db.execute(
        "UPDATE refresh_tokens SET revoked = TRUE WHERE session_id = ? AND revoked = FALSE",
        &[(session_id).into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE sessions SET revoked = TRUE WHERE id = ?",
        &[(session_id).into_arg()],
    )
    .await?;
    Ok(())
}

/// `create_session_pair` — new session + 30-day refresh token, event +
/// audit, then the access token (scope `2fa_setup` for privileged users
/// without TOTP, else `full`; `sid` always set).
pub async fn create_session_pair(
    ctx: &Ctx<'_>,
    user: &UserRow,
    device_info: Option<&str>,
    ip: Option<&str>,
) -> Result<(String, String), ApiError> {
    let session_id = new_id();
    ctx.db.execute("INSERT INTO sessions (id, user_id, device_info, ip, last_active_at, revoked, created_at) \
         VALUES (?, ?, ?, ?, ?, FALSE, ?)", &[(session_id).into_arg(), (user.id).into_arg(), (device_info).into_arg(), (ip).into_arg(), (utc_now()).into_arg(), (utc_now()).into_arg()])
    .await?;
    let plain = token_urlsafe(48);
    ctx.db.execute("INSERT INTO refresh_tokens (id, user_id, session_id, token_hash, rotated_from, revoked, expires_at, created_at) \
         VALUES (?, ?, ?, ?, NULL, FALSE, ?, ?)", &[(new_id()).into_arg(), (user.id).into_arg(), (session_id).into_arg(), (hash_token(&plain)).into_arg(), (utc_now() + chrono::Duration::days(REFRESH_EXPIRE_DAYS)).into_arg(), (utc_now()).into_arg()])
    .await?;
    emit_event(
        ctx.db,
        "user",
        &user.id.to_string(),
        "user.logged_in",
        json!({ "session_id": session_id.to_string() }),
    )
    .await?;
    log_audit(
        ctx.db,
        Some(user.id),
        "user",
        "user.login",
        "session",
        Some(&session_id.to_string()),
        None,
        Some(json!({ "ip": ip })),
        ip,
        device_info,
    )
    .await?;
    let scope = if must_enroll_2fa(ctx.db, user).await? {
        SCOPE_2FA_SETUP
    } else {
        SCOPE_FULL
    };
    let access = create_access_token(
        &ctx.state.config.jwt_secret,
        &user.id.to_string(),
        Some(session_id),
        scope,
        ctx.state.config.access_token_expire_minutes,
    )?;
    Ok((access, plain))
}

pub(crate) fn token_pair_json(access: &str, refresh: &str, must_enroll: bool) -> Value {
    json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "bearer",
        "must_enroll_2fa": must_enroll,
    })
}

pub async fn token_pair_for(
    ctx: &Ctx<'_>,
    user: &UserRow,
    device_info: Option<&str>,
    ip: Option<&str>,
) -> Result<Value, ApiError> {
    let (access, refresh) = create_session_pair(ctx, user, device_info, ip).await?;
    let must = must_enroll_2fa(ctx.db, user).await?;
    Ok(token_pair_json(&access, &refresh, must))
}

/// `authenticate` — rate limit, pending-registration gate, credentials,
/// TOTP (with atomic step claim), then reset the login limiter.
pub async fn authenticate_user(
    ctx: &Ctx<'_>,
    identifier: &str,
    password: &str,
    totp_code: Option<&str>,
    ip: Option<&str>,
) -> Result<UserRow, ApiError> {
    let rl_key = format!("login:{}", ip.unwrap_or("unknown"));
    check_rl(ctx, &rl_key, 20, 300).await?;
    let user = get_user_by_identifier(ctx.db, identifier).await?;
    if let Some(u) = &user {
        if u.is_active && pending_setup_row(ctx.db, u.id).await?.is_some() {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "registration_incomplete",
                "ثبت‌نام شما کامل نشده است؛ ابتدا ایمیل یا شماره‌تان را تأیید کنید و رمز عبور بگذارید",
            ));
        }
    }
    let Some(user) = user else {
        return Err(ApiError::unauthorized(
            "invalid_credentials",
            "ایمیل/شماره یا رمز عبور نادرست است",
        ));
    };
    if !user.is_active || !verify_password(&user.password_hash, password) {
        return Err(ApiError::unauthorized(
            "invalid_credentials",
            "ایمیل/شماره یا رمز عبور نادرست است",
        ));
    }
    if let Some(secret_envelope) = &user.totp_secret {
        let secret = decrypt_totp_secret(ctx, secret_envelope)?;
        let step = verify_totp_step(&secret, totp_code.unwrap_or(""), utc_now().timestamp());
        let Some(step) = step else {
            return Err(ApiError::with_details(
                StatusCode::UNAUTHORIZED,
                "requires_2fa",
                "کد تأیید دوعاملی لازم است",
                json!({ "requires_2fa": true }),
            ));
        };
        claim_totp_step(ctx.db, &user, step, 401).await?;
    }
    ctx.state.limiter.reset(&rl_key).await;
    Ok(user)
}

pub fn decrypt_totp_secret(ctx: &Ctx<'_>, envelope_str: &str) -> Result<String, ApiError> {
    let envelope: Value = serde_json::from_str(envelope_str).map_err(|_| ApiError::internal())?;
    decrypt_value(&ctx.state.config, &envelope)
}

pub async fn claim_totp_step(
    db: &Db,
    user: &UserRow,
    step: i64,
    error_status: u16,
) -> Result<(), ApiError> {
    let res = db.execute("UPDATE users SET totp_last_step = ? WHERE id = ? AND (totp_last_step IS NULL OR totp_last_step < ?)", &[(step).into_arg(), (user.id).into_arg(), (step).into_arg()])
    .await?;
    if res != 1 {
        let status = StatusCode::from_u16(error_status).unwrap_or(StatusCode::UNAUTHORIZED);
        return Err(ApiError::new(
            status,
            "code_reused",
            "این کد تأیید قبلاً استفاده شده است",
        ));
    }
    Ok(())
}

struct RefreshRow {
    id: Uuid,
    user_id: Uuid,
    session_id: Option<Uuid>,
    revoked: bool,
    expires_at: DateTime<Utc>,
}

async fn handle_token_reuse(ctx: &Ctx<'_>, row: &RefreshRow) -> ApiError {
    // Best-effort revoke + event + audit, then always 401.
    if let Some(sid) = row.session_id {
        let _ = revoke_session(ctx.db, sid).await;
    } else {
        let _ = revoke_user_tokens(ctx.db, row.user_id).await;
    }
    let _ = emit_event(
        ctx.db,
        "user",
        &row.user_id.to_string(),
        "token.reuse_detected",
        json!({ "session_id": row.session_id.map(|s| s.to_string()) }),
    )
    .await;
    let _ = log_audit(
        ctx.db,
        Some(row.user_id),
        "user",
        "token.reuse_detected",
        "refresh_token",
        Some(&row.id.to_string()),
        None,
        Some(json!({ "session_revoked": true })),
        None,
        None,
    )
    .await;
    ApiError::unauthorized("invalid_token", "رفرش‌توکن نامعتبر یا منقضی است")
}

async fn rotate_token(ctx: &Ctx<'_>, row: &RefreshRow) -> Result<(String, String), ApiError> {
    let user = load_user(ctx.db, row.user_id)
        .await?
        .ok_or_else(|| ApiError::unauthorized("invalid_token", "رفرش‌توکن نامعتبر است"))?;
    if !user.is_active {
        return Err(ApiError::unauthorized(
            "invalid_token",
            "رفرش‌توکن نامعتبر است",
        ));
    }
    let claim = ctx
        .db
        .execute(
            "UPDATE refresh_tokens SET revoked = TRUE WHERE id = ? AND revoked = FALSE",
            &[(row.id).into_arg()],
        )
        .await?;
    if claim != 1 {
        return Err(handle_token_reuse(ctx, row).await);
    }
    let new_plain = token_urlsafe(48);
    ctx.db.execute("INSERT INTO refresh_tokens (id, user_id, session_id, token_hash, rotated_from, revoked, expires_at, created_at) \
         VALUES (?, ?, ?, ?, ?, FALSE, ?, ?)", &[(new_id()).into_arg(), (user.id).into_arg(), (row.session_id).into_arg(), (hash_token(&new_plain)).into_arg(), (row.id).into_arg(), (utc_now() + chrono::Duration::days(REFRESH_EXPIRE_DAYS)).into_arg(), (utc_now()).into_arg()])
    .await?;
    if let Some(sid) = row.session_id {
        ctx.db
            .execute(
                "UPDATE sessions SET last_active_at = ? WHERE id = ?",
                &[(utc_now()).into_arg(), (sid).into_arg()],
            )
            .await?;
    }
    emit_event(
        ctx.db,
        "user",
        &user.id.to_string(),
        "token.refreshed",
        json!({ "rotated_from": row.id.to_string() }),
    )
    .await?;
    log_audit(
        ctx.db,
        Some(user.id),
        "user",
        "token.refresh",
        "refresh_token",
        Some(&row.id.to_string()),
        None,
        None,
        None,
        None,
    )
    .await?;
    let scope = if must_enroll_2fa(ctx.db, &user).await? {
        SCOPE_2FA_SETUP
    } else {
        SCOPE_FULL
    };
    let access = create_access_token(
        &ctx.state.config.jwt_secret,
        &user.id.to_string(),
        row.session_id,
        scope,
        ctx.state.config.access_token_expire_minutes,
    )?;
    Ok((access, new_plain))
}

pub async fn refresh_pair(
    ctx: &Ctx<'_>,
    refresh_token: &str,
) -> Result<(String, String), ApiError> {
    let row = ctx.db.fetch_optional("SELECT id, user_id, session_id, revoked, expires_at FROM refresh_tokens WHERE token_hash = ?", &[(hash_token(refresh_token)).into_arg()])
    .await?;
    let Some(row) = row else {
        return Err(ApiError::unauthorized(
            "invalid_token",
            "رفرش‌توکن نامعتبر یا منقضی است",
        ));
    };
    let row = RefreshRow {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        session_id: row.try_get("session_id")?,
        revoked: row.try_get("revoked")?,
        expires_at: row.try_get("expires_at")?,
    };
    if row.expires_at < utc_now() {
        return Err(ApiError::unauthorized(
            "invalid_token",
            "رفرش‌توکن نامعتبر یا منقضی است",
        ));
    }
    if row.revoked {
        return Err(handle_token_reuse(ctx, &row).await);
    }
    rotate_token(ctx, &row).await
}

pub async fn logout(ctx: &Ctx<'_>, user: &UserRow, refresh_token: &str) -> Result<(), ApiError> {
    let row = ctx
        .db
        .fetch_optional(
            "SELECT id, user_id, session_id FROM refresh_tokens WHERE token_hash = ?",
            &[(hash_token(refresh_token)).into_arg()],
        )
        .await?;
    if let Some(row) = row {
        let owner: Uuid = row.try_get("user_id")?;
        if owner == user.id {
            let id: Uuid = row.try_get("id")?;
            let session_id: Option<Uuid> = row.try_get("session_id")?;
            ctx.db
                .execute(
                    "UPDATE refresh_tokens SET revoked = TRUE WHERE id = ?",
                    &[(id).into_arg()],
                )
                .await?;
            if let Some(sid) = session_id {
                revoke_session(ctx.db, sid).await?;
            }
            emit_event(
                ctx.db,
                "user",
                &user.id.to_string(),
                "user.logged_out",
                json!({ "token_id": id.to_string() }),
            )
            .await?;
            log_audit(
                ctx.db,
                Some(user.id),
                "user",
                "user.logout",
                "refresh_token",
                Some(&id.to_string()),
                None,
                None,
                None,
                None,
            )
            .await?;
        }
    }
    Ok(())
}

pub async fn logout_all(ctx: &Ctx<'_>, user: &UserRow) -> Result<(), ApiError> {
    revoke_user_tokens(ctx.db, user.id).await?;
    emit_event(
        ctx.db,
        "user",
        &user.id.to_string(),
        "user.logged_out_all",
        json!({}),
    )
    .await?;
    log_audit(
        ctx.db,
        Some(user.id),
        "user",
        "user.logout_all",
        "user",
        Some(&user.id.to_string()),
        None,
        None,
        None,
        None,
    )
    .await?;
    Ok(())
}
