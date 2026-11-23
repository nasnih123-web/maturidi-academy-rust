//! Auth routes — mirrors `app/modules/auth/router.py` (22 routes).
//!
//! Response shapes, messages and status codes are copied from the
//! router + service. Registration is the web flow: password optional
//! at signup (a random hash makes login impossible until the user sets
//! a password after verifying), Turnstile captcha first, terms gate,
//! anti-enumeration on every public lookup.

use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    auth_service::*,
    authn::{authenticate, require_full_scope, UserRow},
    captcha::enforce_captcha,
    crypto::{hash_password, hash_token, token_urlsafe, verify_password},
    db::{new_id, utc_now, IntoArg},
    errors::{ApiError, ApiResult},
    settings::{encrypt_value, get_typed_setting},
    totp::{
        generate_recovery_codes, generate_totp_secret, totp_provisioning_uri, verify_totp_step,
    },
    validate::{is_valid_email, opt_str, parse_body, req_str},
    AppState,
};

fn client_ip_ua(
    state: &AppState,
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
) -> (Option<String>, Option<String>) {
    let ip = crate::authn::client_ip(headers, peer, &state.config.trusted_proxies);
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (ip, ua)
}

fn bad_token(msg: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_token", msg)
}

fn invalid_code(msg: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid_code", msg)
}

fn user_summary(user: &UserRow) -> Value {
    json!({
        "id": user.id.to_string(),
        "full_name": user.full_name,
        "email": user.email,
        "phone": user.phone,
        "locale": user.locale,
        "is_verified": user.is_verified,
        "created_at": user.created_at.to_rfc3339(),
    })
}

// ---------- email verification ----------

async fn consume_email_verify_token(ctx: &Ctx<'_>, token: &str) -> Result<UserRow, ApiError> {
    let row = fetch_token_row(ctx.db, "password_reset_tokens", token, Some("email_verify")).await?;
    let Some(row) = row else {
        return Err(bad_token("توکن تأیید نامعتبر یا منقضی است"));
    };
    if row.expires_at < utc_now() {
        return Err(bad_token("توکن تأیید نامعتبر یا منقضی است"));
    }
    let user = crate::authn::load_user(ctx.db, row.user_id)
        .await?
        .ok_or_else(|| bad_token("توکن تأیید نامعتبر است"))?;
    consume_token(
        ctx.db,
        "password_reset_tokens",
        row.id,
        bad_token("توکن تأیید نامعتبر یا منقضی است"),
    )
    .await?;
    Ok(user)
}

async fn resumed_verify_user(ctx: &Ctx<'_>, token: &str) -> Result<Option<UserRow>, ApiError> {
    let row = ctx
        .db
        .fetch_optional(
            "SELECT id, user_id, expires_at FROM password_reset_tokens \
         WHERE token_hash = ? AND purpose = 'email_verify' AND used = TRUE",
            &[(hash_token(token)).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let expires_at: DateTime<Utc> = row.try_get("expires_at")?;
    if expires_at < utc_now() {
        return Ok(None);
    }
    let user_id: Uuid = row.try_get("user_id")?;
    let user = crate::authn::load_user(ctx.db, user_id).await?;
    match user {
        Some(u) if u.is_verified => Ok(Some(u)),
        _ => Ok(None),
    }
}

async fn sms_required_for_phone(ctx: &Ctx<'_>, phone: Option<&str>) -> Result<bool, ApiError> {
    let Some(phone) = phone else {
        return Ok(false);
    };
    let iso = crate::countries::country_iso_from_phone(phone);
    let Some(iso) = iso else {
        return Ok(false);
    };
    let required = get_typed_setting(
        ctx.db,
        &ctx.state.config,
        "auth.sms_required_countries",
        json!([]),
    )
    .await?;
    Ok(required
        .as_array()
        .map(|a| a.iter().any(|v| v.as_str() == Some(iso)))
        .unwrap_or(false))
}

async fn registration_status_after_verify(
    ctx: &Ctx<'_>,
    user: &UserRow,
) -> Result<(String, Option<String>, Option<String>), ApiError> {
    if has_sessions(ctx.db, user.id).await? {
        return Ok(("already_active".to_string(), None, None));
    }
    if sms_required_for_phone(ctx, user.phone.as_deref()).await? {
        return Ok((
            "phone_required".to_string(),
            None,
            mask_phone(user.phone.as_deref()),
        ));
    }
    let setup = issue_setup_token(ctx.db, user).await?;
    Ok(("verified".to_string(), Some(setup), None))
}

async fn verify_email_inner(ctx: &Ctx<'_>, token: &str) -> Result<UserRow, ApiError> {
    match consume_email_verify_token(ctx, token).await {
        Ok(user) => {
            ctx.db
                .execute(
                    "UPDATE users SET is_verified = TRUE WHERE id = ?",
                    &[(user.id).into_arg()],
                )
                .await?;
            emit_event(
                ctx.db,
                "user",
                &user.id.to_string(),
                "user.email_verified",
                json!({}),
            )
            .await?;
            log_audit(
                ctx.db,
                Some(user.id),
                "user",
                "user.verify_email",
                "user",
                Some(&user.id.to_string()),
                None,
                None,
                None,
                None,
            )
            .await?;
            Ok(crate::authn::load_user(ctx.db, user.id).await?.unwrap())
        }
        Err(e) => {
            let resumed = resumed_verify_user(ctx, token).await?;
            match resumed {
                Some(u) => {
                    log_audit(
                        ctx.db,
                        Some(u.id),
                        "user",
                        "user.verify_email_resumed",
                        "user",
                        Some(&u.id.to_string()),
                        None,
                        Some(json!({ "resumed": true })),
                        None,
                        None,
                    )
                    .await?;
                    Ok(u)
                }
                None => Err(e),
            }
        }
    }
}

async fn verify_email_registration_inner(
    ctx: &Ctx<'_>,
    token: &str,
) -> Result<(String, Option<String>, Option<String>), ApiError> {
    match consume_email_verify_token(ctx, token).await {
        Ok(user) => {
            ctx.db
                .execute(
                    "UPDATE users SET is_verified = TRUE WHERE id = ?",
                    &[(user.id).into_arg()],
                )
                .await?;
            emit_event(
                ctx.db,
                "user",
                &user.id.to_string(),
                "user.email_verified",
                json!({}),
            )
            .await?;
            log_audit(
                ctx.db,
                Some(user.id),
                "user",
                "user.verify_email",
                "user",
                Some(&user.id.to_string()),
                None,
                None,
                None,
                None,
            )
            .await?;
            let user = crate::authn::load_user(ctx.db, user.id).await?.unwrap();
            registration_status_after_verify(ctx, &user).await
        }
        Err(e) => {
            let resumed = resumed_verify_user(ctx, token).await?;
            match resumed {
                Some(u) => {
                    log_audit(
                        ctx.db,
                        Some(u.id),
                        "user",
                        "user.verify_email_resumed",
                        "user",
                        Some(&u.id.to_string()),
                        None,
                        Some(json!({ "resumed": true })),
                        None,
                        None,
                    )
                    .await?;
                    if pending_setup_row(ctx.db, u.id).await?.is_none() {
                        Ok(("already_active".to_string(), None, None))
                    } else {
                        registration_status_after_verify(ctx, &u).await
                    }
                }
                None => Err(e),
            }
        }
    }
}

// ---------- phone codes ----------

fn six_digit_code() -> String {
    let n = {
        use rand::Rng;
        rand::thread_rng().gen_range(100_000..1_000_000)
    };
    format!("{n:06}")
}

async fn request_register_phone_code_inner(ctx: &Ctx<'_>, phone: &str) -> Result<(), ApiError> {
    let user = get_user_by_identifier(ctx.db, phone).await?;
    let Some(user) = user else { return Ok(()) };
    if pending_setup_row(ctx.db, user.id).await?.is_none() {
        return Ok(());
    }
    if !ctx
        .state
        .limiter
        .is_allowed(
            &format!("reg_sms:{phone}"),
            5,
            3600,
            true,
            &ctx.state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست کد بیش از حد مجاز است",
        ));
    }
    let code = six_digit_code();
    ctx.db.execute("INSERT INTO phone_verification_codes (id, user_id, phone, code_hash, expires_at, used, created_at) \
         VALUES (?, ?, ?, ?, ?, FALSE, ?)", &[(new_id()).into_arg(), (user.id).into_arg(), (phone).into_arg(), (hash_token(&code)).into_arg(), (utc_now() + chrono::Duration::minutes(PHONE_CODE_EXPIRE_MINUTES)).into_arg(), (utc_now()).into_arg()])
    .await?;
    // Best-effort like Python: the code is already stored, so a
    // delivery failure never breaks the request — the user can ask
    // for a resend. The failure is logged (redacted); the response
    // keeps Python's wording either way.
    if let Err(err) = crate::mail::send_sms(
        &ctx.state.config,
        phone,
        &format!("کد تأیید آکادمی ماتریدی: {code}"),
    )
    .await
    {
        tracing::warn!(
            "phone code sms to {phone} failed: {}",
            crate::mail::redact_secrets(&err.to_string())
        );
    }
    Ok(())
}

async fn verify_register_phone_code_inner(
    ctx: &Ctx<'_>,
    phone: &str,
    code: &str,
) -> Result<String, ApiError> {
    if !ctx
        .state
        .limiter
        .is_allowed(
            &format!("reg_sms_verify:{phone}"),
            10,
            3600,
            true,
            &ctx.state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد تلاش بیش از حد مجاز است",
        ));
    }
    let user = get_user_by_identifier(ctx.db, phone).await?;
    let mut row_id: Option<Uuid> = None;
    let mut user_id: Option<Uuid> = None;
    if let Some(u) = &user {
        let row = ctx
            .db
            .fetch_optional(
                "SELECT id, code_hash, expires_at FROM phone_verification_codes \
             WHERE user_id = ? AND phone = ? AND used = FALSE ORDER BY created_at DESC LIMIT 1",
                &[(u.id).into_arg(), (phone).into_arg()],
            )
            .await?;
        if let Some(r) = row {
            let expires_at: DateTime<Utc> = r.try_get("expires_at")?;
            let code_hash: String = r.try_get("code_hash")?;
            if expires_at >= utc_now() && code_hash == hash_token(code) {
                row_id = Some(r.try_get("id")?);
                user_id = Some(u.id);
            }
        }
    }
    let (Some(row_id), Some(user_id)) = (row_id, user_id) else {
        return Err(invalid_code("کد تأیید نامعتبر یا منقضی است"));
    };
    consume_token(
        ctx.db,
        "phone_verification_codes",
        row_id,
        invalid_code("کد تأیید نامعتبر یا منقضی است"),
    )
    .await?;
    let user = crate::authn::load_user(ctx.db, user_id).await?.unwrap();
    ctx.db
        .execute(
            "UPDATE users SET is_verified = TRUE WHERE id = ?",
            &[(user.id).into_arg()],
        )
        .await?;
    emit_event(
        ctx.db,
        "user",
        &user.id.to_string(),
        "user.phone_verified",
        json!({}),
    )
    .await?;
    log_audit(
        ctx.db,
        Some(user.id),
        "user",
        "user.verify_phone",
        "user",
        Some(&user.id.to_string()),
        None,
        None,
        None,
        None,
    )
    .await?;
    issue_setup_token(ctx.db, &user).await
}

async fn set_password_with_setup_token_inner(
    ctx: &Ctx<'_>,
    token: &str,
    new_password: &str,
) -> Result<UserRow, ApiError> {
    let row = fetch_token_row(
        ctx.db,
        "password_reset_tokens",
        token,
        Some("password_setup"),
    )
    .await?;
    let Some(row) = row else {
        return Err(bad_token("توکن نامعتبر یا منقضی است"));
    };
    if row.expires_at < utc_now() {
        return Err(bad_token("توکن نامعتبر یا منقضی است"));
    }
    let user = crate::authn::load_user(ctx.db, row.user_id)
        .await?
        .ok_or_else(|| bad_token("توکن نامعتبر یا منقضی است"))?;
    if !user.is_verified {
        return Err(bad_token("توکن نامعتبر یا منقضی است"));
    }
    consume_token(
        ctx.db,
        "password_reset_tokens",
        row.id,
        bad_token("توکن نامعتبر یا منقضی است"),
    )
    .await?;
    clear_setup_tokens(ctx.db, user.id).await?;
    let hash = hash_password(new_password)?;
    ctx.db
        .execute(
            "UPDATE users SET password_hash = ? WHERE id = ?",
            &[(hash).into_arg(), (user.id).into_arg()],
        )
        .await?;
    Ok(crate::authn::load_user(ctx.db, user.id).await?.unwrap())
}

// ---------- password reset ----------

async fn request_password_reset_inner(
    ctx: &Ctx<'_>,
    email: &str,
    ip: Option<&str>,
) -> Result<(), ApiError> {
    let normalized = email.trim().to_lowercase();
    if !ctx
        .state
        .limiter
        .is_allowed(
            &format!("pwreset:ip:{}", ip.unwrap_or("unknown")),
            10,
            3600,
            true,
            &ctx.state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    if !normalized.is_empty()
        && !ctx
            .state
            .limiter
            .is_allowed(
                &format!("pwreset:email:{normalized}"),
                3,
                3600,
                true,
                &ctx.state.config.app_env,
            )
            .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    let user = get_user_by_identifier(ctx.db, email).await?;
    if let Some(user) = user {
        if user.email.is_some() {
            let token = token_urlsafe(32);
            ctx.db.execute("INSERT INTO password_reset_tokens (id, user_id, token_hash, purpose, expires_at, used, created_at) \
                 VALUES (?, ?, ?, 'reset', ?, FALSE, ?)", &[(new_id()).into_arg(), (user.id).into_arg(), (hash_token(&token)).into_arg(), (utc_now() + chrono::Duration::hours(RESET_TOKEN_EXPIRE_HOURS)).into_arg(), (utc_now()).into_arg()])
            .await?;
            if let Some(to) = &user.email {
                // Best-effort like Python: the token is already
                // stored, so a delivery failure never breaks the
                // request — the response stays conditional either way.
                let locale = crate::mail_templates::resolve_email_locale(
                    user.phone.as_deref(),
                    Some(user.locale.as_str()),
                );
                let action_url = crate::mail_templates::public_url(
                    &ctx.state.config.app_base_url,
                    &format!("/{locale}/reset-password?token={token}"),
                );
                let logo_url =
                    crate::mail_templates::public_url(&ctx.state.config.app_base_url, "/logo.jpg");
                if let Some(action_url) = action_url {
                    let rendered = crate::mail_templates::render_password_reset_email(
                        locale,
                        &action_url,
                        &token,
                        logo_url.as_deref(),
                        Some(user.full_name.as_str()),
                    );
                    if let Err(err) = crate::mail::send_email(
                        &ctx.state.config,
                        to,
                        &rendered.subject,
                        &rendered.text,
                        &rendered.html,
                    )
                    .await
                    {
                        tracing::warn!(
                            "password reset email to {to} failed: {}",
                            crate::mail::redact_secrets(&err.to_string())
                        );
                    }
                } else {
                    tracing::warn!("password reset email to {to} skipped: invalid APP_BASE_URL");
                }
            }
        }
    }
    Ok(())
}

async fn confirm_password_reset_inner(
    ctx: &Ctx<'_>,
    token: &str,
    new_password: &str,
) -> Result<(), ApiError> {
    let row = fetch_token_row(ctx.db, "password_reset_tokens", token, Some("reset")).await?;
    let Some(row) = row else {
        return Err(bad_token("توکن بازیابی نامعتبر یا منقضی است"));
    };
    if row.expires_at < utc_now() {
        return Err(bad_token("توکن بازیابی نامعتبر یا منقضی است"));
    }
    let user = crate::authn::load_user(ctx.db, row.user_id)
        .await?
        .ok_or_else(|| bad_token("توکن بازیابی نامعتبر است"))?;
    if !user.is_active {
        return Err(bad_token("توکن بازیابی نامعتبر است"));
    }
    consume_token(
        ctx.db,
        "password_reset_tokens",
        row.id,
        bad_token("توکن بازیابی نامعتبر یا منقضی است"),
    )
    .await?;
    let hash = hash_password(new_password)?;
    ctx.db
        .execute(
            "UPDATE users SET password_hash = ? WHERE id = ?",
            &[(hash).into_arg(), (user.id).into_arg()],
        )
        .await?;
    revoke_user_tokens(ctx.db, user.id).await?;
    emit_event(
        ctx.db,
        "user",
        &user.id.to_string(),
        "user.password_reset",
        json!({}),
    )
    .await?;
    log_audit(
        ctx.db,
        Some(user.id),
        "user",
        "user.password_reset",
        "user",
        Some(&user.id.to_string()),
        None,
        Some(json!({ "password": { "changed": true } })),
        None,
        None,
    )
    .await?;
    Ok(())
}

// ---------- 2FA ----------

async fn verify_second_factor(
    ctx: &Ctx<'_>,
    user: &UserRow,
    password: &str,
    code: Option<&str>,
    recovery_code: Option<&str>,
) -> Result<&'static str, ApiError> {
    if !verify_password(&user.password_hash, password) {
        return Err(ApiError::unauthorized(
            "invalid_credentials",
            "رمز عبور نادرست است",
        ));
    }
    let Some(envelope) = &user.totp_secret else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "no_2fa",
            "احراز دوعاملی برای این حساب فعال نیست",
        ));
    };
    if let Some(code) = code {
        let secret = decrypt_totp_secret(ctx, envelope)?;
        let step = verify_totp_step(&secret, code, utc_now().timestamp());
        let Some(step) = step else {
            return Err(invalid_code("کد تأیید دوعاملی نادرست است"));
        };
        claim_totp_step(ctx.db, user, step, 400).await?;
        return Ok("totp");
    }
    if let Some(recovery_code) = recovery_code {
        let wanted = hash_token(&recovery_code.trim().to_uppercase());
        let rows = ctx
            .db
            .fetch_all(
                "SELECT id, code_hash FROM recovery_codes WHERE user_id = ? AND used = FALSE",
                &[(user.id).into_arg()],
            )
            .await?;
        let mut matched: Option<Uuid> = None;
        for r in rows {
            let h: String = r.try_get("code_hash")?;
            if h == wanted {
                matched = Some(r.try_get("id")?);
                break;
            }
        }
        let Some(id) = matched else {
            return Err(invalid_code("کد بازیابی نامعتبر است"));
        };
        consume_token(
            ctx.db,
            "recovery_codes",
            id,
            invalid_code("کد بازیابی نامعتبر است"),
        )
        .await?;
        return Ok("recovery");
    }
    Err(ApiError::new(
        StatusCode::BAD_REQUEST,
        "second_factor_required",
        "کد تأیید دوعاملی یا کد بازیابی لازم است",
    ))
}

async fn revoke_other_sessions(
    ctx: &Ctx<'_>,
    user_id: Uuid,
    keep: Option<Uuid>,
) -> Result<(), ApiError> {
    if let Some(keep_id) = keep {
        ctx.db
            .execute(
                "UPDATE sessions SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE AND id != ?",
                &[(user_id).into_arg(), (keep_id).into_arg()],
            )
            .await?;
        ctx.db.execute("UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE AND (session_id IS NULL OR session_id != ?)", &[(user_id).into_arg(), (keep_id).into_arg()])
            .await?;
    } else {
        ctx.db
            .execute(
                "UPDATE sessions SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
                &[(user_id).into_arg()],
            )
            .await?;
        ctx.db
            .execute(
                "UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
                &[(user_id).into_arg()],
            )
            .await?;
    }
    Ok(())
}

async fn store_recovery_codes(ctx: &Ctx<'_>, user_id: Uuid) -> Result<Vec<String>, ApiError> {
    ctx.db
        .execute(
            "DELETE FROM recovery_codes WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let codes = generate_recovery_codes(10);
    for code in &codes {
        ctx.db.execute("INSERT INTO recovery_codes (id, user_id, code_hash, used, created_at) VALUES (?, ?, ?, FALSE, ?)", &[(new_id()).into_arg(), (user_id).into_arg(), (hash_token(code)).into_arg(), (utc_now()).into_arg()])
        .await?;
    }
    Ok(codes)
}

// ---------- handlers ----------

pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let (ip, ua) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let full_name = req_str(&parsed, "full_name", 2, 200)?;
    let email = opt_str(&parsed, "email", 1, 320)?;
    if let Some(e) = &email {
        if !is_valid_email(e) {
            return Err(ApiError::validation(vec![
                json!({ "field": "email", "msg": "invalid email" }),
            ]));
        }
    }
    let phone = opt_str(&parsed, "phone", 8, 32)?;
    let password = opt_str(&parsed, "password", 8, 128)?;
    let locale = opt_str(&parsed, "locale", 1, 8)?.unwrap_or_else(|| "fa".to_string());
    let accept_terms = parsed
        .get("accept_terms")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let captcha_token = parsed.get("captcha_token").and_then(|v| v.as_str());
    enforce_captcha(&state, captcha_token, ip.as_deref()).await?;
    if !accept_terms {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "terms_required",
            "پذیرش قوانین و حریم‌خصوصی الزامی است",
        ));
    }
    if email.is_none() && phone.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "contact_required",
            "ایمیل یا شماره تلفن الزامی است",
        ));
    }
    let rl_key = format!(
        "register:{}",
        ip.clone().unwrap_or_else(|| "unknown".to_string())
    );
    if !state
        .limiter
        .is_allowed(&rl_key, 10, 3600, true, &state.config.app_env)
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    let existing = if let Some(e) = &email {
        get_user_by_identifier(db, e).await?
    } else {
        None
    };
    if let Some(ex) = &existing {
        if pending_setup_row(db, ex.id).await?.is_none() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "email_taken",
                "این ایمیل قبلاً ثبت شده است",
            ));
        }
    }
    if let Some(p) = &phone {
        let phone_owner = get_user_by_identifier(db, p).await?;
        if let Some(owner) = phone_owner {
            let same = existing.as_ref().map(|e| e.id == owner.id).unwrap_or(false);
            if !same {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "phone_taken",
                    "این شماره قبلاً ثبت شده است",
                ));
            }
        }
    }
    if let Some(mut ex) = existing {
        ex.full_name = full_name;
        ex.locale = locale;
        if let Some(p) = &phone {
            if ex.phone.is_none() {
                ex.phone = Some(p.clone());
            }
        }
        db.execute(
            "UPDATE users SET full_name = ?, locale = ?, phone = ? WHERE id = ?",
            &[
                (&ex.full_name).into_arg(),
                (&ex.locale).into_arg(),
                (&ex.phone).into_arg(),
                (ex.id).into_arg(),
            ],
        )
        .await?;
        if ex.email.is_some() {
            issue_verification_email(&ctx, &ex).await?;
        }
        let user = crate::authn::load_user(db, ex.id).await?.unwrap();
        return Ok((StatusCode::CREATED, Json(user_summary(&user))));
    }
    let user_id = new_id();
    let password_hash = hash_password(password.as_deref().unwrap_or(&token_urlsafe(32)))?;
    db.execute("INSERT INTO users (id, full_name, phone, email, password_hash, locale, is_active, is_verified, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, TRUE, FALSE, ?)", &[(user_id).into_arg(), (&full_name).into_arg(), (&phone).into_arg(), (&email).into_arg(), (&password_hash).into_arg(), (&locale).into_arg(), (utc_now()).into_arg()])
    .await?;
    for consent_type in ["terms", "privacy"] {
        db.execute(
            "INSERT INTO user_consent (id, user_id, consent_type, text_version, accepted_at, ip) \
             VALUES (?, ?, ?, ?, ?, ?)",
            &[
                (new_id()).into_arg(),
                (user_id).into_arg(),
                (consent_type).into_arg(),
                (LEGAL_TEXT_VERSION).into_arg(),
                (utc_now()).into_arg(),
                (ip.as_deref()).into_arg(),
            ],
        )
        .await?;
    }
    for channel in ["email", "sms", "push", "web_push", "in_app"] {
        for notif_type in ["security", "product", "marketing"] {
            let enabled =
                !(matches!(channel, "sms" | "push" | "web_push") && notif_type == "marketing");
            db.execute("INSERT INTO notification_preferences (id, user_id, channel, notif_type, enabled, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)", &[(new_id()).into_arg(), (user_id).into_arg(), (channel).into_arg(), (notif_type).into_arg(), (enabled).into_arg(), (utc_now()).into_arg()])
            .await?;
        }
    }
    if password.is_none() {
        db.execute("INSERT INTO password_reset_tokens (id, user_id, token_hash, purpose, expires_at, used, created_at) \
             VALUES (?, ?, ?, 'password_setup', ?, FALSE, ?)", &[(new_id()).into_arg(), (user_id).into_arg(), (hash_token(&token_urlsafe(32))).into_arg(), (utc_now() + chrono::Duration::days(REGISTRATION_MARKER_EXPIRE_DAYS)).into_arg(), (utc_now()).into_arg()])
        .await?;
    }
    let user = crate::authn::load_user(db, user_id).await?.unwrap();
    if user.email.is_some() {
        issue_verification_email(&ctx, &user).await?;
    }
    emit_event(
        db,
        "user",
        &user.id.to_string(),
        "user.registered",
        json!({ "email": user.email, "phone_set": user.phone.is_some(), "locale": user.locale }),
    )
    .await?;
    log_audit(
        db,
        Some(user.id),
        "user",
        "user.register",
        "user",
        Some(&user.id.to_string()),
        None,
        Some(json!({ "email": user.email, "locale": user.locale })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(user_summary(&user))))
}

pub async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let (ip, ua) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let identifier = req_str(&parsed, "identifier", 3, 320)?;
    let password = req_str(&parsed, "password", 1, 128)?;
    let totp_code = opt_str(&parsed, "totp_code", 1, 16)?;
    let user = authenticate_user(
        &ctx,
        &identifier,
        &password,
        totp_code.as_deref(),
        ip.as_deref(),
    )
    .await?;
    Ok(Json(
        token_pair_for(&ctx, &user, ua.as_deref(), ip.as_deref()).await?,
    ))
}

pub async fn refresh(State(state): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let refresh_token = req_str(&parsed, "refresh_token", 10, 4096)?;
    let (access, refresh) = refresh_pair(&ctx, &refresh_token).await?;
    Ok(Json(crate::auth_service::token_pair_json(
        &access, &refresh, false,
    )))
}

pub async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/logout").await?;
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let refresh_token = req_str(&parsed, "refresh_token", 10, 4096)?;
    crate::auth_service::logout(&ctx, &ctx_user.user, &refresh_token).await?;
    Ok(Json(json!({ "message": "با موفقیت خارج شدید" })))
}

pub async fn logout_all(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/logout-all").await?;
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    crate::auth_service::logout_all(&ctx, &ctx_user.user).await?;
    Ok(Json(json!({ "message": "از همهٔ دستگاه‌ها خارج شدید" })))
}

pub async fn verify_email(State(state): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let token = req_str(&parsed, "token", 10, 4096)?;
    verify_email_inner(&ctx, &token).await?;
    Ok(Json(json!({ "message": "ایمیل با موفقیت تأیید شد" })))
}

pub async fn register_verify_email(
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let token = req_str(&parsed, "token", 10, 4096)?;
    let (status, setup_token, phone_hint) = verify_email_registration_inner(&ctx, &token).await?;
    Ok(Json(json!({
        "status": status,
        "setup_token": setup_token,
        "phone_hint": phone_hint,
    })))
}

pub async fn register_phone_code_request(
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let phone = req_str(&parsed, "phone", 8, 32)?;
    request_register_phone_code_inner(&ctx, &phone).await?;
    Ok(Json(
        json!({ "message": "اگر ثبت‌نامی با این شماره در انتظار تکمیل باشد، کد تأیید پیامک می‌شود" }),
    ))
}

pub async fn register_phone_code_verify(
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let phone = req_str(&parsed, "phone", 8, 32)?;
    let code = req_str(&parsed, "code", 4, 16)?;
    let setup_token = verify_register_phone_code_inner(&ctx, &phone, &code).await?;
    Ok(Json(json!({ "setup_token": setup_token })))
}

pub async fn register_set_password(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let (ip, ua) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let setup_token = req_str(&parsed, "setup_token", 16, 256)?;
    let new_password = req_str(&parsed, "new_password", 8, 128)?;
    let user = set_password_with_setup_token_inner(&ctx, &setup_token, &new_password).await?;
    Ok(Json(
        token_pair_for(&ctx, &user, ua.as_deref(), ip.as_deref()).await?,
    ))
}

pub async fn register_resend(State(state): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let identifier = req_str(&parsed, "identifier", 3, 320)?;
    let user = get_user_by_identifier(db, &identifier).await?;
    if let Some(user) = user {
        if user.email.is_some() && pending_setup_row(db, user.id).await?.is_some() {
            let rl_key = format!("reg_resend:{}", user.id);
            if state
                .limiter
                .is_allowed(&rl_key, 3, 3600, true, &state.config.app_env)
                .await
            {
                issue_verification_email(&ctx, &user).await?;
            }
        }
    }
    Ok(Json(
        json!({ "message": "اگر ثبت‌نام نیمه‌کاره‌ای با این مشخصات باشد، لینک تأیید دوباره ارسال شد" }),
    ))
}

pub async fn verify_phone_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/verify-phone/request").await?;
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let phone = req_str(&parsed, "phone", 8, 32)?;
    if !state
        .limiter
        .is_allowed(
            &format!("sms:{}", ctx_user.user.id),
            5,
            3600,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست کد بیش از حد مجاز است",
        ));
    }
    let code = six_digit_code();
    db.execute("INSERT INTO phone_verification_codes (id, user_id, phone, code_hash, expires_at, used, created_at) \
         VALUES (?, ?, ?, ?, ?, FALSE, ?)", &[(new_id()).into_arg(), (ctx_user.user.id).into_arg(), (&phone).into_arg(), (hash_token(&code)).into_arg(), (utc_now() + chrono::Duration::minutes(PHONE_CODE_EXPIRE_MINUTES)).into_arg(), (utc_now()).into_arg()])
    .await?;
    // Best-effort like Python: the code is already stored; a
    // delivery failure is logged (redacted) and the response keeps
    // Python's wording either way.
    if let Err(err) = crate::mail::send_sms(
        &ctx.state.config,
        &phone,
        &format!("کد تأیید آکادمی ماتریدی: {code}"),
    )
    .await
    {
        tracing::warn!(
            "phone code sms to {phone} failed: {}",
            crate::mail::redact_secrets(&err.to_string())
        );
    }
    let _ = ctx;
    Ok(Json(json!({ "message": "کد تأیید پیامک شد" })))
}

pub async fn verify_phone(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/verify-phone").await?;
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let phone = req_str(&parsed, "phone", 8, 32)?;
    let code = req_str(&parsed, "code", 4, 16)?;
    let row = db
        .fetch_optional(
            "SELECT id, code_hash, expires_at FROM phone_verification_codes \
         WHERE user_id = ? AND phone = ? AND used = FALSE ORDER BY created_at DESC LIMIT 1",
            &[(ctx_user.user.id).into_arg(), (&phone).into_arg()],
        )
        .await?;
    let valid = match &row {
        Some(r) => {
            let expires_at: DateTime<Utc> = r.try_get("expires_at")?;
            let code_hash: String = r.try_get("code_hash")?;
            expires_at >= utc_now() && code_hash == hash_token(&code)
        }
        None => false,
    };
    if !valid {
        return Err(invalid_code("کد تأیید نامعتبر یا منقضی است"));
    }
    let row = row.unwrap();
    let row_id: Uuid = row.try_get("id")?;
    consume_token(
        db,
        "phone_verification_codes",
        row_id,
        invalid_code("کد تأیید نامعتبر یا منقضی است"),
    )
    .await?;
    db.execute(
        "UPDATE users SET phone = ?, is_verified = TRUE WHERE id = ?",
        &[(&phone).into_arg(), (ctx_user.user.id).into_arg()],
    )
    .await?;
    emit_event(
        db,
        "user",
        &ctx_user.user.id.to_string(),
        "user.phone_verified",
        json!({}),
    )
    .await?;
    log_audit(
        db,
        Some(ctx_user.user.id),
        "user",
        "user.verify_phone",
        "user",
        Some(&ctx_user.user.id.to_string()),
        None,
        None,
        None,
        None,
    )
    .await?;
    let _ = ctx;
    Ok(Json(json!({ "message": "شماره تلفن با موفقیت تأیید شد" })))
}

pub async fn password_reset_request(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let (ip, _) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let email = req_str(&parsed, "email", 1, 320)?;
    if !is_valid_email(&email) {
        return Err(ApiError::validation(vec![
            json!({ "field": "email", "msg": "invalid email" }),
        ]));
    }
    request_password_reset_inner(&ctx, &email, ip.as_deref()).await?;
    Ok(Json(
        json!({ "message": "اگر این ایمیل ثبت شده باشد، لینک بازیابی ارسال شد" }),
    ))
}

pub async fn password_reset_confirm(
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let token = req_str(&parsed, "token", 10, 4096)?;
    let new_password = req_str(&parsed, "new_password", 8, 128)?;
    confirm_password_reset_inner(&ctx, &token, &new_password).await?;
    Ok(Json(json!({ "message": "رمز عبور با موفقیت تغییر کرد" })))
}

pub async fn twofa_setup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/2fa/setup").await?;
    let secret = generate_totp_secret();
    let account = ctx_user
        .user
        .email
        .clone()
        .or_else(|| ctx_user.user.phone.clone())
        .unwrap_or_else(|| ctx_user.user.id.to_string());
    let uri = totp_provisioning_uri(&secret, &account);
    Ok(Json(json!({ "secret": secret, "provisioning_uri": uri })))
}

pub async fn twofa_enable(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/2fa/enable").await?;
    let (ip, _) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let secret = req_str(&parsed, "secret", 10, 256)?;
    let code = req_str(&parsed, "code", 4, 16)?;
    if !state
        .limiter
        .is_allowed(
            &format!("2fa:enable:{}", ctx_user.user.id),
            20,
            600,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد تلاش بیش از حد مجاز است",
        ));
    }
    let step = verify_totp_step(&secret, &code, utc_now().timestamp());
    let Some(step) = step else {
        return Err(invalid_code("کد تأیید دوعاملی نادرست است"));
    };
    claim_totp_step(db, &ctx_user.user, step, 400).await?;
    let envelope = encrypt_value(&state.config, &secret);
    let envelope_str = serde_json::to_string(&envelope).map_err(|_| ApiError::internal())?;
    db.execute(
        "UPDATE users SET totp_secret = ?, totp_enabled_at = ? WHERE id = ?",
        &[
            (envelope_str).into_arg(),
            (utc_now()).into_arg(),
            (ctx_user.user.id).into_arg(),
        ],
    )
    .await?;
    let codes = store_recovery_codes(&ctx, ctx_user.user.id).await?;
    emit_event(
        db,
        "user",
        &ctx_user.user.id.to_string(),
        "user.2fa_enabled",
        json!({}),
    )
    .await?;
    log_audit(
        db,
        Some(ctx_user.user.id),
        "user",
        "user.2fa_enable",
        "user",
        Some(&ctx_user.user.id.to_string()),
        None,
        None,
        ip.as_deref(),
        None,
    )
    .await?;
    Ok(Json(json!({ "recovery_codes": codes })))
}

pub async fn twofa_recover(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let (ip, _) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let email = req_str(&parsed, "email", 1, 320)?;
    if !is_valid_email(&email) {
        return Err(ApiError::validation(vec![
            json!({ "field": "email", "msg": "invalid email" }),
        ]));
    }
    let password = req_str(&parsed, "password", 1, 128)?;
    let recovery_code = req_str(&parsed, "recovery_code", 4, 32)?;
    if !state
        .limiter
        .is_allowed(
            &format!(
                "2fa:recover:{}",
                ip.clone().unwrap_or_else(|| "unknown".to_string())
            ),
            10,
            3600,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد تلاش بیش از حد مجاز است",
        ));
    }
    let user = get_user_by_identifier(db, &email).await?;
    let Some(user) = user else {
        return Err(ApiError::unauthorized(
            "invalid_credentials",
            "ایمیل یا رمز عبور نادرست است",
        ));
    };
    if !user.is_active || !verify_password(&user.password_hash, &password) {
        return Err(ApiError::unauthorized(
            "invalid_credentials",
            "ایمیل یا رمز عبور نادرست است",
        ));
    }
    if user.totp_secret.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "no_2fa",
            "احراز دوعاملی برای این حساب فعال نیست",
        ));
    }
    let wanted = hash_token(&recovery_code.trim().to_uppercase());
    let rows = db
        .fetch_all(
            "SELECT id, code_hash FROM recovery_codes WHERE user_id = ? AND used = FALSE",
            &[(user.id).into_arg()],
        )
        .await?;
    let mut matched: Option<Uuid> = None;
    for r in rows {
        let h: String = r.try_get("code_hash")?;
        if h == wanted {
            matched = Some(r.try_get("id")?);
            break;
        }
    }
    let Some(id) = matched else {
        return Err(invalid_code("کد بازیابی نامعتبر است"));
    };
    consume_token(
        db,
        "recovery_codes",
        id,
        invalid_code("کد بازیابی نامعتبر است"),
    )
    .await?;
    db.execute("UPDATE users SET totp_secret = NULL, totp_last_step = NULL, totp_enabled_at = NULL WHERE id = ?", &[(user.id).into_arg()])
        .await?;
    revoke_user_tokens(db, user.id).await?;
    emit_event(
        db,
        "user",
        &user.id.to_string(),
        "user.2fa_recovered",
        json!({}),
    )
    .await?;
    log_audit(
        db,
        Some(user.id),
        "user",
        "user.2fa_recover",
        "user",
        Some(&user.id.to_string()),
        None,
        None,
        ip.as_deref(),
        None,
    )
    .await?;
    Ok(Json(
        json!({ "message": "احراز دوعاملی غیرفعال شد؛ لطفاً دوباره فعال کنید" }),
    ))
}

pub async fn twofa_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/2fa/status").await?;
    let db = state.db_or_500()?;
    let remaining: i64 = db
        .fetch_one(
            "SELECT COUNT(*) as c FROM recovery_codes WHERE user_id = ? AND used = FALSE",
            &[(ctx_user.user.id).into_arg()],
        )
        .await?
        .try_get("c")?;
    Ok(Json(json!({
        "enabled": ctx_user.user.totp_secret.is_some(),
        "enrolled_at": ctx_user.user.totp_enabled_at.map(|d| d.to_rfc3339()),
        "recovery_codes_remaining": remaining,
    })))
}

pub async fn twofa_disable(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/2fa/disable").await?;
    let (ip, _) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let password = req_str(&parsed, "password", 1, 128)?;
    let code = opt_str(&parsed, "code", 1, 16)?;
    let recovery_code = opt_str(&parsed, "recovery_code", 1, 32)?;
    if !state
        .limiter
        .is_allowed(
            &format!("2fa:disable:{}", ctx_user.user.id),
            10,
            600,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد تلاش بیش از حد مجاز است",
        ));
    }
    let method = verify_second_factor(
        &ctx,
        &ctx_user.user,
        &password,
        code.as_deref(),
        recovery_code.as_deref(),
    )
    .await?;
    db.execute("UPDATE users SET totp_secret = NULL, totp_last_step = NULL, totp_enabled_at = NULL WHERE id = ?", &[(ctx_user.user.id).into_arg()])
        .await?;
    db.execute(
        "DELETE FROM recovery_codes WHERE user_id = ?",
        &[(ctx_user.user.id).into_arg()],
    )
    .await?;
    revoke_other_sessions(&ctx, ctx_user.user.id, ctx_user.session_id).await?;
    emit_event(
        db,
        "user",
        &ctx_user.user.id.to_string(),
        "user.2fa_disabled",
        json!({ "method": method }),
    )
    .await?;
    log_audit(
        db,
        Some(ctx_user.user.id),
        "user",
        "user.2fa_disable",
        "user",
        Some(&ctx_user.user.id.to_string()),
        None,
        Some(json!({ "method": method })),
        ip.as_deref(),
        None,
    )
    .await?;
    Ok(Json(json!({ "message": "احراز دوعاملی غیرفعال شد" })))
}

pub async fn twofa_regenerate(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/2fa/recovery/regenerate").await?;
    let (ip, _) = client_ip_ua(&state, &headers, Some(peer.0));
    let db = state.db_or_500()?;
    let ctx = Ctx::new(&state, db);
    let parsed = parse_body(&body)?;
    let password = req_str(&parsed, "password", 1, 128)?;
    let code = opt_str(&parsed, "code", 1, 16)?;
    let recovery_code = opt_str(&parsed, "recovery_code", 1, 32)?;
    if !state
        .limiter
        .is_allowed(
            &format!("2fa:regenerate:{}", ctx_user.user.id),
            10,
            600,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد تلاش بیش از حد مجاز است",
        ));
    }
    let method = verify_second_factor(
        &ctx,
        &ctx_user.user,
        &password,
        code.as_deref(),
        recovery_code.as_deref(),
    )
    .await?;
    let codes = store_recovery_codes(&ctx, ctx_user.user.id).await?;
    emit_event(
        db,
        "user",
        &ctx_user.user.id.to_string(),
        "user.2fa_recovery_regenerated",
        json!({ "method": method }),
    )
    .await?;
    log_audit(
        db,
        Some(ctx_user.user.id),
        "user",
        "user.2fa_recovery_regenerate",
        "user",
        Some(&ctx_user.user.id.to_string()),
        None,
        Some(json!({ "method": method, "count": codes.len() })),
        ip.as_deref(),
        None,
    )
    .await?;
    Ok(Json(json!({ "recovery_codes": codes })))
}

pub async fn ws_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx_user = authenticate(&state, &headers, "/api/v1/auth/ws-ticket").await?;
    require_full_scope(&state, &ctx_user).await?;
    let Some(session_id) = ctx_user.session_id else {
        return Err(ApiError::unauthorized(
            "session_required",
            "صدور بلیط وب‌سوکت نیازمند نشست معتبر است",
        ));
    };
    let db = state.db_or_500()?;
    let ticket = token_urlsafe(32);
    db.execute("INSERT INTO ws_tickets (id, user_id, session_id, ticket_hash, expires_at, used_at, created_at) \
         VALUES (?, ?, ?, ?, ?, NULL, ?)", &[(new_id()).into_arg(), (ctx_user.user.id).into_arg(), (session_id).into_arg(), (hash_token(&ticket)).into_arg(), (utc_now() + chrono::Duration::seconds(WS_TICKET_EXPIRE_SECONDS)).into_arg(), (utc_now()).into_arg()])
    .await?;
    Ok(Json(
        json!({ "ticket": ticket, "expires_in": WS_TICKET_EXPIRE_SECONDS }),
    ))
}

// ---------- small public re-exports used by tests ----------
