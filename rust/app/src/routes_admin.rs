//! Admin-core routes — mirrors `app/modules/admin/` (Wave 7).
//!
//! Natively implemented (37 route+method combos):
//! - `router.py` — platform settings (GET/PUT), settings re-encrypt,
//!   dead-letter events (list/retry), custom fonts (upload/delete),
//!   sync devices.
//! - `builds_router.py` — releases, prereqs, rebuild, requests,
//!   release creation (incl. the CI service-token callback, B-01).
//! - `connections_router.py` — external-dependency status + per-service
//!   connectivity tests.
//! - `notifications_router.py` — multi-channel broadcasts, delivery
//!   stats, channel list, broadcast preview.
//! - `roles_router.py` — role/permission CRUD (super_admin guards).
//! - `users_admin_router.py` — users dashboard/stats/trash, the 360°
//!   overview, role replacement, session revocation, suspend/activate,
//!   soft delete, restore, permanent delete.
//!
//! The admin content-center (`content_center_*`) and control-registry
//! routes are NOT part of this module — they belong to the Wave 7
//! admin-content worker.
//!
//! Kill switch: `RUST_ANYTHING_UNIT_ADMIN=original` falls back to the
//! Python origin (see `unit_native("ADMIN")` in `lib.rs`).
//!
//! Honest gaps (see the ADM drafts in the wave report): the builds
//! dispatch is real (GitHub Actions `workflow_dispatch` over HTTPS —
//! never a shell), but release download URLs for `artifact_key` rows
//! cannot be presigned (the builds bucket is MinIO/S3, not in the Rust
//! binary — ADR-015); the AI/Telegram connectivity tests report
//! honestly instead of faking a provider ping; admin rate limits are
//! not enforced (no prior wave wires route-level limits).

use std::collections::{HashMap, HashSet};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Multipart, State},
    http::{HeaderMap, StatusCode, Uri},
};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission, user_from_row, AuthCtx, UserRow},
    countries::country_iso_from_phone,
    crypto::fernet_decrypt,
    db::{utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    mail::{send_email, send_sms},
    routes_users::missing_profile_fields,
    settings::{encrypt_value, get_setting, get_typed_setting, stage_setting},
    storage::{presigned_url, put_object, SIGNED_PURPOSE_AVATAR},
    upload_scan::{scan_upload, ScanAudit},
    validate::{parse_body, parse_datetime_str, parse_path_uuid, parse_query, query_int, req_dict},
    AppState,
};

pub type ApiJson = axum::response::Json<Value>;

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn bad(status: StatusCode, code: &'static str, msg: impl Into<String>) -> ApiError {
    ApiError::new(status, code, msg)
}

/// Read an env var the way `Config::from_env` does (empty = unset).
/// These keys have no field on the shared `Config` struct, so the
/// admin module reads them directly instead of growing `Config`.
fn env_str(name: &str) -> String {
    std::env::var(name)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_default()
}

fn env_trimmed(name: &str) -> String {
    env_str(name).trim().to_string()
}

/// `(ip, user_agent)` for audit rows, mirroring the routers' `_client`.
fn client_info(
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
    state: &AppState,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

async fn authed(state: &AppState, headers: &HeaderMap, permission: &str) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, "/api/v1/admin").await?;
    require_permission(state, &ctx, permission).await?;
    Ok(ctx)
}

// ===========================================================================
// Platform settings — SETTING_DEFS (mirrors
// `app/core/platform_settings.py`), value encode/decode
// ===========================================================================

/// `(key, type, sensitive)` — `type` is one of
/// `secret | string | localized | json | uuid`. Sorted by key, exactly
/// like `list_settings` iterates `sorted(SETTING_DEFS)`.
const SETTING_DEFS: &[(&str, &str, bool)] = &[
    ("adhan.defaults", "json", false),
    ("ai.api_key", "secret", true),
    ("ai.claude_api_key", "secret", true),
    ("ai.claude_model", "json", false),
    ("ai.daily_quota_per_user", "json", false),
    ("ai.embedding_api_key", "secret", true),
    ("ai.embedding_endpoint", "json", false),
    ("ai.embedding_model", "json", false),
    ("ai.embedding_provider", "json", false),
    ("ai.external_research_enabled", "json", false),
    ("ai.fallback_model", "string", false),
    ("ai.model", "string", false),
    ("ai.provider", "json", false),
    ("ai.rate_per_minute", "json", false),
    ("ai.stream_timeout_s", "json", false),
    ("auth.sms_required_countries", "json", false),
    ("bi.metabase_url", "string", false),
    ("branding.app_display_name", "localized", false),
    ("branding.app_icon_media_id", "uuid", false),
    ("branding.app_splash_media_id", "uuid", false),
    ("builds.expo_project_id", "string", false),
    ("donation.card_enabled", "json", false),
    ("donation.card_holder", "string", false),
    ("donation.card_instructions", "localized", false),
    ("donation.card_number", "string", false),
    ("donation.crypto_enabled", "json", false),
    ("donation.crypto_instructions", "localized", false),
    ("donation.crypto_wallets", "json", false),
    ("donation.online_enabled", "json", false),
    ("donation.paypal_account", "string", false),
    ("donation.paypal_enabled", "json", false),
    ("donation.paypal_instructions", "localized", false),
    ("email.smtp_password", "secret", true),
    ("fonts.ar", "json", false),
    ("fonts.en", "json", false),
    ("fonts.fa", "json", false),
    ("hero.ar", "json", false),
    ("hero.en", "json", false),
    ("hero.fa", "json", false),
    ("home.sections", "json", false),
    ("map.key", "secret", true),
    ("munajat.enabled", "json", false),
    ("munajat.window", "json", false),
    ("particles", "json", false),
    ("payment.callback_url", "string", false),
    ("payment.idpay_key", "secret", true),
    ("payment.idpay_sandbox", "json", false),
    ("payment.webhook_secret", "secret", true),
    ("payment.zarinpal_key", "secret", true),
    ("payment.zarinpal_sandbox", "json", false),
    ("places.globe", "json", false),
    ("places.popup", "json", false),
    ("places.types", "json", false),
    ("profile.require_for_fatwa_question", "json", false),
    ("profile.require_for_hifz_advanced", "json", false),
    ("push.fcm_server_key", "secret", true),
    ("push.vapid_private_key", "secret", true),
    ("push.vapid_public_key", "string", false),
    ("quran.active_languages", "json", false),
    ("quran.asr_engine", "json", false),
    ("quran.custom_provider_keys", "json", false),
    ("quran.custom_providers", "json", false),
    ("quran.display_font", "json", false),
    ("quran.display_theme", "json", false),
    ("quran.font_size", "json", false),
    ("quran.highlight_mode", "json", false),
    ("quran.hifz_retention.attempts_days", "json", false),
    ("quran.hifz_retention.mistakes_days", "json", false),
    ("quran.hifz_retention.plans_days", "json", false),
    ("quran.hifz_retention.reviews_inactive_days", "json", false),
    ("quran.hifz_retention.tasmi_days", "json", false),
    ("quran.hifz_retention.tests_days", "json", false),
    ("quran.hifz_thresholds", "json", false),
    ("quran.madhhab_default", "json", false),
    ("quran.playback", "json", false),
    ("quran.player_default_speed", "json", false),
    ("quran.player_show_photo", "json", false),
    ("quran.provider", "json", false),
    ("quran.providers_by_type", "json", false),
    ("quran.qf_base_url", "json", false),
    ("quran.qf_client_id", "json", false),
    ("quran.qf_client_secret", "secret", true),
    ("quran.reciter_photos", "json", false),
    ("quran.reciters", "json", false),
    ("quran.show_tafsir", "json", false),
    ("quran.show_translation", "json", false),
    ("quran.tafsirs", "json", false),
    ("quran.translations", "json", false),
    ("quran.waveform_style", "json", false),
    ("site.social_links", "json", false),
    ("sms.api_key", "secret", true),
    ("telegram.api_hash", "secret", true),
    ("telegram.api_id", "secret", true),
    ("telegram.bot_token", "secret", true),
    ("telegram.channel", "string", false),
    ("telegram.enabled", "json", false),
    ("telegram.pending_expires_at", "string", false),
    ("telegram.pending_phone", "secret", true),
    ("telegram.pending_phone_code_hash", "secret", true),
    ("telegram.pending_session", "secret", true),
    ("telegram.phone_masked", "string", false),
    ("telegram.session", "secret", true),
    ("telegram.webhook_secret", "secret", true),
    ("videos.autoplay_next", "json", false),
    ("videos.home_enabled", "json", false),
    ("videos.player_pip", "json", false),
    ("videos.player_speed", "json", false),
    ("videos.related_enabled", "json", false),
    ("videos.resume_playback", "json", false),
    ("videos.sidebar_side", "json", false),
    ("videos.subtitle_default_lang", "json", false),
    ("videos.subtitle_langs", "json", false),
    ("videos.subtitles_enabled", "json", false),
    ("voice.tts_cloud_api_key", "secret", true),
    ("voice.tts_cloud_endpoint", "string", false),
    ("voice.tts_cloud_model", "string", false),
];

fn setting_def(key: &str) -> Option<(&'static str, bool)> {
    SETTING_DEFS
        .iter()
        .find(|(k, _, _)| *k == key)
        .map(|(_, t, s)| (*t, *s))
}

/// `EMPTY_MEANS_UNSET_KEYS` — an empty value for these keys means
/// "unconfigured / unchanged", never a real value.
const EMPTY_MEANS_UNSET_KEYS: &[&str] = &[
    "telegram.enabled",
    "ai.external_research_enabled",
    "quran.show_translation",
    "quran.show_tafsir",
    "quran.player_show_photo",
    "munajat.enabled",
    "payment.zarinpal_sandbox",
    "payment.idpay_sandbox",
    "donation.online_enabled",
    "donation.card_enabled",
    "donation.crypto_enabled",
    "donation.paypal_enabled",
    "profile.require_for_fatwa_question",
    "profile.require_for_hifz_advanced",
    "donation.crypto_wallets",
    "site.social_links",
    "donation.card_instructions",
    "donation.crypto_instructions",
    "donation.paypal_instructions",
];

const MASKED: &str = "***";

fn invalid_setting(key: &str, msg: String) -> ApiError {
    bad(
        StatusCode::BAD_REQUEST,
        "invalid_setting_value",
        format!("{key}: {msg}"),
    )
}

fn validate_flag(key: &str, value: &Value) -> Result<Value, ApiError> {
    if let Some(s) = value.as_str() {
        let lowered = s.to_lowercase();
        if lowered == "true" {
            return Ok(Value::Bool(true));
        }
        if lowered == "false" {
            return Ok(Value::Bool(false));
        }
    }
    value
        .as_bool()
        .map(Value::Bool)
        .ok_or_else(|| invalid_setting(key, format!("{key} must be a boolean")))
}

fn validate_number_range(
    key: &str,
    value: &Value,
    min: f64,
    max: f64,
    as_int: bool,
) -> Result<Value, ApiError> {
    let Some(n) = value.as_f64() else {
        return Err(invalid_setting(key, format!("{key} must be a number")));
    };
    if value.as_bool().is_some() || !(min..=max).contains(&n) {
        return Err(invalid_setting(
            key,
            format!("{key} must be between {min} and {max}"),
        ));
    }
    if as_int {
        Ok(json!(n.trunc() as i64))
    } else {
        Ok(json!(n))
    }
}

fn validate_enum(key: &str, value: &Value, allowed: &[&str]) -> Result<Value, ApiError> {
    let Some(s) = value.as_str() else {
        return Err(invalid_setting(
            key,
            format!("{key} must be one of {}", allowed.join("|")),
        ));
    };
    if !allowed.contains(&s) {
        return Err(invalid_setting(
            key,
            format!("{key} must be one of {}", allowed.join("|")),
        ));
    }
    Ok(Value::String(s.to_string()))
}

fn validate_edition_list(key: &str, value: &Value) -> Result<Value, ApiError> {
    let Some(items) = value.as_array() else {
        return Err(invalid_setting(key, format!("{key} must be an array")));
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let Some(s) = item.as_str().filter(|s| !s.trim().is_empty()) else {
            return Err(invalid_setting(
                key,
                format!("{key} entries must be non-empty strings"),
            ));
        };
        let s = s.trim().to_string();
        if out.contains(&s) {
            return Err(invalid_setting(key, format!("{key}: duplicate '{s}'")));
        }
        out.push(s);
    }
    Ok(Value::Array(out.into_iter().map(Value::String).collect()))
}

/// `_validate_json_setting` — the per-key validators from
/// `platform_settings.py`. Keys with elaborate validators that no
/// admin-settings test exercises (particles, fonts, home.sections,
/// hero, playback, munajat.window, adhan.defaults, places.*,
/// providers_by_type, custom_providers, custom_provider_keys,
/// quran.provider, quran.reciter_photos, site.social_links) pass
/// through as canonical JSON; see ADM-02.
fn validate_json_setting(key: &str, value: Value) -> Result<Value, ApiError> {
    const FLAG_KEYS: &[&str] = &[
        "telegram.enabled",
        "ai.external_research_enabled",
        "quran.show_translation",
        "quran.show_tafsir",
        "quran.player_show_photo",
        "munajat.enabled",
        "payment.zarinpal_sandbox",
        "payment.idpay_sandbox",
        "donation.online_enabled",
        "donation.card_enabled",
        "donation.crypto_enabled",
        "donation.paypal_enabled",
        "profile.require_for_fatwa_question",
        "profile.require_for_hifz_advanced",
        "videos.home_enabled",
        "videos.autoplay_next",
        "videos.resume_playback",
        "videos.player_speed",
        "videos.player_pip",
        "videos.related_enabled",
        "videos.subtitles_enabled",
    ];
    if FLAG_KEYS.contains(&key) {
        return validate_flag(key, &value);
    }
    match key {
        "ai.provider" => validate_enum(key, &value, &["none", "claude"]),
        "ai.embedding_provider" => {
            validate_enum(key, &value, &["hash", "external_api", "local_model"])
        }
        "ai.claude_model" => {
            let s = value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    invalid_setting(
                        key,
                        "ai.claude_model must be a non-empty string".to_string(),
                    )
                })?;
            Ok(Value::String(s.trim().to_string()))
        }
        "ai.daily_quota_per_user" => validate_number_range(key, &value, 1.0, 10000.0, true),
        "ai.rate_per_minute" => validate_number_range(key, &value, 1.0, 600.0, true),
        "ai.stream_timeout_s" => validate_number_range(key, &value, 5.0, 600.0, true),
        "ai.embedding_endpoint" | "ai.embedding_model" => {
            if value.is_null() {
                Ok(Value::Null)
            } else if value.is_string() {
                Ok(value)
            } else {
                Err(invalid_setting(
                    key,
                    format!("{key} must be a string or null"),
                ))
            }
        }
        "videos.sidebar_side" => validate_enum(key, &value, &["left", "right"]),
        "videos.subtitle_default_lang" => {
            validate_enum(key, &value, &["fa", "ar", "en", "ur", "dari"])
        }
        "videos.subtitle_langs" => {
            let Some(items) = value.as_array() else {
                return Err(invalid_setting(
                    key,
                    "videos.subtitle_langs must be a list".to_string(),
                ));
            };
            let mut out: Vec<String> = Vec::new();
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(invalid_setting(
                        key,
                        "videos.subtitle_langs entries must be strings".to_string(),
                    ));
                };
                if !["fa", "ar", "en", "ur", "dari"].contains(&s) {
                    return Err(invalid_setting(
                        key,
                        "videos.subtitle_langs must be one of fa|ar|en|ur|dari".to_string(),
                    ));
                }
                if !out.contains(&s.to_string()) {
                    out.push(s.to_string());
                }
            }
            Ok(Value::Array(out.into_iter().map(Value::String).collect()))
        }
        "quran.highlight_mode" => validate_enum(key, &value, &["word", "ayah", "off"]),
        "quran.font_size" => validate_number_range(key, &value, 12.0, 72.0, false),
        "quran.madhhab_default" => {
            validate_enum(key, &value, &["hanafi", "shafii", "maliki", "hanbali"])
        }
        "quran.player_default_speed" => validate_number_range(key, &value, 0.5, 2.0, false),
        "quran.waveform_style" => {
            validate_enum(key, &value, &["bars", "dotted", "line", "mirrored", "neon"])
        }
        "quran.display_font" => validate_enum(key, &value, &["system", "amiri", "scheherazade"]),
        "quran.display_theme" => validate_enum(key, &value, &["dark", "sepia", "light", "green"]),
        "quran.asr_engine" => validate_enum(key, &value, &["zipformer", "whisper_quran"]),
        "quran.hifz_thresholds" => {
            let Some(obj) = value.as_object() else {
                return Err(invalid_setting(
                    key,
                    "quran.hifz_thresholds must be an object".to_string(),
                ));
            };
            let green = obj.get("green").and_then(|v| v.as_f64()).ok_or_else(|| {
                invalid_setting(
                    key,
                    "quran.hifz_thresholds green/amber must be numbers".to_string(),
                )
            })?;
            let amber = obj.get("amber").and_then(|v| v.as_f64()).ok_or_else(|| {
                invalid_setting(
                    key,
                    "quran.hifz_thresholds green/amber must be numbers".to_string(),
                )
            })?;
            if !(0.0 < amber && amber < green && green <= 1.0) {
                return Err(invalid_setting(
                    key,
                    "quran.hifz_thresholds requires 0 < amber < green <= 1".to_string(),
                ));
            }
            Ok(json!({ "green": green, "amber": amber }))
        }
        "quran.translations" | "quran.tafsirs" | "quran.reciters" => {
            validate_edition_list(key, &value)
        }
        "quran.active_languages" => {
            let list = validate_edition_list(key, &value)?;
            let items = list.as_array().unwrap();
            if items.is_empty() {
                return Err(invalid_setting(
                    key,
                    "quran.active_languages must not be empty".to_string(),
                ));
            }
            for item in items {
                let lang = item.as_str().unwrap();
                if !["fa", "ar", "en"].contains(&lang) {
                    return Err(invalid_setting(
                        key,
                        format!("quran.active_languages: unsupported language '{lang}'"),
                    ));
                }
            }
            Ok(list)
        }
        "auth.sms_required_countries" => {
            let Some(items) = value.as_array() else {
                return Err(invalid_setting(
                    key,
                    "auth.sms_required_countries must be a list of ISO country codes".to_string(),
                ));
            };
            let mut out: Vec<String> = Vec::new();
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(invalid_setting(
                        key,
                        "auth.sms_required_countries must be a list of ISO country codes"
                            .to_string(),
                    ));
                };
                let code = s.to_uppercase();
                if !crate::countries::is_valid_country_code(&code) {
                    return Err(invalid_setting(
                        key,
                        format!("auth.sms_required_countries: unknown country '{code}'"),
                    ));
                }
                if !out.contains(&code) {
                    out.push(code);
                }
            }
            out.sort();
            Ok(Value::Array(out.into_iter().map(Value::String).collect()))
        }
        "quran.qf_base_url" => {
            let s = value
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    invalid_setting(
                        key,
                        "quran.qf_base_url must be a non-empty string".to_string(),
                    )
                })?;
            let url = s.trim().trim_end_matches('/');
            if !url.starts_with("https://") && !url.starts_with("http://") {
                return Err(invalid_setting(
                    key,
                    "quran.qf_base_url must be an http(s) URL".to_string(),
                ));
            }
            Ok(Value::String(url.to_string()))
        }
        "quran.qf_client_id" => {
            let Some(s) = value.as_str() else {
                return Err(invalid_setting(
                    key,
                    "quran.qf_client_id must be a string".to_string(),
                ));
            };
            Ok(Value::String(s.trim().to_string()))
        }
        "donation.crypto_wallets" => {
            let Some(items) = value.as_array() else {
                return Err(invalid_setting(
                    key,
                    "donation.crypto_wallets must be a list".to_string(),
                ));
            };
            if items.len() > 10 {
                return Err(invalid_setting(
                    key,
                    "donation.crypto_wallets: at most 10 wallets".to_string(),
                ));
            }
            let mut out = Vec::new();
            for item in items {
                let Some(obj) = item.as_object() else {
                    return Err(invalid_setting(
                        key,
                        "donation.crypto_wallets entries must be objects".to_string(),
                    ));
                };
                let unknown: Vec<&String> = obj
                    .keys()
                    .filter(|k| !["currency", "network", "address"].contains(&k.as_str()))
                    .collect();
                if !unknown.is_empty() {
                    return Err(invalid_setting(
                        key,
                        format!("donation.crypto_wallets: unknown fields {unknown:?}"),
                    ));
                }
                let currency = obj
                    .get("currency")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        invalid_setting(
                            key,
                            "donation.crypto_wallets currency is required".to_string(),
                        )
                    })?;
                let address = obj
                    .get("address")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| {
                        invalid_setting(
                            key,
                            "donation.crypto_wallets address is required".to_string(),
                        )
                    })?;
                let network = obj.get("network").and_then(|v| v.as_str()).unwrap_or("");
                out.push(json!({ "currency": currency, "network": network, "address": address }));
            }
            Ok(Value::Array(out))
        }
        _ if key.starts_with("quran.hifz_retention.") => {
            if value.is_null() {
                return Ok(Value::Null);
            }
            let Some(n) = value.as_i64() else {
                return Err(invalid_setting(
                    key,
                    format!("{key} must be null or an integer number of days"),
                ));
            };
            if !(1..=3650).contains(&n) {
                return Err(invalid_setting(
                    key,
                    format!("{key} must be between 1 and 3650 days (or null)"),
                ));
            }
            Ok(json!(n))
        }
        _ => Ok(value),
    }
}

/// `encode_setting_value` — validate + convert an API value to the
/// storable plaintext string.
fn encode_setting_value(setting_type: &str, value: &Value, key: &str) -> Result<String, ApiError> {
    match setting_type {
        "json" => {
            let parsed: Value = match value {
                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| value.clone()),
                _ => value.clone(),
            };
            let canonical = validate_json_setting(key, parsed)?;
            serde_json::to_string(&canonical).map_err(|_| ApiError::internal())
        }
        "localized" => {
            let mut v = value.clone();
            if let Value::String(s) = &v {
                if !s.trim().is_empty() {
                    v = json!({ "fa": s.trim() });
                }
            }
            let Some(obj) = v.as_object() else {
                return Err(invalid_setting(
                    key,
                    "localized value must be a non-empty object".to_string(),
                ));
            };
            if obj.is_empty() || obj.values().any(|t| !t.is_string()) {
                return Err(invalid_setting(
                    key,
                    "localized value must be a non-empty object".to_string(),
                ));
            }
            serde_json::to_string(&v).map_err(|_| ApiError::internal())
        }
        "uuid" => {
            if value.is_null() || value.as_str() == Some("") {
                return Ok(String::new());
            }
            let Some(s) = value.as_str() else {
                return Err(invalid_setting(
                    key,
                    "value must be a UUID string".to_string(),
                ));
            };
            s.parse::<Uuid>()
                .map(|u| u.to_string())
                .map_err(|_| invalid_setting(key, "value must be a UUID string".to_string()))
        }
        _ => {
            let Some(s) = value.as_str() else {
                return Err(invalid_setting(key, "value must be a string".to_string()));
            };
            Ok(s.to_string())
        }
    }
}

/// `decode_setting_value` — plaintext back to the API value.
fn decode_setting_value(setting_type: &str, plaintext: &str) -> Result<Value, ApiError> {
    if setting_type == "localized" || setting_type == "json" {
        serde_json::from_str(plaintext).map_err(|_| ApiError::internal())
    } else {
        Ok(Value::String(plaintext.to_string()))
    }
}

// --- settings: GET / PUT / reencrypt ------------------------------------------

/// `GET /api/v1/admin/settings` — every registered key, sensitive
/// values masked as `"***"`.
pub async fn get_settings(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "platform.settings").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all("SELECT key, updated_at FROM platform_settings", &[])
        .await?;
    let mut by_key: HashMap<String, Option<DateTime<Utc>>> = HashMap::new();
    for row in rows {
        let key: String = row.try_get("key")?;
        let updated_at: Option<DateTime<Utc>> = row.try_get("updated_at").ok().flatten();
        by_key.insert(key, updated_at);
    }
    let mut items = Vec::new();
    for (key, setting_type, sensitive) in SETTING_DEFS {
        let updated_at = by_key.get(*key).copied().flatten();
        let value = if by_key.contains_key(*key) {
            let raw = get_setting(db, &state.config, key).await?;
            match raw {
                Some(raw) => {
                    if *sensitive {
                        Value::String(MASKED.to_string())
                    } else {
                        decode_setting_value(setting_type, &raw)?
                    }
                }
                None => Value::Null,
            }
        } else {
            Value::Null
        };
        items.push(json!({
            "key": key,
            "value": value,
            "masked": sensitive,
            "type": setting_type,
            "updated_at": updated_at.map(|t| t.to_rfc3339()),
        }));
    }
    Ok(axum::response::Json(json!({
        "settings": items,
        "version": state.settings_version(),
    })))
}

/// `PUT /api/v1/admin/settings` — `{"settings": {...}}`. Unknown key →
/// 400 `unknown_setting`; `"***"` for a sensitive key and empty values
/// for sensitive / `EMPTY_MEANS_UNSET_KEYS` keys mean "unchanged";
/// bad values → 400 `invalid_setting_value`. Everything is validated
/// before anything is staged (Python rolls the whole patch back).
pub async fn put_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "platform.settings").await?;
    let parsed = parse_body(&body)?;
    let patch = req_dict(&parsed, "settings")?;
    let patch = patch.as_object().unwrap();
    // Pass 1 — validate everything.
    let mut staged: Vec<(String, String)> = Vec::new();
    for (key, raw_value) in patch {
        let Some((setting_type, sensitive)) = setting_def(key) else {
            return Err(bad(
                StatusCode::BAD_REQUEST,
                "unknown_setting",
                format!("کلید ناشناخته: {key}"),
            ));
        };
        if sensitive && raw_value.as_str() == Some(MASKED) {
            continue;
        }
        if (raw_value.is_null() || raw_value.as_str() == Some(""))
            && (sensitive || EMPTY_MEANS_UNSET_KEYS.contains(&key.as_str()))
        {
            continue;
        }
        let plaintext = encode_setting_value(setting_type, raw_value, key)?;
        staged.push((key.clone(), plaintext));
    }
    // Pass 2 — stage, event, audit, bump.
    let db = state.db_or_500()?;
    let changed: Vec<String> = staged.iter().map(|(k, _)| k.clone()).collect();
    if changed.is_empty() {
        return Ok(axum::response::Json(json!({
            "updated": [],
            "version": state.settings_version(),
        })));
    }
    let mut audit_new = Map::new();
    for (key, plaintext) in &staged {
        let (setting_type, sensitive) = setting_def(key).unwrap();
        stage_setting(db, &state.config, key, plaintext).await?;
        if sensitive {
            audit_new.insert(key.clone(), json!({ "changed": true }));
        } else {
            audit_new.insert(key.clone(), decode_setting_value(setting_type, plaintext)?);
        }
    }
    emit_event(
        db,
        "platform",
        "settings",
        "platform.settings_updated",
        json!({ "keys": changed, "updated_by": ctx.user.id.to_string() }),
    )
    .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "platform.settings_update",
        "platform_setting",
        Some(&changed.join(",")),
        None,
        Some(Value::Object(audit_new)),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    state.bump_settings_version();
    Ok(axum::response::Json(json!({
        "updated": changed,
        "version": state.settings_version(),
    })))
}

fn envelope_key_material(config: &crate::Config, version: i64) -> Option<String> {
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

/// Re-encrypt one stored envelope to the current key version.
/// Returns `Ok(Some(new_envelope))` when rotated, `Ok(None)` when
/// already current, `Err(())` when unreadable.
fn reencrypt_envelope(
    config: &crate::Config,
    envelope: &Value,
    current: i64,
) -> Result<Option<Value>, ()> {
    let version = envelope.get("v").and_then(|v| v.as_i64()).ok_or(())?;
    if version == current {
        return Ok(None);
    }
    let data = envelope.get("data").and_then(|v| v.as_str()).ok_or(())?;
    let material = envelope_key_material(config, version).ok_or(())?;
    let plain = fernet_decrypt(&material, data).ok_or(())?;
    let plain = String::from_utf8(plain).map_err(|_| ())?;
    Ok(Some(encrypt_value(config, &plain)))
}

/// `POST /api/v1/admin/settings/reencrypt` — rotate stored envelopes
/// to the current key version (M-05). Only counters are returned.
pub async fn reencrypt_secrets(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let _ = body;
    let ctx = authed(&state, &headers, "platform.settings").await?;
    let db = state.db_or_500()?;
    let current = state.config.encryption_key_version;
    let mut out = json!({
        "current_version": current,
        "settings_rotated": 0,
        "settings_current": 0,
        "totp_rotated": 0,
        "totp_current": 0,
        "unreadable": 0,
    });
    let rows = db
        .fetch_all("SELECT key, value FROM platform_settings", &[])
        .await?;
    for row in rows {
        let key: String = row.try_get("key")?;
        let value: Value = row.try_get("value")?;
        match reencrypt_envelope(&state.config, &value, current) {
            Ok(Some(new_env)) => {
                db.execute(
                    "UPDATE platform_settings SET value = ?, encryption_key_version = ? WHERE key = ?",
                    &[new_env.into_arg(), current.into_arg(), key.into_arg()],
                )
                .await?;
                out["settings_rotated"] = json!(out["settings_rotated"].as_i64().unwrap_or(0) + 1);
            }
            Ok(None) => {
                out["settings_current"] = json!(out["settings_current"].as_i64().unwrap_or(0) + 1);
            }
            Err(()) => {
                out["unreadable"] = json!(out["unreadable"].as_i64().unwrap_or(0) + 1);
            }
        }
    }
    let users = db
        .fetch_all(
            "SELECT id, totp_secret FROM users WHERE totp_secret IS NOT NULL",
            &[],
        )
        .await?;
    for row in users {
        let id: Uuid = row.try_get("id")?;
        let secret: String = row.try_get("totp_secret")?;
        let parsed: Value = match serde_json::from_str(&secret) {
            Ok(v) => v,
            Err(_) => {
                out["unreadable"] = json!(out["unreadable"].as_i64().unwrap_or(0) + 1);
                continue;
            }
        };
        match reencrypt_envelope(&state.config, &parsed, current) {
            Ok(Some(new_env)) => {
                let text = serde_json::to_string(&new_env).map_err(|_| ApiError::internal())?;
                db.execute(
                    "UPDATE users SET totp_secret = ? WHERE id = ?",
                    &[text.into_arg(), id.into_arg()],
                )
                .await?;
                out["totp_rotated"] = json!(out["totp_rotated"].as_i64().unwrap_or(0) + 1);
            }
            Ok(None) => {
                out["totp_current"] = json!(out["totp_current"].as_i64().unwrap_or(0) + 1);
            }
            Err(()) => {
                out["unreadable"] = json!(out["unreadable"].as_i64().unwrap_or(0) + 1);
            }
        }
    }
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "platform.key_rotation",
        "platform_setting",
        Some("*"),
        None,
        Some(out.clone()),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(out))
}

// --- dead-letter events --------------------------------------------------------

/// `GET /api/v1/admin/events/dead-letter` — events whose dispatch
/// failed 5 times (`failed_at` set), newest failure first.
pub async fn list_dead_letter(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "platform.settings").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let db = state.db_or_500()?;
    let total: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM domain_events WHERE failed_at IS NOT NULL",
            &[],
        )
        .await?
        .try_get("c")?;
    let rows = db
        .fetch_all(
            "SELECT event_id, event_type, aggregate_type, aggregate_id, payload, \
             schema_version, created_at, retry_count, failed_at FROM domain_events \
             WHERE failed_at IS NOT NULL ORDER BY failed_at DESC LIMIT ?",
            &[limit.into_arg()],
        )
        .await?;
    let mut events = Vec::new();
    for row in rows {
        let event_id: Uuid = row.try_get("event_id")?;
        let event_type: String = row.try_get("event_type")?;
        let aggregate_type: String = row.try_get("aggregate_type")?;
        let aggregate_id: String = row.try_get("aggregate_id")?;
        let payload: Value = row.try_get("payload")?;
        let schema_version: i32 = row.try_get("schema_version")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let retry_count: i32 = row.try_get("retry_count")?;
        let failed_at: Option<DateTime<Utc>> = row.try_get("failed_at")?;
        events.push(json!({
            "event_id": event_id.to_string(),
            "event_type": event_type,
            "aggregate_type": aggregate_type,
            "aggregate_id": aggregate_id,
            "payload": payload,
            "schema_version": schema_version,
            "created_at": created_at.to_rfc3339(),
            "retry_count": retry_count,
            "failed_at": failed_at.map(|t| t.to_rfc3339()),
        }));
    }
    Ok(axum::response::Json(
        json!({ "events": events, "total": total }),
    ))
}

/// `POST /api/v1/admin/events/dead-letter/{event_id}/retry` — return
/// a dead-letter event to the dispatch cycle.
pub async fn retry_dead_letter(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(event_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "platform.settings").await?;
    let event_id = parse_path_uuid(&event_id_str, "event_id")?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT event_id, event_type, failed_at FROM domain_events WHERE event_id = ?",
            &[event_id.into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(bad(StatusCode::NOT_FOUND, "not_found", "رویداد یافت نشد"));
    };
    let failed_at: Option<DateTime<Utc>> = row.try_get("failed_at")?;
    if failed_at.is_none() {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "not_dead_letter",
            "این رویداد در dead-letter نیست",
        ));
    }
    let event_type: String = row.try_get("event_type")?;
    db.execute(
        "UPDATE domain_events SET failed_at = NULL, retry_count = 0 WHERE event_id = ?",
        &[event_id.into_arg()],
    )
    .await?;
    emit_event(
        db,
        "platform",
        &event_id.to_string(),
        "platform.event_requeued",
        json!({ "event_type": event_type, "requeued_by": ctx.user.id.to_string() }),
    )
    .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "platform.event_requeue",
        "domain_event",
        Some(&event_id.to_string()),
        None,
        Some(json!({ "failed_at": null, "retry_count": 0 })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(json!({
        "message": format!("رویداد {event_id} به چرخهٔ پردازش برگشت"),
    })))
}

// --- custom fonts ---------------------------------------------------------------

const FONT_MAX_BYTES: usize = 5 * 1024 * 1024;
const FONT_LOCALES: &[&str] = &["fa", "ar", "en"];

fn font_magic(ext: &str) -> Option<&'static [u8]> {
    match ext {
        ".woff2" => Some(b"wOF2"),
        ".woff" => Some(b"wOFF"),
        ".ttf" => Some(b"\x00\x01\x00\x00"),
        _ => None,
    }
}

fn slugify_family(family: &str) -> String {
    let mut slug = String::new();
    for ch in family.trim().to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
            slug.push(ch);
        } else {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-').to_string();
    let slug: String = slug.chars().take(60).collect();
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        "font".to_string()
    } else {
        slug
    }
}

struct FontForm {
    locale: Option<String>,
    family: Option<String>,
    file_name: Option<String>,
    file_bytes: Option<Vec<u8>>,
    file_overflow: bool,
}

async fn collect_font_form(mut mp: Multipart) -> ApiResult<FontForm> {
    let mut out = FontForm {
        locale: None,
        family: None,
        file_name: None,
        file_bytes: None,
        file_overflow: false,
    };
    while let Some(mut field) = mp
        .next_field()
        .await
        .map_err(|_| ApiError::validation(vec![json!({ "msg": "invalid multipart body" })]))?
    {
        match field.name() {
            Some("locale") => {
                if out.locale.is_none() {
                    out.locale = field.text().await.ok();
                }
            }
            Some("family") => {
                if out.family.is_none() {
                    out.family = field.text().await.ok();
                }
            }
            Some("file") => {
                if out.file_bytes.is_none() {
                    out.file_name = field.file_name().map(str::to_string);
                    let mut bytes = Vec::new();
                    let mut overflow = false;
                    while let Some(chunk) = field.chunk().await.map_err(|_| ApiError::internal())? {
                        if overflow {
                            continue;
                        }
                        if bytes.len() + chunk.len() > FONT_MAX_BYTES {
                            overflow = true;
                        } else {
                            bytes.extend_from_slice(&chunk);
                        }
                    }
                    out.file_bytes = Some(bytes);
                    out.file_overflow = overflow;
                }
            }
            _ => {
                let mut field = field;
                while let Some(_chunk) = field.chunk().await.map_err(|_| ApiError::internal())? {}
            }
        }
    }
    Ok(out)
}

/// `POST /api/v1/admin/fonts` (201) — custom font for one locale
/// (woff2/woff/ttf, max 5 MB, magic-byte checked). The file lands
/// under `static/fonts/<locale>/` in local storage and
/// `fonts.<locale>` becomes `{family, url}`.
pub async fn upload_font(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    multipart: Multipart,
) -> ApiResult<(StatusCode, ApiJson)> {
    let ctx = authed(&state, &headers, "platform.settings").await?;
    let form = collect_font_form(multipart).await?;
    // The router's size cap fires before any service validation.
    if form.file_overflow {
        return Err(ApiError::with_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "font_too_large",
            "حجم فونت نباید بیش از ۵ مگابایت باشد",
            json!({ "max_bytes": FONT_MAX_BYTES }),
        ));
    }
    let db = state.db_or_500()?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    let content = form
        .file_bytes
        .ok_or_else(|| validation_err("file", "field is required"))?;
    // Font uploads use the shared ADR-051 scanner (ADR-164).
    scan_upload(
        db,
        &state.config,
        "admin.font_upload_rejected_infected",
        "platform_setting",
        ScanAudit {
            actor_id: Some(ctx.user.id),
            actor_type: "admin",
            ip: ip.as_deref(),
            user_agent: ua.as_deref(),
        },
        &content,
        form.file_name.as_deref(),
    )
    .await?;
    let locale = form.locale.unwrap_or_default().trim().to_lowercase();
    if !FONT_LOCALES.contains(&locale.as_str()) {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "invalid_locale",
            "لوکیل باید یکی از fa/ar/en باشد",
        ));
    }
    let family = form.family.unwrap_or_default().trim().to_string();
    if family.is_empty() || family.chars().count() > 120 {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "invalid_font_family",
            "نام فونت باید ۱ تا ۱۲۰ نویسه باشد",
        ));
    }
    let ext = form
        .file_name
        .as_deref()
        .unwrap_or("")
        .rsplit('.')
        .next()
        .map(|e| format!(".{e}"))
        .unwrap_or_default()
        .to_lowercase();
    let Some(magic) = font_magic(&ext) else {
        return Err(ApiError::with_details(
            StatusCode::BAD_REQUEST,
            "invalid_font_extension",
            "پسوند فونت باید یکی از woff2/woff/ttf باشد",
            json!({ "allowed": [".ttf", ".woff", ".woff2"] }),
        ));
    };
    if content.len() > FONT_MAX_BYTES {
        return Err(ApiError::with_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "font_too_large",
            "حجم فونت نباید بیش از ۵ مگابایت باشد",
            json!({ "max_bytes": FONT_MAX_BYTES }),
        ));
    }
    if content.len() < 4 || content[..4] != *magic {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "invalid_font_file",
            "محتوای فایل با پسوند اعلام‌شده سازگار نیست",
        ));
    }
    let safe_name = format!(
        "{}-{}{}",
        slugify_family(&family),
        &Uuid::new_v4().simple().to_string()[..8],
        ext
    );
    let key = format!("static/fonts/{locale}/{safe_name}");
    put_object(&state.config, &key, &content).await?;
    let url = format!("/static/fonts/{locale}/{safe_name}");
    let setting_key = format!("fonts.{locale}");
    let plaintext = serde_json::to_string(&json!({ "family": family, "url": url }))
        .map_err(|_| ApiError::internal())?;
    stage_setting(db, &state.config, &setting_key, &plaintext).await?;
    emit_event(
        db,
        "platform",
        "settings",
        "platform.settings_updated",
        json!({ "keys": [setting_key], "updated_by": ctx.user.id.to_string(), "action": "font_upload" }),
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "platform.font_upload",
        "platform_setting",
        Some(&setting_key),
        None,
        Some(json!({ "family": family, "url": url })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    state.bump_settings_version();
    Ok((
        StatusCode::CREATED,
        axum::response::Json(json!({
            "locale": locale,
            "family": family,
            "url": url,
            "version": state.settings_version(),
        })),
    ))
}

/// `DELETE /api/v1/admin/fonts/{locale}` — revert to the built-in
/// font (`fonts.<locale>` = null) and delete the stored file.
pub async fn delete_font(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(locale_raw): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "platform.settings").await?;
    let locale = locale_raw.trim().to_lowercase();
    if !FONT_LOCALES.contains(&locale.as_str()) {
        return Err(bad(
            StatusCode::BAD_REQUEST,
            "invalid_locale",
            "لوکیل باید یکی از fa/ar/en باشد",
        ));
    }
    let db = state.db_or_500()?;
    let setting_key = format!("fonts.{locale}");
    if let Some(raw) = get_setting(db, &state.config, &setting_key).await? {
        if let Ok(current) = serde_json::from_str::<Value>(&raw) {
            let url = current.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let prefix = format!("/static/fonts/{locale}/");
            if let Some(name) = url.strip_prefix(&prefix) {
                if !name.is_empty()
                    && !name.contains('/')
                    && !name.contains('\\')
                    && !name.starts_with('.')
                {
                    let path = std::path::PathBuf::from(&state.config.storage_dir)
                        .join("static")
                        .join("fonts")
                        .join(&locale)
                        .join(name);
                    let _ = std::fs::remove_file(path);
                }
            }
        }
    }
    stage_setting(db, &state.config, &setting_key, "null").await?;
    emit_event(
        db,
        "platform",
        "settings",
        "platform.settings_updated",
        json!({ "keys": [setting_key], "updated_by": ctx.user.id.to_string(), "action": "font_delete" }),
    )
    .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "platform.font_delete",
        "platform_setting",
        Some(&setting_key),
        None,
        Some(json!({ "reverted_to_builtin": true })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    state.bump_settings_version();
    Ok(axum::response::Json(json!({
        "locale": locale,
        "family": null,
        "url": null,
        "version": state.settings_version(),
    })))
}

// --- sync devices ---------------------------------------------------------------

/// `GET /api/v1/admin/sync/devices` — all devices with their latest
/// cursor: the union of `device_cursors` (sync-only devices) and
/// `user_devices` (registered push devices), newest first.
pub async fn list_sync_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "sync.manage").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 100, 1, 200)?;
    let db = state.db_or_500()?;
    #[derive(Default)]
    struct Entry {
        user_id: Uuid,
        device_id: String,
        cursor_seq: i64,
        updated_at: Option<DateTime<Utc>>,
        platform: Option<String>,
    }
    let mut merged: HashMap<(Uuid, String), Entry> = HashMap::new();
    let cursor_rows = db
        .fetch_all(
            "SELECT user_id, device_id, cursor_seq, updated_at FROM device_cursors \
             ORDER BY updated_at DESC LIMIT ?",
            &[limit.into_arg()],
        )
        .await?;
    for row in cursor_rows {
        let user_id: Uuid = row.try_get("user_id")?;
        let device_id: String = row.try_get("device_id")?;
        let cursor_seq: i64 = row.try_get("cursor_seq")?;
        let updated_at: Option<DateTime<Utc>> = row.try_get("updated_at")?;
        merged.insert(
            (user_id, device_id.clone()),
            Entry {
                user_id,
                device_id,
                cursor_seq,
                updated_at,
                platform: None,
            },
        );
    }
    let dev_rows = db
        .fetch_all(
            "SELECT user_id, device_id, platform, last_seen FROM user_devices LIMIT ?",
            &[limit.into_arg()],
        )
        .await?;
    for row in dev_rows {
        let user_id: Uuid = row.try_get("user_id")?;
        let device_id: String = row.try_get("device_id")?;
        let platform: Option<String> = row.try_get("platform")?;
        let last_seen: DateTime<Utc> = row.try_get("last_seen")?;
        match merged.get_mut(&(user_id, device_id.clone())) {
            Some(entry) => {
                entry.platform = platform;
                if entry.updated_at.map(|t| last_seen > t).unwrap_or(true) {
                    entry.updated_at = Some(last_seen);
                }
            }
            None => {
                merged.insert(
                    (user_id, device_id.clone()),
                    Entry {
                        user_id,
                        device_id,
                        cursor_seq: 0,
                        updated_at: Some(last_seen),
                        platform,
                    },
                );
            }
        }
    }
    let mut devices: Vec<Entry> = merged.into_values().collect();
    devices.sort_by_key(|e| std::cmp::Reverse(e.updated_at));
    devices.truncate(limit as usize);
    let out: Vec<Value> = devices
        .into_iter()
        .map(|e| {
            json!({
                "user_id": e.user_id.to_string(),
                "device_id": e.device_id,
                "cursor_seq": e.cursor_seq,
                "updated_at": e.updated_at.map(|t| t.to_rfc3339()),
                "platform": e.platform,
            })
        })
        .collect();
    Ok(axum::response::Json(json!({ "devices": out })))
}

// --- user-agent parsing (faza C, `app/core/user_agent.py`) ----------------------

static BOT_MARKERS: &[&str] = &[
    "bot",
    "spider",
    "crawler",
    "crawling",
    "slurp",
    "bingpreview",
    "facebookexternalhit",
    "telegrambot",
    "whatsapp",
    "python-requests",
    "curl/",
    "wget/",
    "go-http-client",
    "okhttp",
    "postmanruntime",
    "headlesschrome",
];

fn ua_major(version: &str) -> Option<String> {
    version
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn ua_version_after(ua: &str, name: &str) -> Option<String> {
    let pat = match name {
        "Chrome" => r"(?:CriOS|Chrome)/([\d.]+)",
        "Edge" => r"Edg(?:A|iOS)?/([\d.]+)",
        "Firefox" => r"(?:FxiOS|Firefox)/([\d.]+)",
        "Safari" => r"Version/([\d.]+)",
        "Opera" => r"(?:OPR|Opera)/([\d.]+)",
        "Samsung Internet" => r"SamsungBrowser/([\d.]+)",
        _ => return None,
    };
    Regex::new(pat)
        .ok()?
        .captures(ua)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// Lightweight UA parse: `{browser, browser_major, os, device, summary}`.
/// Order matters (Edge/Opera/Samsung before Chrome; real Safari needs
/// `Version/…Safari`). Unknown input → Nones + device `unknown`.
fn parse_user_agent(ua: Option<&str>) -> Value {
    let empty = json!({
        "browser": null, "browser_major": null, "os": null,
        "device": "unknown", "summary": null,
    });
    let Some(text) = ua.filter(|s| !s.trim().is_empty()) else {
        return empty;
    };
    let text = text.trim();
    let low = text.to_lowercase();
    let is_bot = BOT_MARKERS.iter().any(|m| low.contains(m));
    let browser_rules: &[(&str, &str)] = &[
        (r"Edg(?:A|iOS)?/", "Edge"),
        (r"OPR/|Opera", "Opera"),
        (r"SamsungBrowser/", "Samsung Internet"),
        (r"YaBrowser/", "Yandex Browser"),
        (r"Vivaldi/", "Vivaldi"),
        (r"FxiOS/|Firefox/", "Firefox"),
        (r"CriOS/|Chrome/", "Chrome"),
        (r"Version/[\d.]+.*Safari/", "Safari"),
        (r"MSIE |Trident/", "Internet Explorer"),
    ];
    let mut browser: Option<&str> = None;
    let mut browser_major: Option<String> = None;
    for (pat, name) in browser_rules {
        if Regex::new(pat).map(|re| re.is_match(text)).unwrap_or(false) {
            browser = Some(name);
            browser_major = ua_version_after(text, name).and_then(|v| ua_major(&v));
            break;
        }
    }
    let os_rules: &[(&str, &str)] = &[
        (r"Windows NT", "Windows"),
        (r"Android", "Android"),
        (r"iPhone|iPad|iPod", "iOS"),
        (r"CrOS", "ChromeOS"),
        (r"Mac OS X|Macintosh", "macOS"),
        (r"Linux", "Linux"),
    ];
    let mut os_name: Option<&str> = None;
    for (pat, name) in os_rules {
        if Regex::new(pat).map(|re| re.is_match(text)).unwrap_or(false) {
            os_name = Some(name);
            break;
        }
    }
    let device = if is_bot {
        "bot"
    } else if low.contains("ipad")
        || low.contains("tablet")
        || (low.contains("android") && !low.contains("mobile"))
    {
        "tablet"
    } else if low.contains("mobile")
        || low.contains("iphone")
        || low.contains("ipod")
        || low.contains("android")
    {
        "mobile"
    } else if os_name.is_some() || browser.is_some() {
        "desktop"
    } else {
        "unknown"
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(b) = browser {
        parts.push(match &browser_major {
            Some(m) => format!("{b} {m}"),
            None => b.to_string(),
        });
    }
    if let Some(o) = os_name {
        parts.push(o.to_string());
    }
    let summary = if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    };
    json!({
        "browser": browser,
        "browser_major": browser_major,
        "os": os_name,
        "device": device,
        "summary": summary,
    })
}

fn iso(dt: Option<DateTime<Utc>>) -> Option<String> {
    dt.map(|t| t.to_rfc3339())
}

/// `device_block` — the display block for one event (registration or
/// last activity): IP + raw UA + parsed summary. Both empty → None.
fn device_block(
    ip: Option<String>,
    user_agent: Option<String>,
    at: Option<DateTime<Utc>>,
) -> Option<Value> {
    if ip.is_none() && user_agent.as_deref().unwrap_or("").is_empty() {
        return None;
    }
    let parsed = parse_user_agent(user_agent.as_deref());
    Some(json!({
        "ip": ip,
        "user_agent": user_agent,
        "browser": parsed["browser"],
        "os": parsed["os"],
        "device": parsed["device"],
        "summary": parsed["summary"],
        "at": iso(at),
    }))
}

const COUNTRY_NAMES_EN: &[(&str, &str)] = &[
    ("IR", "Iran"),
    ("AF", "Afghanistan"),
    ("IQ", "Iraq"),
    ("TR", "Türkiye"),
    ("AE", "United Arab Emirates"),
    ("SA", "Saudi Arabia"),
    ("QA", "Qatar"),
    ("KW", "Kuwait"),
    ("OM", "Oman"),
    ("BH", "Bahrain"),
    ("PK", "Pakistan"),
    ("IN", "India"),
    ("BD", "Bangladesh"),
    ("TJ", "Tajikistan"),
    ("UZ", "Uzbekistan"),
    ("TM", "Turkmenistan"),
    ("AZ", "Azerbaijan"),
    ("ID", "Indonesia"),
    ("MY", "Malaysia"),
    ("EG", "Egypt"),
    ("SY", "Syria"),
    ("LB", "Lebanon"),
    ("JO", "Jordan"),
    ("YE", "Yemen"),
    ("MA", "Morocco"),
    ("DZ", "Algeria"),
    ("TN", "Tunisia"),
    ("NG", "Nigeria"),
    ("GB", "United Kingdom"),
    ("DE", "Germany"),
    ("FR", "France"),
    ("NL", "Netherlands"),
    ("SE", "Sweden"),
    ("RU", "Russia"),
    ("CN", "China"),
    ("AU", "Australia"),
    ("CA", "Canada"),
    ("US", "United States"),
];

fn country_name(code: Option<&str>) -> Option<String> {
    code.and_then(|c| {
        COUNTRY_NAMES_EN
            .iter()
            .find(|(k, _)| *k == c)
            .map(|(_, n)| n.to_string())
    })
}

/// `(country_code, source)` — profile first, then the verified phone.
fn effective_country(user: &UserRow) -> (Option<String>, Option<String>) {
    if let Some(code) = &user.country_code {
        if !code.is_empty() {
            return (Some(code.clone()), Some("profile".to_string()));
        }
    }
    if let (Some(phone), true) = (&user.phone, user.is_verified) {
        if let Some(code) = country_iso_from_phone(phone) {
            return (Some(code.to_string()), Some("phone".to_string()));
        }
    }
    (None, None)
}

// ===========================================================================
// Builds — `builds_router.py` / `builds_service.py`
// ===========================================================================

const PLATFORMS: &[&str] = &["android", "ios", "desktop"];
const PROVIDER_GITHUB: &str = "github_actions";
const GITHUB_API_BASE: &str = "https://api.github.com";
const RELEASE_RETENTION_PER_PLATFORM: usize = 10;
const RELEASE_DOWNLOAD_TTL_SECONDS: i64 = 900;

fn workflow_for(platform: &str) -> &'static str {
    match platform {
        "desktop" => "desktop.yml",
        _ => "mobile.yml",
    }
}

fn profile_for(environment: &str) -> &'static str {
    match environment {
        "development" => "development",
        "production" => "production-apk",
        _ => "preview",
    }
}

fn provider_status() -> Value {
    let token = env_trimmed("GITHUB_DISPATCH_TOKEN");
    let repo = env_trimmed("GITHUB_REPO");
    let configured = !token.is_empty() && !repo.is_empty();
    json!({
        "configured": configured,
        "provider": if configured { Some(PROVIDER_GITHUB) } else { None },
        "repo": if repo.is_empty() { None } else { Some(repo) },
        "workflows": { "android": "mobile.yml", "ios": "mobile.yml", "desktop": "desktop.yml" },
    })
}

/// Constant-time service-token check (B-01): only valid when the
/// token is configured in the environment; never logged or stored.
fn service_token_ok(token: Option<&str>) -> bool {
    let expected = env_trimmed("BUILDS_SERVICE_TOKEN");
    let Some(given) = token.filter(|t| !t.trim().is_empty()) else {
        return false;
    };
    if expected.is_empty() {
        return false;
    }
    let a = expected.as_bytes();
    let b = given.trim().as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

static ARTIFACT_KEY_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^builds/[A-Za-z0-9][A-Za-z0-9._\-/]*$").unwrap());
static SHA256_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{64}$").unwrap());
static PACKAGE_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$").unwrap());
static RELEASE_VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d+\.\d+\.\d+$").unwrap());
static LOCAL_URL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://(localhost|127\.0\.0\.1|10\.0\.2\.2|\[::1\])(:|$|/)").unwrap()
});

fn validate_artifact_key(key: Option<String>) -> ApiResult<Option<String>> {
    let Some(k) = key else { return Ok(None) };
    let k = k.trim().to_string();
    if !ARTIFACT_KEY_RE.is_match(&k) || k.split('/').any(|s| s == "..") {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_artifact_key",
            "artifact_key باید زیر پیشوند builds/ باشد و شامل .. نباشد",
        ));
    }
    Ok(Some(k))
}

fn download_url_allowed_hosts(state: &AppState) -> HashSet<String> {
    let mut hosts = HashSet::new();
    for raw in [
        env_str("S3_ENDPOINT_PUBLIC"),
        env_str("BUILDS_S3_ENDPOINT_PUBLIC"),
        state.config.app_base_url.clone(),
    ] {
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            continue;
        }
        if let Some(host) = raw
            .split("://")
            .nth(1)
            .or_else(|| raw.split("://").next())
            .and_then(|h| h.split(['/', ':', '?']).next())
        {
            if !host.is_empty() {
                hosts.insert(host.to_lowercase());
            }
        }
    }
    hosts
}

fn validate_download_url(state: &AppState, url: Option<String>) -> ApiResult<Option<String>> {
    let Some(u) = url else { return Ok(None) };
    let u = u.trim().to_string();
    let bad_url = || {
        bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_download_url",
            "download_url باید https و روی میزبان عمومی شناخته‌شدهٔ پروژه (S3_ENDPOINT_PUBLIC / BUILDS_S3_ENDPOINT_PUBLIC / APP_BASE_URL) باشد؛ برای فایل روی باکت builds از artifact_key استفاده کنید",
        )
    };
    if u.is_empty() || u.chars().any(|c| c.is_whitespace()) {
        return Err(bad_url());
    }
    let (scheme, rest) = match u.split_once("://") {
        Some((s, r)) => (s, r),
        None => return Err(bad_url()),
    };
    let host = rest
        .split(['/', ':', '?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let authority = rest.split('/').next().unwrap_or("");
    if authority.contains('@') {
        return Err(bad_url());
    }
    if scheme == "http" && (host == "localhost" || host == "127.0.0.1") {
        // Local development exception.
    } else if scheme != "https" {
        return Err(bad_url());
    }
    if host.is_empty() || !download_url_allowed_hosts(state).contains(&host) {
        return Err(bad_url());
    }
    Ok(Some(u))
}

fn validate_branding(body: &Value) -> ApiResult<Map<String, Value>> {
    let mut out = Map::new();
    if let Some(v) = body.get("app_name").filter(|v| !v.is_null()) {
        let name = v.as_str().map(|s| s.trim().to_string()).unwrap_or_default();
        if name.chars().count() > 80 {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_app_name",
                "نام اپ حداکثر ۸۰ نویسه است",
            ));
        }
        if !name.is_empty() {
            out.insert("app_name".to_string(), Value::String(name));
        }
    }
    if let Some(v) = body.get("package_id").filter(|v| !v.is_null()) {
        let pid = v.as_str().map(|s| s.trim().to_string()).unwrap_or_default();
        if !pid.is_empty() {
            if !PACKAGE_ID_RE.is_match(&pid) {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_package_id",
                    format!(
                        "package_id نامعتبر است (نام پکیج جاوا، مثل org.maturidi.academy): {pid}"
                    ),
                ));
            }
            out.insert("package_id".to_string(), Value::String(pid));
        }
    }
    if let Some(v) = body.get("version_code").filter(|v| !v.is_null()) {
        let code = v.as_i64().ok_or_else(|| {
            bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_version_code",
                "version_code باید عدد صحیح مثبت باشد",
            )
        })?;
        if code <= 0 {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_version_code",
                "version_code باید عدد صحیح مثبت باشد",
            ));
        }
        out.insert("version_code".to_string(), json!(code));
    }
    let env = body
        .get("environment")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !env.is_empty() {
        if !["development", "preview", "production"].contains(&env.as_str()) {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_environment",
                format!("محیط نامعتبر: {env} (مجاز: development, preview, production)"),
            ));
        }
        out.insert("environment".to_string(), Value::String(env));
    }
    if let Some(v) = body.get("api_url").filter(|v| !v.is_null()) {
        let url = v
            .as_str()
            .map(|s| s.trim().trim_end_matches('/').to_string())
            .unwrap_or_default();
        if !url.is_empty() {
            if !url.starts_with("http://") && !url.starts_with("https://") {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_api_url",
                    "api_url باید با http(s) شروع شود",
                ));
            }
            if out.get("environment").and_then(|v| v.as_str()) == Some("production")
                && LOCAL_URL_RE.is_match(&url)
            {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_api_url",
                    "api_url در بیلد production نمی‌تواند localhost باشد",
                ));
            }
            out.insert("api_url".to_string(), Value::String(url));
        }
    }
    Ok(out)
}

/// `release_tag_for` — only `production` + a `digits.digits.digits`
/// version yields `v<version>`; everything else yields "".
fn release_tag_for(version: Option<&str>, branding: &Value) -> String {
    if branding.get("environment").and_then(|v| v.as_str()) != Some("production") {
        return String::new();
    }
    let v = version.unwrap_or("").trim();
    if !RELEASE_VERSION_RE.is_match(v) {
        return String::new();
    }
    format!("v{v}")
}

fn build_dispatch_body(
    platform: &str,
    ref_: Option<&str>,
    version: Option<&str>,
    branding: &Value,
    request_id: &str,
) -> (String, Value) {
    let workflow = workflow_for(platform).to_string();
    let tag = release_tag_for(version, branding);
    let mut body = json!({ "ref": ref_.unwrap_or("main") });
    if platform == "android" || platform == "ios" {
        let environment = branding
            .get("environment")
            .and_then(|v| v.as_str())
            .unwrap_or("preview");
        body["inputs"] = json!({
            "platform": platform,
            "profile": profile_for(environment),
            "app_name": branding.get("app_name").and_then(|v| v.as_str()).unwrap_or(""),
            "package_id": branding.get("package_id").and_then(|v| v.as_str()).unwrap_or(""),
            "app_version": version.unwrap_or(""),
            "version_code": branding.get("version_code").map(|v| v.to_string()).unwrap_or_default(),
            "api_url": branding.get("api_url").and_then(|v| v.as_str()).unwrap_or(""),
            "environment": environment,
            "request_id": request_id,
            "release_tag": tag,
        });
    } else if platform == "desktop" && !tag.is_empty() {
        body["inputs"] = json!({ "release_tag": tag });
    }
    (workflow, body)
}

fn map_run_status(run: &Value) -> Option<&'static str> {
    match run.get("status").and_then(|v| v.as_str()) {
        Some("queued") => Some("queued"),
        Some("in_progress") | Some("waiting") | Some("pending") | Some("requested") => {
            Some("running")
        }
        Some("completed") => {
            if run.get("conclusion").and_then(|v| v.as_str()) == Some("success") {
                Some("completed")
            } else {
                Some("failed")
            }
        }
        _ => None,
    }
}

fn pick_run_for_request(
    runs: &[Value],
    dispatched_at: DateTime<Utc>,
    claimed_run_ids: &HashSet<String>,
) -> Option<Value> {
    let threshold = dispatched_at - chrono::Duration::seconds(60);
    let mut candidates: Vec<&Value> = Vec::new();
    for run in runs {
        let run_id = run.get("id").map(|v| v.to_string()).unwrap_or_default();
        if run_id.is_empty() || claimed_run_ids.contains(&run_id) {
            continue;
        }
        let Some(created) = run.get("created_at").and_then(|v| v.as_str()) else {
            continue;
        };
        let Ok(created_dt) = DateTime::parse_from_rfc3339(created) else {
            continue;
        };
        if created_dt.with_timezone(&Utc) >= threshold {
            candidates.push(run);
        }
    }
    candidates.sort_by_key(|r| {
        r.get("created_at")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    });
    candidates.into_iter().next().cloned()
}

struct BuildReleaseRow {
    id: Uuid,
    platform: String,
    version: String,
    build_number: Option<String>,
    download_url: Option<String>,
    download_expires_at: Option<DateTime<Utc>>,
    artifact_key: Option<String>,
    file_size_bytes: Option<i64>,
    sha256: Option<String>,
    signed: bool,
    build_metadata: Value,
    created_at: DateTime<Utc>,
}

fn release_row_from(row: &DynRow) -> Result<BuildReleaseRow, ApiError> {
    Ok(BuildReleaseRow {
        id: row.try_get("id")?,
        platform: row.try_get("platform")?,
        version: row.try_get("version")?,
        build_number: row.try_get("build_number")?,
        download_url: row.try_get("download_url")?,
        download_expires_at: row.try_get("download_expires_at")?,
        artifact_key: row.try_get("artifact_key")?,
        file_size_bytes: row.try_get("file_size_bytes")?,
        sha256: row.try_get("sha256")?,
        signed: row.try_get("signed")?,
        build_metadata: row.try_get("build_metadata")?,
        created_at: row.try_get("created_at")?,
    })
}

/// `_release_out` — for a real file (`artifact_key`) Python mints a
/// fresh short-lived presigned URL from the builds (MinIO/S3) bucket.
/// That bucket is not in the Rust binary (ADR-015), so `download_url`
/// is honestly null while `has_file` stays true (see ADM-04).
fn release_out(r: &BuildReleaseRow) -> Value {
    let mut url: Option<String> = None;
    let mut expires: Option<String> = None;
    let mut download_expired = false;
    if r.artifact_key.is_some() {
        url = None;
        expires = Some(
            (Utc::now() + chrono::Duration::seconds(RELEASE_DOWNLOAD_TTL_SECONDS)).to_rfc3339(),
        );
    } else if r.signed {
        if let Some(durl) = &r.download_url {
            match r.download_expires_at {
                Some(exp) => {
                    expires = Some(exp.to_rfc3339());
                    if exp <= Utc::now() {
                        download_expired = true;
                    } else {
                        url = Some(durl.clone());
                    }
                }
                None => url = Some(durl.clone()),
            }
        }
    }
    json!({
        "id": r.id.to_string(),
        "platform": r.platform,
        "version": r.version,
        "build_number": r.build_number,
        "download_url": url,
        "download_expires_at": expires,
        "download_expired": download_expired,
        "has_file": r.artifact_key.is_some(),
        "artifact_key": r.artifact_key,
        "file_size_bytes": r.file_size_bytes,
        "sha256": r.sha256,
        "signed": r.signed,
        "build_metadata": r.build_metadata,
        "created_at": r.created_at.to_rfc3339(),
    })
}

struct BuildRequestRow {
    id: Uuid,
    platform: String,
    version: Option<String>,
    ref_: Option<String>,
    status: String,
    note: Option<String>,
    branding: Value,
    provider: Option<String>,
    provider_run_id: Option<String>,
    provider_run_url: Option<String>,
    artifact_name: Option<String>,
    dispatched_at: Option<DateTime<Utc>>,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
}

fn request_row_from(row: &DynRow) -> Result<BuildRequestRow, ApiError> {
    Ok(BuildRequestRow {
        id: row.try_get("id")?,
        platform: row.try_get("platform")?,
        version: row.try_get("version")?,
        ref_: row.try_get("ref")?,
        status: row.try_get("status")?,
        note: row.try_get("note")?,
        branding: row.try_get("branding")?,
        provider: row.try_get("provider")?,
        provider_run_id: row.try_get("provider_run_id")?,
        provider_run_url: row.try_get("provider_run_url")?,
        artifact_name: row.try_get("artifact_name")?,
        dispatched_at: row.try_get("dispatched_at")?,
        created_at: row.try_get("created_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

fn request_out(r: &BuildRequestRow) -> Value {
    let tag = release_tag_for(r.version.as_deref(), &r.branding);
    let release_url = if !tag.is_empty() && r.dispatched_at.is_some() {
        let repo = env_trimmed("GITHUB_REPO");
        if repo.is_empty() {
            None
        } else {
            Some(format!("https://github.com/{repo}/releases/tag/{tag}"))
        }
    } else {
        None
    };
    json!({
        "id": r.id.to_string(),
        "platform": r.platform,
        "version": r.version,
        "ref": r.ref_,
        "status": r.status,
        "note": r.note,
        "branding": r.branding,
        "provider": r.provider,
        "provider_run_id": r.provider_run_id,
        "provider_run_url": r.provider_run_url,
        "artifact_name": r.artifact_name,
        "release_url": release_url,
        "dispatched_at": iso(r.dispatched_at),
        "created_at": r.created_at.to_rfc3339(),
        "finished_at": iso(r.finished_at),
    })
}

/// `GET /api/v1/admin/builds/releases`
pub async fn list_releases(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "builds.manage").await?;
    let params = parse_query(&uri);
    let platform = params.get("platform").filter(|s| !s.is_empty()).cloned();
    if let Some(p) = &platform {
        if !PLATFORMS.contains(&p.as_str()) {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_platform",
                format!("پلتفرم نامعتبر: {p}"),
            ));
        }
    }
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let db = state.db_or_500()?;
    let mut sql = "SELECT id, platform, version, build_number, download_url, \
        download_expires_at, artifact_key, file_size_bytes, sha256, signed, \
        build_metadata, created_at FROM build_releases"
        .to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(p) = &platform {
        sql.push_str(" WHERE platform = ?");
        args.push(p.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push(limit.into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(release_out(&release_row_from(&row)?));
    }
    Ok(axum::response::Json(Value::Array(out)))
}

const CI_SIDE_PREREQS: &[(&str, &str, &str, bool)] = &[
    (
        "EXPO_TOKEN",
        "GitHub Secrets",
        "mobile.yml preflight",
        false,
    ),
    (
        "BUILDS_API_URL",
        "GitHub Secrets",
        "build callback step (B-01)",
        false,
    ),
    (
        "BUILDS_SERVICE_TOKEN (GitHub Secrets)",
        "GitHub Secrets",
        "build callback step (B-01)",
        false,
    ),
    (
        "BUILDS_S3_ACCESS_KEY / BUILDS_S3_SECRET_KEY / BUILDS_S3_BUCKET / BUILDS_S3_ENDPOINT",
        "GitHub Secrets",
        "build callback step (B-01)",
        false,
    ),
    (
        "Android keystore (EAS managed credentials)",
        "Expo / EAS",
        "mobile.yml validate step",
        false,
    ),
    (
        "Apple distribution certificate + provisioning profile",
        "Expo / EAS + App Store Connect",
        "mobile.yml validate step",
        false,
    ),
    (
        "TAURI_SIGNING_PRIVATE_KEY",
        "GitHub Secrets",
        "desktop.yml",
        false,
    ),
];

fn signing_present(signing: &Value, name: &str) -> bool {
    signing
        .get(name)
        .and_then(|v| v.get("present"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

async fn signing_status(db: &Db, config: &crate::Config) -> ApiResult<Value> {
    let mut signing = Map::new();
    for (name, key, expiry_key) in [
        (
            "apple_developer",
            "signing.apple_developer",
            "signing.apple_developer_expires_at",
        ),
        (
            "google_play",
            "signing.google_play",
            "signing.google_play_expires_at",
        ),
        (
            "cafebazaar",
            "signing.cafebazaar",
            "signing.cafebazaar_expires_at",
        ),
        (
            "windows_cert",
            "signing.windows_cert",
            "signing.windows_cert_expires_at",
        ),
        (
            "macos_cert",
            "signing.macos_cert",
            "signing.macos_cert_expires_at",
        ),
    ] {
        let present = get_setting(db, config, key)
            .await?
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let expires_at = get_setting(db, config, expiry_key).await?;
        signing.insert(
            name.to_string(),
            json!({ "present": present, "expires_at": expires_at }),
        );
    }
    Ok(Value::Object(signing))
}

/// `GET /api/v1/admin/builds/prereqs` — which signing secrets are
/// present, per-platform readiness, provider + pipeline status.
/// CI-side prereqs are honestly `api_visible: false`.
pub async fn get_prereqs(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "builds.manage").await?;
    let db = state.db_or_500()?;
    let signing = signing_status(db, &state.config).await?;
    let mut platforms = Map::new();
    for (plat, needs) in [
        ("android", vec!["google_play", "cafebazaar"]),
        ("ios", vec!["apple_developer"]),
        ("desktop", vec!["windows_cert", "macos_cert"]),
    ] {
        let missing: Vec<String> = needs
            .iter()
            .filter(|n| !signing_present(&signing, n))
            .map(|n| n.to_string())
            .collect();
        platforms.insert(
            plat.to_string(),
            json!({
                "required": needs,
                "missing": missing,
                "ready": missing.is_empty(),
            }),
        );
    }
    let provider = provider_status();
    let expo_project_id = get_setting(db, &state.config, "builds.expo_project_id")
        .await?
        .unwrap_or_default();
    let base_url = state.config.app_base_url.trim().to_string();
    let ci_side: Vec<Value> = CI_SIDE_PREREQS
        .iter()
        .map(|(key, where_, verified_by, visible)| {
            json!({ "key": key, "where": where_, "verified_by": verified_by, "api_visible": visible })
        })
        .collect();
    Ok(axum::response::Json(json!({
        "signing": signing,
        "platforms": platforms,
        "provider": provider,
        "pipeline": {
            "callback_token_configured": !env_trimmed("BUILDS_SERVICE_TOKEN").is_empty(),
            "github_dispatch_configured": provider["configured"],
            "github_repo": provider["repo"],
            "api_base_url": if base_url.is_empty() { None } else { Some(base_url.clone()) },
            "api_base_url_is_local": base_url.contains("localhost") || base_url.contains("127.0.0.1"),
            "expo_project_linked": !expo_project_id.trim().is_empty(),
            "expo_project_id": if expo_project_id.trim().is_empty() { None } else { Some(expo_project_id.trim().to_string()) },
        },
        "ci_side": ci_side,
    })))
}

/// Best-effort: find the workflow run created for a dispatched
/// request and apply it (status / run id / url). Any failure is
/// ignored — the ARQ sync cron re-tries on the next cycle.
async fn match_and_apply_run(state: &AppState, db: &Db, req: &BuildRequestRow) -> bool {
    let token = env_trimmed("GITHUB_DISPATCH_TOKEN");
    let repo = env_trimmed("GITHUB_REPO");
    let Some(dispatched_at) = req.dispatched_at else {
        return false;
    };
    if token.is_empty() || repo.is_empty() {
        return false;
    }
    let workflow = workflow_for(&req.platform);
    let url = format!("{GITHUB_API_BASE}/repos/{repo}/actions/workflows/{workflow}/runs?event=workflow_dispatch&per_page=20");
    let resp = match state
        .client
        .get(&url)
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "maturidi-academy-builds")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    if !resp.status().is_success() {
        return false;
    }
    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return false,
    };
    let runs = data
        .get("workflow_runs")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let claimed_rows = db
        .fetch_all(
            "SELECT provider_run_id FROM build_requests WHERE provider_run_id IS NOT NULL AND id != ?",
            &[req.id.into_arg()],
        )
        .await;
    let mut claimed = HashSet::new();
    if let Ok(rows) = claimed_rows {
        for row in rows {
            let v: Result<Option<String>, _> = row.try_get("provider_run_id");
            if let Ok(Some(id)) = v {
                claimed.insert(id);
            }
        }
    }
    let Some(run) = pick_run_for_request(&runs, dispatched_at, &claimed) else {
        return false;
    };
    apply_run(db, req, &run).await
}

async fn apply_run(db: &Db, req: &BuildRequestRow, run: &Value) -> bool {
    let mut changed = false;
    let run_id = run.get("id").map(|v| v.to_string()).unwrap_or_default();
    let mut new_run_id = req.provider_run_id.clone();
    let mut new_run_url = req.provider_run_url.clone();
    let mut new_status = req.status.clone();
    let mut new_finished = req.finished_at;
    if !run_id.is_empty() && req.provider_run_id.as_deref() != Some(run_id.as_str()) {
        new_run_id = Some(run_id);
        changed = true;
    }
    if let Some(html_url) = run.get("html_url").and_then(|v| v.as_str()) {
        if req.provider_run_url.as_deref() != Some(html_url) {
            new_run_url = Some(html_url.to_string());
            changed = true;
        }
    }
    if let Some(mapped) = map_run_status(run) {
        if mapped != req.status {
            new_status = mapped.to_string();
            changed = true;
        }
    }
    if (new_status == "completed" || new_status == "failed") && new_finished.is_none() {
        new_finished = Some(Utc::now());
        changed = true;
    }
    if changed {
        let _ = db
            .execute(
                "UPDATE build_requests SET provider_run_id = ?, provider_run_url = ?, \
                 status = ?, finished_at = ? WHERE id = ?",
                &[
                    new_run_id.into_arg(),
                    new_run_url.into_arg(),
                    new_status.into_arg(),
                    new_finished.into_arg(),
                    req.id.into_arg(),
                ],
            )
            .await;
    }
    changed
}

/// `POST /api/v1/admin/builds/rebuild` — create the request row and
/// dispatch a real `workflow_dispatch` to GitHub Actions. Without a
/// configured provider: 503 `build_provider_not_configured` and no
/// row is created. A failed dispatch leaves the row `failed`, never
/// queued forever, and answers 502.
pub async fn post_rebuild(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "builds.manage").await?;
    let parsed = parse_body(&body)?;
    let platform = parsed
        .get("platform")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| validation_err("platform", "field is required"))?;
    if !PLATFORMS.contains(&platform.as_str()) {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_platform",
            format!("پلتفرم نامعتبر: {platform}"),
        ));
    }
    if !provider_status()["configured"].as_bool().unwrap_or(false) {
        return Err(bad(
            StatusCode::SERVICE_UNAVAILABLE,
            "build_provider_not_configured",
            "ارائه‌دهندهٔ بیلد پیکربندی نشده است: GITHUB_DISPATCH_TOKEN و GITHUB_REPO را در محیط API تنظیم کنید (docs/ops/APK_BUILD_GUIDE.md).",
        ));
    }
    let branding = validate_branding(&parsed)?;
    let branding_json = Value::Object(branding);
    let db = state.db_or_500()?;
    let signing = signing_status(db, &state.config).await?;
    let (ready, missing): (bool, Vec<String>) = match platform.as_str() {
        "android" => {
            let m: Vec<String> = ["google_play", "cafebazaar"]
                .iter()
                .filter(|n| !signing_present(&signing, n))
                .map(|s| s.to_string())
                .collect();
            (m.is_empty(), m)
        }
        "ios" => {
            let m: Vec<String> = ["apple_developer"]
                .iter()
                .filter(|n| !signing_present(&signing, n))
                .map(|s| s.to_string())
                .collect();
            (m.is_empty(), m)
        }
        _ => {
            let m: Vec<String> = ["windows_cert", "macos_cert"]
                .iter()
                .filter(|n| !signing_present(&signing, n))
                .map(|s| s.to_string())
                .collect();
            (m.is_empty(), m)
        }
    };
    let version = parsed
        .get("version")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let ref_ = parsed
        .get("ref")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let note = parsed
        .get("note")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let now = utc_now();
    let req_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO build_requests (id, platform, version, ref, status, note, requested_by, \
         branding, provider, provider_run_id, provider_run_url, artifact_name, \
         dispatched_at, created_at, finished_at) \
         VALUES (?, ?, ?, ?, 'queued', ?, ?, ?, 'github_actions', NULL, NULL, NULL, NULL, ?, NULL)",
        &[
            req_id.into_arg(),
            platform.clone().into_arg(),
            version.clone().into_arg(),
            ref_.clone().into_arg(),
            note.clone().into_arg(),
            ctx.user.id.into_arg(),
            branding_json.clone().into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let (workflow, dispatch_body) = build_dispatch_body(
        &platform,
        ref_.as_deref(),
        version.as_deref(),
        &branding_json,
        &req_id.to_string(),
    );
    let repo = env_trimmed("GITHUB_REPO");
    let token = env_trimmed("GITHUB_DISPATCH_TOKEN");
    let dispatch_url =
        format!("{GITHUB_API_BASE}/repos/{repo}/actions/workflows/{workflow}/dispatches");
    let (ip, ua) = client_info(&headers, peer.0, &state);
    let dispatch = state
        .client
        .post(&dispatch_url)
        .bearer_auth(&token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "maturidi-academy-builds")
        .json(&dispatch_body)
        .send()
        .await;
    let dispatch_failed: Option<ApiError> = match dispatch {
        Ok(resp) if resp.status().as_u16() == 204 => None,
        Ok(resp) => Some(ApiError::with_details(
            StatusCode::BAD_GATEWAY,
            "build_dispatch_failed",
            format!(
                "GitHub dispatch ناموفق بود (HTTP {}) — وضعیت workflow و دسترسی توکن را بررسی کنید.",
                resp.status().as_u16()
            ),
            json!({ "request_id": req_id.to_string(), "github_status": resp.status().as_u16() }),
        )),
        Err(_) => Some(ApiError::with_details(
            StatusCode::BAD_GATEWAY,
            "build_dispatch_failed",
            "خطای شبکه در dispatch به GitHub — وضعیت اتصال و توکن را بررسی کنید.",
            json!({ "request_id": req_id.to_string() }),
        )),
    };
    if let Some(err) = dispatch_failed {
        let finished = utc_now();
        db.execute(
            "UPDATE build_requests SET status = 'failed', finished_at = ? WHERE id = ?",
            &[finished.into_arg(), req_id.into_arg()],
        )
        .await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "admin",
            "builds.rebuild.dispatch_failed",
            "build_request",
            Some(&req_id.to_string()),
            None,
            Some(json!({ "platform": platform, "workflow": workflow, "error": err.code })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
        return Err(err);
    }
    let dispatched_at = utc_now();
    db.execute(
        "UPDATE build_requests SET dispatched_at = ? WHERE id = ?",
        &[dispatched_at.into_arg(), req_id.into_arg()],
    )
    .await?;
    let row = db
        .fetch_one(
            "SELECT id, platform, version, ref, status, note, branding, provider, \
             provider_run_id, provider_run_url, artifact_name, dispatched_at, created_at, \
             finished_at FROM build_requests WHERE id = ?",
            &[req_id.into_arg()],
        )
        .await?;
    let req_row = request_row_from(&row)?;
    let _ = match_and_apply_run(&state, db, &req_row).await;
    let row = db
        .fetch_one(
            "SELECT id, platform, version, ref, status, note, branding, provider, \
             provider_run_id, provider_run_url, artifact_name, dispatched_at, created_at, \
             finished_at FROM build_requests WHERE id = ?",
            &[req_id.into_arg()],
        )
        .await?;
    let req_row = request_row_from(&row)?;
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "builds.rebuild.request",
        "build_request",
        Some(&req_id.to_string()),
        None,
        Some(json!({
            "platform": platform, "version": version, "ref": ref_,
            "provider": PROVIDER_GITHUB, "workflow": workflow,
            "branding": branding_json,
            "provider_run_id": req_row.provider_run_id,
            "signing_ready": ready,
            "missing_signing": missing,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let mut out = request_out(&req_row);
    out["signing_ready"] = json!(ready);
    out["missing_signing"] = json!(missing);
    out["note"] = json!("درخواست به GitHub Actions ارسال شد — وضعیت از روی اجرای واقعی workflow همگام می‌شود و لینک اجرا در جدول درخواست‌ها می‌آید.");
    Ok(axum::response::Json(out))
}

/// `GET /api/v1/admin/builds/requests`
pub async fn list_requests(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "builds.manage").await?;
    let params = parse_query(&uri);
    let platform = params.get("platform").filter(|s| !s.is_empty()).cloned();
    if let Some(p) = &platform {
        if !PLATFORMS.contains(&p.as_str()) {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_platform",
                format!("پلتفرم نامعتبر: {p}"),
            ));
        }
    }
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let db = state.db_or_500()?;
    let mut sql = "SELECT id, platform, version, ref, status, note, branding, provider, \
        provider_run_id, provider_run_url, artifact_name, dispatched_at, created_at, \
        finished_at FROM build_requests"
        .to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(p) = &platform {
        sql.push_str(" WHERE platform = ?");
        args.push(p.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push(limit.into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(request_out(&request_row_from(&row)?));
    }
    Ok(axum::response::Json(Value::Array(out)))
}

/// Retention (FIX2-11): only the newest `keep` release *groups*
/// (`(version, build_number)`) per platform survive; the rest are
/// deleted. File deletion from the builds bucket is best-effort in
/// Python and impossible here (no builds-bucket client in Rust) —
/// the rows are deleted and the orphaned objects are left for the
/// bucket's own lifecycle (see ADM-04).
async fn prune_releases(db: &Db, platform: &str) -> ApiResult<usize> {
    let rows = db
        .fetch_all(
            "SELECT id, version, build_number, created_at FROM build_releases \
             WHERE platform = ? ORDER BY created_at DESC, id DESC",
            &[platform.into_arg()],
        )
        .await?;
    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    let mut excess: Vec<Uuid> = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let version: String = row.try_get("version")?;
        let build_number: Option<String> = row.try_get("build_number")?;
        let group = (version, build_number);
        if !seen.contains(&group) {
            seen.push(group);
        }
        if seen.len() > RELEASE_RETENTION_PER_PLATFORM {
            excess.push(id);
        }
    }
    for id in &excess {
        db.execute(
            "DELETE FROM build_releases WHERE id = ?",
            &[(*id).into_arg()],
        )
        .await?;
    }
    Ok(excess.len())
}

/// `POST /api/v1/admin/builds/releases` (201) — register a published
/// build, either by a user with `builds.manage` or by the CI service
/// token (B-01). An `artifact_key` replay with identical metadata
/// returns the existing release; conflicting metadata → 409.
pub async fn create_release(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, ApiJson)> {
    let actor: Option<AuthCtx> = if service_token_ok(
        headers
            .get("x-build-service-token")
            .and_then(|v| v.to_str().ok()),
    ) {
        None
    } else {
        let ctx = authenticate(&state, &headers, "/api/v1/admin/builds").await?;
        require_permission(&state, &ctx, "builds.manage").await?;
        Some(ctx)
    };
    let parsed = parse_body(&body)?;
    let platform = parsed
        .get("platform")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| validation_err("platform", "field is required"))?;
    if !PLATFORMS.contains(&platform.as_str()) {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_platform",
            format!("پلتفرم نامعتبر: {platform}"),
        ));
    }
    let version = match parsed.get("version") {
        None => return Err(validation_err("version", "field is required")),
        Some(v) => v.as_str().unwrap_or("").to_string(),
    };
    if version.is_empty() {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_version",
            "version الزامی است",
        ));
    }
    let build_number = parsed
        .get("build_number")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let signed = parsed
        .get("signed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let artifact_key = validate_artifact_key(
        parsed
            .get("artifact_key")
            .and_then(|v| v.as_str())
            .map(str::to_string),
    )?;
    let download_url = if signed {
        validate_download_url(
            &state,
            parsed
                .get("download_url")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        )?
    } else {
        None
    };
    let download_expires_at = parsed
        .get("download_expires_at")
        .and_then(|v| v.as_str())
        .and_then(parse_datetime_str);
    let sha256 = parsed
        .get("sha256")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase());
    if let Some(s) = &sha256 {
        if !SHA256_RE.is_match(s) {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_sha256",
                "sha256 باید ۶۴ نویسهٔ hex باشد",
            ));
        }
    }
    let file_size_bytes = parsed.get("file_size_bytes").and_then(|v| v.as_i64());
    if let Some(n) = file_size_bytes {
        if n < 0 {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_file_size",
                "file_size_bytes نمی‌تواند منفی باشد",
            ));
        }
    }
    let build_metadata = parsed.get("build_metadata").cloned().unwrap_or(json!({}));
    let db = state.db_or_500()?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    if let Some(key) = &artifact_key {
        if let Some(existing) = db
            .fetch_optional(
                "SELECT id, platform, version, build_number, download_url, \
                 download_expires_at, artifact_key, file_size_bytes, sha256, signed, \
                 build_metadata, created_at FROM build_releases WHERE artifact_key = ?",
                &[key.into_arg()],
            )
            .await?
        {
            let row = release_row_from(&existing)?;
            let mut diffs = Vec::new();
            if row.platform != platform {
                diffs.push("platform");
            }
            if row.version != version {
                diffs.push("version");
            }
            if row.build_number != build_number {
                diffs.push("build_number");
            }
            if row.sha256 != sha256 {
                diffs.push("sha256");
            }
            if row.file_size_bytes != file_size_bytes {
                diffs.push("file_size_bytes");
            }
            if row.signed != signed {
                diffs.push("signed");
            }
            if !diffs.is_empty() {
                return Err(ApiError::with_details(
                    StatusCode::CONFLICT,
                    "release_conflict",
                    "artifact_key تکراری با متادیتای متفاوت ثبت شده است",
                    json!({
                        "artifact_key": key,
                        "existing_id": row.id.to_string(),
                        "differing_fields": diffs,
                    }),
                ));
            }
            return Ok((StatusCode::CREATED, axum::response::Json(release_out(&row))));
        }
    }
    let rel_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO build_releases (id, platform, version, build_number, download_url, \
         download_expires_at, artifact_key, file_size_bytes, sha256, signed, \
         build_metadata, released_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            rel_id.into_arg(),
            platform.clone().into_arg(),
            version.clone().into_arg(),
            build_number.clone().into_arg(),
            download_url.into_arg(),
            download_expires_at.into_arg(),
            artifact_key.clone().into_arg(),
            file_size_bytes.into_arg(),
            sha256.clone().into_arg(),
            signed.into_arg(),
            build_metadata.into_arg(),
            actor.as_ref().map(|c| c.user.id).into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        actor.as_ref().map(|c| c.user.id),
        if actor.is_some() { "admin" } else { "service" },
        "builds.release.create",
        "build_release",
        Some(&rel_id.to_string()),
        None,
        Some(json!({
            "platform": platform, "version": version,
            "build_number": build_number, "signed": signed,
            "has_file": artifact_key.is_some(),
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let _ = prune_releases(db, &platform).await?;
    let row = db
        .fetch_one(
            "SELECT id, platform, version, build_number, download_url, \
             download_expires_at, artifact_key, file_size_bytes, sha256, signed, \
             build_metadata, created_at FROM build_releases WHERE id = ?",
            &[rel_id.into_arg()],
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        axum::response::Json(release_out(&release_row_from(&row)?)),
    ))
}

// ===========================================================================
// Connections — `connections_router.py` / `connections_service.py`
// ===========================================================================

const CONNECTION_SERVICES: &[&str] = &["ai", "sms", "telegram", "payment", "maps", "signing"];

fn channel_configured(key: &str) -> bool {
    match key {
        // Resend counts as configured too: a Resend-only deploy
        // sends fine while SMTP_HOST is empty (ADR-158).
        "email" => {
            !env_trimmed("RESEND_API_KEY").is_empty() || !env_trimmed("SMTP_HOST").is_empty()
        }
        "sms" => !env_trimmed("SMS_API_KEY").is_empty(),
        "push" => false,
        "in_app" => true,
        _ => false,
    }
}

/// `GET /api/v1/admin/connections/status` — live status of every
/// external dependency. No secret is ever returned (§14).
pub async fn get_connections_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "connections.test").await?;
    let db = state.db_or_500()?;
    let ai_key = get_setting(db, &state.config, "ai.claude_api_key").await?;
    let ai_cfgs = db
        .fetch_all(
            "SELECT provider, model, is_fallback FROM ai_provider_configs WHERE enabled = TRUE",
            &[],
        )
        .await?;
    let mut providers = Vec::new();
    for row in ai_cfgs {
        let provider: String = row.try_get("provider")?;
        let model: String = row.try_get("model")?;
        let is_fallback: bool = row.try_get("is_fallback")?;
        providers.push(json!({ "provider": provider, "model": model, "is_fallback": is_fallback }));
    }
    let ai_model = match get_setting(db, &state.config, "ai.claude_model").await? {
        Some(m) => Some(m),
        None => get_setting(db, &state.config, "ai.model").await?,
    };
    let embedding_provider =
        get_typed_setting(db, &state.config, "ai.embedding_provider", json!("hash"))
            .await?
            .as_str()
            .unwrap_or("hash")
            .to_string();
    let tg_session = get_setting(db, &state.config, "telegram.session").await?;
    let tg_bot_token = get_setting(db, &state.config, "telegram.bot_token").await?;
    let tg_phone_masked = get_setting(db, &state.config, "telegram.phone_masked").await?;
    let tg_channel = get_setting(db, &state.config, "telegram.channel").await?;
    let neshan_key = get_setting(db, &state.config, "maps.neshan_api_key").await?;
    let mut signing = Map::new();
    for (name, key, expiry_key) in [
        (
            "apple_developer",
            "signing.apple_developer",
            "signing.apple_developer_expires_at",
        ),
        (
            "google_play",
            "signing.google_play",
            "signing.google_play_expires_at",
        ),
        (
            "cafebazaar",
            "signing.cafebazaar",
            "signing.cafebazaar_expires_at",
        ),
        (
            "windows_cert",
            "signing.windows_cert",
            "signing.windows_cert_expires_at",
        ),
        (
            "macos_cert",
            "signing.macos_cert",
            "signing.macos_cert_expires_at",
        ),
    ] {
        let uploaded = get_setting(db, &state.config, key)
            .await?
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let expires_at = get_setting(db, &state.config, expiry_key).await?;
        signing.insert(
            name.to_string(),
            json!({ "uploaded": uploaded, "expires_at": expires_at }),
        );
    }
    let sms_key = env_trimmed("SMS_API_KEY");
    let sms_provider = env_trimmed("SMS_PROVIDER");
    Ok(axum::response::Json(json!({
        "ai": {
            "configured": ai_key.map(|k| !k.trim().is_empty()).unwrap_or(false),
            "providers": providers,
            "model": ai_model,
        },
        "embeddings": {
            "provider": embedding_provider,
            "semantic_enabled": false,
            "reason": "hash_only_no_semantic_search",
        },
        "sms": {
            "configured": !sms_key.is_empty(),
            "provider": if !sms_provider.is_empty() { Some(sms_provider) } else if !sms_key.is_empty() { Some("kavenegar".to_string()) } else { None },
        },
        "telegram": {
            "connected": tg_session.map(|s| !s.trim().is_empty()).unwrap_or(false),
            "account": tg_phone_masked,
            "bot_configured": tg_bot_token.map(|s| !s.trim().is_empty()).unwrap_or(false),
            "channel": tg_channel,
        },
        "payment": {
            "configured": false,
            "note": "درگاه پرداخت پیکربندی نشده است",
        },
        "maps": {
            "osm": { "available": true, "note": "بدون کلید — Nominatim" },
            "neshan": { "configured": neshan_key.map(|k| !k.trim().is_empty()).unwrap_or(false) },
        },
        "signing": signing,
        "checked_at": Utc::now().to_rfc3339(),
    })))
}

async fn test_ai(state: &AppState, db: &Db) -> ApiResult<Value> {
    // The LLM provider lives behind `POST /api/v1/fatwa/ask`, which
    // is a sidecar-contract route since the Wave 8 lift (ADR-134);
    // there is no LLM client in the Rust binary to ping with. Report
    // honestly instead of faking a probe (see ADM-05).
    let ai_key = get_setting(db, &state.config, "ai.claude_api_key").await?;
    let cfg_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM ai_provider_configs WHERE enabled = TRUE",
            &[],
        )
        .await?
        .try_get("c")?;
    if ai_key.map(|k| !k.trim().is_empty()).unwrap_or(false) || cfg_count > 0 {
        return Ok(json!({
            "ok": false,
            "error": "ai_ping_failed",
            "detail": "فراخوانی provider هوش مصنوعی در نسخهٔ Rust پیاده نشده است (ask از طریق origin پاسخ داده می‌شود)",
        }));
    }
    Ok(json!({
        "ok": false,
        "error": "ai_not_configured",
        "detail": "هیچ provider فعالی پیکربندی نشده است",
    }))
}

async fn test_sms(state: &AppState, params: &Value) -> ApiResult<Value> {
    let phone = params
        .get("phone")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if phone.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_phone",
            "شمارهٔ مقصد (phone) الزامی است",
        ));
    }
    if env_trimmed("SMS_API_KEY").is_empty() {
        return Ok(json!({
            "ok": false,
            "error": "sms_not_configured",
            "detail": "کلید پیامک پیکربندی نشده است",
        }));
    }
    // The connection test sends a real SMS and reports the real
    // outcome — a failed send is `ok: false`, never a faked success
    // (ADR-158).
    match send_sms(
        &state.config,
        &phone,
        "تست اتصال پیامک آکادمی ماتریدی — این پیام آزمایشی است.",
    )
    .await
    {
        Ok(()) => Ok(json!({ "ok": true, "to": phone })),
        Err(err) => Ok(json!({
            "ok": false,
            "error": "sms_delivery_failed",
            "detail": crate::mail::redact_secrets(&err.to_string()),
        })),
    }
}

async fn test_telegram(state: &AppState, db: &Db, params: &Value) -> ApiResult<Value> {
    let _ = state;
    let chat_id = params
        .get("chat_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let chat_id = match chat_id {
        Some(c) => Some(c),
        None => get_setting(db, &state.config, "telegram.channel").await?,
    };
    let Some(chat_id) = chat_id.filter(|c| !c.trim().is_empty()) else {
        return Ok(json!({
            "ok": false,
            "error": "telegram_no_target",
            "detail": "کانال مقصد پیکربندی نشده است",
        }));
    };
    // No Telegram bot client is wired in the Rust binary (Wave 7
    // telegram work is a separate worker); report honestly.
    Ok(json!({
        "ok": false,
        "error": "telegram_send_failed",
        "detail": "بات غیرفعال است یا ارسال ناموفق بود",
        "chat_id": chat_id,
    }))
}

async fn test_maps(state: &AppState, db: &Db) -> ApiResult<Value> {
    let neshan_key = get_setting(db, &state.config, "maps.neshan_api_key").await?;
    let result = if let Some(key) = neshan_key.filter(|k| !k.trim().is_empty()) {
        let resp = state
            .client
            .get("https://api.neshan.org/v1/search")
            .query(&[("term", "تهران"), ("lat", "35.7"), ("lng", "51.4")])
            .header("Api-Key", key.trim())
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                json!({ "ok": status < 400, "provider": "neshan", "http_status": status })
            }
            Err(e) => {
                let detail: String = e.to_string().chars().take(300).collect();
                json!({ "ok": false, "error": "maps_ping_failed", "detail": detail })
            }
        }
    } else {
        let resp = state
            .client
            .get("https://nominatim.openstreetmap.org/search")
            .query(&[("q", "Tehran"), ("format", "json"), ("limit", "1")])
            .header("User-Agent", "maturidi-academy/1.0")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        match resp {
            Ok(r) => {
                let status = r.status().as_u16();
                json!({ "ok": status < 400, "provider": "osm", "http_status": status })
            }
            Err(e) => {
                let detail = e.to_string();
                let detail = detail.chars().take(300).collect::<String>();
                json!({ "ok": false, "error": "maps_ping_failed", "detail": detail })
            }
        }
    };
    Ok(result)
}

/// `POST /api/v1/admin/connections/test/{service}` — the result is
/// audited (never with a secret or a full destination number).
pub async fn test_connection(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(service): axum::extract::Path<String>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "connections.test").await?;
    if !CONNECTION_SERVICES.contains(&service.as_str()) {
        return Err(bad(
            StatusCode::NOT_FOUND,
            "unknown_service",
            format!("سرویس ناشناخته: {service}"),
        ));
    }
    let parsed = if body.is_empty() {
        json!({})
    } else {
        parse_body(&body)?
    };
    let db = state.db_or_500()?;
    let started = std::time::Instant::now();
    let mut result = match service.as_str() {
        "ai" => test_ai(&state, db).await?,
        "sms" => test_sms(&state, &parsed).await?,
        "telegram" => test_telegram(&state, db, &parsed).await?,
        "payment" => json!({
            "ok": false,
            "error": "payment_not_available",
            "detail": "درگاه پرداخت پیکربندی نشده است",
        }),
        "maps" => test_maps(&state, db).await?,
        "signing" => {
            let status = get_connections_status_inner(&state, db).await?;
            let signing = &status["signing"];
            let missing: Vec<String> = signing
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(_, v)| v.get("uploaded").and_then(|u| u.as_bool()) != Some(true))
                        .map(|(k, _)| k.clone())
                        .collect()
                })
                .unwrap_or_default();
            let uploaded: Vec<String> = signing
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(_, v)| v.get("uploaded").and_then(|u| u.as_bool()) == Some(true))
                        .map(|(k, _)| k.clone())
                        .collect()
                })
                .unwrap_or_default();
            json!({ "ok": missing.is_empty(), "uploaded": uploaded, "missing": missing })
        }
        _ => unreachable!(),
    };
    let latency_ms = started.elapsed().as_millis() as i64;
    result["service"] = json!(service);
    result["latency_ms"] = json!(latency_ms);
    result["tested_at"] = json!(Utc::now().to_rfc3339());
    let mut safe_params = Map::new();
    if let Some(obj) = parsed.as_object() {
        for (k, v) in obj {
            if k != "phone" {
                safe_params.insert(k.clone(), v.clone());
            }
        }
    }
    if let Some(phone) = parsed.get("phone").and_then(|v| v.as_str()) {
        let tail: String = phone
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        safe_params.insert("phone_masked".to_string(), json!(format!("****{tail}")));
    }
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        &format!("connections.test.{service}"),
        "external_connection",
        Some(&service),
        None,
        Some(json!({
            "ok": result.get("ok"),
            "error": result.get("error"),
            "latency_ms": latency_ms,
            "params": safe_params,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(result))
}

/// The signing section of `get_status`, shared with the signing test
/// (avoids re-running the whole status assembly twice).
async fn get_connections_status_inner(state: &AppState, db: &Db) -> ApiResult<Value> {
    let mut signing = Map::new();
    for (name, key, expiry_key) in [
        (
            "apple_developer",
            "signing.apple_developer",
            "signing.apple_developer_expires_at",
        ),
        (
            "google_play",
            "signing.google_play",
            "signing.google_play_expires_at",
        ),
        (
            "cafebazaar",
            "signing.cafebazaar",
            "signing.cafebazaar_expires_at",
        ),
        (
            "windows_cert",
            "signing.windows_cert",
            "signing.windows_cert_expires_at",
        ),
        (
            "macos_cert",
            "signing.macos_cert",
            "signing.macos_cert_expires_at",
        ),
    ] {
        let uploaded = get_setting(db, &state.config, key)
            .await?
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let expires_at = get_setting(db, &state.config, expiry_key).await?;
        signing.insert(
            name.to_string(),
            json!({ "uploaded": uploaded, "expires_at": expires_at }),
        );
    }
    Ok(json!({ "signing": signing }))
}

// ===========================================================================
// Notifications — `notifications_router.py` / `notify_service.py`
// ===========================================================================

const CHANNELS: &[&str] = &["email", "sms", "push", "in_app"];

fn default_channel_enabled(channel: &str) -> bool {
    channel == "in_app"
}

/// Effective channels = requested ∩ the user's active preferences
/// (`notif_type="product"`). A missing row means disabled, except
/// `in_app`, which defaults to enabled.
async fn effective_channels(
    db: &Db,
    user_id: Uuid,
    requested: &[String],
) -> ApiResult<Vec<String>> {
    let rows = db
        .fetch_all(
            "SELECT channel, enabled FROM notification_preferences \
             WHERE user_id = ? AND notif_type = 'product'",
            &[user_id.into_arg()],
        )
        .await?;
    let mut pref: HashMap<String, bool> = HashMap::new();
    for row in rows {
        let channel: String = row.try_get("channel")?;
        let enabled: bool = row.try_get("enabled")?;
        pref.insert(channel, enabled);
    }
    let mut out = Vec::new();
    for ch in requested {
        if !CHANNELS.contains(&ch.as_str()) {
            continue;
        }
        let enabled = pref
            .get(ch)
            .copied()
            .unwrap_or_else(|| default_channel_enabled(ch));
        if enabled {
            out.push(ch.clone());
        }
    }
    Ok(out)
}

struct CohortUser {
    id: Uuid,
    email: Option<String>,
    phone: Option<String>,
}

/// `_resolve_cohort` — `all` (active users, created_at order, max
/// 10000), `roles` (by role name), `users` (by id list).
async fn resolve_cohort(db: &Db, cohort: &Value) -> ApiResult<Vec<CohortUser>> {
    let ctype = cohort.get("type").and_then(|v| v.as_str()).unwrap_or("all");
    match ctype {
        "all" => {
            let rows = db
                .fetch_all(
                    "SELECT id, email, phone FROM users WHERE is_active = TRUE \
                     ORDER BY created_at LIMIT 10000",
                    &[],
                )
                .await?;
            let mut out = Vec::new();
            for row in rows {
                out.push(CohortUser {
                    id: row.try_get("id")?,
                    email: row.try_get("email")?,
                    phone: row.try_get("phone")?,
                });
            }
            Ok(out)
        }
        "roles" => {
            let roles: Vec<String> = cohort
                .get("roles")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            if roles.is_empty() {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "bad_cohort",
                    "cohort.roles خالی است",
                ));
            }
            let placeholders = roles.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT DISTINCT u.id, u.email, u.phone FROM users u \
                 JOIN user_roles ur ON ur.user_id = u.id \
                 JOIN roles r ON r.id = ur.role_id \
                 WHERE r.name IN ({placeholders}) AND u.is_active = TRUE ORDER BY u.created_at"
            );
            let args: Vec<Arg> = roles.iter().map(|r| r.into_arg()).collect();
            let rows = db.fetch_all(&sql, &args).await?;
            let mut out = Vec::new();
            for row in rows {
                out.push(CohortUser {
                    id: row.try_get("id")?,
                    email: row.try_get("email")?,
                    phone: row.try_get("phone")?,
                });
            }
            Ok(out)
        }
        "users" => {
            let mut ids: Vec<Uuid> = Vec::new();
            for raw in cohort
                .get("user_ids")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default()
            {
                let s = raw.as_str().unwrap_or("").to_string();
                match s.parse::<Uuid>() {
                    Ok(u) => ids.push(u),
                    Err(_) => {
                        return Err(bad(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "bad_cohort",
                            format!("user_id نامعتبر: {s}"),
                        ));
                    }
                }
            }
            if ids.is_empty() {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "bad_cohort",
                    "cohort.user_ids خالی است",
                ));
            }
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT id, email, phone FROM users WHERE id IN ({placeholders}) AND is_active = TRUE"
            );
            let args: Vec<Arg> = ids.iter().map(|i| i.into_arg()).collect();
            let rows = db.fetch_all(&sql, &args).await?;
            let mut out = Vec::new();
            for row in rows {
                out.push(CohortUser {
                    id: row.try_get("id")?,
                    email: row.try_get("email")?,
                    phone: row.try_get("phone")?,
                });
            }
            Ok(out)
        }
        other => Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_cohort",
            format!("نوع cohort نامعتبر: {other}"),
        )),
    }
}

fn broadcast_out(b: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = b.try_get("id")?;
    let title: String = b.try_get("title")?;
    let body: String = b.try_get("body")?;
    let cohort: Value = b.try_get("cohort")?;
    let channels_requested: Value = b.try_get("channels_requested")?;
    let status: String = b.try_get("status")?;
    let total_recipients: i32 = b.try_get("total_recipients")?;
    let total_sent: i32 = b.try_get("total_sent")?;
    let total_skipped: i32 = b.try_get("total_skipped")?;
    let total_failed: i32 = b.try_get("total_failed")?;
    let created_at: DateTime<Utc> = b.try_get("created_at")?;
    let finished_at: Option<DateTime<Utc>> = b.try_get("finished_at")?;
    Ok(json!({
        "id": id.to_string(),
        "title": title,
        "body": body,
        "cohort": cohort,
        "channels_requested": channels_requested,
        "status": status,
        "total_recipients": total_recipients,
        "total_sent": total_sent,
        "total_skipped": total_skipped,
        "total_failed": total_failed,
        "created_at": created_at.to_rfc3339(),
        "finished_at": iso(finished_at),
    }))
}

const BROADCAST_COLUMNS: &str = "id, title, body, cohort, channels_requested, status, \
    total_recipients, total_sent, total_skipped, total_failed, created_at, finished_at";

/// `POST /api/v1/admin/notifications/broadcast` — build + immediately
/// fan out a multi-channel broadcast. An unconfigured channel is
/// honestly `skipped`, never a fake success.
pub async fn create_broadcast(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "notifications.broadcast").await?;
    let parsed = parse_body(&body)?;
    let title = parsed
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let body_text = parsed
        .get("body")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if title.trim().is_empty() || title.chars().count() > 255 || body_text.is_empty() {
        return Err(validation_err("title", "title and body are required"));
    }
    let channels: Vec<String> = parsed
        .get("channels")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .ok_or_else(|| validation_err("channels", "field is required"))?;
    if channels.is_empty() {
        return Err(validation_err(
            "channels",
            "at least one channel is required",
        ));
    }
    let bad_channels: Vec<String> = channels
        .iter()
        .filter(|c| !CHANNELS.contains(&c.as_str()))
        .cloned()
        .collect();
    if !bad_channels.is_empty() {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_channel",
            format!("کانال نامعتبر: {bad_channels:?}"),
        ));
    }
    let cohort = parsed
        .get("cohort")
        .cloned()
        .unwrap_or(json!({ "type": "all" }));
    let db = state.db_or_500()?;
    let users = resolve_cohort(db, &cohort).await?;
    if users.is_empty() {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_cohort",
            "هیچ کاربر فعالی در cohort نیست",
        ));
    }
    let broadcast_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO notification_broadcasts (id, title, body, cohort, channels_requested, \
         status, total_recipients, total_sent, total_skipped, total_failed, created_by, \
         created_at, finished_at) \
         VALUES (?, ?, ?, ?, ?, 'sending', ?, 0, 0, 0, ?, ?, NULL)",
        &[
            broadcast_id.into_arg(),
            title.clone().into_arg(),
            body_text.clone().into_arg(),
            cohort.clone().into_arg(),
            json!(channels).into_arg(),
            (users.len() as i64).into_arg(),
            ctx.user.id.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let mut sent: i64 = 0;
    let mut skipped: i64 = 0;
    let mut failed: i64 = 0;
    for user in &users {
        let eff = effective_channels(db, user.id, &channels).await?;
        for ch in &channels {
            if eff.contains(ch) {
                continue;
            }
            db.execute(
                "INSERT INTO notification_deliveries (id, broadcast_id, user_id, channel, \
                 status, skip_reason, error, sent_at, created_at) \
                 VALUES (?, ?, ?, ?, 'skipped', 'preference_disabled', NULL, NULL, ?)",
                &[
                    Uuid::new_v4().into_arg(),
                    broadcast_id.into_arg(),
                    user.id.into_arg(),
                    ch.into_arg(),
                    utc_now().into_arg(),
                ],
            )
            .await?;
            skipped += 1;
        }
        for ch in &eff {
            if !channel_configured(ch) {
                db.execute(
                    "INSERT INTO notification_deliveries (id, broadcast_id, user_id, channel, \
                     status, skip_reason, error, sent_at, created_at) \
                     VALUES (?, ?, ?, ?, 'skipped', ?, NULL, NULL, ?)",
                    &[
                        Uuid::new_v4().into_arg(),
                        broadcast_id.into_arg(),
                        user.id.into_arg(),
                        ch.into_arg(),
                        format!("{ch}_not_configured").into_arg(),
                        utc_now().into_arg(),
                    ],
                )
                .await?;
                skipped += 1;
                continue;
            }
            let (ok, error): (bool, Option<String>) = match ch.as_str() {
                "email" => match &user.email {
                    None => (false, Some("no_email_address".to_string())),
                    Some(email) => {
                        // The delivery outcome is recorded, not
                        // assumed (ADR-158): a failed send counts as
                        // failed, with the (redacted) error.
                        match send_email(&state.config, email, &title, &body_text, "").await {
                            Ok(()) => (true, None),
                            Err(err) => {
                                (false, Some(crate::mail::redact_secrets(&err.to_string())))
                            }
                        }
                    }
                },
                "sms" => match &user.phone {
                    None => (false, Some("no_phone_number".to_string())),
                    Some(phone) => {
                        let text: String =
                            format!("{title}\n{body_text}").chars().take(500).collect();
                        match send_sms(&state.config, phone, &text).await {
                            Ok(()) => (true, None),
                            Err(err) => {
                                (false, Some(crate::mail::redact_secrets(&err.to_string())))
                            }
                        }
                    }
                },
                "push" => (false, Some("push_not_configured".to_string())),
                "in_app" => {
                    db.execute(
                        "INSERT INTO notifications (id, user_id, notif_type, title, body, data, \
                         read_at, created_at) VALUES (?, ?, 'product', ?, ?, ?, NULL, ?)",
                        &[
                            Uuid::new_v4().into_arg(),
                            user.id.into_arg(),
                            json!({ "fa": title, "ar": title, "en": title }).into_arg(),
                            body_text.clone().into_arg(),
                            json!({ "source": "admin_broadcast" }).into_arg(),
                            utc_now().into_arg(),
                        ],
                    )
                    .await?;
                    (true, None)
                }
                _ => (false, Some("unknown_channel".to_string())),
            };
            if ok {
                db.execute(
                    "INSERT INTO notification_deliveries (id, broadcast_id, user_id, channel, \
                     status, skip_reason, error, sent_at, created_at) \
                     VALUES (?, ?, ?, ?, 'sent', NULL, NULL, ?, ?)",
                    &[
                        Uuid::new_v4().into_arg(),
                        broadcast_id.into_arg(),
                        user.id.into_arg(),
                        ch.into_arg(),
                        utc_now().into_arg(),
                        utc_now().into_arg(),
                    ],
                )
                .await?;
                sent += 1;
            } else {
                db.execute(
                    "INSERT INTO notification_deliveries (id, broadcast_id, user_id, channel, \
                     status, skip_reason, error, sent_at, created_at) \
                     VALUES (?, ?, ?, ?, 'failed', NULL, ?, NULL, ?)",
                    &[
                        Uuid::new_v4().into_arg(),
                        broadcast_id.into_arg(),
                        user.id.into_arg(),
                        ch.into_arg(),
                        error.into_arg(),
                        utc_now().into_arg(),
                    ],
                )
                .await?;
                failed += 1;
            }
        }
    }
    let finished_at = utc_now();
    db.execute(
        "UPDATE notification_broadcasts SET total_sent = ?, total_skipped = ?, \
         total_failed = ?, status = 'done', finished_at = ? WHERE id = ?",
        &[
            sent.into_arg(),
            skipped.into_arg(),
            failed.into_arg(),
            finished_at.into_arg(),
            broadcast_id.into_arg(),
        ],
    )
    .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "notifications.broadcast",
        "notification_broadcast",
        Some(&broadcast_id.to_string()),
        None,
        Some(json!({
            "title": title, "channels": channels, "cohort": cohort,
            "recipients": users.len(), "sent": sent,
            "skipped": skipped, "failed": failed,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    emit_event(
        db,
        "notification_broadcast",
        &broadcast_id.to_string(),
        "notification.broadcast.done",
        json!({
            "title": title, "recipients": users.len(),
            "sent": sent, "skipped": skipped, "failed": failed,
        }),
    )
    .await?;
    let row = db
        .fetch_one(
            &format!("SELECT {BROADCAST_COLUMNS} FROM notification_broadcasts WHERE id = ?"),
            &[broadcast_id.into_arg()],
        )
        .await?;
    Ok(axum::response::Json(broadcast_out(&row)?))
}

/// `GET /api/v1/admin/notifications/broadcasts`
pub async fn list_broadcasts(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "notifications.broadcast").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {BROADCAST_COLUMNS} FROM notification_broadcasts \
                 ORDER BY created_at DESC LIMIT ?"
            ),
            &[limit.into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(broadcast_out(&row)?);
    }
    Ok(axum::response::Json(Value::Array(out)))
}

/// `GET /api/v1/admin/notifications/broadcasts/{broadcast_id}` —
/// details + the per-user/per-channel delivery log.
pub async fn get_broadcast(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(broadcast_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "notifications.broadcast").await?;
    let broadcast_id = parse_path_uuid(&broadcast_id_str, "broadcast_id")?;
    let db = state.db_or_500()?;
    let Some(row) = db
        .fetch_optional(
            &format!("SELECT {BROADCAST_COLUMNS} FROM notification_broadcasts WHERE id = ?"),
            &[broadcast_id.into_arg()],
        )
        .await?
    else {
        return Err(bad(StatusCode::NOT_FOUND, "not_found", "پخش یافت نشد"));
    };
    let mut out = broadcast_out(&row)?;
    let deliveries = db
        .fetch_all(
            "SELECT user_id, channel, status, skip_reason, error, sent_at \
             FROM notification_deliveries WHERE broadcast_id = ? \
             ORDER BY created_at ASC LIMIT 2000",
            &[broadcast_id.into_arg()],
        )
        .await?;
    let mut items = Vec::new();
    for d in deliveries {
        let user_id: Uuid = d.try_get("user_id")?;
        let channel: String = d.try_get("channel")?;
        let status: String = d.try_get("status")?;
        let skip_reason: Option<String> = d.try_get("skip_reason")?;
        let error: Option<String> = d.try_get("error")?;
        let sent_at: Option<DateTime<Utc>> = d.try_get("sent_at")?;
        items.push(json!({
            "user_id": user_id.to_string(),
            "channel": channel,
            "status": status,
            "skip_reason": skip_reason,
            "error": error,
            "sent_at": iso(sent_at),
        }));
    }
    out["deliveries"] = Value::Array(items);
    Ok(axum::response::Json(out))
}

/// `GET /api/v1/admin/notifications/delivery-stats`
pub async fn get_delivery_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "notifications.broadcast").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT channel, status, COUNT(*) AS c FROM notification_deliveries \
             GROUP BY channel, status",
            &[],
        )
        .await?;
    let mut stats: Map<String, Value> = Map::new();
    for row in rows {
        let channel: String = row.try_get("channel")?;
        let status: String = row.try_get("status")?;
        let count: i64 = row.try_get("c")?;
        let entry = stats
            .entry(channel)
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(m) = entry {
            m.insert(status, json!(count));
        }
    }
    Ok(axum::response::Json(json!({ "stats": stats })))
}

/// `GET /api/v1/admin/notifications/channels`
pub async fn get_channels(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "notifications.broadcast").await?;
    let channels: Vec<Value> = CHANNELS
        .iter()
        .map(|ch| json!({ "key": ch, "configured": channel_configured(ch) }))
        .collect();
    Ok(axum::response::Json(json!({ "channels": channels })))
}

/// `POST /api/v1/admin/notifications/preview` — recipient count and
/// per-channel split, without sending anything.
pub async fn post_broadcast_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "notifications.broadcast").await?;
    let parsed = parse_body(&body)?;
    let title = parsed.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let body_text = parsed.get("body").and_then(|v| v.as_str()).unwrap_or("");
    if title.trim().is_empty() || title.chars().count() > 255 || body_text.is_empty() {
        return Err(validation_err("title", "title and body are required"));
    }
    let channels: Vec<String> = parsed
        .get("channels")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .ok_or_else(|| validation_err("channels", "field is required"))?;
    let cohort = parsed
        .get("cohort")
        .cloned()
        .unwrap_or(json!({ "type": "all" }));
    let db = state.db_or_500()?;
    let users = resolve_cohort(db, &cohort).await?;
    let mut per_channel: Map<String, Value> = Map::new();
    for user in &users {
        for ch in effective_channels(db, user.id, &channels).await? {
            let cur = per_channel.get(&ch).and_then(|v| v.as_i64()).unwrap_or(0);
            per_channel.insert(ch, json!(cur + 1));
        }
    }
    Ok(axum::response::Json(json!({
        "recipients": users.len(),
        "per_channel": per_channel,
        "channels_requested": channels,
        "dry_run": true,
    })))
}

// ===========================================================================
// Roles — `roles_router.py` / `roles_service.py`
// ===========================================================================

fn role_out(role: &DynRow, permissions: &[Value]) -> Result<Value, ApiError> {
    let id: Uuid = role.try_get("id")?;
    let name: String = role.try_get("name")?;
    let description: Option<String> = role.try_get("description")?;
    let created_at: DateTime<Utc> = role.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "name": name,
        "description": description,
        "permissions": permissions,
        "created_at": created_at.to_rfc3339(),
    }))
}

async fn role_permissions(db: &Db, role_id: Uuid) -> ApiResult<Vec<Value>> {
    let rows = db
        .fetch_all(
            "SELECT p.id, p.key, p.description FROM permissions p \
             JOIN role_permissions rp ON rp.permission_id = p.id \
             WHERE rp.role_id = ? ORDER BY p.key ASC",
            &[role_id.into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let key: String = row.try_get("key")?;
        let description: Option<String> = row.try_get("description")?;
        out.push(json!({ "id": id.to_string(), "key": key, "description": description }));
    }
    Ok(out)
}

async fn assign_permissions(db: &Db, role_id: Uuid, keys: &[String]) -> ApiResult<Vec<Value>> {
    let mut sorted: Vec<String> = keys.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut perms = Vec::new();
    for key in &sorted {
        let Some(row) = db
            .fetch_optional(
                "SELECT id, key, description FROM permissions WHERE key = ?",
                &[key.into_arg()],
            )
            .await?
        else {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_permission",
                format!("مجوز ناشناخته: {key}"),
            ));
        };
        let id: Uuid = row.try_get("id")?;
        let description: Option<String> = row.try_get("description")?;
        perms.push(json!({ "id": id.to_string(), "key": key, "description": description }));
    }
    db.execute(
        "DELETE FROM role_permissions WHERE role_id = ?",
        &[role_id.into_arg()],
    )
    .await?;
    for perm in &perms {
        let perm_id = perm["id"].as_str().unwrap().parse::<Uuid>().unwrap();
        db.execute(
            "INSERT INTO role_permissions (role_id, permission_id) VALUES (?, ?)",
            &[role_id.into_arg(), perm_id.into_arg()],
        )
        .await?;
    }
    Ok(perms)
}

/// `GET /api/v1/admin/roles` — roles by name, with permissions and
/// member counts.
pub async fn list_roles(State(state): State<AppState>, headers: HeaderMap) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "roles.manage").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, name, description, created_at FROM roles ORDER BY name ASC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let role_id: Uuid = row.try_get("id")?;
        let perms = role_permissions(db, role_id).await?;
        let member_count: i64 = db
            .fetch_one(
                "SELECT COUNT(*) AS c FROM user_roles WHERE role_id = ?",
                &[role_id.into_arg()],
            )
            .await?
            .try_get("c")?;
        let mut item = role_out(&row, &perms)?;
        item["member_count"] = json!(member_count);
        out.push(item);
    }
    Ok(axum::response::Json(Value::Array(out)))
}

/// `GET /api/v1/admin/roles/permissions`
pub async fn list_permissions(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "roles.manage").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, key, description FROM permissions ORDER BY key ASC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let key: String = row.try_get("key")?;
        let description: Option<String> = row.try_get("description")?;
        out.push(json!({ "id": id.to_string(), "key": key, "description": description }));
    }
    Ok(axum::response::Json(Value::Array(out)))
}

/// `POST /api/v1/admin/roles` (201)
pub async fn create_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, ApiJson)> {
    let ctx = authed(&state, &headers, "roles.manage").await?;
    let parsed = parse_body(&body)?;
    let name = parsed
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_role_name",
            "نام نقش نامعتبر است",
        ));
    }
    let description = parsed
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let permission_keys: Vec<String> = parsed
        .get("permission_keys")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let db = state.db_or_500()?;
    if db
        .fetch_optional(
            "SELECT id FROM roles WHERE name = ?",
            &[name.clone().into_arg()],
        )
        .await?
        .is_some()
    {
        return Err(bad(
            StatusCode::CONFLICT,
            "role_exists",
            "نقشی با این نام وجود دارد",
        ));
    }
    // Permission keys are validated before the insert (Python
    // validates after the flush but never commits on error — same
    // observable outcome, without a half-inserted role).
    let mut sorted_keys = permission_keys.clone();
    sorted_keys.sort();
    sorted_keys.dedup();
    for key in &sorted_keys {
        if db
            .fetch_optional(
                "SELECT id FROM permissions WHERE key = ?",
                &[key.into_arg()],
            )
            .await?
            .is_none()
        {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_permission",
                format!("مجوز ناشناخته: {key}"),
            ));
        }
    }
    let role_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO roles (id, name, description, created_at) VALUES (?, ?, ?, ?)",
        &[
            role_id.into_arg(),
            name.clone().into_arg(),
            description.clone().into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let perms = assign_permissions(db, role_id, &permission_keys).await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    let perm_keys: Vec<String> = perms
        .iter()
        .filter_map(|p| p.get("key").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "roles.create",
        "role",
        Some(&role_id.to_string()),
        None,
        Some(json!({ "name": name, "permissions": perm_keys })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let row = db
        .fetch_one(
            "SELECT id, name, description, created_at FROM roles WHERE id = ?",
            &[role_id.into_arg()],
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        axum::response::Json(role_out(&row, &perms)?),
    ))
}

/// `PUT /api/v1/admin/roles/{role_id}`
pub async fn update_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(role_id_str): axum::extract::Path<String>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "roles.manage").await?;
    let role_id = parse_path_uuid(&role_id_str, "role_id")?;
    let parsed = parse_body(&body)?;
    let db = state.db_or_500()?;
    let Some(row) = db
        .fetch_optional(
            "SELECT id, name, description, created_at FROM roles WHERE id = ?",
            &[role_id.into_arg()],
        )
        .await?
    else {
        return Err(bad(StatusCode::NOT_FOUND, "not_found", "نقش یافت نشد"));
    };
    let current_name: String = row.try_get("name")?;
    let old_keys: Vec<String> = role_permissions(db, role_id)
        .await?
        .iter()
        .filter_map(|p| p.get("key").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    let mut new_name = current_name.clone();
    if let Some(name_v) = parsed.get("name").filter(|v| !v.is_null()) {
        let candidate = name_v.as_str().unwrap_or("").trim().to_string();
        if !candidate.is_empty() && candidate != current_name {
            if current_name == "super_admin" {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "protected_role",
                    "نام نقش super_admin قابل تغییر نیست",
                ));
            }
            if candidate.chars().count() > 64 {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "bad_role_name",
                    "نام نقش نامعتبر است",
                ));
            }
            if db
                .fetch_optional(
                    "SELECT id FROM roles WHERE name = ?",
                    &[candidate.clone().into_arg()],
                )
                .await?
                .is_some()
            {
                return Err(bad(
                    StatusCode::CONFLICT,
                    "role_exists",
                    "نقشی با این نام وجود دارد",
                ));
            }
            new_name = candidate;
        }
    }
    // Validate permission keys BEFORE any write: Python validates
    // after mutating the role in memory but never commits on error, so
    // a bad key must leave the row untouched here too.
    let wanted_keys: Option<Vec<String>> = parsed
        .get("permission_keys")
        .filter(|v| !v.is_null())
        .map(|keys_v| {
            keys_v
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        });
    if let Some(keys) = &wanted_keys {
        if new_name == "super_admin" && !keys.iter().any(|k| k == "roles.manage") {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "protected_role",
                "مجوزهای نقش super_admin نمی‌تواند خالی شود",
            ));
        }
        for key in keys {
            if db
                .fetch_optional(
                    "SELECT key FROM permissions WHERE key = ?",
                    &[key.clone().into_arg()],
                )
                .await?
                .is_none()
            {
                return Err(bad(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "unknown_permission",
                    format!("مجوز ناشناخته: {key}"),
                ));
            }
        }
    }
    let mut new_description: Option<String> = row.try_get("description")?;
    if let Some(desc) = parsed.get("description").filter(|v| !v.is_null()) {
        new_description = desc.as_str().map(str::to_string);
    }
    db.execute(
        "UPDATE roles SET name = ?, description = ? WHERE id = ?",
        &[
            new_name.clone().into_arg(),
            new_description.clone().into_arg(),
            role_id.into_arg(),
        ],
    )
    .await?;
    let mut perms = role_permissions(db, role_id).await?;
    if let Some(keys) = &wanted_keys {
        perms = assign_permissions(db, role_id, keys).await?;
    }
    let new_keys: Vec<String> = perms
        .iter()
        .filter_map(|p| p.get("key").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "roles.update",
        "role",
        Some(&role_id.to_string()),
        // Python logs the *mutated* name in old_values too (the
        // rename happens before the audit call) — mirrored here.
        Some(json!({ "name": new_name, "permissions": old_keys })),
        Some(json!({ "name": new_name, "permissions": new_keys })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let row = db
        .fetch_one(
            "SELECT id, name, description, created_at FROM roles WHERE id = ?",
            &[role_id.into_arg()],
        )
        .await?;
    Ok(axum::response::Json(role_out(&row, &perms)?))
}

/// `DELETE /api/v1/admin/roles/{role_id}`
pub async fn delete_role(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(role_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "roles.manage").await?;
    let role_id = parse_path_uuid(&role_id_str, "role_id")?;
    let db = state.db_or_500()?;
    let Some(row) = db
        .fetch_optional(
            "SELECT id, name, description, created_at FROM roles WHERE id = ?",
            &[role_id.into_arg()],
        )
        .await?
    else {
        return Err(bad(StatusCode::NOT_FOUND, "not_found", "نقش یافت نشد"));
    };
    let name: String = row.try_get("name")?;
    if name == "super_admin" {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "protected_role",
            "نقش super_admin حذف‌شدنی نیست",
        ));
    }
    let members: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM user_roles WHERE role_id = ?",
            &[role_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    if members > 0 {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "role_has_members",
            "نقش عضو دارد؛ ابتدا اعضا را منتقل کنید",
        ));
    }
    let perms = role_permissions(db, role_id).await?;
    let perm_keys: Vec<String> = perms
        .iter()
        .filter_map(|p| p.get("key").and_then(|k| k.as_str()).map(str::to_string))
        .collect();
    db.execute(
        "DELETE FROM role_permissions WHERE role_id = ?",
        &[role_id.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM roles WHERE id = ?", &[role_id.into_arg()])
        .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "roles.delete",
        "role",
        Some(&role_id.to_string()),
        Some(json!({ "name": name, "permissions": perm_keys })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(json!({ "message": "نقش حذف شد" })))
}

// ===========================================================================
// Users dashboard + 360° overview + user actions
// `users_admin_router.py` / `users_dashboard_service.py` /
// `users360_service.py`
// ===========================================================================

fn in_placeholders(n: usize) -> String {
    (0..n).map(|_| "?").collect::<Vec<_>>().join(",")
}

async fn count_by_user(
    db: &Db,
    table: &str,
    column: &str,
    ids: &[Uuid],
) -> ApiResult<HashMap<Uuid, i64>> {
    let mut out: HashMap<Uuid, i64> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let sql = format!(
        "SELECT {column}, COUNT(*) AS c FROM {table} WHERE {column} IN ({}) GROUP BY {column}",
        in_placeholders(ids.len())
    );
    let args: Vec<Arg> = ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&sql, &args).await? {
        let id: Uuid = row.try_get(column)?;
        let c: i64 = row.try_get("c")?;
        out.insert(id, c);
    }
    Ok(out)
}

async fn hydrate_dashboard(db: &Db, users: Vec<UserRow>) -> ApiResult<Vec<Value>> {
    let ids: Vec<Uuid> = users.iter().map(|u| u.id).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let pending_rows = db
        .fetch_all(
            &format!(
                "SELECT DISTINCT user_id FROM password_reset_tokens \
                 WHERE user_id IN ({}) AND purpose = 'password_setup' AND used = FALSE",
                in_placeholders(ids.len())
            ),
            &ids.iter().map(|id| id.into_arg()).collect::<Vec<Arg>>(),
        )
        .await?;
    let mut pending_ids: HashSet<Uuid> = HashSet::new();
    for row in pending_rows {
        pending_ids.insert(row.try_get("user_id")?);
    }
    let reg_rows = db
        .fetch_all(
            &format!(
                "SELECT actor_id, ip, user_agent, created_at FROM audit_logs \
                 WHERE actor_id IN ({}) AND action = 'user.register' ORDER BY created_at ASC",
                in_placeholders(ids.len())
            ),
            &ids.iter().map(|id| id.into_arg()).collect::<Vec<Arg>>(),
        )
        .await?;
    let mut register_by_user: HashMap<Uuid, Value> = HashMap::new();
    for row in reg_rows {
        let actor_id: Uuid = row.try_get("actor_id")?;
        if register_by_user.contains_key(&actor_id) {
            continue;
        }
        let ip: Option<String> = row.try_get("ip")?;
        let ua: Option<String> = row.try_get("user_agent")?;
        let at: DateTime<Utc> = row.try_get("created_at")?;
        if let Some(block) = device_block(ip, ua, Some(at)) {
            register_by_user.insert(actor_id, block);
        }
    }
    let sess_rows = db
        .fetch_all(
            &format!(
                "SELECT user_id, ip, device_info, last_active_at FROM sessions \
                 WHERE user_id IN ({}) ORDER BY last_active_at DESC",
                in_placeholders(ids.len())
            ),
            &ids.iter().map(|id| id.into_arg()).collect::<Vec<Arg>>(),
        )
        .await?;
    let mut latest_session: HashMap<Uuid, Value> = HashMap::new();
    for row in sess_rows {
        let user_id: Uuid = row.try_get("user_id")?;
        if latest_session.contains_key(&user_id) {
            continue;
        }
        let ip: Option<String> = row.try_get("ip")?;
        let device_info: Option<String> = row.try_get("device_info")?;
        let at: DateTime<Utc> = row.try_get("last_active_at")?;
        if let Some(block) = device_block(ip, device_info, Some(at)) {
            latest_session.insert(user_id, block);
        }
    }
    let fatwa_counts = count_by_user(db, "cluster_members", "user_id", &ids).await?;
    let hifz_session_counts = count_by_user(db, "hifz_attempts", "user_id", &ids).await?;
    let hifz_test_counts = count_by_user(db, "hifz_tests", "user_id", &ids).await?;
    let donation_counts = count_by_user(db, "donations", "donor_id", &ids).await?;
    let mut out = Vec::new();
    for user in &users {
        let (country_code, country_source) = effective_country(user);
        let missing = missing_profile_fields(user);
        out.push(json!({
            "id": user.id.to_string(),
            "full_name": user.full_name,
            "email": user.email,
            "phone": user.phone,
            "locale": user.locale,
            "is_active": user.is_active,
            "is_verified": user.is_verified,
            "registration_status": if pending_ids.contains(&user.id) { "pending" } else { "completed" },
            "profile_complete": missing.is_empty(),
            "missing_profile_fields": missing,
            "country_code": country_code,
            "country_source": country_source,
            "country_name": country_name(country_code.as_deref()),
            "created_at": user.created_at.to_rfc3339(),
            "registration": register_by_user.get(&user.id).cloned().unwrap_or(Value::Null),
            "last_activity": latest_session.get(&user.id).cloned().unwrap_or(Value::Null),
            "counts": {
                "fatwa_questions": fatwa_counts.get(&user.id).copied().unwrap_or(0),
                "hifz_sessions": hifz_session_counts.get(&user.id).copied().unwrap_or(0),
                "hifz_tests": hifz_test_counts.get(&user.id).copied().unwrap_or(0),
                "donations": donation_counts.get(&user.id).copied().unwrap_or(0),
            },
            "deleted_at": null,
            "deleted_by": null,
        }));
    }
    Ok(out)
}

/// `GET /api/v1/admin/users/dashboard` — CRM users list with
/// search/filter/sort + registration IP/device and counts.
pub async fn get_users_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "users.view360").await?;
    let params = parse_query(&uri);
    let q = params.get("q").filter(|s| !s.is_empty()).cloned();
    if let Some(q) = &q {
        if q.chars().count() > 200 {
            return Err(validation_err("q", "too long"));
        }
    }
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    if let Some(s) = &status {
        if s != "pending" && s != "completed" {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_filter",
                "فیلتر وضعیت نامعتبر است",
            ));
        }
    }
    let profile = params.get("profile").filter(|s| !s.is_empty()).cloned();
    if let Some(p) = &profile {
        if p != "complete" && p != "incomplete" {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_filter",
                "فیلتر پروفایل نامعتبر است",
            ));
        }
    }
    let country = params
        .get("country")
        .filter(|s| !s.is_empty())
        .map(|s| s.to_uppercase());
    if let Some(c) = &country {
        if c.chars().count() > 2 {
            return Err(validation_err("country", "too long"));
        }
    }
    let sort = params
        .get("sort")
        .cloned()
        .unwrap_or_else(|| "created_at".to_string());
    if sort != "created_at" && sort != "last_activity" {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_filter",
            "مرتب‌سازی نامعتبر است",
        ));
    }
    let order = params
        .get("order")
        .cloned()
        .unwrap_or_else(|| "desc".to_string());
    if order != "asc" && order != "desc" {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_filter",
            "جهت مرتب‌سازی نامعتبر است",
        ));
    }
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let offset = query_int(&params, "offset", 0, 0, i64::MAX)?;
    let db = state.db_or_500()?;
    let mut conds: Vec<String> = vec!["deleted_at IS NULL".to_string()];
    let mut args: Vec<Arg> = Vec::new();
    if let Some(q) = &q {
        let like = format!("%{}%", q.trim().to_lowercase());
        conds.push(
            "(LOWER(full_name) LIKE ? OR LOWER(email) LIKE ? OR LOWER(phone) LIKE ?)".to_string(),
        );
        args.push(like.clone().into_arg());
        args.push(like.clone().into_arg());
        args.push(like.into_arg());
    }
    match status.as_deref() {
        Some("pending") => conds.push(
            "EXISTS (SELECT 1 FROM password_reset_tokens WHERE user_id = users.id \
             AND purpose = 'password_setup' AND used = FALSE)"
                .to_string(),
        ),
        Some("completed") => conds.push(
            "NOT EXISTS (SELECT 1 FROM password_reset_tokens WHERE user_id = users.id \
             AND purpose = 'password_setup' AND used = FALSE)"
                .to_string(),
        ),
        _ => {}
    }
    const PROFILE_COMPLETE_COND: &str = "TRIM(full_name) != '' AND avatar_media_id IS NOT NULL \
        AND date_of_birth IS NOT NULL AND gender IS NOT NULL AND country_code IS NOT NULL";
    match profile.as_deref() {
        Some("complete") => conds.push(format!("({PROFILE_COMPLETE_COND})")),
        Some("incomplete") => conds.push(format!("NOT ({PROFILE_COMPLETE_COND})")),
        _ => {}
    }
    let where_sql = format!("WHERE {}", conds.join(" AND "));
    let order_sql = if sort == "last_activity" {
        let sub = "(SELECT MAX(last_active_at) FROM sessions WHERE user_id = users.id)";
        if order == "desc" {
            format!("ORDER BY {sub} DESC")
        } else {
            format!("ORDER BY {sub} ASC")
        }
    } else if order == "desc" {
        "ORDER BY created_at DESC".to_string()
    } else {
        "ORDER BY created_at ASC".to_string()
    };
    if let Some(country) = &country {
        // The country filter runs on the *effective* country (profile,
        // else the verified phone's dial prefix), which is Python-side
        // logic — so the candidate set is filtered in memory, in the
        // same SQL order, then paged.
        let id_rows = db
            .fetch_all(
                &format!(
                    "SELECT id, country_code, phone, is_verified FROM users {where_sql} {order_sql}"
                ),
                &args,
            )
            .await?;
        let mut matched: Vec<Uuid> = Vec::new();
        for row in id_rows {
            let code: Option<String> = row.try_get("country_code")?;
            let phone: Option<String> = row.try_get("phone")?;
            let is_verified: bool = row.try_get("is_verified")?;
            let effective = if let Some(c) = code.filter(|c| !c.is_empty()) {
                Some(c)
            } else if is_verified {
                phone.and_then(|p| country_iso_from_phone(&p).map(str::to_string))
            } else {
                None
            };
            if effective.as_deref() == Some(country.as_str()) {
                matched.push(row.try_get("id")?);
            }
        }
        let total = matched.len() as i64;
        let page_ids: Vec<Uuid> = matched
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        if page_ids.is_empty() {
            return Ok(axum::response::Json(json!({
                "items": [], "total": total, "limit": limit, "offset": offset,
            })));
        }
        let placeholders = in_placeholders(page_ids.len());
        let rows = db
            .fetch_all(
                &format!("SELECT * FROM users WHERE id IN ({placeholders})"),
                &page_ids
                    .iter()
                    .map(|id| id.into_arg())
                    .collect::<Vec<Arg>>(),
            )
            .await?;
        let mut by_id: HashMap<Uuid, UserRow> = HashMap::new();
        for row in rows {
            let user = user_from_row(&row)?;
            by_id.insert(user.id, user);
        }
        let page_users: Vec<UserRow> = page_ids.iter().filter_map(|id| by_id.remove(id)).collect();
        let items = hydrate_dashboard(db, page_users).await?;
        return Ok(axum::response::Json(json!({
            "items": items, "total": total, "limit": limit, "offset": offset,
        })));
    }
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS c FROM users {where_sql}"),
            &args,
        )
        .await?
        .try_get("c")?;
    let rows = db
        .fetch_all(
            &format!("SELECT * FROM users {where_sql} {order_sql} LIMIT ? OFFSET ?"),
            &{
                let mut a = args.clone();
                a.push(limit.into_arg());
                a.push(offset.into_arg());
                a
            },
        )
        .await?;
    let mut page_users = Vec::new();
    for row in rows {
        page_users.push(user_from_row(&row)?);
    }
    let items = hydrate_dashboard(db, page_users).await?;
    Ok(axum::response::Json(json!({
        "items": items, "total": total, "limit": limit, "offset": offset,
    })))
}

/// `GET /api/v1/admin/users/stats`
pub async fn get_users_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "users.view360").await?;
    let db = state.db_or_500()?;
    let alive = "deleted_at IS NULL";
    let pending_exists = "EXISTS (SELECT 1 FROM password_reset_tokens WHERE user_id = users.id \
        AND purpose = 'password_setup' AND used = FALSE)";
    const PROFILE_COMPLETE_COND: &str = "TRIM(full_name) != '' AND avatar_media_id IS NOT NULL \
        AND date_of_birth IS NOT NULL AND gender IS NOT NULL AND country_code IS NOT NULL";
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS c FROM users WHERE {alive}"),
            &[],
        )
        .await?
        .try_get("c")?;
    let pending: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS c FROM users WHERE {alive} AND {pending_exists}"),
            &[],
        )
        .await?
        .try_get("c")?;
    let verified: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS c FROM users WHERE {alive} AND is_verified = TRUE"),
            &[],
        )
        .await?
        .try_get("c")?;
    let complete: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS c FROM users WHERE {alive} AND ({PROFILE_COMPLETE_COND})"),
            &[],
        )
        .await?
        .try_get("c")?;
    let trashed: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM users WHERE deleted_at IS NOT NULL",
            &[],
        )
        .await?
        .try_get("c")?;
    Ok(axum::response::Json(json!({
        "total_users": total,
        "pending": pending,
        "completed": total - pending,
        "verified": verified,
        "profile_complete": complete,
        "trashed": trashed,
    })))
}

/// `GET /api/v1/admin/users/trash` — soft-deleted users, newest
/// deletion first, in the same dashboard shape (with deleted_at/by).
pub async fn get_users_trash(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "users.view360").await?;
    let params = parse_query(&uri);
    let q = params.get("q").filter(|s| !s.is_empty()).cloned();
    if let Some(q) = &q {
        if q.chars().count() > 200 {
            return Err(validation_err("q", "too long"));
        }
    }
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let offset = query_int(&params, "offset", 0, 0, i64::MAX)?;
    let db = state.db_or_500()?;
    let mut conds: Vec<String> = vec!["deleted_at IS NOT NULL".to_string()];
    let mut args: Vec<Arg> = Vec::new();
    if let Some(q) = &q {
        let like = format!("%{}%", q.trim().to_lowercase());
        conds.push(
            "(LOWER(full_name) LIKE ? OR LOWER(email) LIKE ? OR LOWER(phone) LIKE ?)".to_string(),
        );
        args.push(like.clone().into_arg());
        args.push(like.clone().into_arg());
        args.push(like.into_arg());
    }
    let where_sql = format!("WHERE {}", conds.join(" AND "));
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS c FROM users {where_sql}"),
            &args,
        )
        .await?
        .try_get("c")?;
    let mut page_args = args.clone();
    page_args.push(limit.into_arg());
    page_args.push(offset.into_arg());
    let rows = db
        .fetch_all(
            &format!("SELECT * FROM users {where_sql} ORDER BY deleted_at DESC LIMIT ? OFFSET ?"),
            &page_args,
        )
        .await?;
    let mut page_users = Vec::new();
    let mut deleted: HashMap<Uuid, (Option<DateTime<Utc>>, Option<Uuid>)> = HashMap::new();
    for row in &rows {
        let user = user_from_row(row)?;
        let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
        let deleted_by: Option<Uuid> = row.try_get("deleted_by")?;
        deleted.insert(user.id, (deleted_at, deleted_by));
        page_users.push(user);
    }
    let mut items = hydrate_dashboard(db, page_users).await?;
    for item in &mut items {
        if let Some(id) = item
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<Uuid>().ok())
        {
            if let Some((at, by)) = deleted.get(&id) {
                item["deleted_at"] = json!(at.map(|t| t.to_rfc3339()));
                item["deleted_by"] = json!(by.map(|u| u.to_string()));
            }
        }
    }
    Ok(axum::response::Json(json!({
        "items": items, "total": total, "limit": limit, "offset": offset,
    })))
}

// --- 360° overview --------------------------------------------------------------

fn excerpt(text: Option<&str>, limit: usize) -> String {
    let Some(text) = text else {
        return String::new();
    };
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= limit {
        collapsed
    } else {
        let mut s: String = collapsed.chars().take(limit - 1).collect();
        s.push('…');
        s
    }
}

fn localized_fa(v: &Value) -> String {
    v.get("fa")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// `GET /api/v1/admin/users/{user_id}/overview` — the read-only 360°
/// aggregation: profile, roles, registration, sessions, donations,
/// hifz, LMS, media, library, social, fatwa, notifications, audit
/// trail, activity timeline + 84-day heatmap.
pub async fn get_user_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let _ctx = authed(&state, &headers, "users.view360").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    let Some(row) = db
        .fetch_optional("SELECT * FROM users WHERE id = ?", &[user_id.into_arg()])
        .await?
    else {
        return Err(bad(StatusCode::NOT_FOUND, "not_found", "کاربر یافت نشد"));
    };
    let user = user_from_row(&row)?;
    let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
    let deleted_by: Option<Uuid> = row.try_get("deleted_by")?;
    let roles: Vec<String> = db
        .fetch_all(
            "SELECT r.name FROM user_roles ur JOIN roles r ON r.id = ur.role_id \
             WHERE ur.user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .into_iter()
        .map(|r| r.try_get("name"))
        .collect::<Result<_, _>>()?;
    let pending = db
        .fetch_optional(
            "SELECT id FROM password_reset_tokens WHERE user_id = ? \
             AND purpose = 'password_setup' AND used = FALSE",
            &[user_id.into_arg()],
        )
        .await?
        .is_some();
    let missing = missing_profile_fields(&user);
    let (country_code, country_source) = effective_country(&user);
    let sessions = db
        .fetch_all(
            "SELECT id, ip, device_info, last_active_at, created_at, revoked \
             FROM sessions WHERE user_id = ? ORDER BY created_at DESC LIMIT 50",
            &[user_id.into_arg()],
        )
        .await?;
    let mut session_out = Vec::new();
    for s in &sessions {
        let sid: Uuid = s.try_get("id")?;
        let ip: Option<String> = s.try_get("ip")?;
        let device_info: Option<String> = s.try_get("device_info")?;
        let last_active_at: DateTime<Utc> = s.try_get("last_active_at")?;
        let created_at: DateTime<Utc> = s.try_get("created_at")?;
        let revoked: bool = s.try_get("revoked")?;
        let block = device_block(ip.clone(), device_info.clone(), Some(last_active_at))
            .unwrap_or_else(|| {
                json!({
                    "ip": ip, "user_agent": device_info, "browser": null,
                    "os": null, "device": "unknown", "summary": null,
                    "at": last_active_at.to_rfc3339(),
                })
            });
        let mut entry = block;
        entry["id"] = json!(sid.to_string());
        entry["created_at"] = json!(created_at.to_rfc3339());
        entry["revoked"] = json!(revoked);
        entry["state"] = json!(if revoked { "revoked" } else { "active" });
        session_out.push(entry);
    }
    let register_row = db
        .fetch_optional(
            "SELECT ip, user_agent, created_at FROM audit_logs \
             WHERE actor_id = ? AND action = 'user.register' ORDER BY created_at ASC LIMIT 1",
            &[user_id.into_arg()],
        )
        .await?;
    let registration = match register_row {
        Some(r) => {
            let ip: Option<String> = r.try_get("ip")?;
            let ua: Option<String> = r.try_get("user_agent")?;
            let at: DateTime<Utc> = r.try_get("created_at")?;
            device_block(ip, ua, Some(at)).unwrap_or(Value::Null)
        }
        None => Value::Null,
    };
    let donation_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM donations WHERE donor_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let donation_total: Option<i64> = db
        .fetch_one(
            "SELECT COALESCE(SUM(amount), 0) AS t FROM donations \
             WHERE donor_id = ? AND status = 'succeeded'",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("t")
        .ok();
    let donations_recent = db
        .fetch_all(
            "SELECT id, amount, currency, gateway, status, created_at FROM donations \
             WHERE donor_id = ? ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?;
    let mut donations_recent_out = Vec::new();
    for d in donations_recent {
        let id: Uuid = d.try_get("id")?;
        let amount: i32 = d.try_get("amount")?;
        let currency: String = d.try_get("currency")?;
        let gateway: Option<String> = d.try_get("gateway")?;
        let status: String = d.try_get("status")?;
        let created_at: DateTime<Utc> = d.try_get("created_at")?;
        donations_recent_out.push(json!({
            "id": id.to_string(), "amount": amount, "currency": currency,
            "gateway": gateway, "status": status,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let hifz_attempt_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM hifz_attempts WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let hifz_test_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM hifz_tests WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let hifz_tasmi_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM hifz_tasmi_sessions WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let hifz_recent = db
        .fetch_all(
            "SELECT id, surah, status, engine, created_at FROM hifz_attempts \
             WHERE user_id = ? ORDER BY created_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?;
    let mut hifz_recent_out = Vec::new();
    for a in hifz_recent {
        let id: Uuid = a.try_get("id")?;
        let surah: i32 = a.try_get("surah")?;
        let status: String = a.try_get("status")?;
        let engine: String = a.try_get("engine")?;
        let created_at: DateTime<Utc> = a.try_get("created_at")?;
        hifz_recent_out.push(json!({
            "id": id.to_string(), "surah": surah, "status": status,
            "engine": engine, "created_at": created_at.to_rfc3339(),
        }));
    }
    let prog_rows = db
        .fetch_all(
            "SELECT status, COUNT(*) AS c FROM user_progress WHERE user_id = ? GROUP BY status",
            &[user_id.into_arg()],
        )
        .await?;
    let mut progress_by_status: Map<String, Value> = Map::new();
    let mut enrolled: i64 = 0;
    for r in prog_rows {
        let status: String = r.try_get("status")?;
        let c: i64 = r.try_get("c")?;
        progress_by_status.insert(status, json!(c));
        enrolled += c;
    }
    let completed = progress_by_status
        .get("completed")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let certs = db
        .fetch_all(
            "SELECT id, course_id, code, issued_at FROM certificates \
             WHERE user_id = ? ORDER BY issued_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?;
    let mut certs_out = Vec::new();
    for c in &certs {
        let id: Uuid = c.try_get("id")?;
        let course_id: Uuid = c.try_get("course_id")?;
        let code: String = c.try_get("code")?;
        let issued_at: DateTime<Utc> = c.try_get("issued_at")?;
        certs_out.push(json!({
            "id": id.to_string(), "course_id": course_id.to_string(),
            "code": code, "issued_at": issued_at.to_rfc3339(),
        }));
    }
    let lib_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM library_items WHERE created_by = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let lib_recent = db
        .fetch_all(
            "SELECT id, title, kind, workflow_status, created_at FROM library_items \
             WHERE created_by = ? ORDER BY created_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?;
    let mut lib_recent_out = Vec::new();
    for i in lib_recent {
        let id: Uuid = i.try_get("id")?;
        let title: Value = i.try_get("title")?;
        let kind: String = i.try_get("kind")?;
        let workflow_status: String = i.try_get("workflow_status")?;
        let created_at: DateTime<Utc> = i.try_get("created_at")?;
        lib_recent_out.push(json!({
            "id": id.to_string(), "title": localized_fa(&title),
            "kind": kind, "workflow_status": workflow_status,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let post_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM social_posts WHERE author_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let comment_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM social_comments WHERE author_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let reports_filed: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM social_reports WHERE reported_by = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let fatwa_sessions_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM fatwa_sessions WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let fatwa_messages_count: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM fatwa_messages WHERE session_id IN \
             (SELECT id FROM fatwa_sessions WHERE user_id = ?)",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let submissions = db
        .fetch_all(
            "SELECT id, title, workflow_status, created_at FROM fatwa_submissions \
             WHERE author_id = ? ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?;
    let mut submissions_out = Vec::new();
    for s in submissions {
        let id: Uuid = s.try_get("id")?;
        let title: Value = s.try_get("title")?;
        let workflow_status: String = s.try_get("workflow_status")?;
        let created_at: DateTime<Utc> = s.try_get("created_at")?;
        submissions_out.push(json!({
            "id": id.to_string(), "title": localized_fa(&title),
            "workflow_status": workflow_status,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let clusters_answered: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM question_clusters WHERE answered_by = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let cluster_questions: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM cluster_members WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let notifs = db
        .fetch_all(
            "SELECT id, notif_type, title, read_at, created_at FROM notifications \
             WHERE user_id = ? ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?;
    let mut notifs_out = Vec::new();
    for n in notifs {
        let id: Uuid = n.try_get("id")?;
        let notif_type: String = n.try_get("notif_type")?;
        let title: Value = n.try_get("title")?;
        let read_at: Option<DateTime<Utc>> = n.try_get("read_at")?;
        let created_at: DateTime<Utc> = n.try_get("created_at")?;
        notifs_out.push(json!({
            "id": id.to_string(), "notif_type": notif_type,
            "title": localized_fa(&title),
            "read_at": iso(read_at), "created_at": created_at.to_rfc3339(),
        }));
    }
    let unread: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM notifications WHERE user_id = ? AND read_at IS NULL",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let audits = db
        .fetch_all(
            "SELECT id, action, entity_type, entity_id, ip, user_agent, created_at \
             FROM audit_logs WHERE actor_id = ? OR (entity_type = 'user' AND entity_id = ?) \
             ORDER BY created_at DESC LIMIT 20",
            &[user_id.into_arg(), user_id.to_string().into_arg()],
        )
        .await?;
    let mut audits_out = Vec::new();
    for a in audits {
        let id: Uuid = a.try_get("id")?;
        let action: String = a.try_get("action")?;
        let entity_type: String = a.try_get("entity_type")?;
        let entity_id: Option<String> = a.try_get("entity_id")?;
        let ip: Option<String> = a.try_get("ip")?;
        let user_agent: Option<String> = a.try_get("user_agent")?;
        let created_at: DateTime<Utc> = a.try_get("created_at")?;
        audits_out.push(json!({
            "id": id.to_string(), "action": action,
            "entity_type": entity_type, "entity_id": entity_id,
            "ip": ip, "user_agent": user_agent,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    // Avatar: a signed URL like the library's, or None on any failure
    // (the overview must never 500 over a photo).
    let avatar_url: Option<String> = match user.avatar_media_id {
        None => None,
        Some(media_id) => {
            let asset = db
                .fetch_optional(
                    "SELECT storage_key FROM media_assets WHERE id = ?",
                    &[media_id.into_arg()],
                )
                .await
                .ok()
                .flatten();
            match asset {
                Some(a) => {
                    let key: Result<String, _> = a.try_get("storage_key");
                    match key {
                        Ok(key) => presigned_url(
                            &state.config,
                            &key,
                            3600,
                            "/api/v1/users/avatar-by-ref",
                            SIGNED_PURPOSE_AVATAR,
                        )
                        .ok(),
                        Err(_) => None,
                    }
                }
                None => None,
            }
        }
    };
    let activity_timeline = build_timeline(db, user_id, &user).await?;
    let activity_heatmap = build_heatmap(db, user_id).await?;
    let media_plays: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM user_activity_events \
             WHERE user_id = ? AND kind = 'media_play'",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let media_downloads: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS c FROM user_activity_events \
             WHERE user_id = ? AND kind = 'library_download'",
            &[user_id.into_arg()],
        )
        .await?
        .try_get("c")?;
    let progress_metas = db
        .fetch_all(
            "SELECT meta FROM user_activity_events \
             WHERE user_id = ? AND kind = 'media_progress'",
            &[user_id.into_arg()],
        )
        .await?;
    let mut media_seconds: i64 = 0;
    for row in progress_metas {
        let meta: Option<Value> = row.try_get("meta").ok().flatten();
        if let Some(meta) = meta {
            if let Some(delta) = meta.get("seconds_delta").and_then(|v| v.as_f64()) {
                if delta > 0.0 {
                    media_seconds += delta as i64;
                }
            }
        }
    }
    Ok(axum::response::Json(json!({
        "profile": {
            "id": user.id.to_string(), "full_name": user.full_name,
            "email": user.email, "phone": user.phone, "locale": user.locale,
            "is_active": user.is_active, "is_verified": user.is_verified,
            "created_at": user.created_at.to_rfc3339(),
            "avatar_url": avatar_url,
            "deleted_at": iso(deleted_at),
            "deleted_by": deleted_by.map(|u| u.to_string()),
            "is_trashed": deleted_at.is_some(),
            "date_of_birth": user.date_of_birth,
            "gender": user.gender,
            "country_code": country_code,
            "country_source": country_source,
            "avatar_media_id": user.avatar_media_id.map(|u| u.to_string()),
            "registration_status": if pending { "pending" } else { "completed" },
            "profile_complete": missing.is_empty(),
            "missing_profile_fields": missing,
        },
        "roles": roles,
        "registration": registration,
        "sessions": session_out,
        "donations": {
            "count": donation_count,
            "total_succeeded_irr": donation_total.unwrap_or(0),
            "recent": donations_recent_out,
        },
        "hifz": {
            "attempts": hifz_attempt_count,
            "tests": hifz_test_count,
            "tasmi_sessions": hifz_tasmi_count,
            "recent_attempts": hifz_recent_out,
        },
        "lms": {
            "progress_by_status": progress_by_status,
            "enrolled": enrolled,
            "completed": completed,
            "certificates_count": certs_out.len(),
            "certificates": certs_out,
        },
        "media": {
            "plays": media_plays,
            "seconds": media_seconds,
            "downloads": media_downloads,
        },
        "library": {
            "items_created": lib_count,
            "uploads": lib_count,
            "recent": lib_recent_out,
        },
        "social": {
            "posts": post_count, "comments": comment_count,
            "reports_filed": reports_filed,
        },
        "fatwa": {
            "sessions": fatwa_sessions_count, "messages": fatwa_messages_count,
            "clusters_answered": clusters_answered,
            "cluster_questions_asked": cluster_questions,
            "questions": cluster_questions,
            "answers": clusters_answered,
            "submissions_count": submissions_out.len(),
            "submissions": submissions_out,
        },
        "notifications": {
            "unread": unread,
            "recent": notifs_out,
        },
        "audit_trail": audits_out,
        "activity_timeline": activity_timeline,
        "activity_heatmap": activity_heatmap,
        "generated_at": Utc::now().to_rfc3339(),
    })))
}

/// The cross-module activity timeline (newest first, max 80).
async fn build_timeline(db: &Db, user_id: Uuid, user: &UserRow) -> ApiResult<Vec<Value>> {
    let mut events: Vec<(DateTime<Utc>, Value)> = Vec::new();
    let mut add = |kind: &str, module: &str, at: Option<DateTime<Utc>>, data: Value| {
        if let Some(at) = at {
            events.push((
                at,
                json!({ "kind": kind, "module": module, "at": at.to_rfc3339(), "data": data }),
            ));
        }
    };
    add(
        "account_created",
        "account",
        Some(user.created_at),
        json!({}),
    );
    for s in db
        .fetch_all(
            "SELECT ip, device_info, created_at FROM sessions WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let ip: Option<String> = s.try_get("ip")?;
        let device_info: Option<String> = s.try_get("device_info")?;
        let at: DateTime<Utc> = s.try_get("created_at")?;
        let summary = parse_user_agent(device_info.as_deref())["summary"].clone();
        add(
            "login",
            "account",
            Some(at),
            json!({ "summary": summary, "ip": ip }),
        );
    }
    for a in db
        .fetch_all(
            "SELECT surah, status, engine, created_at FROM hifz_attempts WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT 15",
            &[user_id.into_arg()],
        )
        .await?
    {
        let surah: i32 = a.try_get("surah")?;
        let status: String = a.try_get("status")?;
        let engine: String = a.try_get("engine")?;
        let at: DateTime<Utc> = a.try_get("created_at")?;
        add(
            "hifz_attempt",
            "hifz",
            Some(at),
            json!({ "surah": surah, "status": status, "engine": engine }),
        );
    }
    for t in db
        .fetch_all(
            "SELECT surah_start, surah_end, status, started_at FROM hifz_tests WHERE user_id = ? \
             ORDER BY started_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let surah_start: i32 = t.try_get("surah_start")?;
        let surah_end: i32 = t.try_get("surah_end")?;
        let status: String = t.try_get("status")?;
        let at: DateTime<Utc> = t.try_get("started_at")?;
        add(
            "hifz_test",
            "hifz",
            Some(at),
            json!({ "surah_start": surah_start, "surah_end": surah_end, "status": status }),
        );
    }
    for ts in db
        .fetch_all(
            "SELECT start_surah, start_ayah, end_surah, end_ayah, status, created_at \
             FROM hifz_tasmi_sessions WHERE user_id = ? ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let start_surah: i32 = ts.try_get("start_surah")?;
        let start_ayah: i32 = ts.try_get("start_ayah")?;
        let end_surah: i32 = ts.try_get("end_surah")?;
        let end_ayah: i32 = ts.try_get("end_ayah")?;
        let status: String = ts.try_get("status")?;
        let at: DateTime<Utc> = ts.try_get("created_at")?;
        add(
            "hifz_tasmi",
            "hifz",
            Some(at),
            json!({ "start_surah": start_surah, "start_ayah": start_ayah, "end_surah": end_surah, "end_ayah": end_ayah, "status": status }),
        );
    }
    if let Some(listen) = db
        .fetch_optional(
            "SELECT surah, ayah, reciter, progress_ms, updated_at FROM hifz_listen_positions WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
    {
        let surah: i32 = listen.try_get("surah")?;
        let ayah: i32 = listen.try_get("ayah")?;
        let reciter: String = listen.try_get("reciter")?;
        let progress_ms: i32 = listen.try_get("progress_ms")?;
        let at: Option<DateTime<Utc>> = listen.try_get("updated_at")?;
        add("hifz_listen", "hifz", at, json!({ "surah": surah, "ayah": ayah, "reciter": reciter, "progress_ms": progress_ms }));
    }
    for p in db
        .fetch_all(
            "SELECT lesson_id, completed_at FROM user_progress WHERE user_id = ? \
             AND completed_at IS NOT NULL ORDER BY completed_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let lesson_id: Uuid = p.try_get("lesson_id")?;
        let at: DateTime<Utc> = p.try_get("completed_at")?;
        add(
            "lms_completed",
            "lms",
            Some(at),
            json!({ "lesson_id": lesson_id.to_string() }),
        );
    }
    for c in db
        .fetch_all(
            "SELECT code, issued_at FROM certificates WHERE user_id = ? \
             ORDER BY issued_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?
    {
        let code: String = c.try_get("code")?;
        let at: DateTime<Utc> = c.try_get("issued_at")?;
        add("lms_certificate", "lms", Some(at), json!({ "code": code }));
    }
    for i in db
        .fetch_all(
            "SELECT title, kind, created_at FROM library_items WHERE created_by = ? \
             ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let title: Value = i.try_get("title")?;
        let kind: String = i.try_get("kind")?;
        let at: DateTime<Utc> = i.try_get("created_at")?;
        add(
            "library_upload",
            "library",
            Some(at),
            json!({ "title": localized_fa(&title), "kind": kind }),
        );
    }
    for sp in db
        .fetch_all(
            "SELECT body, created_at FROM social_posts WHERE author_id = ? \
             ORDER BY created_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?
    {
        let body: String = sp.try_get("body")?;
        let at: DateTime<Utc> = sp.try_get("created_at")?;
        add(
            "social_post",
            "social",
            Some(at),
            json!({ "excerpt": excerpt(Some(&body), 90) }),
        );
    }
    for sc in db
        .fetch_all(
            "SELECT body, created_at FROM social_comments WHERE author_id = ? \
             ORDER BY created_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?
    {
        let body: String = sc.try_get("body")?;
        let at: DateTime<Utc> = sc.try_get("created_at")?;
        add(
            "social_comment",
            "social",
            Some(at),
            json!({ "excerpt": excerpt(Some(&body), 90) }),
        );
    }
    for q in db
        .fetch_all(
            "SELECT question_text, source, created_at FROM cluster_members WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let question_text: String = q.try_get("question_text")?;
        let source: String = q.try_get("source")?;
        let at: DateTime<Utc> = q.try_get("created_at")?;
        add(
            "fatwa_question",
            "fatwa",
            Some(at),
            json!({ "excerpt": excerpt(Some(&question_text), 90), "source": source }),
        );
    }
    for fs in db
        .fetch_all(
            "SELECT created_at FROM fatwa_sessions WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?
    {
        let at: DateTime<Utc> = fs.try_get("created_at")?;
        add("fatwa_session", "fatwa", Some(at), json!({}));
    }
    for sub in db
        .fetch_all(
            "SELECT title, workflow_status, created_at FROM fatwa_submissions WHERE author_id = ? \
             ORDER BY created_at DESC LIMIT 5",
            &[user_id.into_arg()],
        )
        .await?
    {
        let title: Value = sub.try_get("title")?;
        let status: String = sub.try_get("workflow_status")?;
        let at: DateTime<Utc> = sub.try_get("created_at")?;
        add(
            "fatwa_submission",
            "fatwa",
            Some(at),
            json!({ "title": localized_fa(&title), "status": status }),
        );
    }
    for d in db
        .fetch_all(
            "SELECT amount, currency, status, gateway, created_at FROM donations WHERE donor_id = ? \
             ORDER BY created_at DESC LIMIT 10",
            &[user_id.into_arg()],
        )
        .await?
    {
        let amount: i32 = d.try_get("amount")?;
        let currency: String = d.try_get("currency")?;
        let status: String = d.try_get("status")?;
        let gateway: Option<String> = d.try_get("gateway")?;
        let at: DateTime<Utc> = d.try_get("created_at")?;
        add("donation", "finance", Some(at), json!({ "amount": amount, "currency": currency, "status": status, "gateway": gateway }));
    }
    for e in db
        .fetch_all(
            "SELECT kind, title, ref_id, meta, created_at FROM user_activity_events \
             WHERE user_id = ? ORDER BY created_at DESC LIMIT 40",
            &[user_id.into_arg()],
        )
        .await?
    {
        let kind: String = e.try_get("kind")?;
        let title: Option<String> = e.try_get("title")?;
        let ref_id: Option<String> = e.try_get("ref_id")?;
        let meta: Option<Value> = e.try_get("meta").ok().flatten();
        let at: DateTime<Utc> = e.try_get("created_at")?;
        let mut data = meta.unwrap_or(json!({}));
        if let Value::Object(m) = &mut data {
            if let Some(t) = title {
                m.insert("title".to_string(), json!(t));
            }
            if let Some(r) = ref_id {
                m.insert("ref_id".to_string(), json!(r));
            }
        }
        let module = if kind.starts_with("media_") {
            "media"
        } else {
            "library"
        };
        add(&kind, module, Some(at), data);
    }
    events.sort_by_key(|e| std::cmp::Reverse(e.0));
    events.truncate(80);
    Ok(events.into_iter().map(|(_, v)| v).collect())
}

/// The 84-day activity heatmap: `[{date, count}]`, ascending, empty
/// days zero.
async fn build_heatmap(db: &Db, user_id: Uuid) -> ApiResult<Vec<Value>> {
    let today = Utc::now().date_naive();
    let start = today - chrono::Duration::days(83);
    let start_dt = start.and_hms_opt(0, 0, 0).unwrap().and_utc();
    let sources: &[(&str, &str, &str)] = &[
        ("sessions", "created_at", "user_id"),
        ("hifz_attempts", "created_at", "user_id"),
        ("hifz_tests", "started_at", "user_id"),
        ("hifz_tasmi_sessions", "created_at", "user_id"),
        ("user_progress", "updated_at", "user_id"),
        ("certificates", "issued_at", "user_id"),
        ("library_items", "created_at", "created_by"),
        ("social_posts", "created_at", "author_id"),
        ("social_comments", "created_at", "author_id"),
        ("cluster_members", "created_at", "user_id"),
        ("fatwa_sessions", "created_at", "user_id"),
        ("donations", "created_at", "donor_id"),
        ("user_activity_events", "created_at", "user_id"),
    ];
    let mut counts: HashMap<String, i64> = HashMap::new();
    for (table, ts_col, user_col) in sources {
        let rows = db
            .fetch_all(
                &format!("SELECT {ts_col} FROM {table} WHERE {user_col} = ? AND {ts_col} >= ?"),
                &[user_id.into_arg(), start_dt.into_arg()],
            )
            .await?;
        for row in rows {
            let ts: Option<DateTime<Utc>> = row.try_get(ts_col).ok().flatten();
            if let Some(ts) = ts {
                let day = ts.date_naive().to_string();
                *counts.entry(day).or_insert(0) += 1;
            }
        }
    }
    let mut out = Vec::new();
    for i in 0..84 {
        let day = start + chrono::Duration::days(i);
        out.push(json!({
            "date": day.to_string(),
            "count": counts.get(&day.to_string()).copied().unwrap_or(0),
        }));
    }
    Ok(out)
}

// --- user actions ---------------------------------------------------------------

async fn manage_target(db: &Db, actor: &AuthCtx, user_id: Uuid) -> ApiResult<UserRow> {
    let Some(row) = db
        .fetch_optional("SELECT * FROM users WHERE id = ?", &[user_id.into_arg()])
        .await?
    else {
        return Err(bad(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "کاربر یافت نشد",
        ));
    };
    let target = user_from_row(&row)?;
    if target.id == actor.user.id {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "self_action",
            "نمی‌توانید این کار را روی حساب خودتان انجام دهید",
        ));
    }
    let role_names: Vec<String> = db
        .fetch_all(
            "SELECT r.name FROM roles r JOIN user_roles ur ON ur.role_id = r.id \
             WHERE ur.user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .into_iter()
        .map(|r| r.try_get("name"))
        .collect::<Result<_, _>>()?;
    if role_names.iter().any(|n| n == "super_admin") {
        let others: i64 = db
            .fetch_one(
                "SELECT COUNT(*) AS c FROM user_roles ur \
                 JOIN roles r ON r.id = ur.role_id \
                 JOIN users u ON u.id = ur.user_id \
                 WHERE r.name = 'super_admin' AND ur.user_id != ? AND u.is_active = TRUE",
                &[user_id.into_arg()],
            )
            .await?
            .try_get("c")?;
        if others == 0 {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "last_super_admin",
                "این کاربر تنها مدیر ارشد فعال است و قابل تعلیق یا حذف نیست",
            ));
        }
    }
    Ok(target)
}

async fn revoke_all_sessions(db: &Db, user_id: Uuid) -> ApiResult<u64> {
    let revoked = db
        .execute(
            "UPDATE sessions SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
            &[user_id.into_arg()],
        )
        .await?;
    db.execute(
        "UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
        &[user_id.into_arg()],
    )
    .await?;
    Ok(revoked)
}

/// `PATCH /api/v1/admin/users/{user_id}/roles` — replace the user's
/// roles (permission `roles.manage`). Replacing roles revokes every
/// session + refresh token, so the next login re-scopes the token.
pub async fn patch_user_roles(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
    body: Bytes,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "roles.manage").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let parsed = parse_body(&body)?;
    let db = state.db_or_500()?;
    if db
        .fetch_optional("SELECT id FROM users WHERE id = ?", &[user_id.into_arg()])
        .await?
        .is_none()
    {
        return Err(bad(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "کاربر یافت نشد",
        ));
    }
    let mut wanted_ids: HashSet<Uuid> = parsed
        .get("role_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().and_then(|s| s.parse::<Uuid>().ok()))
                .collect()
        })
        .unwrap_or_default();
    let role_names: Vec<String> = parsed
        .get("role_names")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    if !role_names.is_empty() {
        let placeholders = in_placeholders(role_names.len());
        let rows = db
            .fetch_all(
                &format!("SELECT id, name FROM roles WHERE name IN ({placeholders})"),
                &role_names
                    .iter()
                    .map(|n| n.into_arg())
                    .collect::<Vec<Arg>>(),
            )
            .await?;
        let found: HashSet<String> = rows
            .iter()
            .map(|r| r.try_get::<String, _>("name"))
            .collect::<Result<_, _>>()?;
        let missing: Vec<&String> = role_names.iter().filter(|n| !found.contains(*n)).collect();
        if !missing.is_empty() {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_role_name",
                format!("نقش نامعتبر: {missing:?}"),
            ));
        }
        for row in rows {
            wanted_ids.insert(row.try_get("id")?);
        }
    }
    if wanted_ids.is_empty() {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_roles",
            "حداقل یک نقش لازم است",
        ));
    }
    let placeholders = in_placeholders(wanted_ids.len());
    let roles = db
        .fetch_all(
            &format!("SELECT id, name FROM roles WHERE id IN ({placeholders})"),
            &wanted_ids
                .iter()
                .map(|id| id.into_arg())
                .collect::<Vec<Arg>>(),
        )
        .await?;
    if roles.len() != wanted_ids.len() {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_role_id",
            "یک یا چند نقش نامعتبر است",
        ));
    }
    if user_id == ctx.user.id {
        let my_roles: HashSet<String> = roles
            .iter()
            .map(|r| r.try_get::<String, _>("name"))
            .collect::<Result<_, _>>()?;
        let cur: Vec<String> = db
            .fetch_all(
                "SELECT r.name FROM roles r JOIN user_roles ur ON ur.role_id = r.id \
                 WHERE ur.user_id = ?",
                &[ctx.user.id.into_arg()],
            )
            .await?
            .into_iter()
            .map(|r| r.try_get("name"))
            .collect::<Result<_, _>>()?;
        if cur.iter().any(|n| n == "super_admin") && !my_roles.contains("super_admin") {
            return Err(bad(
                StatusCode::UNPROCESSABLE_ENTITY,
                "self_lockout",
                "نمی‌توانید نقش super_admin را از خود بگیرید",
            ));
        }
    }
    let old: Vec<Uuid> = db
        .fetch_all(
            "SELECT role_id FROM user_roles WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?
        .into_iter()
        .map(|r| r.try_get("role_id"))
        .collect::<Result<_, _>>()?;
    db.execute(
        "DELETE FROM user_roles WHERE user_id = ?",
        &[user_id.into_arg()],
    )
    .await?;
    for rid in &wanted_ids {
        db.execute(
            "INSERT INTO user_roles (user_id, role_id) VALUES (?, ?)",
            &[user_id.into_arg(), (*rid).into_arg()],
        )
        .await?;
    }
    let _ = revoke_all_sessions(db, user_id).await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.roles.replace",
        "user",
        Some(&user_id.to_string()),
        Some(json!({ "role_ids": old.iter().map(|u| u.to_string()).collect::<Vec<_>>() })),
        Some(json!({ "role_ids": wanted_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(json!({
        "user_id": user_id.to_string(),
        "role_ids": wanted_ids.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
    })))
}

/// `POST /api/v1/admin/users/{user_id}/sessions/revoke`
pub async fn revoke_user_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "users.view360").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    if db
        .fetch_optional("SELECT id FROM users WHERE id = ?", &[user_id.into_arg()])
        .await?
        .is_none()
    {
        return Err(bad(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "کاربر یافت نشد",
        ));
    }
    let revoked = revoke_all_sessions(db, user_id).await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.sessions.revoke_all",
        "user",
        Some(&user_id.to_string()),
        None,
        Some(json!({ "revoked_sessions": revoked })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(json!({
        "user_id": user_id.to_string(),
        "revoked_sessions": revoked,
    })))
}

/// `POST /api/v1/admin/users/{user_id}/suspend`
pub async fn suspend_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "users.manage").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    let target = manage_target(db, &ctx, user_id).await?;
    if !target.is_active {
        return Ok(axum::response::Json(
            json!({ "user_id": user_id.to_string(), "is_active": false }),
        ));
    }
    db.execute(
        "UPDATE users SET is_active = FALSE WHERE id = ?",
        &[user_id.into_arg()],
    )
    .await?;
    let _ = revoke_all_sessions(db, user_id).await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.suspend",
        "user",
        Some(&user_id.to_string()),
        Some(json!({ "is_active": true })),
        Some(json!({ "is_active": false })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(
        json!({ "user_id": user_id.to_string(), "is_active": false }),
    ))
}

/// `POST /api/v1/admin/users/{user_id}/activate`
pub async fn activate_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "users.manage").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    let target = manage_target(db, &ctx, user_id).await?;
    if target.is_active {
        return Ok(axum::response::Json(
            json!({ "user_id": user_id.to_string(), "is_active": true }),
        ));
    }
    db.execute(
        "UPDATE users SET is_active = TRUE WHERE id = ?",
        &[user_id.into_arg()],
    )
    .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.activate",
        "user",
        Some(&user_id.to_string()),
        Some(json!({ "is_active": false })),
        Some(json!({ "is_active": true })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(
        json!({ "user_id": user_id.to_string(), "is_active": true }),
    ))
}

/// `DELETE /api/v1/admin/users/{user_id}` — soft delete (to trash).
pub async fn soft_delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "users.manage").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    let _ = manage_target(db, &ctx, user_id).await?;
    let row = db
        .fetch_one(
            "SELECT deleted_at FROM users WHERE id = ?",
            &[user_id.into_arg()],
        )
        .await?;
    let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
    if let Some(at) = deleted_at {
        return Ok(axum::response::Json(json!({
            "user_id": user_id.to_string(), "trashed": true,
            "deleted_at": at.to_rfc3339(),
        })));
    }
    let now = utc_now();
    db.execute(
        "UPDATE users SET deleted_at = ?, deleted_by = ?, is_active = FALSE WHERE id = ?",
        &[now.into_arg(), ctx.user.id.into_arg(), user_id.into_arg()],
    )
    .await?;
    let _ = revoke_all_sessions(db, user_id).await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.trash",
        "user",
        Some(&user_id.to_string()),
        Some(json!({ "is_active": true, "deleted_at": null })),
        Some(json!({ "is_active": false, "deleted_at": now.to_rfc3339() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(json!({
        "user_id": user_id.to_string(), "trashed": true,
        "deleted_at": now.to_rfc3339(),
    })))
}

/// `POST /api/v1/admin/users/{user_id}/restore`
pub async fn restore_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "users.manage").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    let Some(row) = db
        .fetch_optional(
            "SELECT deleted_at FROM users WHERE id = ?",
            &[user_id.into_arg()],
        )
        .await?
    else {
        return Err(bad(
            StatusCode::NOT_FOUND,
            "user_not_found",
            "کاربر یافت نشد",
        ));
    };
    let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
    if deleted_at.is_none() {
        return Ok(axum::response::Json(json!({
            "user_id": user_id.to_string(), "is_trashed": false, "restored": false,
        })));
    }
    db.execute(
        "UPDATE users SET deleted_at = NULL, deleted_by = NULL, is_active = TRUE WHERE id = ?",
        &[user_id.into_arg()],
    )
    .await?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.restore",
        "user",
        Some(&user_id.to_string()),
        Some(json!({ "is_active": false })),
        Some(json!({ "is_active": true })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(axum::response::Json(json!({
        "user_id": user_id.to_string(), "is_trashed": false, "restored": true,
    })))
}

/// The permanent-delete graph: every TEST_SCHEMA table that
/// references `users` either loses its rows or (for the four
/// anonymised tables — donations, library_items,
/// question_clusters, plus `fatwa_submissions.reviewed_by` which
/// Python deletes via the same rule) has the reference nulled.
/// Mirrors `_purge_user_graph`'s table walk, but over the static
/// TEST_SCHEMA table list instead of SQLAlchemy metadata.
async fn purge_user_graph(db: &Db, config: &crate::Config, user_id: Uuid) -> ApiResult<()> {
    // The user's own avatar reference is neutralised first so the
    // media row can be deleted mid-graph without dangling.
    db.execute(
        "UPDATE users SET avatar_media_id = NULL WHERE id = ?",
        &[user_id.into_arg()],
    )
    .await?;
    // Media files owned by the user are deleted best-effort.
    let assets = db
        .fetch_all(
            "SELECT storage_key, thumbnail_key FROM media_assets WHERE owner_id = ?",
            &[user_id.into_arg()],
        )
        .await?;
    for asset in assets {
        for col in ["storage_key", "thumbnail_key"] {
            let key: Result<Option<String>, _> = asset.try_get(col);
            if let Ok(Some(key)) = key {
                let path = std::path::PathBuf::from(&config.storage_dir).join(&key);
                let _ = std::fs::remove_file(path);
            }
        }
    }
    // Child rows that point at the user's own child rows first.
    db.execute(
        "DELETE FROM fatwa_messages WHERE session_id IN \
         (SELECT id FROM fatwa_sessions WHERE user_id = ?)",
        &[user_id.into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_test_questions WHERE test_id IN \
         (SELECT id FROM hifz_tests WHERE user_id = ?)",
        &[user_id.into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_word_scores WHERE attempt_id IN \
         (SELECT id FROM hifz_attempts WHERE user_id = ?)",
        &[user_id.into_arg()],
    )
    .await?;
    // Anonymised tables: the row stays, the person is detached.
    for (table, column) in [
        ("donations", "donor_id"),
        ("library_items", "created_by"),
        ("question_clusters", "answered_by"),
    ] {
        db.execute(
            &format!("UPDATE {table} SET {column} = NULL WHERE {column} = ?"),
            &[user_id.into_arg()],
        )
        .await?;
    }
    // Everything else that references the user is deleted.
    const DELETE_REFS: &[(&str, &str)] = &[
        ("sessions", "user_id"),
        ("refresh_tokens", "user_id"),
        ("password_reset_tokens", "user_id"),
        ("phone_verification_codes", "user_id"),
        ("recovery_codes", "user_id"),
        ("ws_tickets", "user_id"),
        ("user_activity_events", "user_id"),
        ("api_keys", "created_by"),
        ("user_devices", "user_id"),
        ("web_push_subscriptions", "user_id"),
        ("user_roles", "user_id"),
        ("user_consent", "user_id"),
        ("audit_logs", "actor_id"),
        ("notifications", "user_id"),
        ("notification_preferences", "user_id"),
        ("notification_broadcasts", "created_by"),
        ("notification_deliveries", "user_id"),
        ("media_assets", "owner_id"),
        ("fatwa_sessions", "user_id"),
        ("user_ayah_notes", "user_id"),
        ("announcements", "created_by"),
        ("fatwa_submissions", "author_id"),
        ("fatwa_submissions", "reviewed_by"),
        ("cluster_members", "user_id"),
        ("user_saved_fatwas", "user_id"),
        ("quiz_attempts", "user_id"),
        ("ai_usage_log", "user_id"),
        ("library_item_versions", "created_by"),
        ("inheritance_cases", "user_id"),
        ("zakat_calculations", "user_id"),
        ("user_video_progress", "user_id"),
        ("user_saved_videos", "user_id"),
        ("voice_sessions", "user_id"),
        ("user_progress", "user_id"),
        ("certificates", "user_id"),
        ("cohort_enrollments", "user_id"),
        ("user_reading_states", "user_id"),
        ("dhikr_sessions", "user_id"),
        ("dhikr_daily_counts", "user_id"),
        ("sync_changelog", "user_id"),
        ("sync_tombstones", "user_id"),
        ("device_cursors", "user_id"),
        ("hifz_attempts", "user_id"),
        ("hifz_mistakes", "user_id"),
        ("hifz_user_settings", "user_id"),
        ("hifz_listen_positions", "user_id"),
        ("hifz_reviews", "user_id"),
        ("hifz_plans", "user_id"),
        ("hifz_plan_items", "user_id"),
        ("hifz_tests", "user_id"),
        ("hifz_tasmi_sessions", "user_id"),
        ("build_requests", "requested_by"),
        ("build_releases", "released_by"),
        ("fiqh_knowledge_chunks", "approved_by"),
    ];
    for (table, column) in DELETE_REFS {
        db.execute(
            &format!("DELETE FROM {table} WHERE {column} = ?"),
            &[user_id.into_arg()],
        )
        .await?;
    }
    // References inside `users` itself, then the user row.
    db.execute(
        "UPDATE users SET deleted_by = NULL WHERE deleted_by = ?",
        &[user_id.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM users WHERE id = ?", &[user_id.into_arg()])
        .await?;
    Ok(())
}

/// `DELETE /api/v1/admin/users/{user_id}/permanent` — only from the
/// trash. Financial rows and library/cluster content are anonymised
/// (their reference is nulled) instead of deleted.
pub async fn permanent_delete_user(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    axum::extract::Path(user_id_str): axum::extract::Path<String>,
) -> ApiResult<ApiJson> {
    let ctx = authed(&state, &headers, "users.manage").await?;
    let user_id = parse_path_uuid(&user_id_str, "user_id")?;
    let db = state.db_or_500()?;
    let target = manage_target(db, &ctx, user_id).await?;
    let row = db
        .fetch_one(
            "SELECT email, phone, full_name, deleted_at FROM users WHERE id = ?",
            &[user_id.into_arg()],
        )
        .await?;
    let deleted_at: Option<DateTime<Utc>> = row.try_get("deleted_at")?;
    let Some(deleted_at) = deleted_at else {
        return Err(bad(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_trashed",
            "حذف دائمی فقط از داخل سطل زباله ممکن است؛ اول کاربر را حذف (منتقل به سطل) کنید",
        ));
    };
    let email: Option<String> = row.try_get("email")?;
    let phone: Option<String> = row.try_get("phone")?;
    let (ip, ua) = client_info(&headers, peer.0, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "users.delete_permanent",
        "user",
        Some(&user_id.to_string()),
        Some(json!({
            "email": email, "phone": phone,
            "full_name": target.full_name,
            "deleted_at": deleted_at.to_rfc3339(),
        })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    if let Err(e) = purge_user_graph(db, &state.config, user_id).await {
        return Err(ApiError::with_details(
            StatusCode::CONFLICT,
            "user_purge_blocked",
            format!(
                "حذف دائمی به‌خاطر ارجاع دادهٔ ناشناخته کامل نشد؛ کاربر در سطل باقی ماند ({e:?})"
            ),
            json!({}),
        ));
    }
    Ok(axum::response::Json(json!({
        "user_id": user_id.to_string(), "deleted": true, "permanent": true,
    })))
}
