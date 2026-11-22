//! Users routes — mirrors `app/modules/users/router.py` and
//! `service.py`: profile, avatar, sessions, consent, GDPR export /
//! delete, admin list, devices, web-push, notifications, avatar-by-ref.
//!
//! Profile completeness is defined on five `users` columns
//! (`full_name`, `avatar_media_id`, `date_of_birth`, `gender`,
//! `country_code`); gender is only `male`/`female`; birth dates must
//! not be in the future and age ≤ 120; phones are normalized to E.164.
//! Avatar upload is fail-closed without a virus scan (503
//! `antivirus_unavailable`) unless the dev-only bypass flag is set —
//! the same gate as Python's `scan_upload_or_reject` (ADR-015).

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Json, Response},
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, Datelike, Utc};
use regex::Regex;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, require_permission, roles_permissions_for, AuthCtx, UserRow},
    countries::{country_iso_from_phone, is_valid_country_code},
    db::{new_id, utc_now, Arg, Db, IntoArg},
    errors::{ApiError, ApiResult},
    settings::{get_setting, get_typed_setting},
    storage,
    validate::{opt_str, opt_str_raw, opt_uuid, parse_body, req_str},
    AppState,
};

pub const PROFILE_COMPLETION_FIELDS: &[&str] = &[
    "full_name",
    "avatar_media_id",
    "date_of_birth",
    "gender",
    "country_code",
];
const MAX_PROFILE_AGE_YEARS: i64 = 120;
const AVATAR_MAX_BYTES: usize = 5 * 1024 * 1024;
const CURSOR_LIMIT: i64 = 50;

pub fn missing_profile_fields(user: &UserRow) -> Vec<String> {
    let mut missing = Vec::new();
    for field in PROFILE_COMPLETION_FIELDS {
        let is_missing = match *field {
            "full_name" => user.full_name.trim().is_empty(),
            "avatar_media_id" => user.avatar_media_id.is_none(),
            "date_of_birth" => user.date_of_birth.is_none(),
            "gender" => user.gender.is_none(),
            "country_code" => user.country_code.is_none(),
            _ => false,
        };
        if is_missing {
            missing.push(field.to_string());
        }
    }
    missing
}

pub fn profile_is_complete(user: &UserRow) -> bool {
    missing_profile_fields(user).is_empty()
}

async fn profile_required_sections(
    db: &Db,
    config: &crate::Config,
) -> Result<Vec<String>, ApiError> {
    let mut required = Vec::new();
    for (section, key) in [
        ("fatwa_question", "profile.require_for_fatwa_question"),
        ("hifz_advanced", "profile.require_for_hifz_advanced"),
    ] {
        let v = get_typed_setting(db, config, key, Value::Bool(false)).await?;
        if v == Value::Bool(true) {
            required.push(section.to_string());
        }
    }
    Ok(required)
}

/// `build_user_out` — `UserOut` with real RBAC roles/permissions.
pub async fn build_user_out(
    db: &Db,
    config: &crate::Config,
    user: &UserRow,
) -> Result<Value, ApiError> {
    let mapping = roles_permissions_for(db, &[user.id]).await?;
    let (roles, perms) = mapping.get(&user.id).cloned().unwrap_or_default();
    Ok(user_out_json(
        user,
        roles,
        perms,
        profile_is_complete(user),
        missing_profile_fields(user),
        profile_required_sections(db, config).await?,
    ))
}

fn user_out_json(
    user: &UserRow,
    roles: Vec<String>,
    perms: Vec<String>,
    complete: bool,
    missing: Vec<String>,
    required_sections: Vec<String>,
) -> Value {
    json!({
        "id": user.id.to_string(),
        "full_name": user.full_name,
        "email": user.email,
        "phone": user.phone,
        "locale": user.locale,
        "is_active": user.is_active,
        "is_verified": user.is_verified,
        "created_at": user.created_at.to_rfc3339(),
        "roles": roles,
        "permissions": perms,
        "avatar_media_id": user.avatar_media_id.map(|u| u.to_string()),
        "date_of_birth": user.date_of_birth,
        "gender": user.gender,
        "country_code": user.country_code,
        "profile_complete": complete,
        "missing_profile_fields": missing,
        "profile_required_sections": required_sections,
    })
}

fn verified_phone_country(user: &UserRow) -> Option<&'static str> {
    if user.phone.is_none() || !user.is_verified {
        return None;
    }
    country_iso_from_phone(user.phone.as_deref().unwrap_or(""))
}

fn validate_date_of_birth(value: &str) -> Result<(), ApiError> {
    let today = Utc::now().date_naive();
    let parsed = chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").map_err(|_| {
        ApiError::validation(vec![
            json!({ "field": "date_of_birth", "msg": "invalid date" }),
        ])
    })?;
    if parsed > today {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "تاریخ تولد نمی‌تواند در آینده باشد",
        ));
    }
    let mut age = today.year() - parsed.year();
    if (today.month(), today.day()) < (parsed.month(), parsed.day()) {
        age -= 1;
    }
    if age > MAX_PROFILE_AGE_YEARS as i32 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "تاریخ تولد معتبر نیست",
        ));
    }
    Ok(())
}

fn validate_gender(value: &str) -> Result<(), ApiError> {
    if value != "male" && value != "female" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "جنسیت نامعتبر است",
        ));
    }
    Ok(())
}

fn normalize_profile_phone(value: &str) -> Result<String, ApiError> {
    let cleaned: String = value
        .trim()
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')'))
        .collect();
    let re = Regex::new(r"^\+[1-9]\d{6,14}$").unwrap();
    if !re.is_match(&cleaned) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "شمارهٔ تلفن باید با کد کشور و قالب بین‌المللی وارد شود (مثل +98912…)",
        ));
    }
    Ok(cleaned)
}

fn validate_country_code(user: &UserRow, value: &str) -> Result<(), ApiError> {
    let code = value.trim().to_uppercase();
    if !is_valid_country_code(&code) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "کد کشور نامعتبر است",
        ));
    }
    if let Some(phone_country) = verified_phone_country(user) {
        if code != phone_country {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                "کشور باید با کشور شمارهٔ تلفن تأییدشده یکی باشد",
            ));
        }
    }
    Ok(())
}

async fn authed(state: &AppState, headers: &HeaderMap, path: &str) -> Result<AuthCtx, ApiError> {
    authenticate(state, headers, path).await
}

fn client_ip_ua(headers: &HeaderMap) -> (Option<String>, Option<String>) {
    let ip = headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').next().unwrap_or("").trim().to_string());
    let ua = headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    (ip, ua)
}

pub async fn get_me(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me").await?;
    let db = state.db_or_500()?;
    Ok(Json(build_user_out(db, &state.config, &ctx.user).await?))
}

pub async fn update_me(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me").await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let mut user = ctx.user.clone();
    // Field length checks from `MeUpdate` (Pydantic bounds).
    let full_name = opt_str(&parsed, "full_name", 2, 200)?;
    let locale = opt_str(&parsed, "locale", 1, 8)?;
    let avatar_media_id = opt_uuid(&parsed, "avatar_media_id")?;
    let date_of_birth = opt_str(&parsed, "date_of_birth", 10, 10)?;
    let gender = opt_str(&parsed, "gender", 1, 16)?;
    let country_code = opt_str(&parsed, "country_code", 2, 2)?;
    let phone = opt_str_raw(&parsed, "phone", 32)?;

    let old = json!({
        "full_name": user.full_name,
        "locale": user.locale,
        "date_of_birth": user.date_of_birth,
        "gender": user.gender,
        "country_code": user.country_code,
        "phone": user.phone,
    });
    if let Some(v) = full_name {
        user.full_name = v;
    }
    if let Some(v) = locale {
        user.locale = v;
    }
    if let Some(avatar_id) = avatar_media_id {
        let exists = db
            .fetch_optional(
                "SELECT id FROM media_assets WHERE id = ?",
                &[(avatar_id).into_arg()],
            )
            .await?;
        if exists.is_none() {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                "رسانهٔ عکس پروفایل یافت نشد",
            ));
        }
        user.avatar_media_id = Some(avatar_id);
    }
    if let Some(v) = date_of_birth {
        validate_date_of_birth(&v)?;
        user.date_of_birth = Some(v);
    }
    if let Some(v) = gender {
        validate_gender(&v)?;
        user.gender = Some(v);
    }
    if let Some(raw_phone) = phone {
        let new_phone: Option<String> = if raw_phone.trim().is_empty() {
            user.phone = None;
            None
        } else {
            Some(normalize_profile_phone(&raw_phone)?)
        };
        if let Some(p) = &new_phone {
            if Some(p.clone()) != user.phone {
                let clash = db
                    .fetch_optional(
                        "SELECT id FROM users WHERE phone = ? AND id != ?",
                        &[(p).into_arg(), (user.id).into_arg()],
                    )
                    .await?;
                if clash.is_some() {
                    return Err(ApiError::new(
                        StatusCode::CONFLICT,
                        "phone_in_use",
                        "این شمارهٔ تلفن برای حساب دیگری ثبت شده است",
                    ));
                }
                user.phone = Some(p.clone());
            }
        }
    }
    if let Some(v) = country_code {
        validate_country_code(&user, &v)?;
        user.country_code = Some(v.trim().to_uppercase());
    }
    let new = json!({
        "full_name": user.full_name,
        "locale": user.locale,
        "date_of_birth": user.date_of_birth,
        "gender": user.gender,
        "country_code": user.country_code,
        "phone": user.phone,
    });
    let changed: Vec<&str> = [
        "full_name",
        "locale",
        "date_of_birth",
        "gender",
        "country_code",
        "phone",
    ]
    .into_iter()
    .filter(|k| old.get(*k) != new.get(*k))
    .collect();
    db.execute(
        "UPDATE users SET full_name = ?, locale = ?, avatar_media_id = ?, date_of_birth = ?, \
         gender = ?, country_code = ?, phone = ? WHERE id = ?",
        &[
            (&user.full_name).into_arg(),
            (&user.locale).into_arg(),
            (user.avatar_media_id).into_arg(),
            (&user.date_of_birth).into_arg(),
            (&user.gender).into_arg(),
            (&user.country_code).into_arg(),
            (&user.phone).into_arg(),
            (user.id).into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "user",
        &user.id.to_string(),
        "user.profile_updated",
        json!({ "fields": changed }),
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers);
    log_audit(
        db,
        Some(user.id),
        "user",
        "user.profile_update",
        "user",
        Some(&user.id.to_string()),
        Some(old),
        Some(new),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(build_user_out(db, &state.config, &user).await?))
}

fn sniff_avatar_image(data: &[u8]) -> Option<(&'static str, &'static str)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(("image/png", "png"))
    } else if data.starts_with(b"\xff\xd8\xff") {
        Some(("image/jpeg", "jpg"))
    } else if data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some(("image/webp", "webp"))
    } else {
        None
    }
}

pub async fn upload_avatar(
    State(state): State<AppState>,
    headers: HeaderMap,
    mut multipart: axum::extract::Multipart,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/avatar").await?;
    let db = state.db_or_500()?;
    let mut content: Option<(Vec<u8>, Option<String>)> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| ApiError::validation(vec![json!({ "msg": "invalid multipart body" })]))?
    {
        if field.name() == Some("file") {
            let filename = field.file_name().map(String::from);
            let mut bytes = Vec::new();
            let mut total = 0usize;
            let mut chunk_stream = field;
            while let Some(chunk) = chunk_stream
                .chunk()
                .await
                .map_err(|_| ApiError::internal())?
            {
                total += chunk.len();
                if total > AVATAR_MAX_BYTES {
                    return Err(ApiError::with_details(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "avatar_too_large",
                        "حجم عکس پروفایل نباید بیش از ۵ مگابایت باشد",
                        json!({ "max_bytes": AVATAR_MAX_BYTES }),
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            content = Some((bytes, filename));
        }
    }
    let Some((bytes, filename)) = content else {
        return Err(ApiError::validation(vec![
            json!({ "msg": "file is required" }),
        ]));
    };
    // Virus scan gate — fail-closed unless the dev-only bypass flag is set.
    if !(state.config.clamav_allow_insecure_dev_bypass && state.config.app_env == "dev") {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "antivirus_unavailable",
            "اسکن ویروس در دسترس نیست؛ آپلود به‌صورت fail-closed رد شد. سرویس ClamAV را بررسی کنید.",
        ));
    }
    let Some((mime, ext)) = sniff_avatar_image(&bytes) else {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_avatar_image",
            "عکس پروفایل باید PNG یا JPG یا WEBP باشد",
        ));
    };
    let _ = filename;
    let mut user = ctx.user.clone();
    let key = format!("avatars/{}/{}.{}", user.id, Uuid::new_v4().simple(), ext);
    storage::put_object(&state.config, &key, &bytes).await?;
    let asset_id = new_id();
    let checksum = {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(&bytes);
        h.finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    db.execute(
        "INSERT INTO media_assets (id, owner_id, kind, storage_key, thumbnail_key, mime_type, \
         size_bytes, duration_seconds, checksum, visibility, created_at) \
         VALUES (?, ?, 'image', ?, NULL, ?, ?, NULL, ?, 'public', ?)",
        &[
            (asset_id).into_arg(),
            (user.id).into_arg(),
            (&key).into_arg(),
            (mime).into_arg(),
            (bytes.len() as i64).into_arg(),
            (checksum).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    user.avatar_media_id = Some(asset_id);
    db.execute(
        "UPDATE users SET avatar_media_id = ? WHERE id = ?",
        &[(asset_id).into_arg(), (user.id).into_arg()],
    )
    .await?;
    emit_event(
        db,
        "user",
        &user.id.to_string(),
        "user.avatar_updated",
        json!({ "media_id": asset_id.to_string() }),
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers);
    log_audit(
        db,
        Some(user.id),
        "user",
        "user.avatar_update",
        "user",
        Some(&user.id.to_string()),
        None,
        Some(json!({ "avatar_media_id": asset_id.to_string() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(build_user_out(db, &state.config, &user).await?))
}

pub async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/sessions").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, device_info, ip, last_active_at, revoked, created_at FROM sessions \
         WHERE user_id = ? AND revoked = FALSE ORDER BY last_active_at DESC",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let last_active: DateTime<Utc> = row.try_get("last_active_at")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let id: Uuid = row.try_get("id")?;
        out.push(json!({
            "id": id.to_string(),
            "device_info": row.try_get::<Option<String>, _>("device_info")?,
            "ip": row.try_get::<Option<String>, _>("ip")?,
            "last_active_at": last_active.to_rfc3339(),
            "revoked": row.try_get::<bool, _>("revoked")?,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    Ok(Json(Value::Array(out)))
}

pub async fn revoke_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/sessions").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, user_id, revoked FROM sessions WHERE id = ?",
            &[(session_id).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found("not_found", "نشست یافت نشد"));
    };
    let owner: Uuid = row.try_get("user_id")?;
    let revoked: bool = row.try_get("revoked")?;
    if owner != ctx.user.id || revoked {
        return Err(ApiError::not_found("not_found", "نشست یافت نشد"));
    }
    db.execute(
        "UPDATE sessions SET revoked = TRUE WHERE id = ?",
        &[(session_id).into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE refresh_tokens SET revoked = TRUE WHERE session_id = ? AND revoked = FALSE",
        &[(session_id).into_arg()],
    )
    .await?;
    emit_event(
        db,
        "user",
        &ctx.user.id.to_string(),
        "user.session_revoked",
        json!({ "session_id": session_id.to_string() }),
    )
    .await?;
    let (ip, _) = client_ip_ua(&headers);
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "user.session_revoke",
        "session",
        Some(&session_id.to_string()),
        None,
        None,
        ip.as_deref(),
        None,
    )
    .await?;
    Ok(Json(json!({ "message": "نشست بسته شد" })))
}

pub async fn list_consents(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/consent").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT consent_type, text_version, accepted_at, ip FROM user_consent \
         WHERE user_id = ? ORDER BY accepted_at DESC",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let accepted_at: DateTime<Utc> = row.try_get("accepted_at")?;
        out.push(json!({
            "consent_type": row.try_get::<String, _>("consent_type")?,
            "text_version": row.try_get::<String, _>("text_version")?,
            "accepted_at": accepted_at.to_rfc3339(),
            "ip": row.try_get::<Option<String>, _>("ip")?,
        }));
    }
    Ok(Json(Value::Array(out)))
}

pub async fn export_data(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/export").await?;
    let db = state.db_or_500()?;
    let user = &ctx.user;
    let sessions = db.fetch_all("SELECT id, device_info, ip, last_active_at, created_at FROM sessions WHERE user_id = ?", &[(user.id).into_arg()])
    .await?;
    let mut sessions_json = Vec::new();
    for row in sessions {
        let id: Uuid = row.try_get("id")?;
        let last_active: DateTime<Utc> = row.try_get("last_active_at")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        sessions_json.push(json!({
            "id": id.to_string(),
            "device_info": row.try_get::<Option<String>, _>("device_info")?,
            "ip": row.try_get::<Option<String>, _>("ip")?,
            "last_active_at": last_active.to_rfc3339(),
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let consents = db
        .fetch_all(
            "SELECT consent_type, text_version, accepted_at FROM user_consent WHERE user_id = ?",
            &[(user.id).into_arg()],
        )
        .await?;
    let mut consents_json = Vec::new();
    for row in consents {
        let accepted_at: DateTime<Utc> = row.try_get("accepted_at")?;
        consents_json.push(json!({
            "consent_type": row.try_get::<String, _>("consent_type")?,
            "text_version": row.try_get::<String, _>("text_version")?,
            "accepted_at": accepted_at.to_rfc3339(),
        }));
    }
    let prefs = db
        .fetch_all(
            "SELECT channel, notif_type, enabled FROM notification_preferences WHERE user_id = ?",
            &[(user.id).into_arg()],
        )
        .await?;
    let mut prefs_json = Vec::new();
    for row in prefs {
        prefs_json.push(json!({
            "channel": row.try_get::<String, _>("channel")?,
            "notif_type": row.try_get::<String, _>("notif_type")?,
            "enabled": row.try_get::<bool, _>("enabled")?,
        }));
    }
    // Fatwa sessions + messages (owner only).
    let fatwa_sessions = db.fetch_all("SELECT id, locale, created_at FROM fatwa_sessions WHERE user_id = ? ORDER BY created_at ASC", &[(user.id).into_arg()])
    .await?;
    let mut fatwa_json = Vec::new();
    for s in fatwa_sessions {
        let sid: Uuid = s.try_get("id")?;
        let created_at: DateTime<Utc> = s.try_get("created_at")?;
        let messages = db
            .fetch_all(
                "SELECT id, role, content, citations, status, created_at FROM fatwa_messages \
             WHERE session_id = ? ORDER BY created_at ASC",
                &[(sid).into_arg()],
            )
            .await?;
        let mut msgs = Vec::new();
        for m in messages {
            let mid: Uuid = m.try_get("id")?;
            let m_created: DateTime<Utc> = m.try_get("created_at")?;
            let citations: Option<Value> = m.try_get("citations").ok();
            msgs.push(json!({
                "id": mid.to_string(),
                "role": m.try_get::<String, _>("role")?,
                "content": m.try_get::<String, _>("content")?,
                "citations": citations,
                "status": m.try_get::<Option<String>, _>("status")?,
                "created_at": m_created.to_rfc3339(),
            }));
        }
        fatwa_json.push(json!({
            "id": sid.to_string(),
            "locale": s.try_get::<Option<String>, _>("locale")?,
            "created_at": created_at.to_rfc3339(),
            "messages": msgs,
        }));
    }
    let notes = db
        .fetch_all(
            "SELECT id, surah, ayah, note_text, is_private, version, created_at, updated_at \
         FROM user_ayah_notes WHERE user_id = ? ORDER BY created_at ASC",
            &[(user.id).into_arg()],
        )
        .await?;
    let mut notes_json = Vec::new();
    for n in notes {
        let nid: Uuid = n.try_get("id")?;
        let created_at: DateTime<Utc> = n.try_get("created_at")?;
        let updated_at: Option<DateTime<Utc>> = n.try_get("updated_at")?;
        notes_json.push(json!({
            "id": nid.to_string(),
            "surah": n.try_get::<i64, _>("surah")?,
            "ayah": n.try_get::<i64, _>("ayah")?,
            "note_text": n.try_get::<String, _>("note_text")?,
            "is_private": n.try_get::<bool, _>("is_private")?,
            "version": n.try_get::<i64, _>("version")?,
            "created_at": created_at.to_rfc3339(),
            "updated_at": updated_at.map(|d| d.to_rfc3339()),
        }));
    }
    let donations = db.fetch_all("SELECT id, amount, currency, gateway, gateway_reference, status, donor_name, campaign_id, created_at \
         FROM donations WHERE donor_id = ? ORDER BY created_at ASC", &[(user.id).into_arg()])
    .await?;
    let mut donations_json = Vec::new();
    for d in donations {
        let did: Uuid = d.try_get("id")?;
        let created_at: DateTime<Utc> = d.try_get("created_at")?;
        donations_json.push(json!({
            "id": did.to_string(),
            "amount": d.try_get::<f64, _>("amount")?,
            "currency": d.try_get::<String, _>("currency")?,
            "gateway": d.try_get::<Option<String>, _>("gateway")?,
            "gateway_reference": d.try_get::<Option<String>, _>("gateway_reference")?,
            "status": d.try_get::<String, _>("status")?,
            "donor_name": d.try_get::<Option<String>, _>("donor_name")?,
            "campaign_id": d.try_get::<Option<String>, _>("campaign_id")?,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let data = json!({
        "profile": {
            "id": user.id.to_string(),
            "full_name": user.full_name,
            "email": user.email,
            "phone": user.phone,
            "locale": user.locale,
            "date_of_birth": user.date_of_birth,
            "gender": user.gender,
            "country_code": user.country_code,
            "is_verified": user.is_verified,
            "created_at": user.created_at.to_rfc3339(),
        },
        "sessions": sessions_json,
        "consents": consents_json,
        "notification_preferences": prefs_json,
        "fatwa_sessions": fatwa_json,
        "quran_notes": notes_json,
        "donations": donations_json,
    });
    emit_event(
        db,
        "user",
        &user.id.to_string(),
        "user.data_exported",
        json!({}),
    )
    .await?;
    log_audit(
        db,
        Some(user.id),
        "user",
        "user.data_export",
        "user",
        Some(&user.id.to_string()),
        None,
        None,
        None,
        None,
    )
    .await?;
    Ok(Json(data))
}

pub async fn delete_me(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me").await?;
    let db = state.db_or_500()?;
    let user = &ctx.user;
    let random_hash = crate::crypto::hash_password(&crate::crypto::token_urlsafe(32))?;
    db.execute(
        "UPDATE users SET full_name = 'حساب حذف‌شده', email = NULL, phone = NULL, \
         password_hash = ?, totp_secret = NULL, avatar_media_id = NULL, date_of_birth = NULL, \
         gender = NULL, country_code = NULL, is_active = FALSE, is_verified = FALSE WHERE id = ?",
        &[(random_hash).into_arg(), (user.id).into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
        &[(user.id).into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE sessions SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
        &[(user.id).into_arg()],
    )
    .await?;
    emit_event(db, "user", &user.id.to_string(), "user.deleted", json!({})).await?;
    let (ip, ua) = client_ip_ua(&headers);
    log_audit(
        db,
        Some(user.id),
        "user",
        "user.delete",
        "user",
        Some(&user.id.to_string()),
        None,
        Some(json!({ "anonymized": true })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(
        json!({ "message": "حساب شما ناشناس‌سازی و غیرفعال شد" }),
    ))
}

fn encode_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    let raw = json!({ "t": created_at.to_rfc3339(), "id": id.to_string() }).to_string();
    URL_SAFE.encode(raw)
}

fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let decoded = URL_SAFE.decode(cursor).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "کرسر صفحه‌بندی نامعتبر است",
        )
    })?;
    let v: Value = serde_json::from_slice(&decoded).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "کرسر صفحه‌بندی نامعتبر است",
        )
    })?;
    let t = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
    let id = v.get("id").and_then(|i| i.as_str()).unwrap_or("");
    let dt = DateTime::parse_from_rfc3339(t)
        .map(|d| d.with_timezone(&Utc))
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_cursor",
                "کرسر صفحه‌بندی نامعتبر است",
            )
        })?;
    let uid = id.parse::<Uuid>().map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "کرسر صفحه‌بندی نامعتبر است",
        )
    })?;
    Ok((dt, uid))
}

pub async fn admin_list_users(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/users").await?;
    require_permission(&state, &ctx, "users.manage").await?;
    let db = state.db_or_500()?;
    let limit: i64 = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, CURSOR_LIMIT);
    let cursor = query.get("cursor").cloned();
    let mut sql = "SELECT * FROM users ORDER BY created_at ASC, id ASC LIMIT ?".to_string();
    let mut cursor_pair: Option<(DateTime<Utc>, Uuid)> = None;
    if let Some(c) = &cursor {
        cursor_pair = Some(decode_cursor(c)?);
        sql = "SELECT * FROM users WHERE (created_at > ?) OR (created_at = ? AND id > ?) \
               ORDER BY created_at ASC, id ASC LIMIT ?"
            .to_string();
    }
    let mut args: Vec<Arg> = Vec::new();
    if let Some((ts, uid)) = cursor_pair {
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(uid.into_arg());
    }
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    // Fetched limit+1 rows to detect a next page.
    let has_more = rows.len() as i64 > limit;
    let page_rows: Vec<_> = rows.into_iter().take(limit as usize).collect();
    let mut items = Vec::new();
    let mut ids = Vec::new();
    let mut users = Vec::new();
    for row in &page_rows {
        let user = crate::authn::user_from_row(row)?;
        ids.push(user.id);
        users.push(user);
    }
    let mapping = roles_permissions_for(db, &ids).await?;
    let required = profile_required_sections(db, &state.config).await?;
    for user in &users {
        let (roles, perms) = mapping.get(&user.id).cloned().unwrap_or_default();
        items.push(user_out_json(
            user,
            roles,
            perms,
            profile_is_complete(user),
            missing_profile_fields(user),
            required.clone(),
        ));
    }
    let next_cursor = if has_more {
        page_rows.last().map(|row| {
            let created_at: DateTime<Utc> = row.try_get("created_at").unwrap();
            let id: Uuid = row.try_get("id").unwrap();
            encode_cursor(created_at, id)
        })
    } else {
        None
    };
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

pub async fn register_device(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/devices").await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let device_id = req_str(&parsed, "device_id", 1, 64)?;
    let platform = opt_str(&parsed, "platform", 1, 16)?;
    let push_token = opt_str(&parsed, "push_token", 1, 512)?;
    if let Some(p) = &platform {
        if !["android", "ios", "web", "desktop"].contains(&p.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "validation_error",
                "پلتفرم نامعتبر است",
            ));
        }
    }
    let row = db
        .fetch_optional(
            "SELECT id, platform FROM user_devices WHERE user_id = ? AND device_id = ?",
            &[(ctx.user.id).into_arg(), (&device_id).into_arg()],
        )
        .await?;
    let now = utc_now();
    let device_row_id: Uuid = match row {
        Some(r) => {
            let id: Uuid = r.try_get("id")?;
            let old_platform: Option<String> = r.try_get("platform")?;
            let new_platform = platform.clone().or(old_platform);
            // Python keeps the old push_token when none is sent.
            if let Some(token) = &push_token {
                db.execute("UPDATE user_devices SET platform = ?, push_token = ?, last_seen = ? WHERE id = ?", &[(new_platform).into_arg(), (token).into_arg(), (now).into_arg(), (id).into_arg()])
                .await?;
            } else {
                db.execute(
                    "UPDATE user_devices SET platform = ?, last_seen = ? WHERE id = ?",
                    &[(new_platform).into_arg(), (now).into_arg(), (id).into_arg()],
                )
                .await?;
            }
            id
        }
        None => {
            let id = new_id();
            db.execute("INSERT INTO user_devices (id, user_id, device_id, platform, push_token, last_seen, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?)", &[(id).into_arg(), (ctx.user.id).into_arg(), (&device_id).into_arg(), (&platform).into_arg(), (&push_token).into_arg(), (now).into_arg(), (now).into_arg()])
            .await?;
            id
        }
    };
    let platform_out = db
        .fetch_one(
            "SELECT platform FROM user_devices WHERE id = ?",
            &[(device_row_id).into_arg()],
        )
        .await?;
    let platform_out: Option<String> = platform_out.try_get("platform")?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "device.register",
        "user_device",
        Some(&device_row_id.to_string()),
        None,
        Some(json!({ "device_id": device_id, "push_token": { "changed": push_token.is_some() } })),
        None,
        None,
    )
    .await?;
    Ok(Json(json!({
        "device_id": device_id,
        "platform": platform_out,
        "registered": true,
    })))
}

pub async fn vapid_key(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/notifications/web-push/vapid-key").await?;
    let _ = ctx;
    let db = state.db_or_500()?;
    let key = get_setting(db, &state.config, "push.vapid_public_key").await?;
    Ok(Json(json!({
        "vapid_public_key": key,
        "configured": key.is_some(),
    })))
}

pub async fn subscribe_web_push(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/web-push/subscriptions").await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let endpoint = req_str(&parsed, "endpoint", 10, 1024)?;
    let p256dh = req_str(&parsed, "p256dh", 10, 256)?;
    let auth = req_str(&parsed, "auth", 5, 128)?;
    if !endpoint.starts_with("https://") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "endpoint باید https باشد",
        ));
    }
    let row = db
        .fetch_optional(
            "SELECT id FROM web_push_subscriptions WHERE user_id = ? AND endpoint = ?",
            &[(ctx.user.id).into_arg(), (&endpoint).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let id: Uuid = r.try_get("id")?;
            db.execute(
                "UPDATE web_push_subscriptions SET p256dh = ?, auth = ? WHERE id = ?",
                &[(&p256dh).into_arg(), (&auth).into_arg(), (id).into_arg()],
            )
            .await?;
        }
        None => {
            db.execute("INSERT INTO web_push_subscriptions (id, user_id, endpoint, p256dh, auth, created_at) \
                 VALUES (?, ?, ?, ?, ?, ?)", &[(new_id()).into_arg(), (ctx.user.id).into_arg(), (&endpoint).into_arg(), (&p256dh).into_arg(), (&auth).into_arg(), (utc_now()).into_arg()])
            .await?;
        }
    }
    Ok(Json(json!({ "subscribed": true })))
}

pub async fn unsubscribe_web_push(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/web-push/subscriptions").await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let endpoint = req_str(&parsed, "endpoint", 10, 1024)?;
    db.execute(
        "DELETE FROM web_push_subscriptions WHERE user_id = ? AND endpoint = ?",
        &[(ctx.user.id).into_arg(), (&endpoint).into_arg()],
    )
    .await?;
    Ok(Json(json!({ "subscribed": false })))
}

pub async fn list_my_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<HashMap<String, String>>,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/notifications").await?;
    let db = state.db_or_500()?;
    let unread_only = query
        .get("unread_only")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);
    let limit: i64 = query
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(20)
        .clamp(1, CURSOR_LIMIT);
    let cursor = query.get("cursor").cloned();
    let mut sql = "SELECT * FROM notifications WHERE user_id = ?".to_string();
    if unread_only {
        sql.push_str(" AND read_at IS NULL");
    }
    let mut cursor_pair: Option<(DateTime<Utc>, Uuid)> = None;
    if let Some(c) = &cursor {
        cursor_pair = Some(decode_cursor(c)?);
        sql.push_str(" AND ((created_at < ?) OR (created_at = ? AND id < ?))");
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    let mut args: Vec<Arg> = vec![ctx.user.id.into_arg()];
    if let Some((ts, uid)) = cursor_pair {
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(uid.into_arg());
    }
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let has_more = rows.len() as i64 > limit;
    let next_cursor = if has_more {
        let row = &rows[limit as usize];
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let id: Uuid = row.try_get("id")?;
        Some(encode_cursor(created_at, id))
    } else {
        None
    };
    let mut items = Vec::new();
    for row in rows.into_iter().take(limit as usize) {
        let id: Uuid = row.try_get("id")?;
        let title: Value = row.try_get("title")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let read_at: Option<DateTime<Utc>> = row.try_get("read_at")?;
        items.push(json!({
            "id": id.to_string(),
            "notif_type": row.try_get::<String, _>("notif_type")?,
            "title": title,
            "body": row.try_get::<Option<String>, _>("body")?,
            "data": row.try_get::<Option<Value>, _>("data")?,
            "read_at": read_at.map(|d| d.to_rfc3339()),
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let unread_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) as c FROM notifications WHERE user_id = ? AND read_at IS NULL",
            &[(ctx.user.id).into_arg()],
        )
        .await?
        .try_get("c")?;
    Ok(Json(json!({
        "items": items,
        "unread_count": unread_count,
        "next_cursor": next_cursor,
    })))
}

pub async fn mark_notification_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(notification_id): Path<Uuid>,
) -> ApiResult<Json<Value>> {
    let ctx = authed(&state, &headers, "/api/v1/users/me/notifications").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM notifications WHERE id = ? AND user_id = ?",
            &[(notification_id).into_arg(), (ctx.user.id).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found("not_found", "اعلان یافت نشد"));
    };
    let read_at: Option<DateTime<Utc>> = row.try_get("read_at")?;
    if read_at.is_none() {
        db.execute(
            "UPDATE notifications SET read_at = ? WHERE id = ?",
            &[(utc_now()).into_arg(), (notification_id).into_arg()],
        )
        .await?;
    }
    let row = db
        .fetch_one(
            "SELECT * FROM notifications WHERE id = ?",
            &[(notification_id).into_arg()],
        )
        .await?;
    let title: Value = row.try_get("title")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let read_at: Option<DateTime<Utc>> = row.try_get("read_at")?;
    Ok(Json(json!({
        "id": notification_id.to_string(),
        "notif_type": row.try_get::<String, _>("notif_type")?,
        "title": title,
        "body": row.try_get::<Option<String>, _>("body")?,
        "data": row.try_get::<Option<Value>, _>("data")?,
        "read_at": read_at.map(|d| d.to_rfc3339()),
        "created_at": created_at.to_rfc3339(),
    })))
}

pub async fn avatar_by_ref(
    State(state): State<AppState>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    let key = query.get("key").cloned().unwrap_or_default();
    let exp: i64 = query.get("exp").and_then(|v| v.parse().ok()).unwrap_or(0);
    let sig = query.get("sig").cloned().unwrap_or_default();
    let purpose = query.get("purpose").cloned().unwrap_or_default();
    if state.config.storage_provider != "local" {
        return Err(ApiError::internal());
    }
    if purpose != storage::SIGNED_PURPOSE_AVATAR
        || !storage::verify_download_sig(&state.config, &key, exp, &sig, &purpose)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_signature",
            "امضای دانلود نامعتبر یا منقضی است",
        ));
    }
    let db = state.db_or_500()?;
    let asset = db
        .fetch_optional(
            "SELECT mime_type FROM media_assets WHERE storage_key = ?",
            &[(&key).into_arg()],
        )
        .await?;
    let Some(asset) = asset else {
        return Err(ApiError::not_found("not_found", "فایل یافت نشد"));
    };
    let mime: String = asset.try_get("mime_type")?;
    let data = storage::read_bytes(&state.config, &key).await?;
    let response = Response::builder()
        .status(StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, mime)
        .body(axum::body::Body::from(data))
        .map_err(|_| ApiError::internal())?;
    Ok(response)
}
