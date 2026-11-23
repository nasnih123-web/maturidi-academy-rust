//! Local file storage + signed-URL verification — mirrors the
//! `LocalStorageProvider` in `app/core/storage.py` — plus the
//! S3/MinIO provider (Wave 9 PR-3, ADR-162), SigV4-signed over
//! reqwest with no SDK.
//!
//! Files live under `STORAGE_DIR` (default `./storage`), never publicly
//! mounted. Signatures are HMAC-SHA256 over `"{purpose}:{key}:{exp}"`
//! with the storage signing keys: `STORAGE_SIGNING_SECRET`, else the
//! JWT secret as a transitional dev fallback, plus
//! `STORAGE_SIGNING_SECRET_PREVIOUS` during rotation (FIX3-02).
//! For the S3 provider the presigned URL is a direct SigV4 object URL
//! against the public endpoint; local objects keep the HMAC
//! `download-by-ref` URL, which 403s for S3 keys (the original's
//! behaviour). Production must be `s3` — `validate_storage_config`
//! enforces it at boot (see `main`).

use std::path::{Path, PathBuf};

use axum::http::StatusCode;

use crate::{crypto::hmac_sha256_hex, errors::ApiError, sigv4, Config};

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
    match config.storage_provider.as_str() {
        "s3" | "minio" => {
            let resp = s3_request(config, reqwest::Method::PUT, key, Some(data)).await?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(ApiError::internal())
            }
        }
        "local" => {
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
        _ => Err(unknown_provider()),
    }
}

pub async fn read_bytes(config: &Config, key: &str) -> Result<Vec<u8>, ApiError> {
    match config.storage_provider.as_str() {
        "s3" | "minio" => {
            let resp = s3_request(config, reqwest::Method::GET, key, None).await?;
            if resp.status() == StatusCode::NOT_FOUND {
                return Err(ApiError::not_found("not_found", "فایل یافت نشد"));
            }
            if !resp.status().is_success() {
                return Err(ApiError::internal());
            }
            resp.bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|_| ApiError::internal())
        }
        "local" => {
            let path = safe_path(&local_base_dir(config)?, key)?;
            tokio::fs::read(&path)
                .await
                .map_err(|_| ApiError::not_found("not_found", "فایل یافت نشد"))
        }
        _ => Err(unknown_provider()),
    }
}

/// `LocalStorageProvider.get_presigned_url` — mirrors
/// `app/core/storage.py`: `{app_base_url}{download_path}?key=...&exp=...&sig=...&purpose=...`
/// with `sig = HMAC-SHA256("{purpose}:{key}:{exp}")` under the first
/// signing key. The key is embedded raw, exactly like Python (no
/// URL-encoding). For the S3 provider this is instead a direct SigV4
/// presigned GET URL against the *public* endpoint — the host browsers
/// see, never the internal endpoint — and `download_path`/`purpose`
/// do not apply. An unknown provider fails with 503
/// `storage_not_configured` (ADR-015).
pub fn presigned_url(
    config: &Config,
    key: &str,
    expires_seconds: i64,
    download_path: &str,
    purpose: &str,
) -> Result<String, ApiError> {
    match config.storage_provider.as_str() {
        "s3" | "minio" => {
            let _ = (download_path, purpose);
            let (amz_date, date_stamp) = sigv4::now_amz_date();
            let public = if config.s3_endpoint_public.trim().is_empty() {
                config.s3_endpoint.as_str()
            } else {
                config.s3_endpoint_public.as_str()
            };
            Ok(sigv4::presigned_url(
                public,
                &config.s3_bucket,
                key,
                &config.s3_access_key,
                &config.s3_secret_key,
                &config.s3_region,
                expires_seconds,
                &amz_date,
                &date_stamp,
            ))
        }
        "local" => {
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
        _ => Err(unknown_provider()),
    }
}
// ===========================================================================
// S3/MinIO provider (Wave 9 PR-3, ADR-162) — SigV4 over reqwest, no SDK.
// ===========================================================================

use std::time::Duration;

/// Path-style object URL: `{endpoint}/{bucket}/{key}` (the MinIO
/// client's default). Keys in this codebase are already sanitised by
/// `safe_path` (alphanumerics plus `. _ - /`), so no extra encoding
/// is needed here; the canonical URI in `sigv4` encodes the same way.
fn s3_object_url(config: &Config, key: &str) -> String {
    format!(
        "{}/{}/{}",
        config.s3_endpoint.trim_end_matches('/'),
        config.s3_bucket,
        key
    )
}

fn s3_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("storage http client builds")
}

/// One signed S3 request. A transport failure is 503
/// `storage_not_configured` — the bucket is unreachable, which is a
/// configuration/deployment problem, not a per-request error.
async fn s3_request(
    config: &Config,
    method: reqwest::Method,
    key: &str,
    data: Option<&[u8]>,
) -> Result<reqwest::Response, ApiError> {
    let (amz_date, date_stamp) = sigv4::now_amz_date();
    let payload = data.unwrap_or(&[]);
    let (auth, date, payload_hash) = sigv4::sign_request(
        method.as_str(),
        &config.s3_endpoint,
        &config.s3_bucket,
        key,
        &config.s3_access_key,
        &config.s3_secret_key,
        &config.s3_region,
        payload,
        &amz_date,
        &date_stamp,
    );
    s3_client()
        .request(method, s3_object_url(config, key))
        .header("Authorization", auth)
        .header("x-amz-date", date)
        .header("x-amz-content-sha256", payload_hash)
        .header("Content-Type", "application/octet-stream")
        .body(payload.to_vec())
        .send()
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_not_configured",
                "ذخیره‌سازی فایل در دسترس نیست",
            )
        })
}

fn unknown_provider() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "storage_not_configured",
        "ذخیره‌سازی فایل پیکربندی نشده است",
    )
}

/// Delete an object. Local: missing files are a no-op (like S3's
/// idempotent delete). S3: any non-2xx is an internal error.
pub async fn delete_object(config: &Config, key: &str) -> Result<(), ApiError> {
    match config.storage_provider.as_str() {
        "s3" | "minio" => {
            let resp = s3_request(config, reqwest::Method::DELETE, key, None).await?;
            if resp.status().is_success() {
                Ok(())
            } else {
                Err(ApiError::internal())
            }
        }
        "local" => {
            let path = safe_path(&local_base_dir(config)?, key)?;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(_) => Err(ApiError::internal()),
            }
        }
        _ => Err(unknown_provider()),
    }
}

// ===========================================================================
// Boot-time validation (ADR-162) — ported from Python's
// `_secret_problems` / `s3_production_problems` in `app/core/storage.py`.
// ===========================================================================

/// Validate the storage configuration. Returns the problems; empty
/// means acceptable. Strict only in production: dev/test always
/// return empty, so local development never refuses to boot.
/// `main` refuses to start in production when this is non-empty.
/// Messages never contain a secret value.
pub fn validate_storage_config(config: &Config) -> Vec<String> {
    if config.app_env != "production" {
        return Vec::new();
    }
    let mut problems = Vec::new();
    match config.storage_provider.as_str() {
        "local" => problems.push(
            "storage provider is local in production: files would be lost on redeploy; set STORAGE_PROVIDER=s3".to_string(),
        ),
        "s3" | "minio" => {
            if config.s3_endpoint.trim().is_empty() {
                problems.push("s3_endpoint is empty".to_string());
            } else if is_internal_host(&endpoint_host(&config.s3_endpoint)) {
                problems.push(
                    "s3_endpoint uses an internal host: the bucket would not be reachable from the deployment".to_string(),
                );
            }
            if config.s3_endpoint_public.trim().is_empty() {
                problems.push(
                    "s3_endpoint_public is empty: presigned URLs would expose the internal s3_endpoint host to browsers".to_string(),
                );
            } else if is_internal_host(&endpoint_host(&config.s3_endpoint_public)) {
                problems.push(
                    "s3_endpoint_public uses an internal host: presigned URLs would not be browser-reachable".to_string(),
                );
            }
            if is_weak_secret(&config.s3_access_key, 16) {
                problems.push(
                    "s3_access_key is empty, a placeholder, or too short (min 16 characters)".to_string(),
                );
            }
            if is_weak_secret(&config.s3_secret_key, 32) {
                problems.push(
                    "s3_secret_key is empty, a placeholder, or too short (min 32 characters)".to_string(),
                );
            }
            if !is_valid_bucket(&config.s3_bucket) {
                problems.push(
                    "s3_bucket is empty or invalid (lowercase letters, digits, dots, hyphens)".to_string(),
                );
            }
            let signing = config.storage_signing_secret.trim();
            if signing.is_empty() || signing.len() < 32 || signing == config.jwt_secret.trim() {
                problems.push(
                    "storage_signing_secret is empty, too short (min 32 characters), or equal to jwt_secret".to_string(),
                );
            }
        }
        other => problems.push(format!("unknown STORAGE_PROVIDER: {other}")),
    }
    problems
}

fn endpoint_host(endpoint: &str) -> String {
    let rest = endpoint.split("://").nth(1).unwrap_or(endpoint);
    let hostport = rest.split('/').next().unwrap_or("");
    hostport.split(':').next().unwrap_or("").to_string()
}

/// Localhost / RFC-1918 / single-label hosts are not reachable from
/// a browser or another service.
fn is_internal_host(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    h == "localhost"
        || h == "host.docker.internal"
        || h == "::1"
        || h == "[::1]"
        || h.starts_with("127.")
        || h.starts_with("10.")
        || h.starts_with("192.168.")
        || h.starts_with("172.16.")
        || h.starts_with("172.17.")
        || h.starts_with("172.18.")
        || h.starts_with("172.19.")
        || h.starts_with("172.2")
        || h.starts_with("172.30.")
        || h.starts_with("172.31.")
        || !h.contains('.')
}

fn is_weak_secret(value: &str, min_len: usize) -> bool {
    let v = value.trim();
    v.is_empty() || v.len() < min_len || {
        let lower = v.to_ascii_lowercase();
        lower.contains("minioadmin")
            || lower.contains("change_me")
            || lower.contains("changeme")
            || lower.contains("placeholder")
    }
}

fn is_valid_bucket(bucket: &str) -> bool {
    let b = bucket.trim();
    (3..=63).contains(&b.len())
        && b.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
        && !b.starts_with(['.', '-'])
        && !b.ends_with(['.', '-'])
        && !b.contains("..")
}
