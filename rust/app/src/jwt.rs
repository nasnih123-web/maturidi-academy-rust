//! JWT access tokens — mirrors `app/core/security.py`.
//!
//! HS256, claims `sub`, `type="access"`, `scope` (`full` / `2fa_setup`),
//! `iat`, `exp = now + access_token_expire_minutes*60`,
//! `jti = token_hex(8)`. `extra` claims never overwrite the security
//! claims. Decode errors map to 401 `token_expired` / `invalid_token`,
//! and `type != "access"` is also 401 `invalid_token`.

use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use crate::{crypto::token_hex, errors::ApiError};

pub const SCOPE_FULL: &str = "full";
pub const SCOPE_2FA_SETUP: &str = "2fa_setup";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessClaims {
    pub sub: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub scope: String,
    pub iat: i64,
    pub exp: i64,
    pub jti: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sid: Option<String>,
}

impl AccessClaims {
    pub fn session_id(&self) -> Option<uuid::Uuid> {
        self.sid.as_deref().and_then(|s| s.parse().ok())
    }
}

/// `create_access_token(user_id, extra={"sid": ...}, scope=...)`.
pub fn create_access_token(
    jwt_secret: &str,
    user_id: &str,
    session_id: Option<uuid::Uuid>,
    scope: &str,
    expire_minutes: i64,
) -> Result<String, ApiError> {
    let now = chrono::Utc::now().timestamp();
    let claims = AccessClaims {
        sub: user_id.to_string(),
        typ: "access".to_string(),
        scope: scope.to_string(),
        iat: now,
        exp: now + expire_minutes * 60,
        jti: token_hex(8),
        sid: session_id.map(|s| s.to_string()),
    };
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
    jsonwebtoken::encode(
        &header,
        &claims,
        &jsonwebtoken::EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .map_err(|_| ApiError::internal())
}

/// `decode_access_token` — 401 `token_expired` / `invalid_token`.
pub fn decode_access_token(jwt_secret: &str, token: &str) -> Result<AccessClaims, ApiError> {
    let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::HS256);
    validation.validate_exp = true;
    // `sub` is required by the crate's default required-spec-claims set;
    // our tokens always carry it. `iat` is not required.
    let data = jsonwebtoken::decode::<AccessClaims>(
        token,
        &jsonwebtoken::DecodingKey::from_secret(jwt_secret.as_bytes()),
        &validation,
    )
    .map_err(|err| match err.kind() {
        jsonwebtoken::errors::ErrorKind::ExpiredSignature => ApiError::new(
            StatusCode::UNAUTHORIZED,
            "token_expired",
            "توکن منقضی شده است",
        ),
        _ => ApiError::unauthorized("invalid_token", "توکن نامعتبر است"),
    })?;
    if data.claims.typ != "access" {
        return Err(ApiError::unauthorized("invalid_token", "توکن نامعتبر است"));
    }
    Ok(data.claims)
}
