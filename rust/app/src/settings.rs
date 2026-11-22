//! Platform settings — mirrors `app/core/platform_settings.py`.
//!
//! Values are stored as a versioned Fernet envelope
//! (`{"v": version, "data": "<fernet token>"}`) in `platform_settings`.
//! `get_typed_setting` parses `localized`/`json` values as JSON, others
//! as plain strings. The settings version counter is process-local and
//! starts at 0, like a fresh Python process that has not updated any
//! setting yet (Wave 2 never writes settings).

use crate::db::IntoArg;
use serde_json::Value;

use crate::{
    crypto::{fernet_decrypt, fernet_encrypt},
    db::Db,
    errors::ApiError,
};

/// Setting keys whose stored value is JSON (type `json` / `localized` in
/// `SETTING_DEFS`). Everything else is a plain string.
pub fn setting_is_json(key: &str) -> bool {
    // All public meta keys are `json` except the VAPID key (string).
    key != "push.vapid_public_key"
}

/// Build the Fernet key map from config: `ENCRYPTION_KEYS` JSON
/// (version → material), falling back to the single `ENCRYPTION_KEY`
/// for the current version.
fn key_material(config: &crate::Config, version: i64) -> Option<String> {
    if let Ok(map) = serde_json::from_str::<Value>(&config.encryption_keys) {
        if let Some(v) = map.get(version.to_string()).and_then(|v| v.as_str()) {
            return Some(v.to_string());
        }
    }
    if version == config.encryption_key_version {
        return Some(config.encryption_key.clone());
    }
    None
}

/// `decrypt_value(envelope)` — envelope is the stored JSON value.
pub fn decrypt_value(config: &crate::Config, envelope: &Value) -> Result<String, ApiError> {
    let version = envelope
        .get("v")
        .and_then(|v| v.as_i64())
        .ok_or_else(ApiError::internal)?;
    let data = envelope
        .get("data")
        .and_then(|v| v.as_str())
        .ok_or_else(ApiError::internal)?;
    let material = key_material(config, version).ok_or_else(ApiError::internal)?;
    let plain = fernet_decrypt(&material, data).ok_or_else(ApiError::internal)?;
    String::from_utf8(plain).map_err(|_| ApiError::internal())
}

/// `encrypt_value(plaintext)` — envelope dict with the current version.
pub fn encrypt_value(config: &crate::Config, plaintext: &str) -> Value {
    let material = key_material(config, config.encryption_key_version)
        .unwrap_or_else(|| config.encryption_key.clone());
    let token = fernet_encrypt(&material, plaintext.as_bytes());
    serde_json::json!({ "v": config.encryption_key_version, "data": token })
}

/// `get_setting(db, key)` — decrypted raw string, or `None` when absent.
/// A row that cannot be decrypted is an internal error, like Python.
pub async fn get_setting(
    db: &Db,
    config: &crate::Config,
    key: &str,
) -> Result<Option<String>, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT value FROM platform_settings WHERE key = ?",
            &[(key).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let value: Value = row.try_get("value")?;
    Ok(Some(decrypt_value(config, &value)?))
}

/// `get_typed_setting(db, key, default)` — JSON-typed keys are parsed.
pub async fn get_typed_setting(
    db: &Db,
    config: &crate::Config,
    key: &str,
    default: Value,
) -> Result<Value, ApiError> {
    let Some(raw) = get_setting(db, config, key).await? else {
        return Ok(default);
    };
    if setting_is_json(key) {
        return serde_json::from_str(&raw).map_err(|_| ApiError::internal());
    }
    Ok(Value::String(raw))
}

/// Upsert a setting (used by tests to seed `platform_settings`).
#[allow(dead_code)] // Wave 7: admin settings writes
pub async fn stage_setting(
    db: &Db,
    config: &crate::Config,
    key: &str,
    plaintext: &str,
) -> Result<(), ApiError> {
    let envelope = encrypt_value(config, plaintext);
    // Postgres upsert; sqlite uses the same `ON CONFLICT` syntax.
    db.execute("INSERT INTO platform_settings (key, value, encryption_key_version) VALUES (?, ?, ?) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, encryption_key_version = excluded.encryption_key_version", &[(key).into_arg(), (envelope).into_arg(), (config.encryption_key_version).into_arg()])
    .await?;
    Ok(())
}
