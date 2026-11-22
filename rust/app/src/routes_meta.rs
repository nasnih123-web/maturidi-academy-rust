//! Meta routes — mirrors `app/modules/meta/router.py`.
//!
//! `GET /api/v1/themes/active`, `GET /api/v1/settings/public` (public
//! keys only, no auth), `GET /api/v1/i18n/detect`. Defaults are copied
//! verbatim from `_PUBLIC_DEFAULTS`, including the places globe default
//! (Mecca: lat 21.4225, lng 39.8262) and the default place types.

use axum::{extract::State, http::HeaderMap, response::Json};
use serde_json::{json, Map, Value};

use crate::{
    errors::{ApiError, ApiResult},
    settings::get_typed_setting,
    AppState,
};

fn default_theme() -> Value {
    json!({"fa": "پیش‌فرض", "ar": "الافتراضي", "en": "Default"})
}

fn default_place_types() -> Value {
    json!([
    {"key": "mosque", "emoji": "🕌", "fa": "مسجد", "ar": "مسجد", "en": "Mosque"},
    {"key": "haramain", "emoji": "🕋", "fa": "حرمین شریفین", "ar": "الحرمان الشريفان", "en": "The Two Holy Mosques"},
    {"key": "library", "emoji": "📚", "fa": "کتابخانه", "ar": "مكتبة", "en": "Library"},
    {"key": "museum", "emoji": "🏛️", "fa": "موزه", "ar": "متحف", "en": "Museum"},
    {"key": "pilgrimage", "emoji": "🌙", "fa": "مکان زیارتی", "ar": "مكان زيارة", "en": "Pilgrimage site"},
    {"key": "other", "emoji": "📍", "fa": "سایر", "ar": "أخرى", "en": "Other"}
    ])
}

/// Public keys in the original's order, with their defaults.
fn public_defaults() -> Vec<(&'static str, Value)> {
    vec![
        (
            "particles",
            json!({"enabled": true, "count": 100, "speed": 1.0, "parallax": 1.0, "lines": true}),
        ),
        ("fonts.fa", Value::Null),
        ("fonts.ar", Value::Null),
        ("fonts.en", Value::Null),
        ("hero.fa", Value::Null),
        ("hero.ar", Value::Null),
        ("hero.en", Value::Null),
        ("home.sections", json!([])),
        (
            "places.popup",
            json!({"show_image": true, "show_description": true, "show_type": true, "show_city": true}),
        ),
        ("places.types", default_place_types()),
        (
            "places.globe",
            json!({"defaultLat": 21.4225, "defaultLng": 39.8262, "defaultAltitude": 2.5, "defaultMode": "satellite", "autoRotate": false}),
        ),
        ("quran.madhhab_default", json!("hanafi")),
        ("quran.player_default_speed", json!(1)),
        ("quran.player_show_photo", json!(true)),
        ("quran.waveform_style", json!("bars")),
        ("quran.display_font", json!("system")),
        ("quran.display_theme", json!("dark")),
        (
            "quran.playback",
            json!({"autoplay": false, "repeat": "off"}),
        ),
        ("profile.require_for_fatwa_question", json!(false)),
        ("profile.require_for_hifz_advanced", json!(false)),
        ("site.social_links", json!([])),
        ("videos.home_enabled", json!(true)),
        ("videos.sidebar_side", json!("left")),
        ("videos.autoplay_next", json!(true)),
        ("videos.resume_playback", json!(true)),
        ("videos.player_speed", json!(true)),
        ("videos.player_pip", json!(true)),
        ("videos.related_enabled", json!(true)),
        ("videos.subtitle_default_lang", json!("fa")),
        ("videos.subtitles_enabled", json!(true)),
        (
            "videos.subtitle_langs",
            json!(["fa", "ar", "en", "ur", "dari"]),
        ),
    ]
}

pub async fn themes_active(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all("SELECT id, name FROM themes WHERE is_active = TRUE", &[])
        .await?;
    if rows.len() > 1 {
        return Err(ApiError::internal());
    }
    match rows.first() {
        None => Ok(Json(json!({ "name": default_theme(), "is_active": true }))),
        Some(row) => {
            let id: uuid::Uuid = row.try_get("id")?;
            let name: Value = row.try_get("name")?;
            Ok(Json(
                json!({ "id": id.to_string(), "name": name, "is_active": true }),
            ))
        }
    }
}

pub async fn settings_public(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let mut settings = Map::new();
    for (key, default) in public_defaults() {
        let value = get_typed_setting(db, &state.config, key, default).await?;
        settings.insert(key.to_string(), value);
    }
    let site_key = state.config.turnstile_site_key.trim();
    Ok(Json(json!({
        "settings": Value::Object(settings),
        "version": state.settings_version(),
        "turnstile_site_key": if site_key.is_empty() { Value::Null } else { Value::String(site_key.to_string()) },
    })))
}

pub async fn i18n_detect(headers: HeaderMap) -> Json<Value> {
    let accept = headers
        .get(axum::http::header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let mut locale = "fa";
    for part in accept.split(',') {
        let lang = part.split(';').next().unwrap_or("").trim().to_lowercase();
        if lang.starts_with("ar") {
            locale = "ar";
            break;
        }
        if lang.starts_with("en") {
            locale = "en";
            break;
        }
        if lang.starts_with("fa") {
            locale = "fa";
            break;
        }
    }
    Json(json!({ "locale": locale }))
}
