//! Public embeddable widgets — mirrors
//! `app/modules/widgets/{router,service,schemas,cors}.py` (Wave 7).
//!
//! All four routes are public in the user sense (no login) but gated
//! by an API key: `X-API-Key` header, or a signed `?embed_token=`
//! (`routes_api_keys::validate_key` / `validate_embed_token`, one
//! scope per widget). A raw key in the query string deliberately does
//! NOT authenticate (Pass 4 / A5). Each key hit consumes rate-limit
//! + monthly quota, exactly like Python.
//!
//! * `GET /api/v1/widgets/prayer-times` — PrayTimes math from the
//!   shared `adhan.rs` engine (astronomy, not a fiqh table — ADR-009
//!   does not apply; the method angles are the `adhan.py` data).
//! * `GET /api/v1/widgets/ayah-of-day` — deterministic daily rotation
//!   over ten (surah, ayah) references; the text always comes from
//!   the Quran provider registry, never hardcoded. In-process daily
//!   cache (success until midnight, 60 s negative cache); Python's
//!   Redis tier is not ported — see the ADR draft in the wave report.
//! * `GET /api/v1/widgets/latest-fatwa` — newest published fatwa.
//! * `GET /api/v1/widgets/zakat-mini` — the real Wave 6 zakat engine
//!   (`compute_zakat`) with `assets={"cash": amount}`; no approved
//!   rule → honest 503 `zakat_rule_not_configured`.
//!
//! The widget CORS wildcard (`WidgetCorsMiddleware` in Python —
//! `Access-Control-Allow-Origin: *` on `/api/v1/widgets/*`, no
//! credentials, OPTIONS → 204) is middleware, not a route: it is NOT
//! implemented in the Rust binary — see the ADR draft in the wave
//! report (deploy-layer concern).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, Datelike, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::calc_decimal::{dec_from_json, Dec};
use crate::calc_engines::{compute_zakat, ZakatRates};

use crate::{
    adhan::prayer_times as adhan_prayer_times,
    db::{new_id, utc_now, Db, IntoArg},
    errors::{ApiError, ApiResult},
    quran_data::{adhan_method, get_surah, BUNDLED_EDITION_OVERRIDES, DEFAULT_RECITERS},
    quran_providers::{
        build_provider_map, clean_custom_list, clean_keys_map, provider_unavailable, Provider,
    },
    routes_api_keys::{validate_embed_token, validate_key},
    settings::{get_setting, get_typed_setting},
    validate::parse_query,
    AppState,
};

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Widget API-key gate — `_require_key` in `widgets/router.py`.
async fn require_key(
    state: &AppState,
    headers: &HeaderMap,
    params: &HashMap<String, String>,
    scope: &str,
) -> Result<(), ApiError> {
    let raw = headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty());
    if let Some(raw) = raw {
        validate_key(state, Some(raw), scope).await?;
    } else {
        let token = params.get("embed_token").map(|s| s.as_str());
        validate_embed_token(state, token, scope).await?;
    }
    Ok(())
}

fn query_req_f64(params: &HashMap<String, String>, name: &str) -> Result<f64, ApiError> {
    match params.get(name) {
        Some(v) => match v.trim().parse::<f64>() {
            Ok(n) if n.is_finite() => Ok(n),
            _ => Err(validation_err(name, "must be a number")),
        },
        None => Err(validation_err(name, "field is required")),
    }
}

fn query_f64(
    params: &HashMap<String, String>,
    name: &str,
    default: f64,
    min: f64,
    max: f64,
) -> Result<f64, ApiError> {
    match params.get(name) {
        Some(v) => match v.trim().parse::<f64>() {
            Ok(n) if n.is_finite() && (min..=max).contains(&n) => Ok(n),
            _ => Err(validation_err(name, "out of range")),
        },
        None => Ok(default),
    }
}

// --- prayer-times ------------------------------------------------------------

/// `GET /api/v1/widgets/prayer-times` — query validation (422) runs
/// before the key gate, like FastAPI; the service's coordinate check
/// (`bad_coordinates`) runs after it, because the router puts no
/// `ge`/`le` on `lat`/`lng`.
pub async fn prayer_times(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let lat = query_req_f64(&params, "lat")?;
    let lng = query_req_f64(&params, "lng")?;
    let method = params
        .get("method")
        .cloned()
        .unwrap_or_else(|| "mwl".to_string());
    let madhab = params
        .get("madhab")
        .cloned()
        .unwrap_or_else(|| "shafi".to_string());
    let tz = query_f64(&params, "tz", 3.5, -12.0, 14.0)?;
    require_key(&state, &headers, &params, "prayer-times").await?;
    if !(-90.0..=90.0).contains(&lat) || !(-180.0..=180.0).contains(&lng) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_coordinates",
            "مختصات نامعتبر است",
        ));
    }
    let today = Utc::now().date_naive();
    let times = adhan_prayer_times(today, lat, lng, tz, &method, &madhab, None).map_err(|_| {
        let detail = if adhan_method(&method).is_none() {
            format!("unknown adhan method: {method}")
        } else {
            format!("unknown madhab: {madhab}")
        };
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_prayer_params",
            detail,
        )
    })?;
    let mut ordered = Map::new();
    for key in [
        "imsak", "fajr", "sunrise", "dhuhr", "asr", "maghrib", "isha", "midnight",
    ] {
        if let Some(v) = times.get(key) {
            ordered.insert(key.to_string(), json!(v));
        }
    }
    Ok(Json(json!({
        "date": today.format("%Y-%m-%d").to_string(),
        "lat": lat,
        "lng": lng,
        "method": method,
        "madhab": madhab,
        "tz": tz,
        "times": Value::Object(ordered),
    })))
}

// --- ayah-of-day -------------------------------------------------------------

/// Fixed (surah, ayah) references only — the text comes from the
/// provider, never from this list (`widgets/service.py`).
const AYAH_ROTATION: &[(i64, i64)] = &[
    (2, 255),
    (13, 28),
    (94, 6),
    (2, 152),
    (39, 53),
    (55, 13),
    (3, 139),
    (65, 3),
    (94, 5),
    (20, 114),
];

/// In-process ayah cache (Python's Redis tier is not ported):
/// `cache_key → (expires_at, Some(bundle) | None-on-error)`.
type AyahCache = HashMap<String, (Instant, Option<Value>)>;
static AYAH_CACHE: LazyLock<Mutex<AyahCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn ayah_success_ttl() -> Duration {
    let now = Utc::now();
    let midnight = now
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc() + chrono::Duration::days(1))
        .unwrap_or(now + chrono::Duration::days(1));
    let secs = (midnight - now).num_seconds().max(0) as u64;
    Duration::from_secs(secs.clamp(300, 21600))
}

struct EditionSelection {
    providers_by_type: HashMap<String, String>,
    provider_map: Vec<(String, Provider)>,
    translation_edition: Option<String>,
    tafsir_edition: Option<String>,
    reciter_edition: Option<String>,
}

struct GroupKw {
    text: Option<String>,
    translation: Option<String>,
    tafsir: Option<String>,
    reciter: Option<String>,
}

fn str_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// `routes_quran::edition_selection` (same defaults, no overrides).
async fn edition_selection(db: &Db, config: &crate::Config) -> Result<EditionSelection, ApiError> {
    let mut resolved: HashMap<String, String> = [
        ("audio", "alquran_cloud"),
        ("text", "self_hosted"),
        ("translation", "alquran_cloud"),
        ("tafsir", "alquran_cloud"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let stored = get_typed_setting(
        db,
        config,
        "quran.providers_by_type",
        json!({
            "audio": "alquran_cloud",
            "text": "self_hosted",
            "translation": "alquran_cloud",
            "tafsir": "alquran_cloud",
        }),
    )
    .await?;
    if let Some(obj) = stored.as_object() {
        for (key, pid) in obj {
            if resolved.contains_key(key) {
                if let Some(s) = pid.as_str().filter(|s| !s.trim().is_empty()) {
                    resolved.insert(key.clone(), s.trim().to_string());
                }
            }
        }
    }
    let translations = str_list(
        &get_typed_setting(
            db,
            config,
            "quran.translations",
            json!(["en.sahih", "fa.makarem"]),
        )
        .await?,
    );
    let tafsirs =
        str_list(&get_typed_setting(db, config, "quran.tafsirs", json!(["ar.muyassar"])).await?);
    let reciters = str_list(
        &get_typed_setting(
            db,
            config,
            "quran.reciters",
            Value::Array(
                DEFAULT_RECITERS
                    .iter()
                    .map(|r| Value::String(r.to_string()))
                    .collect(),
            ),
        )
        .await?,
    );
    let custom = clean_custom_list(
        &get_typed_setting(db, config, "quran.custom_providers", json!([])).await?,
    );
    let keys = clean_keys_map(
        &get_typed_setting(db, config, "quran.custom_provider_keys", json!({})).await?,
    );
    let provider_map = build_provider_map(&custom, &keys);
    Ok(EditionSelection {
        providers_by_type: resolved,
        provider_map,
        translation_edition: translations.first().cloned(),
        tafsir_edition: tafsirs.first().cloned(),
        reciter_edition: reciters.first().cloned(),
    })
}

fn add_group(groups: &mut Vec<(String, GroupKw)>, pid: &str, set: impl FnOnce(&mut GroupKw)) {
    if let Some(g) = groups.iter_mut().find(|(id, _)| id == pid) {
        set(&mut g.1);
    } else {
        let mut kw = GroupKw {
            text: None,
            translation: None,
            tafsir: None,
            reciter: None,
        };
        set(&mut kw);
        groups.push((pid.to_string(), kw));
    }
}

fn group_editions(sel: &EditionSelection) -> Vec<(String, GroupKw)> {
    let mut groups: Vec<(String, GroupKw)> = Vec::new();
    let text_pid = sel
        .providers_by_type
        .get("text")
        .cloned()
        .unwrap_or_default();
    add_group(&mut groups, &text_pid, |kw| {
        kw.text = Some("quran-uthmani".to_string());
    });
    if let Some(tr) = &sel.translation_edition {
        let pid = if BUNDLED_EDITION_OVERRIDES.contains(&tr.as_str()) {
            "bundled".to_string()
        } else {
            sel.providers_by_type
                .get("translation")
                .cloned()
                .unwrap_or_default()
        };
        add_group(&mut groups, &pid, |kw| {
            kw.translation = Some(tr.clone());
        });
    }
    if let Some(tf) = &sel.tafsir_edition {
        let pid = if BUNDLED_EDITION_OVERRIDES.contains(&tf.as_str()) {
            "bundled".to_string()
        } else {
            sel.providers_by_type
                .get("tafsir")
                .cloned()
                .unwrap_or_default()
        };
        add_group(&mut groups, &pid, |kw| {
            kw.tafsir = Some(tf.clone());
        });
    }
    if let Some(rec) = &sel.reciter_edition {
        let pid = sel
            .providers_by_type
            .get("audio")
            .cloned()
            .unwrap_or_default();
        add_group(&mut groups, &pid, |kw| {
            kw.reciter = Some(rec.clone());
        });
    }
    groups
}

/// `quran/service.py::get_ayah_bundle` for the rotation reference.
async fn fetch_ayah_bundle(state: &AppState, surah: i64, ayah: i64) -> Result<Value, ApiError> {
    let meta = get_surah(surah as i32)
        .ok_or_else(|| ApiError::not_found("surah_not_found", "سوره‌ای با این شماره وجود ندارد"))?;
    if !(1..=meta.ayahs as i64).contains(&ayah) {
        return Err(ApiError::not_found(
            "ayah_not_found",
            "آیه‌ای با این شماره در این سوره وجود ندارد",
        ));
    }
    let db = state.db_or_500()?;
    let sel = edition_selection(db, &state.config).await?;
    let mut merged: Vec<(String, crate::quran_providers::EditionText)> = Vec::new();
    for (pid, kw) in group_editions(&sel) {
        let Some(provider) = sel
            .provider_map
            .iter()
            .find(|(k, _)| k == &pid)
            .map(|(_, p)| p)
        else {
            return Err(provider_unavailable(&pid));
        };
        let data = provider
            .fetch_ayah(
                surah,
                ayah,
                kw.text.as_deref(),
                kw.translation.as_deref(),
                kw.tafsir.as_deref(),
                kw.reciter.as_deref(),
            )
            .await?;
        for (eid, etext) in data.editions {
            if !merged.iter().any(|(k, _)| k == &eid) {
                merged.push((eid, etext));
            }
        }
    }
    Ok(json!({
        "surah": surah,
        "ayah": ayah,
        "editions": merged.iter().map(|(id, e)| json!({
            "edition": id,
            "text": e.text,
            "audio_url": e.audio_url,
            "juz": e.juz,
            "page": e.page,
        })).collect::<Vec<_>>(),
    }))
}

/// `GET /api/v1/widgets/ayah-of-day` — `Cache-Control: public,
/// max-age=3600` on the response, like the Python router.
pub async fn ayah_of_day(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<(HeaderMap, Json<Value>)> {
    let params = parse_query(&uri);
    require_key(&state, &headers, &params, "ayah-of-day").await?;
    let today = Utc::now().date_naive();
    let idx = (today.ordinal() % AYAH_ROTATION.len() as u32) as usize;
    let (surah, ayah) = AYAH_ROTATION[idx];
    let date = today.format("%Y-%m-%d").to_string();
    let cache_key = format!("widget:ayah-of-day:{date}:{surah}:{ayah}");
    if let Ok(cache) = AYAH_CACHE.lock() {
        if let Some((expires, cached)) = cache.get(&cache_key) {
            if *expires > Instant::now() {
                match cached {
                    Some(bundle) => {
                        let mut h = HeaderMap::new();
                        h.insert("cache-control", "public, max-age=3600".parse().unwrap());
                        return Ok((
                            h,
                            Json(json!({
                                "date": date, "surah": surah, "ayah": ayah, "bundle": bundle,
                            })),
                        ));
                    }
                    None => {
                        return Err(ApiError::new(
                            StatusCode::BAD_GATEWAY,
                            "quran_provider_error",
                            "دریافت آیه ناموفق بود",
                        ));
                    }
                }
            }
        }
    }
    match fetch_ayah_bundle(&state, surah, ayah).await {
        Ok(bundle) => {
            if let Ok(mut cache) = AYAH_CACHE.lock() {
                cache.insert(
                    cache_key,
                    (Instant::now() + ayah_success_ttl(), Some(bundle.clone())),
                );
            }
            let mut h = HeaderMap::new();
            h.insert("cache-control", "public, max-age=3600".parse().unwrap());
            Ok((
                h,
                Json(json!({
                    "date": date, "surah": surah, "ayah": ayah, "bundle": bundle,
                })),
            ))
        }
        Err(_) => {
            // Any provider failure is a 502 here (Python catches the
            // generic `Exception` from the bundle function); APIError
            // 404s from the rotation itself cannot happen.
            if let Ok(mut cache) = AYAH_CACHE.lock() {
                cache.insert(cache_key, (Instant::now() + Duration::from_secs(60), None));
            }
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                "quran_provider_error",
                "دریافت آیه ناموفق بود",
            ))
        }
    }
}

// --- latest-fatwa ------------------------------------------------------------

/// `GET /api/v1/widgets/latest-fatwa` — newest `published` fatwa;
/// `published_at DESC` keeps Postgres' NULLS FIRST, like Python.
pub async fn latest_fatwa(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    require_key(&state, &headers, &params, "latest-fatwa").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, title, body, published_at FROM fatwa_submissions \
             WHERE workflow_status = 'published' ORDER BY published_at DESC LIMIT 1",
            &[],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "no_published_fatwa",
            "هنوز فتوای منتشرشده‌ای نیست",
        ));
    };
    let id: Uuid = row.try_get("id")?;
    let title: Value = row.try_get("title")?;
    let body: String = row.try_get("body")?;
    let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
    let excerpt: String = body.chars().take(400).collect();
    Ok(Json(json!({
        "id": id.to_string(),
        "title": title,
        "excerpt": excerpt,
        "published_at": published_at.map(|d| d.to_rfc3339()),
    })))
}

// --- zakat-mini --------------------------------------------------------------

const PRICE_TTL_DEFAULT: i64 = 21_600;
const CACHE_SOURCE: &str = "default";

struct PriceInfo {
    status: String,
    gold_per_gram: Option<String>,
    silver_per_gram: Option<String>,
    currency: Option<String>,
    source: String,
    fetched_at: Option<String>,
    stale: bool,
}

fn price_info_to_json(info: &PriceInfo) -> Value {
    json!({
        "status": info.status,
        "gold_per_gram": info.gold_per_gram,
        "silver_per_gram": info.silver_per_gram,
        "currency": info.currency,
        "source": info.source,
        "fetched_at": info.fetched_at,
        "stale": info.stale,
    })
}

fn price_out(
    row_gold: Option<&str>,
    row_silver: Option<&str>,
    currency: Option<String>,
    fetched_at: Option<DateTime<Utc>>,
    stale: bool,
    status: &str,
    source: &str,
) -> PriceInfo {
    let fmt6 = |v: Option<&str>| -> Option<String> {
        v.and_then(Dec::parse).and_then(|d| d.fmt_fixed(6).ok())
    };
    PriceInfo {
        status: status.to_string(),
        gold_per_gram: fmt6(row_gold),
        silver_per_gram: fmt6(row_silver),
        currency,
        source: source.to_string(),
        fetched_at: fetched_at.map(|dt| dt.to_rfc3339()),
        stale,
    }
}

async fn price_source_url(db: &Db, config: &crate::Config) -> Option<String> {
    if let Ok(url) = std::env::var("NISAB_PRICE_URL") {
        if !url.trim().is_empty() {
            return Some(url.trim().to_string());
        }
    }
    match get_setting(db, config, "zakat.price_source_url").await {
        Ok(Some(raw)) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        _ => None,
    }
}

async fn price_ttl_seconds(db: &Db, config: &crate::Config) -> i64 {
    match get_setting(db, config, "zakat.price_ttl_seconds").await {
        Ok(Some(raw)) => raw
            .trim()
            .parse::<i64>()
            .map(|v| v.max(60))
            .unwrap_or(PRICE_TTL_DEFAULT),
        _ => PRICE_TTL_DEFAULT,
    }
}

async fn fetch_live_prices(source_url: &str) -> Option<(Option<String>, Option<String>, String)> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()?;
    let resp = client.get(source_url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    if !data.is_object() {
        return None;
    }
    let parse = |v: Option<&Value>| -> Option<Option<String>> {
        match v {
            None | Some(Value::Null) => Some(None),
            Some(v) => dec_from_json(v).map(|d| Some(d.to_py_string())),
        }
    };
    let gold = parse(data.get("gold_per_gram"))?;
    let silver = parse(data.get("silver_per_gram"))?;
    let currency = match data.get("currency") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::String(_)) => "USD".to_string(),
        Some(Value::Number(n)) => n.to_string(),
        Some(Value::Bool(b)) => b.to_string(),
        _ => "USD".to_string(),
    };
    Some((gold, silver, currency))
}

struct CacheRow {
    currency: String,
    gold_per_gram: Option<String>,
    silver_per_gram: Option<String>,
    fetched_at: DateTime<Utc>,
}

async fn read_price_cache(db: &Db) -> Result<Option<CacheRow>, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT currency, CAST(gold_per_gram AS TEXT) AS gold_per_gram, \
             CAST(silver_per_gram AS TEXT) AS silver_per_gram, fetched_at \
             FROM nisab_price_cache WHERE source = ?",
            &[CACHE_SOURCE.into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Some(CacheRow {
            currency: r.try_get("currency")?,
            gold_per_gram: r.try_get("gold_per_gram")?,
            silver_per_gram: r.try_get("silver_per_gram")?,
            fetched_at: r.try_get("fetched_at")?,
        })),
        None => Ok(None),
    }
}

/// `calculators/prices.py::get_nisab_prices` — cache + TTL + live
/// fetch + stale fallback (same as `routes_calculators`).
async fn get_nisab_prices(db: &Db, config: &crate::Config) -> Result<PriceInfo, ApiError> {
    let source_url = price_source_url(db, config).await;
    let ttl = price_ttl_seconds(db, config).await;
    let cached = read_price_cache(db).await?;
    let now = utc_now();
    if let Some(row) = &cached {
        let age = (now - row.fetched_at).num_seconds();
        if age <= ttl && row.gold_per_gram.is_some() {
            return Ok(price_out(
                row.gold_per_gram.as_deref(),
                row.silver_per_gram.as_deref(),
                Some(row.currency.clone()),
                Some(row.fetched_at),
                false,
                "ok",
                "cache",
            ));
        }
    }
    if let Some(url) = &source_url {
        if let Some((gold, silver, currency)) = fetch_live_prices(url).await {
            let fetched_at = utc_now();
            db.execute(
                "INSERT INTO nisab_price_cache (id, source, currency, gold_per_gram, \
                 silver_per_gram, fetched_at, updated_at) VALUES (?, ?, ?, \
                 CAST(? AS NUMERIC(20,6)), CAST(? AS NUMERIC(20,6)), ?, ?) \
                 ON CONFLICT(source) DO UPDATE SET currency = excluded.currency, \
                 gold_per_gram = excluded.gold_per_gram, \
                 silver_per_gram = excluded.silver_per_gram, \
                 fetched_at = excluded.fetched_at, updated_at = excluded.updated_at",
                &[
                    (new_id()).into_arg(),
                    (CACHE_SOURCE).into_arg(),
                    (currency.clone()).into_arg(),
                    (gold.clone()).into_arg(),
                    (silver.clone()).into_arg(),
                    (fetched_at).into_arg(),
                    (fetched_at).into_arg(),
                ],
            )
            .await?;
            return Ok(price_out(
                gold.as_deref(),
                silver.as_deref(),
                Some(currency),
                Some(fetched_at),
                false,
                "ok",
                url,
            ));
        }
    }
    if let Some(row) = &cached {
        if row.gold_per_gram.is_some() {
            return Ok(price_out(
                row.gold_per_gram.as_deref(),
                row.silver_per_gram.as_deref(),
                Some(row.currency.clone()),
                Some(row.fetched_at),
                true,
                "stale",
                "cache",
            ));
        }
    }
    Ok(PriceInfo {
        status: "price_unavailable".to_string(),
        gold_per_gram: None,
        silver_per_gram: None,
        currency: None,
        source: source_url.unwrap_or_else(|| "unconfigured".to_string()),
        fetched_at: None,
        stale: false,
    })
}

struct ActiveRule {
    id: Uuid,
    version: i64,
    rule_data: Value,
}

/// `calculators/service.py::get_active_rule` for zakat, madhab `None`,
/// requiring `asset_categories` — but a missing rule is the widget's
/// 503, not the calculator's 422.
async fn get_active_zakat_rule(db: &Db) -> Result<ActiveRule, ApiError> {
    let rows = db
        .fetch_all(
            "SELECT id, version, madhab, rule_data, effective_from, effective_to \
             FROM zakat_rule_configs WHERE status = 'approved' ORDER BY version DESC",
            &[],
        )
        .await?;
    let now = utc_now();
    for row in rows {
        let effective_from: DateTime<Utc> = row.try_get("effective_from")?;
        let effective_to: Option<DateTime<Utc>> = row.try_get("effective_to")?;
        if effective_from > now {
            continue;
        }
        if let Some(to) = effective_to {
            if now >= to {
                continue;
            }
        }
        let row_madhab: Option<String> = row.try_get("madhab")?;
        if row_madhab.is_some() {
            continue;
        }
        let rule_data: Value = row.try_get("rule_data")?;
        if !rule_data
            .get("asset_categories")
            .is_some_and(|v| v.is_object())
        {
            continue;
        }
        let id: Uuid = row.try_get("id")?;
        let version: i32 = row.try_get("version")?;
        return Ok(ActiveRule {
            id,
            version: version as i64,
            rule_data,
        });
    }
    Err(ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "zakat_rule_not_configured",
        "پیکربندی زکات هنوز توسط عالم تأیید نشده است",
    ))
}

/// `GET /api/v1/widgets/zakat-mini` — runs the real zakat engine and
/// persists the calculation row, like Python's `calculate_zakat`.
pub async fn zakat_mini(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let amount_raw = params
        .get("amount")
        .cloned()
        .ok_or_else(|| validation_err("amount", "field is required"))?;
    require_key(&state, &headers, &params, "zakat-mini").await?;
    let amt = Dec::parse(&amount_raw).ok_or_else(|| {
        ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_amount",
            "مبلغ نامعتبر است",
        )
    })?;
    if amt < Dec::zero() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_amount",
            "مبلغ نمی‌تواند منفی باشد",
        ));
    }
    let db = state.db_or_500()?;
    let rule = get_active_zakat_rule(db).await?;
    let price_info = get_nisab_prices(db, &state.config).await?;
    let rates = ZakatRates {
        gold_per_gram: price_info.gold_per_gram.as_deref().and_then(Dec::parse),
        silver_per_gram: price_info.silver_per_gram.as_deref().and_then(Dec::parse),
        currency: price_info.currency.clone(),
        stale: price_info.stale,
    };
    let result = compute_zakat(
        &[("cash".to_string(), amt)],
        Dec::zero(),
        &rule.rule_data,
        &rates,
        rule.version,
    )
    .map_err(|e| e.to_api())?;
    let id = new_id();
    let created_at = utc_now();
    let amt_str = amt.to_py_string();
    let inputs = json!({
        "assets": { "cash": amt_str },
        "debts": "0",
        "madhab": Value::Null,
    });
    let nisab_snapshot = price_info_to_json(&price_info);
    db.execute(
        "INSERT INTO zakat_calculations (id, user_id, inputs, result, nisab_snapshot, \
         rule_config_id, idempotency_key, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            (id).into_arg(),
            (Option::<Uuid>::None).into_arg(),
            (inputs.clone()).into_arg(),
            (result.clone()).into_arg(),
            (nisab_snapshot.clone()).into_arg(),
            (rule.id).into_arg(),
            (Option::<String>::None).into_arg(),
            (created_at).into_arg(),
        ],
    )
    .await?;
    let zakat_due = result
        .get("zakat_due")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let nisab = result
        .get("nisab_value")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    Ok(Json(json!({
        "amount": amt_str,
        "currency": price_info.currency,
        "zakat_due": zakat_due,
        "nisab": nisab,
        "detail": {
            "id": id,
            "inputs": inputs,
            "result": result,
            "nisab_snapshot": nisab_snapshot,
            "rule_config_id": rule.id,
            "created_at": created_at.to_rfc3339(),
        },
    })))
}
