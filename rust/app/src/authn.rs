//! Authentication dependencies — mirrors
//! `app/modules/auth/dependencies.py` and the RBAC lookups in
//! `app/modules/auth/service.py` (`user_role_names`, `must_enroll_2fa`)
//! and `app/modules/users/service.py` (`roles_permissions_for`).
//!
//! Rules carried over verbatim:
//! - No Bearer token → 401 `unauthorized`.
//! - Bad/expired JWT → 401 `invalid_token` / `token_expired`.
//! - A `2fa_setup` token opens only `/api/v1/auth/2fa/setup`,
//!   `/2fa/enable`, `/2fa/status` and `/logout`; anything else → 403
//!   `requires_2fa`.
//! - A token bound to a revoked session → 401 `session_revoked`.
//! - `last_active_at` is touched only when > 60 s have passed.
//! - Privileged roles (`admin`, `super_admin`) without TOTP must enrol:
//!   their `full` tokens are rejected by every permission/full-scope
//!   check with 403 `requires_2fa` (live re-check, not just the scope
//!   stamped at login).

use std::collections::{HashMap, HashSet};

use axum::http::{HeaderMap, StatusCode};
use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    db::{utc_now, Arg, Db, DynRow, IntoArg},
    errors::ApiError,
    jwt::{decode_access_token, SCOPE_FULL},
    AppState,
};

/// Paths a first-factor (`2fa_setup`) token may call.
pub const TWO_FA_SETUP_ALLOWED_PATHS: &[&str] = &[
    "/api/v1/auth/2fa/setup",
    "/api/v1/auth/2fa/enable",
    "/api/v1/auth/2fa/status",
    "/api/v1/auth/logout",
];

pub const PRIVILEGED_ROLE_NAMES: &[&str] = &["admin", "super_admin"];

/// One `users` row, as the auth layer needs it.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UserRow {
    pub id: Uuid,
    pub full_name: String,
    pub email: Option<String>,
    pub phone: Option<String>,
    pub password_hash: String,
    pub locale: String,
    pub avatar_media_id: Option<Uuid>,
    pub date_of_birth: Option<String>,
    pub gender: Option<String>,
    pub country_code: Option<String>,
    pub is_active: bool,
    pub is_verified: bool,
    pub totp_secret: Option<String>,
    pub totp_last_step: Option<i64>,
    pub totp_enabled_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

pub fn user_from_row(row: &DynRow) -> Result<UserRow, ApiError> {
    Ok(UserRow {
        id: row.try_get("id")?,
        full_name: row.try_get("full_name")?,
        email: row.try_get("email")?,
        phone: row.try_get("phone")?,
        password_hash: row.try_get("password_hash")?,
        locale: row.try_get("locale")?,
        avatar_media_id: row.try_get("avatar_media_id")?,
        date_of_birth: row.try_get("date_of_birth")?,
        gender: row.try_get("gender")?,
        country_code: row.try_get("country_code")?,
        is_active: row.try_get("is_active")?,
        is_verified: row.try_get("is_verified")?,
        totp_secret: row.try_get("totp_secret")?,
        totp_last_step: row.try_get("totp_last_step")?,
        totp_enabled_at: row.try_get("totp_enabled_at")?,
        created_at: row.try_get("created_at")?,
    })
}

pub async fn load_user(db: &Db, user_id: Uuid) -> Result<Option<UserRow>, ApiError> {
    let row = db
        .fetch_optional("SELECT * FROM users WHERE id = ?", &[(user_id).into_arg()])
        .await?;
    match row {
        Some(r) => Ok(Some(user_from_row(&r)?)),
        None => Ok(None),
    }
}

/// Authenticated request context — the user plus what the token carried.
#[derive(Debug, Clone)]
pub struct AuthCtx {
    pub user: UserRow,
    pub scope: String,
    pub session_id: Option<Uuid>,
}

/// `get_current_user` — see module docs for the exact failure mapping.
pub async fn authenticate(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
) -> Result<AuthCtx, ApiError> {
    let db = state.db_or_500()?;
    let token = bearer_token(headers)
        .ok_or_else(|| ApiError::unauthorized("unauthorized", "توکن احراز هویت ارسال نشده است"))?;
    let claims = decode_access_token(&state.config.jwt_secret, &token)?;
    let scope = claims.scope.clone();
    if scope != SCOPE_FULL && !TWO_FA_SETUP_ALLOWED_PATHS.contains(&path) {
        return Err(ApiError::forbidden(
            "requires_2fa",
            "برای این عملیات باید احراز دوعاملی کامل شده باشد",
        ));
    }
    let user_id: Uuid = claims
        .sub
        .parse()
        .map_err(|_| ApiError::unauthorized("invalid_token", "توکن نامعتبر است"))?;
    let mut session_id: Option<Uuid> = None;
    if let Some(sid) = claims.session_id() {
        session_id = Some(sid);
        let row = db
            .fetch_optional(
                "SELECT revoked, last_active_at FROM sessions WHERE id = ?",
                &[(sid).into_arg()],
            )
            .await?;
        let Some(row) = row else {
            return Err(ApiError::unauthorized(
                "session_revoked",
                "نشست پایان یافته است",
            ));
        };
        let revoked: bool = row.try_get("revoked")?;
        if revoked {
            return Err(ApiError::unauthorized(
                "session_revoked",
                "نشست پایان یافته است",
            ));
        }
        let last_active: DateTime<Utc> = row.try_get("last_active_at")?;
        if utc_now().timestamp() - last_active.timestamp() > 60 {
            db.execute(
                "UPDATE sessions SET last_active_at = ? WHERE id = ?",
                &[(utc_now()).into_arg(), (sid).into_arg()],
            )
            .await?;
        }
    }
    let user = load_user(db, user_id).await?;
    let Some(user) = user else {
        return Err(ApiError::unauthorized(
            "unauthorized",
            "کاربر یافت نشد یا غیرفعال است",
        ));
    };
    if !user.is_active {
        return Err(ApiError::unauthorized(
            "unauthorized",
            "کاربر یافت نشد یا غیرفعال است",
        ));
    }
    Ok(AuthCtx {
        user,
        scope,
        session_id,
    })
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

/// `user_role_names` — role names from `user_roles`, never a `role` column.
pub async fn user_role_names(db: &Db, user_id: Uuid) -> Result<HashSet<String>, ApiError> {
    let rows = db.fetch_all("SELECT r.name FROM user_roles ur JOIN roles r ON r.id = ur.role_id WHERE ur.user_id = ?", &[(user_id).into_arg()])
    .await?;
    let mut out = HashSet::new();
    for row in rows {
        let name: String = row.try_get("name")?;
        out.insert(name);
    }
    Ok(out)
}

/// `must_enroll_2fa` — privileged role without TOTP.
pub async fn must_enroll_2fa(db: &Db, user: &UserRow) -> Result<bool, ApiError> {
    if user.totp_secret.is_some() {
        return Ok(false);
    }
    let roles = user_role_names(db, user.id).await?;
    Ok(PRIVILEGED_ROLE_NAMES.iter().any(|r| roles.contains(*r)))
}

/// Live re-check used by `require_full_scope` / `require_permission`.
pub async fn reject_privileged_without_2fa(db: &Db, user: &UserRow) -> Result<(), ApiError> {
    if must_enroll_2fa(db, user).await? {
        return Err(ApiError::forbidden(
            "requires_2fa",
            "برای این عملیات باید احراز دوعاملی کامل شده باشد",
        ));
    }
    Ok(())
}

/// `require_full_scope` — rejects `2fa_setup` tokens and privileged
/// users who have not enrolled TOTP yet.
pub async fn require_full_scope(state: &AppState, ctx: &AuthCtx) -> Result<(), ApiError> {
    if ctx.scope != SCOPE_FULL {
        return Err(ApiError::forbidden(
            "requires_2fa",
            "برای این عملیات باید احراز دوعاملی کامل شده باشد",
        ));
    }
    let db = state.db_or_500()?;
    reject_privileged_without_2fa(db, &ctx.user).await
}

/// `require_permission(key)` — full scope + live 2FA check + RBAC lookup.
pub async fn require_permission(
    state: &AppState,
    ctx: &AuthCtx,
    permission_key: &str,
) -> Result<(), ApiError> {
    require_full_scope(state, ctx).await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
         JOIN role_permissions rp ON rp.role_id = ur.role_id \
         JOIN permissions p ON p.id = rp.permission_id \
         WHERE ur.user_id = ? AND p.key = ? LIMIT 1",
            &[(ctx.user.id).into_arg(), (permission_key).into_arg()],
        )
        .await?;
    if row.is_none() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "دسترسی کافی نیست",
        ));
    }
    Ok(())
}

/// `roles_permissions_for` — sorted (roles, permissions) per user id.
pub async fn roles_permissions_for(
    db: &Db,
    user_ids: &[Uuid],
) -> Result<HashMap<Uuid, (Vec<String>, Vec<String>)>, ApiError> {
    let mut result: HashMap<Uuid, (Vec<String>, Vec<String>)> = user_ids
        .iter()
        .map(|id| (*id, (Vec::new(), Vec::new())))
        .collect();
    if user_ids.is_empty() {
        return Ok(result);
    }
    // `IN` lists are built from validated UUIDs only (no user input).
    let placeholders = user_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let role_sql = format!(
        "SELECT ur.user_id, r.name FROM user_roles ur JOIN roles r ON r.id = ur.role_id \
         WHERE ur.user_id IN ({placeholders})"
    );
    let args: Vec<Arg> = user_ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&role_sql, &args).await? {
        let uid: Uuid = row.try_get("user_id")?;
        let name: String = row.try_get("name")?;
        if let Some(entry) = result.get_mut(&uid) {
            entry.0.push(name);
        }
    }
    let perm_sql = format!(
        "SELECT ur.user_id, p.key FROM user_roles ur \
         JOIN role_permissions rp ON rp.role_id = ur.role_id \
         JOIN permissions p ON p.id = rp.permission_id \
         WHERE ur.user_id IN ({placeholders})"
    );
    let args: Vec<Arg> = user_ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&perm_sql, &args).await? {
        let uid: Uuid = row.try_get("user_id")?;
        let key: String = row.try_get("key")?;
        if let Some(entry) = result.get_mut(&uid) {
            if !entry.1.contains(&key) {
                entry.1.push(key);
            }
        }
    }
    for (roles, perms) in result.values_mut() {
        roles.sort();
        perms.sort();
    }
    Ok(result)
}

/// `get_client_ip` — `X-Forwarded-For` is trusted only when the direct
/// peer is in `trusted_proxies`; otherwise the peer address is the IP.
pub fn client_ip(
    headers: &HeaderMap,
    peer: Option<std::net::SocketAddr>,
    trusted: &[String],
) -> Option<String> {
    let peer_ip = peer.map(|p| p.ip().to_string());
    if let (Some(peer_ip), Some(xff)) = (
        &peer_ip,
        headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()),
    ) {
        if trusted.iter().any(|t| t == peer_ip) {
            if let Some(first) = xff.split(',').next().map(str::trim) {
                if first.parse::<std::net::IpAddr>().is_ok() {
                    return Some(first.to_string());
                }
            }
        }
    }
    peer_ip
}
