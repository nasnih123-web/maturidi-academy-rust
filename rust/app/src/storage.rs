//! Local file storage + signed-URL verification — mirrors the
//! `LocalStorageProvider` in `app/core/storage.py`.
//!
//! Files live under `STORAGE_DIR` (default `./storage`), never publicly
//! mounted. Signatures are HMAC-SHA256 over `"{purpose}:{key}:{exp}"`
//! with the storage signing keys: `STORAGE_SIGNING_SECRET`, else the
//! JWT secret as a transitional dev fallback, plus
//! `STORAGE_SIGNING_SECRET_PREVIOUS` during rotation (FIX3-02).
//! S3/MinIO is not ported in Wave 2 — `STORAGE_PROVIDER != local`
//! fails with 503 `storage_not_configured`, like Python's `get_storage`
//! raising for a misconfigured provider (ADR-015).

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use crate::{crypto::hmac_sha256_hex, errors::ApiError, Config};

pub const SIGNED_PURPOSE_AVATAR: &str = "avatar";

/// `storage_signing_keys(settings)` — current key first, then previous.
pub fn storage_signing_keys(config: &Config) -> Vec<String> {
    let mut keys = Vec::new();
    let current = config.storage_signing_secret.trim();
    if !current.is_empty() {
        keys.push(current.to_string());
    } else {
        keys.push(config.jwt_secret.clone());
    }
    let previous = config.storage_signing_secret_previous.trim();
    if !previous.is_empty() && !keys.contains(&previous.to_string()) {
        keys.push(previous.to_string());
    }
    keys
}

/// `verify_download_sig(key, exp, sig, purpose)` — expiry first, then
/// HMAC against every signing key.
pub fn verify_download_sig(config: &Config, key: &str, exp: i64, sig: &str, purpose: &str) -> bool {
    let now = chrono::Utc::now().timestamp();
    if now > exp {
        return false;
    }
    let msg = format!("{purpose}:{key}:{exp}");
    storage_signing_keys(config)
        .iter()
        .any(|k| hmac_sha256_hex(k.as_bytes(), msg.as_bytes()) == sig)
}

fn safe_path(base: &Path, key: &str) -> Result<PathBuf, ApiError> {
    let safe: String = key
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let safe = safe.trim_start_matches('/');
    if safe.split('/').any(|seg| seg == "..") {
        return Err(ApiError::internal());
    }
    Ok(base.join(safe))
}

/// Local provider base dir. Returns 503 when the provider is not local.
pub fn local_base_dir(config: &Config) -> Result<PathBuf, ApiError> {
    if config.storage_provider != "local" {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_not_configured",
            "ذخیره‌سازی فایل پیکربندی نشده است",
        ));
    }
    Ok(PathBuf::from(&config.storage_dir))
}

pub async fn put_object(config: &Config, key: &str, data: &[u8]) -> Result<(), ApiError> {
    let path = safe_path(&local_base_dir(config)?, key)?;
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|_| ApiError::internal())?;
    }
    tokio::fs::write(&path, data)
        .await
        .map_err(|_| ApiError::internal())
}

pub async fn read_bytes(config: &Config, key: &str) -> Result<Vec<u8>, ApiError> {
    let path = safe_path(&local_base_dir(config)?, key)?;
    tokio::fs::read(&path)
        .await
        .map_err(|_| ApiError::not_found("not_found", "فایل یافت نشد"))
}

/// `LocalStorageProvider.get_presigned_url` — mirrors
/// `app/core/storage.py`: `{app_base_url}{download_path}?key=...&exp=...&sig=...&purpose=...`
/// with `sig = HMAC-SHA256("{purpose}:{key}:{exp}")` under the first
/// signing key. The key is embedded raw, exactly like Python (no
/// URL-encoding). Non-local providers fail with 503
/// `storage_not_configured` (ADR-015).
pub fn presigned_url(
    config: &Config,
    key: &str,
    expires_seconds: i64,
    download_path: &str,
    purpose: &str,
) -> Result<String, ApiError> {
    let _ = local_base_dir(config)?;
    let exp = chrono::Utc::now().timestamp() + expires_seconds;
    let msg = format!("{purpose}:{key}:{exp}");
    let keys = storage_signing_keys(config);
    let sig = hmac_sha256_hex(keys[0].as_bytes(), msg.as_bytes());
    let base = config.app_base_url.trim_end_matches('/');
    Ok(format!(
        "{base}{download_path}?key={key}&exp={exp}&sig={sig}&purpose={purpose}"
    ))
}
