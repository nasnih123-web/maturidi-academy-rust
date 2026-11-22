//! Quran reader + notes + tasbih + adhan routes — mirrors
//! `app/modules/quran/{router,service,schemas}.py`.
//!
//! `GET /api/v1/quran/surahs`, `/surah/{n}`, `/ayah/{s}/{a}`,
//! `/recitation/reciters`, `/recitation/timing`, `/timing/generated`,
//! `/providers`, `PUT /registry/custom-providers/{id}/key`,
//! `/munajat/status`, notes, reading-state; `POST /api/v1/tasbih/...`;
//! `GET /api/v1/adhan/times|range|options|geocode|reverse`.
//!
//! Text comes from the provider registry (`quran_providers.rs`):
//! self-hosted Tanzil text by default, AlQuran Cloud for
//! translation/tafsir/audio, bundled files for the four QUL editions.
//! Provider failure is an honest 503, never a silent fallback.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
};
use chrono::{DateTime, NaiveDate, Timelike, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{
    adhan::{near_torbat, prayer_times, table_day_times},
    audit::emit_event,
    authn::{authenticate, require_permission},
    db::{new_id, utc_now, Arg, Db, IntoArg},
    errors::{ApiError, ApiResult},
    quran_data::{
        adhan_city, adhan_method, get_surah as surah_meta, ADHAN_CITIES, ADHAN_METHODS,
        BUNDLED_EDITION_OVERRIDES, DEFAULT_RECITERS, DEFAULT_RECITER_PHOTOS, TIMING_RECITERS,
        TIMING_RECITER_LABELS,
    },
    quran_providers::{
        build_provider_map, bundled_available, catalog_entries, clean_custom_list, clean_keys_map,
        provider_unavailable, AyahFetch, Provider, SurahFetch,
    },
    settings::{get_typed_setting, stage_setting},
    sync::{record_entity_change, upsert_tombstone},
    validate::{parse_body, parse_path_uuid, parse_query, query_int},
    AppState, Config,
};

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn not_found(code: &'static str, message: &str) -> ApiError {
    ApiError::not_found(code, message)
}

fn bad_request(code: &'static str, message: &str) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, code, message)
}

/// `get_typed_setting` with the Python `_DEFAULTS` value.
pub(crate) async fn qset(
    db: &Db,
    config: &Config,
    key: &str,
    default: Value,
) -> Result<Value, ApiError> {
    get_typed_setting(db, config, key, default).await
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

fn iso_now(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

fn parse_path_int(s: &str, field: &str) -> Result<i64, ApiError> {
    s.parse::<i64>()
        .map_err(|_| validation_err(field, "must be an integer"))
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

fn query_req_f64(
    params: &HashMap<String, String>,
    name: &str,
    min: f64,
    max: f64,
) -> Result<f64, ApiError> {
    match params.get(name) {
        Some(v) => match v.trim().parse::<f64>() {
            Ok(n) if n.is_finite() && (min..=max).contains(&n) => Ok(n),
            _ => Err(validation_err(name, "out of range")),
        },
        None => Err(validation_err(name, "field is required")),
    }
}

fn query_bool_opt(params: &HashMap<String, String>, name: &str) -> Result<Option<bool>, ApiError> {
    match params.get(name) {
        Some(v) => match v.trim().to_lowercase().as_str() {
            "true" | "yes" | "on" | "y" | "1" => Ok(Some(true)),
            "false" | "no" | "off" | "n" | "0" => Ok(Some(false)),
            _ => Err(validation_err(name, "must be a boolean")),
        },
        None => Ok(None),
    }
}

fn query_date(params: &HashMap<String, String>, name: &str) -> Result<Option<NaiveDate>, ApiError> {
    match params.get(name) {
        Some(v) => NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d")
            .map(Some)
            .map_err(|_| validation_err(name, "must be a date")),
        None => Ok(None),
    }
}

// --- provider resolution (service._edition_selection / _group_...) -----

struct EditionSelection {
    providers_by_type: HashMap<String, String>,
    provider_map: Vec<(String, Provider)>,
    translation_edition: Option<String>,
    tafsir_edition: Option<String>,
    reciter_edition: Option<String>,
}

fn find_provider<'a>(map: &'a [(String, Provider)], id: &str) -> Option<&'a Provider> {
    map.iter().find(|(k, _)| k == id).map(|(_, p)| p)
}

async fn resolve_providers_by_type(
    db: &Db,
    config: &Config,
) -> Result<HashMap<String, String>, ApiError> {
    let mut resolved: HashMap<String, String> = [
        ("audio", "alquran_cloud"),
        ("text", "self_hosted"),
        ("translation", "alquran_cloud"),
        ("tafsir", "alquran_cloud"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let stored = qset(
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
    Ok(resolved)
}

async fn edition_selection(
    db: &Db,
    config: &Config,
    reciter: Option<&str>,
) -> Result<EditionSelection, ApiError> {
    let providers_by_type = resolve_providers_by_type(db, config).await?;
    let translations = str_list(
        &qset(
            db,
            config,
            "quran.translations",
            json!(["en.sahih", "fa.makarem"]),
        )
        .await?,
    );
    let tafsirs = str_list(&qset(db, config, "quran.tafsirs", json!(["ar.muyassar"])).await?);
    let reciters = str_list(
        &qset(
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
    let mut reciter_edition = reciters.first().cloned();
    if let Some(r) = reciter.filter(|r| reciters.iter().any(|x| x == r)) {
        reciter_edition = Some(r.to_string());
    }
    let custom = clean_custom_list(&qset(db, config, "quran.custom_providers", json!([])).await?);
    let keys = clean_keys_map(&qset(db, config, "quran.custom_provider_keys", json!({})).await?);
    let provider_map = build_provider_map(&custom, &keys);
    Ok(EditionSelection {
        providers_by_type,
        provider_map,
        translation_edition: translations.first().cloned(),
        tafsir_edition: tafsirs.first().cloned(),
        reciter_edition,
    })
}

struct GroupKw {
    text: Option<String>,
    translation: Option<String>,
    tafsir: Option<String>,
    reciter: Option<String>,
}

/// Group the requested editions by provider, in first-insertion order
/// (text group first, then translation/tafsir/audio).
fn group_editions(
    sel: &EditionSelection,
    translation: Option<&str>,
    tafsir: Option<&str>,
) -> Vec<(String, GroupKw)> {
    let by_type = &sel.providers_by_type;
    let mut groups: Vec<(String, GroupKw)> = Vec::new();
    let text_pid = by_type.get("text").cloned().unwrap_or_default();
    add_group(&mut groups, &text_pid, |kw| {
        kw.text = Some("quran-uthmani".to_string());
    });
    let tr = translation
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| sel.translation_edition.clone());
    if let Some(tr) = tr {
        let pid = if BUNDLED_EDITION_OVERRIDES.contains(&tr.as_str()) {
            "bundled".to_string()
        } else {
            by_type.get("translation").cloned().unwrap_or_default()
        };
        add_group(&mut groups, &pid, |kw| kw.translation = Some(tr.clone()));
    }
    let tf = tafsir
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| sel.tafsir_edition.clone());
    if let Some(tf) = tf {
        let pid = if BUNDLED_EDITION_OVERRIDES.contains(&tf.as_str()) {
            "bundled".to_string()
        } else {
            by_type.get("tafsir").cloned().unwrap_or_default()
        };
        add_group(&mut groups, &pid, |kw| kw.tafsir = Some(tf.clone()));
    }
    if let Some(rec) = &sel.reciter_edition {
        let pid = by_type.get("audio").cloned().unwrap_or_default();
        add_group(&mut groups, &pid, |kw| kw.reciter = Some(rec.clone()));
    }
    groups
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

fn ayah_out_json(
    surah: i64,
    ayah: i64,
    editions: &[(String, crate::quran_providers::EditionText)],
) -> Value {
    json!({
        "surah": surah,
        "ayah": ayah,
        "editions": editions.iter().map(|(id, e)| json!({
            "edition": id,
            "text": e.text,
            "audio_url": e.audio_url,
            "juz": e.juz,
            "page": e.page,
        })).collect::<Vec<_>>(),
    })
}

// --- surahs / bundles ------------------------------------------------------

/// `GET /api/v1/quran/surahs`.
pub async fn list_surahs() -> Json<Value> {
    let items: Vec<Value> = crate::quran_data::SURAHS
        .iter()
        .map(|s| {
            json!({
                "number": s.number,
                "name_ar": s.name_ar,
                "name_en": s.name_en,
                "ayahs": s.ayahs,
                "revelation": s.revelation,
            })
        })
        .collect();
    let total = items.len();
    Json(json!({ "items": items, "total": total }))
}

/// `GET /api/v1/quran/surah/{number}`.
pub async fn get_surah(
    State(state): State<AppState>,
    Path(number_s): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let number = parse_path_int(&number_s, "number")?;
    let meta = surah_meta(number as i32)
        .ok_or_else(|| not_found("surah_not_found", "سوره‌ای با این شماره وجود ندارد"))?;
    let params = parse_query(&uri);
    let db = state.db_or_500()?;
    let sel =
        edition_selection(db, &state.config, params.get("reciter").map(|s| s.as_str())).await?;
    let mut merged: HashMap<i64, Vec<(String, crate::quran_providers::EditionText)>> =
        HashMap::new();
    let mut edition_ids: Vec<String> = Vec::new();
    for (pid, kw) in group_editions(
        &sel,
        params.get("translation").map(|s| s.as_str()),
        params.get("tafsir").map(|s| s.as_str()),
    ) {
        let Some(provider) = find_provider(&sel.provider_map, &pid) else {
            return Err(provider_unavailable(&pid));
        };
        let data: SurahFetch = provider
            .fetch_surah(
                number,
                kw.text.as_deref(),
                kw.translation.as_deref(),
                kw.tafsir.as_deref(),
                kw.reciter.as_deref(),
            )
            .await?;
        for id in &data.edition_ids {
            if !edition_ids.contains(id) {
                edition_ids.push(id.clone());
            }
        }
        for (n, eds) in data.ayahs {
            let slot = merged.entry(n).or_default();
            for (eid, etext) in eds {
                if !slot.iter().any(|(k, _)| k == &eid) {
                    slot.push((eid, etext));
                }
            }
        }
    }
    let mut keys: Vec<i64> = merged.keys().copied().collect();
    keys.sort_unstable();
    let ayahs: Vec<Value> = keys
        .into_iter()
        .map(|n| ayah_out_json(number, n, &merged[&n]))
        .collect();
    Ok(Json(json!({
        "surah": number,
        "name_ar": meta.name_ar,
        "name_en": meta.name_en,
        "ayahs": ayahs,
        "edition_ids": edition_ids,
    })))
}

/// `GET /api/v1/quran/ayah/{surah}/{ayah}`.
pub async fn get_ayah(
    State(state): State<AppState>,
    Path((surah_s, ayah_s)): Path<(String, String)>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let surah = parse_path_int(&surah_s, "surah")?;
    let ayah = parse_path_int(&ayah_s, "ayah")?;
    let meta = surah_meta(surah as i32)
        .ok_or_else(|| not_found("surah_not_found", "سوره‌ای با این شماره وجود ندارد"))?;
    if !(1..=meta.ayahs as i64).contains(&ayah) {
        return Err(not_found(
            "ayah_not_found",
            "آیه‌ای با این شماره در این سوره وجود ندارد",
        ));
    }
    let params = parse_query(&uri);
    let db = state.db_or_500()?;
    let sel =
        edition_selection(db, &state.config, params.get("reciter").map(|s| s.as_str())).await?;
    let mut merged: Vec<(String, crate::quran_providers::EditionText)> = Vec::new();
    for (pid, kw) in group_editions(
        &sel,
        params.get("translation").map(|s| s.as_str()),
        params.get("tafsir").map(|s| s.as_str()),
    ) {
        let Some(provider) = find_provider(&sel.provider_map, &pid) else {
            return Err(provider_unavailable(&pid));
        };
        let data: AyahFetch = provider
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
    Ok(Json(ayah_out_json(surah, ayah, &merged)))
}

// --- recitation timing -----------------------------------------------------

fn timing_error(detail: &str) -> ApiError {
    ApiError::with_details(
        StatusCode::SERVICE_UNAVAILABLE,
        "recitation_timing_unavailable",
        "زمان‌بندی تلاوت در دسترس نیست؛ لطفاً کمی بعد تلاش کنید",
        json!({ "detail": detail }),
    )
}

/// `GET /api/v1/quran/recitation/reciters`.
pub async fn list_recitation_reciters() -> Json<Value> {
    let reciters: Vec<Value> = TIMING_RECITER_LABELS
        .iter()
        .map(|(id, label)| json!({ "id": id, "label": label }))
        .collect();
    let mut generated: Vec<(String, i64)> = generated_available();
    generated.sort();
    Json(json!({
        "reciters": reciters,
        "generated_reciters": generated
            .iter()
            .map(|(id, n)| json!({ "id": id, "surahs": n }))
            .collect::<Vec<_>>(),
    }))
}

fn generated_manifest() -> Option<&'static Value> {
    static MANIFEST: OnceLock<Option<Value>> = OnceLock::new();
    MANIFEST
        .get_or_init(|| {
            serde_json::from_str(include_str!("../data/quran/timings/manifest.json")).ok()
        })
        .as_ref()
}

/// edition → complete surahs count, from the timings manifest
/// (aliases inherit their base edition's count).
fn generated_available() -> Vec<(String, i64)> {
    let Some(manifest) = generated_manifest() else {
        return Vec::new();
    };
    let mut out: HashMap<String, i64> = HashMap::new();
    if let Some(editions) = manifest.get("editions").and_then(|e| e.as_object()) {
        for (edition, entry) in editions {
            let done = entry
                .get("surahs_done")
                .and_then(|v| v.as_array())
                .map(|a| a.len() as i64)
                .unwrap_or(0);
            if done > 0 {
                out.insert(edition.clone(), done);
            }
        }
    }
    if let Some(shared) = manifest.get("shared").and_then(|s| s.as_object()) {
        for (alias, base) in shared {
            if let Some(b) = base.as_str().and_then(|b| out.get(b).copied()) {
                out.insert(alias.clone(), b);
            }
        }
    }
    out.into_iter().collect()
}

/// `GET /api/v1/quran/recitation/timing/generated`.
pub async fn get_generated_timing(
    State(_state): State<AppState>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let edition = params
        .get("edition")
        .cloned()
        .ok_or_else(|| validation_err("edition", "field is required"))?;
    let surah = query_int(&params, "surah", 0, 1, 114)?;
    let Some(manifest) = generated_manifest() else {
        return Err(timing_error("timing manifest unavailable"));
    };
    let base = manifest
        .get("shared")
        .and_then(|s| s.get(&edition))
        .and_then(|b| b.as_str())
        .unwrap_or(&edition)
        .to_string();
    let done: HashSet<i64> = manifest
        .get("editions")
        .and_then(|e| e.get(&base))
        .and_then(|e| e.get("surahs_done"))
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();
    if !done.contains(&surah) {
        return Err(ApiError::with_details(
            StatusCode::SERVICE_UNAVAILABLE,
            "recitation_timing_unavailable",
            "زمان‌بندی تلاوت در دسترس نیست؛ لطفاً کمی بعد تلاش کنید",
            json!({
                "edition": edition,
                "surah": surah,
                "detail": format!("no generated timing for {edition} surah {surah}"),
            }),
        ));
    }
    let raw: &str = match (base.as_str(), surah) {
        ("ar.husary", 1) => include_str!("../data/quran/timings/ar.husary/001.json"),
        ("ar.husary", 67) => include_str!("../data/quran/timings/ar.husary/067.json"),
        _ => {
            return Err(ApiError::with_details(
                StatusCode::SERVICE_UNAVAILABLE,
                "recitation_timing_unavailable",
                "زمان‌بندی تلاوت در دسترس نیست؛ لطفاً کمی بعد تلاش کنید",
                json!({
                    "edition": edition,
                    "surah": surah,
                    "detail": format!("no generated timing for {edition} surah {surah}"),
                }),
            ));
        }
    };
    let body: Value = serde_json::from_str(raw).map_err(|e| timing_error(&e.to_string()))?;
    let mut ayahs: Vec<Value> = Vec::new();
    for entry in body
        .get("ayahs")
        .and_then(|a| a.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let Some(ayah_no) = entry.get("ayah").and_then(|v| v.as_i64()) else {
            return Err(timing_error("generated timing malformed"));
        };
        let duration_ms = entry
            .get("duration_ms")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let mut words: Vec<Value> = Vec::new();
        for w in entry
            .get("words")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let (Some(word), Some(start), Some(end)) = (
                w.get("word").and_then(|v| v.as_i64()),
                w.get("start_ms").and_then(|v| v.as_i64()),
                w.get("end_ms").and_then(|v| v.as_i64()),
            ) else {
                return Err(timing_error("generated timing malformed"));
            };
            if end > start {
                words.push(json!({ "word": word, "start_ms": start, "end_ms": end }));
            }
        }
        words.sort_by_key(|w| w["start_ms"].as_i64().unwrap_or(0));
        ayahs.push(json!({
            "ayah": ayah_no,
            "audio_url": entry.get("audio_url").cloned().unwrap_or(Value::Null),
            "duration_ms": duration_ms,
            "words": words,
        }));
    }
    if ayahs.is_empty() {
        return Err(timing_error("generated timing empty"));
    }
    ayahs.sort_by_key(|a| a["ayah"].as_i64().unwrap_or(0));
    Ok(Json(json!({
        "reciter": edition,
        "surah": surah,
        "source": "phoneme_alignment",
        "ayahs": ayahs,
    })))
}

fn timing_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(15));
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                if let Ok(p) = reqwest::Proxy::all(v.trim()) {
                    builder = builder.proxy(p);
                }
                break;
            }
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

static TIMING_CACHE: OnceLock<Mutex<HashMap<(String, i64), Value>>> = OnceLock::new();

fn timing_cache() -> &'static Mutex<HashMap<(String, i64), Value>> {
    TIMING_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn normalize_timing(
    reciter: &str,
    surah: i64,
    audio_url: &str,
    timestamps: &[Value],
) -> Result<Value, String> {
    let mut ayahs: Vec<Value> = Vec::new();
    for entry in timestamps {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let ayah_no = obj
            .get("verse_key")
            .and_then(|v| v.as_str())
            .and_then(|k| k.split(':').nth(1))
            .and_then(|n| n.parse::<i64>().ok());
        let Some(ayah_no) = ayah_no else {
            continue;
        };
        let (Some(from_ms), Some(to_ms)) = (
            obj.get("timestamp_from").and_then(|v| v.as_i64()),
            obj.get("timestamp_to").and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        if to_ms <= from_ms {
            continue;
        }
        let mut words: Vec<Value> = Vec::new();
        let mut seen: HashSet<i64> = HashSet::new();
        for seg in obj
            .get("segments")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default()
        {
            let Some(arr) = seg.as_array() else {
                continue;
            };
            if arr.len() != 3 {
                continue;
            }
            let (Some(word), Some(start), Some(end)) = (
                arr[0].as_i64().or_else(|| {
                    arr[0]
                        .as_f64()
                        .filter(|f| f.fract() == 0.0)
                        .map(|f| f as i64)
                }),
                arr[1].as_i64().or_else(|| {
                    arr[1]
                        .as_f64()
                        .filter(|f| f.fract() == 0.0)
                        .map(|f| f as i64)
                }),
                arr[2].as_i64().or_else(|| {
                    arr[2]
                        .as_f64()
                        .filter(|f| f.fract() == 0.0)
                        .map(|f| f as i64)
                }),
            ) else {
                continue;
            };
            if seen.contains(&word) || word < 1 || end <= start {
                continue;
            }
            let rel_start = (start - from_ms).max(0);
            let rel_end = (end - from_ms).max(0);
            if rel_end <= rel_start {
                continue;
            }
            seen.insert(word);
            words.push(json!({ "word": word, "start_ms": rel_start, "end_ms": rel_end }));
        }
        words.sort_by_key(|w| w["start_ms"].as_i64().unwrap_or(0));
        ayahs.push(json!({
            "ayah": ayah_no,
            "from_ms": from_ms,
            "to_ms": to_ms,
            "duration_ms": to_ms - from_ms,
            "words": words,
        }));
    }
    ayahs.sort_by_key(|a| a["ayah"].as_i64().unwrap_or(0));
    Ok(json!({
        "reciter": reciter,
        "surah": surah,
        "audio_url": audio_url,
        "source": "quran_com_v4",
        "ayahs": ayahs,
    }))
}

async fn fetch_live_timing(reciter: &str, surah: i64) -> Result<Value, String> {
    let Some((_, qid)) = TIMING_RECITERS.iter().find(|(id, _)| *id == reciter) else {
        return Err(format!("unknown reciter: {reciter}"));
    };
    if !(1..=114).contains(&surah) {
        return Err(format!("invalid surah: {surah}"));
    }
    if let Ok(cache) = timing_cache().lock() {
        if let Some(v) = cache.get(&(reciter.to_string(), surah)) {
            return Ok(v.clone());
        }
    }
    let url =
        format!("https://api.quran.com/api/v4/chapter_recitations/{qid}/{surah}?segments=true");
    let client = timing_client();
    let mut last_err = String::new();
    for attempt in 0..3 {
        match client.get(&url).send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.as_u16() == 429 || status.is_server_error() {
                    last_err = format!("upstream {status}");
                    tokio::time::sleep(Duration::from_millis((500 * (attempt + 1)) as u64)).await;
                    continue;
                }
                if !status.is_success() {
                    return Err(format!("quran.com returned HTTP {status}"));
                }
                let body: Value = resp
                    .json()
                    .await
                    .map_err(|e| format!("invalid JSON: {e}"))?;
                let Some(audio_file) = body.get("audio_file").and_then(|v| v.as_object()) else {
                    return Err("missing audio_file".to_string());
                };
                let Some(audio_url) = audio_file.get("audio_url").and_then(|v| v.as_str()) else {
                    return Err("missing audio_url".to_string());
                };
                if audio_url.is_empty() {
                    return Err("missing audio_url".to_string());
                }
                let Some(timestamps) = audio_file.get("timestamps").and_then(|v| v.as_array())
                else {
                    return Err("missing timestamps".to_string());
                };
                if timestamps.is_empty() {
                    return Err("missing timestamps".to_string());
                }
                let result = normalize_timing(reciter, surah, audio_url, timestamps)?;
                if result["ayahs"]
                    .as_array()
                    .map(|a| a.is_empty())
                    .unwrap_or(true)
                {
                    return Err("no ayahs in response".to_string());
                }
                if let Ok(mut cache) = timing_cache().lock() {
                    cache.insert((reciter.to_string(), surah), result.clone());
                }
                return Ok(result);
            }
            Err(e) => {
                last_err = e.to_string();
                tokio::time::sleep(Duration::from_millis((500 * (attempt + 1)) as u64)).await;
            }
        }
    }
    Err(format!(
        "quran.com unreachable after 3 attempts: {last_err}"
    ))
}

/// `GET /api/v1/quran/recitation/timing`.
pub async fn get_recitation_timing(uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let reciter = params
        .get("reciter")
        .cloned()
        .ok_or_else(|| validation_err("reciter", "field is required"))?;
    let surah = query_int(&params, "surah", 0, 1, 114)?;
    match fetch_live_timing(&reciter, surah).await {
        Ok(v) => Ok(Json(v)),
        Err(detail) => Err(ApiError::with_details(
            StatusCode::SERVICE_UNAVAILABLE,
            "recitation_timing_unavailable",
            "زمان‌بندی تلاوت در دسترس نیست؛ لطفاً کمی بعد تلاش کنید",
            json!({ "reciter": reciter, "surah": surah, "detail": detail }),
        )),
    }
}

// --- providers registry ----------------------------------------------------

/// `GET /api/v1/quran/providers`.
pub async fn get_providers(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let config = &state.config;
    let by_type = resolve_providers_by_type(db, config).await?;
    let custom = clean_custom_list(&qset(db, config, "quran.custom_providers", json!([])).await?);
    let keys = clean_keys_map(&qset(db, config, "quran.custom_provider_keys", json!({})).await?);
    let provider_map = build_provider_map(&custom, &keys);
    let mut providers: Vec<Value> = Vec::new();
    for (pid, p) in &provider_map {
        let types: Vec<&str> = match pid.as_str() {
            "alquran_cloud" => vec!["audio", "text", "translation", "tafsir"],
            "self_hosted" => vec!["text"],
            "bundled" => vec!["translation", "tafsir"],
            _ => custom
                .iter()
                .find(|c| c.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()))
                .and_then(|c| c.get("type").and_then(|v| v.as_str()))
                .into_iter()
                .collect(),
        };
        providers.push(json!({
            "id": p.id(),
            "name": p.name(),
            "base_url": p.base_url(),
            "active": by_type.values().any(|v| v == pid),
            "types": types,
            "implemented": true,
        }));
    }
    let text_pid = by_type.get("text").cloned().unwrap_or_default();
    if find_provider(&provider_map, &text_pid).is_none() {
        return Err(ApiError::with_details(
            StatusCode::SERVICE_UNAVAILABLE,
            "quran_provider_unavailable",
            "provider فعال قرآن شناخته‌شده نیست",
            json!({ "provider": text_pid }),
        ));
    }
    let mut catalog = catalog_entries();
    let qf_base = qset(
        db,
        config,
        "quran.qf_base_url",
        json!("https://api.quran.com/api/v4"),
    )
    .await?;
    if let Some(entry) = catalog
        .iter_mut()
        .find(|e| e.get("id").and_then(|v| v.as_str()) == Some("quran_foundation"))
    {
        entry["base_url"] = qf_base.clone();
    }
    let qf_secret = qset(db, config, "quran.qf_client_secret", json!("")).await?;
    let qf_secret_set = qf_secret
        .as_str()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let custom_keys_set: Vec<String> = {
        let mut ids: Vec<String> = keys
            .keys()
            .filter(|pid| {
                custom
                    .iter()
                    .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(pid.as_str()))
            })
            .cloned()
            .collect();
        ids.sort();
        ids
    };
    let (bundled_tr, bundled_tf) = bundled_available();
    let translations = qset(
        db,
        config,
        "quran.translations",
        json!(["en.sahih", "fa.makarem"]),
    )
    .await?;
    let tafsirs = qset(db, config, "quran.tafsirs", json!(["ar.muyassar"])).await?;
    let reciters = qset(
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
    .await?;
    let photos_raw = qset(
        db,
        config,
        "quran.reciter_photos",
        Value::Object(
            DEFAULT_RECITER_PHOTOS
                .iter()
                .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
                .collect(),
        ),
    )
    .await?;
    let mut photos = Map::new();
    if let Some(obj) = photos_raw.as_object() {
        for (k, v) in obj {
            if let Some(s) = v.as_str().filter(|s| !s.trim().is_empty()) {
                photos.insert(k.clone(), Value::String(s.to_string()));
            }
        }
    }
    let active_languages = qset(
        db,
        config,
        "quran.active_languages",
        json!(["fa", "ar", "en"]),
    )
    .await?;
    let show_translation = qset(db, config, "quran.show_translation", json!(true)).await?;
    let show_tafsir = qset(db, config, "quran.show_tafsir", json!(true)).await?;
    let highlight_mode = qset(db, config, "quran.highlight_mode", json!("ayah")).await?;
    let font_size_raw = qset(db, config, "quran.font_size", json!(20)).await?;
    let font_size = Value::from(font_size_raw.as_f64().unwrap_or(20.0));
    let playback = qset(
        db,
        config,
        "quran.playback",
        json!({"autoplay": false, "repeat": "off"}),
    )
    .await?;
    let qf_client_id = qset(db, config, "quran.qf_client_id", json!("")).await?;
    let custom_providers: Vec<Value> = custom
        .iter()
        .map(|c| {
            json!({
                "id": c.get("id").cloned().unwrap_or(Value::Null),
                "name": c.get("name").cloned().unwrap_or(Value::Null),
                "base_url": c.get("base_url").cloned().unwrap_or(Value::Null),
                "type": c.get("type").cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    Ok(Json(json!({
        "providers": providers,
        "active_provider": text_pid,
        "text_edition": "quran-uthmani",
        "translations": translations,
        "tafsirs": tafsirs,
        "bundled_translations": bundled_tr,
        "bundled_tafsirs": bundled_tf,
        "reciters": reciters,
        "reciter_photos": photos,
        "active_languages": active_languages,
        "show_translation": show_translation,
        "show_tafsir": show_tafsir,
        "highlight_mode": highlight_mode,
        "font_size": font_size,
        "playback": playback,
        "providers_by_type": by_type,
        "custom_providers": custom_providers,
        "provider_catalog": catalog,
        "qf_base_url": qf_base,
        "qf_client_id": qf_client_id,
        "qf_client_secret_set": qf_secret_set,
        "custom_provider_keys_set": custom_keys_set,
    })))
}

/// `PUT /api/v1/quran/registry/custom-providers/{provider_id}/key`
/// (`platform.settings`). Sets or deletes a custom provider's key;
/// the key value is never returned.
pub async fn set_custom_provider_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(provider_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/registry/custom-providers").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let parsed = parse_body(&body)?;
    // Pydantic `CustomProviderKeyIn.key`: `str | None`, max_length=500.
    let key: Option<String> = match parsed.get("key").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() > 500 {
                return Err(validation_err("key", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("key", "must be a string")),
        None => None,
    };
    let db = state.db_or_500()?;
    let custom =
        clean_custom_list(&qset(db, &state.config, "quran.custom_providers", json!([])).await?);
    if !custom
        .iter()
        .any(|c| c.get("id").and_then(|v| v.as_str()) == Some(provider_id.as_str()))
    {
        return Err(not_found(
            "custom_provider_not_found",
            "provider سفارشی با این شناسه ثبت نشده است",
        ));
    }
    let mut keys =
        clean_keys_map(&qset(db, &state.config, "quran.custom_provider_keys", json!({})).await?);
    if let Some(k) = key.filter(|k| !k.trim().is_empty()) {
        keys.insert(provider_id.clone(), k);
    } else {
        keys.remove(&provider_id);
    }
    let key_set = keys.contains_key(&provider_id);
    let plaintext = serde_json::to_string(&keys).unwrap_or_else(|_| "{}".to_string());
    stage_setting(db, &state.config, "quran.custom_provider_keys", &plaintext).await?;
    // No settings-version bump is needed: Rust reads settings from
    // the DB on every request (no settings cache), unlike Python.
    Ok(Json(
        json!({ "provider_id": provider_id, "key_set": key_set }),
    ))
}

// --- munajat ---------------------------------------------------------------

/// `GET /api/v1/quran/munajat/status`.
pub async fn munajat_status(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let tz = query_f64(&params, "tz", 0.0, -12.0, 14.0)?;
    let db = state.db_or_500()?;
    let enabled = qset(db, &state.config, "munajat.enabled", json!(true))
        .await?
        .as_bool()
        .unwrap_or(true);
    let window = qset(
        db,
        &state.config,
        "munajat.window",
        json!({"start": "02:00", "end": "05:00"}),
    )
    .await?;
    let start_s = window
        .get("start")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let end_s = window
        .get("end")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let now_utc = Utc::now();
    let local = now_utc + chrono::Duration::milliseconds((tz * 3_600_000.0) as i64);
    let mut in_window = false;
    if enabled {
        let parse = |s: &str| -> Option<i64> {
            let (h, m) = s.split_once(':')?;
            let h: i64 = h.parse().ok()?;
            let m: i64 = m.parse().ok()?;
            if !(0..24).contains(&h) || !(0..60).contains(&m) {
                return None;
            }
            Some(h * 60 + m)
        };
        if let (Some(sh), Some(eh)) = (
            start_s.as_deref().and_then(parse),
            end_s.as_deref().and_then(parse),
        ) {
            let cur = local.hour() as i64 * 60 + local.minute() as i64;
            in_window = if sh <= eh {
                (sh..eh).contains(&cur)
            } else {
                cur >= sh || cur < eh
            };
        }
    }
    Ok(Json(json!({
        "enabled": enabled,
        "window": { "start": start_s, "end": end_s },
        "in_window": in_window,
        "server_time_utc": iso_now(now_utc),
    })))
}

// --- ayah notes --------------------------------------------------------------

fn note_out(row: &crate::db::DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let surah: i32 = row.try_get("surah")?;
    let ayah: i32 = row.try_get("ayah")?;
    let note_text: String = row.try_get("note_text")?;
    let is_private: bool = row.try_get("is_private")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "surah": surah,
        "ayah": ayah,
        "note_text": note_text,
        "is_private": is_private,
        "created_at": iso_now(created_at),
    }))
}

async fn get_owned_note(
    db: &Db,
    user_id: Uuid,
    note_id: Uuid,
) -> Result<crate::db::DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM user_ayah_notes WHERE id = ?",
            &[(note_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != user_id {
                return Err(not_found(
                    "note_not_found",
                    "یادداشتی با این شناسه یافت نشد",
                ));
            }
            Ok(r)
        }
        None => Err(not_found(
            "note_not_found",
            "یادداشتی با این شناسه یافت نشد",
        )),
    }
}

/// `POST /api/v1/quran/notes` (201).
pub async fn create_note(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/notes").await?;
    let parsed = parse_body(&body)?;
    let surah = crate::validate::req_int_loose(&parsed, "surah", 1, 114)?;
    let ayah = crate::validate::req_int_loose(&parsed, "ayah", 1, 286)?;
    let note_text = crate::validate::req_str_raw(&parsed, "note_text", 1, 5000)?;
    let is_private = crate::validate::opt_bool_loose(&parsed, "is_private")?.unwrap_or(true);
    let Some(meta) = surah_meta(surah as i32) else {
        return Err(not_found(
            "surah_not_found",
            "سوره‌ای با این شماره وجود ندارد",
        ));
    };
    if !(1..=meta.ayahs as i64).contains(&ayah) {
        return Err(not_found(
            "ayah_not_found",
            "آیه‌ای با این شماره در این سوره وجود ندارد",
        ));
    }
    let db = state.db_or_500()?;
    let id = new_id();
    let now = utc_now();
    let text = note_text.trim().to_string();
    db.execute(
        "INSERT INTO user_ayah_notes (id, user_id, surah, ayah, note_text, is_private, \
         version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 1, ?, NULL)",
        &[
            (id).into_arg(),
            (ctx.user.id).into_arg(),
            (surah).into_arg(),
            (ayah).into_arg(),
            (text.clone()).into_arg(),
            (is_private).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "quran_note",
        &id.to_string(),
        "quran.note_created",
        json!({
            "user_id": ctx.user.id.to_string(),
            "surah": surah,
            "ayah": ayah,
            "is_private": is_private,
        }),
    )
    .await?;
    record_entity_change(
        db,
        ctx.user.id,
        "api",
        "user_ayah_note",
        &id.to_string(),
        "upsert",
        Some(json!({
            "id": id.to_string(),
            "surah": surah,
            "ayah": ayah,
            "note_text": text,
            "is_private": is_private,
            "version": 1,
        })),
        1,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id.to_string(),
            "surah": surah,
            "ayah": ayah,
            "note_text": text,
            "is_private": is_private,
            "created_at": iso_now(now),
        })),
    ))
}

/// `GET /api/v1/quran/notes`.
pub async fn list_notes(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/notes").await?;
    let params = parse_query(&uri);
    let surah = crate::validate::query_opt_int(&params, "surah", 1, 114)?;
    let ayah = crate::validate::query_opt_int(&params, "ayah", 1, i64::MAX)?;
    let limit = crate::validate::query_int(&params, "limit", 20, 1, 50)?;
    let cursor = params.get("after").cloned();
    let db = state.db_or_500()?;
    let mut sql = "SELECT * FROM user_ayah_notes WHERE user_id = ?".to_string();
    let mut args: Vec<crate::db::Arg> = vec![(ctx.user.id).into_arg()];
    if let Some(s) = surah {
        sql.push_str(" AND surah = ?");
        args.push((s).into_arg());
    }
    if let Some(a) = ayah {
        sql.push_str(" AND ayah = ?");
        args.push((a).into_arg());
    }
    if let Some(c) = &cursor {
        let (ts, nid) = crate::validate::decode_cursor(c)?;
        sql.push_str(" AND ((created_at < ?) OR (created_at = ? AND id < ?))");
        args.push((ts).into_arg());
        args.push((ts).into_arg());
        args.push((nid).into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut next_cursor: Option<String> = None;
    for (i, row) in rows.iter().enumerate() {
        if i as i64 >= limit {
            let created_at: DateTime<Utc> = rows[(limit - 1) as usize].try_get("created_at")?;
            let last_id: Uuid = rows[(limit - 1) as usize].try_get("id")?;
            next_cursor = Some(crate::validate::encode_cursor(created_at, last_id));
            break;
        }
        items.push(note_out(row)?);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `PUT /api/v1/quran/notes/{note_id}`.
pub async fn update_note(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(note_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/notes").await?;
    let note_id = parse_path_uuid(&note_id_s, "note_id")?;
    let parsed = parse_body(&body)?;
    let note_text = match parsed.get("note_text").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() < 1 || s.chars().count() > 5000 {
                return Err(validation_err("note_text", "length out of range"));
            }
            Some(s.trim().to_string())
        }
        Some(_) => return Err(validation_err("note_text", "must be a string")),
        None => None,
    };
    let is_private = crate::validate::opt_bool_loose(&parsed, "is_private")?;
    let db = state.db_or_500()?;
    let row = get_owned_note(db, ctx.user.id, note_id).await?;
    let version: i32 = row.try_get("version")?;
    let new_version = version + 1;
    let now = utc_now();
    let cur_text: String = row.try_get("note_text")?;
    let cur_private: bool = row.try_get("is_private")?;
    let surah: i32 = row.try_get("surah")?;
    let ayah: i32 = row.try_get("ayah")?;
    let final_text = note_text.unwrap_or(cur_text);
    let final_private = is_private.unwrap_or(cur_private);
    db.execute(
        "UPDATE user_ayah_notes SET note_text = ?, is_private = ?, version = ?, \
         updated_at = ? WHERE id = ?",
        &[
            (final_text.clone()).into_arg(),
            (final_private).into_arg(),
            (new_version).into_arg(),
            (now).into_arg(),
            (note_id).into_arg(),
        ],
    )
    .await?;
    record_entity_change(
        db,
        ctx.user.id,
        "api",
        "user_ayah_note",
        &note_id.to_string(),
        "upsert",
        Some(json!({
            "id": note_id.to_string(),
            "surah": surah,
            "ayah": ayah,
            "note_text": final_text,
            "is_private": final_private,
            "version": new_version,
        })),
        new_version as i64,
    )
    .await?;
    let row = get_owned_note(db, ctx.user.id, note_id).await?;
    Ok(Json(note_out(&row)?))
}

/// `DELETE /api/v1/quran/notes/{note_id}` (204).
pub async fn delete_note(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(note_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/notes").await?;
    let note_id = parse_path_uuid(&note_id_s, "note_id")?;
    let db = state.db_or_500()?;
    let row = get_owned_note(db, ctx.user.id, note_id).await?;
    let version: i32 = row.try_get("version")?;
    db.execute(
        "DELETE FROM user_ayah_notes WHERE id = ?",
        &[(note_id).into_arg()],
    )
    .await?;
    upsert_tombstone(
        db,
        ctx.user.id,
        "user_ayah_note",
        &note_id.to_string(),
        version as i64,
    )
    .await?;
    record_entity_change(
        db,
        ctx.user.id,
        "api",
        "user_ayah_note",
        &note_id.to_string(),
        "delete",
        None,
        version as i64,
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- reading state -----------------------------------------------------------

fn validate_bookmark(v: &Value, idx: usize) -> Result<Value, ApiError> {
    let f = |field: &str| format!("bookmarks[{idx}].{field}");
    let obj = v
        .as_object()
        .ok_or_else(|| validation_err(&f(""), "must be an object"))?;
    let id = obj
        .get("id")
        .and_then(|x| x.as_str())
        .ok_or_else(|| validation_err(&f("id"), "field is required"))?;
    if id.chars().count() < 1 || id.chars().count() > 128 {
        return Err(validation_err(&f("id"), "length out of range"));
    }
    let kind = obj
        .get("kind")
        .and_then(|x| x.as_str())
        .ok_or_else(|| validation_err(&f("kind"), "field is required"))?;
    if kind != "ayah" && kind != "page" {
        return Err(validation_err(&f("kind"), "invalid kind"));
    }
    let surah = obj
        .get("surah")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| validation_err(&f("surah"), "field is required"))?;
    if !(1..=114).contains(&surah) {
        return Err(validation_err(&f("surah"), "out of range"));
    }
    let mut out = json!({
        "id": id,
        "kind": kind,
        "surah": surah,
    });
    if let Some(a) = obj.get("ayah").filter(|x| !x.is_null()) {
        let n = a
            .as_i64()
            .ok_or_else(|| validation_err(&f("ayah"), "must be an integer"))?;
        if !(1..=286).contains(&n) {
            return Err(validation_err(&f("ayah"), "out of range"));
        }
        out["ayah"] = json!(n);
    }
    if let Some(p) = obj.get("page").filter(|x| !x.is_null()) {
        let n = p
            .as_i64()
            .ok_or_else(|| validation_err(&f("page"), "must be an integer"))?;
        if !(1..=623).contains(&n) {
            return Err(validation_err(&f("page"), "out of range"));
        }
        out["page"] = json!(n);
    }
    let mushaf = obj
        .get("mushaf")
        .and_then(|x| x.as_str())
        .ok_or_else(|| validation_err(&f("mushaf"), "field is required"))?;
    if mushaf.chars().count() < 1 || mushaf.chars().count() > 64 {
        return Err(validation_err(&f("mushaf"), "length out of range"));
    }
    out["mushaf"] = json!(mushaf);
    let label = obj
        .get("label")
        .and_then(|x| x.as_str())
        .ok_or_else(|| validation_err(&f("label"), "field is required"))?;
    if label.chars().count() > 500 {
        return Err(validation_err(&f("label"), "length out of range"));
    }
    out["label"] = json!(label);
    let favorite = match obj.get("favorite").filter(|x| !x.is_null()) {
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(validation_err(&f("favorite"), "must be a boolean")),
        None => false,
    };
    out["favorite"] = json!(favorite);
    let created_at = obj
        .get("createdAt")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| validation_err(&f("createdAt"), "field is required"))?;
    if created_at < 0 {
        return Err(validation_err(&f("createdAt"), "out of range"));
    }
    out["createdAt"] = json!(created_at);
    Ok(out)
}

fn validate_last_read(v: &Value) -> Result<Value, ApiError> {
    let f = |field: &str| format!("last_read.{field}");
    let obj = v
        .as_object()
        .ok_or_else(|| validation_err("last_read", "must be an object"))?;
    let surah = obj
        .get("surah")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| validation_err(&f("surah"), "field is required"))?;
    if !(1..=114).contains(&surah) {
        return Err(validation_err(&f("surah"), "out of range"));
    }
    let view = obj
        .get("view")
        .and_then(|x| x.as_str())
        .ok_or_else(|| validation_err(&f("view"), "field is required"))?;
    if view.chars().count() < 1 || view.chars().count() > 16 {
        return Err(validation_err(&f("view"), "length out of range"));
    }
    let mut out = json!({ "surah": surah, "view": view });
    if let Some(t) = obj.get("target").filter(|x| !x.is_null()) {
        let n = t
            .as_i64()
            .ok_or_else(|| validation_err(&f("target"), "must be an integer"))?;
        if n < 1 {
            return Err(validation_err(&f("target"), "out of range"));
        }
        out["target"] = json!(n);
    }
    if let Some(a) = obj.get("ayah").filter(|x| !x.is_null()) {
        let n = a
            .as_i64()
            .ok_or_else(|| validation_err(&f("ayah"), "must be an integer"))?;
        if !(1..=286).contains(&n) {
            return Err(validation_err(&f("ayah"), "out of range"));
        }
        out["ayah"] = json!(n);
    }
    let mushaf = obj
        .get("mushaf")
        .and_then(|x| x.as_str())
        .ok_or_else(|| validation_err(&f("mushaf"), "field is required"))?;
    if mushaf.chars().count() < 1 || mushaf.chars().count() > 64 {
        return Err(validation_err(&f("mushaf"), "length out of range"));
    }
    out["mushaf"] = json!(mushaf);
    let saved_at = obj
        .get("savedAt")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| validation_err(&f("savedAt"), "field is required"))?;
    if saved_at < 0 {
        return Err(validation_err(&f("savedAt"), "out of range"));
    }
    out["savedAt"] = json!(saved_at);
    Ok(out)
}

async fn read_state_row(db: &Db, user_id: Uuid) -> Result<Value, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT bookmarks, last_read, state_updated_at FROM user_reading_states WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    match row {
        None => Ok(json!({ "bookmarks": [], "last_read": Value::Null, "updated_at": 0 })),
        Some(r) => {
            let bookmarks: Value = r.try_get("bookmarks")?;
            let last_read: Option<Value> = r.try_get("last_read")?;
            let updated_at: i64 = r.try_get("state_updated_at")?;
            Ok(json!({
                "bookmarks": bookmarks,
                "last_read": last_read.unwrap_or(Value::Null),
                "updated_at": updated_at,
            }))
        }
    }
}

/// `GET /api/v1/quran/reading-state`.
pub async fn get_reading_state(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/reading-state").await?;
    let db = state.db_or_500()?;
    Ok(Json(read_state_row(db, ctx.user.id).await?))
}

/// `PUT /api/v1/quran/reading-state` — latest `updated_at` wins; a
/// stale PUT answers 409 with the stored state as the body (not the
/// error envelope), exactly like Python.
pub async fn put_reading_state(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/reading-state").await?;
    let parsed = parse_body(&body)?;
    let bookmarks_raw = parsed
        .get("bookmarks")
        .filter(|v| !v.is_null())
        .ok_or_else(|| validation_err("bookmarks", "field is required"))?;
    let bookmarks_arr = bookmarks_raw
        .as_array()
        .ok_or_else(|| validation_err("bookmarks", "must be a list"))?;
    if bookmarks_arr.len() > 5000 {
        return Err(validation_err("bookmarks", "length out of range"));
    }
    let mut bookmarks: Vec<Value> = Vec::new();
    for (i, b) in bookmarks_arr.iter().enumerate() {
        bookmarks.push(validate_bookmark(b, i)?);
    }
    let last_read = match parsed.get("last_read").filter(|v| !v.is_null()) {
        Some(v) => Some(validate_last_read(v)?),
        None => None,
    };
    let updated_at = parsed
        .get("updated_at")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| validation_err("updated_at", "field is required"))?;
    if updated_at < 0 {
        return Err(validation_err("updated_at", "out of range"));
    }
    let db = state.db_or_500()?;
    let stored_row = db
        .fetch_optional(
            "SELECT state_updated_at FROM user_reading_states WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    if let Some(r) = &stored_row {
        let stored: i64 = r.try_get("state_updated_at")?;
        if updated_at < stored {
            let current = read_state_row(db, ctx.user.id).await?;
            return Ok((StatusCode::CONFLICT, Json(current)).into_response());
        }
    }
    let now = utc_now();
    let bookmarks_value = Value::Array(bookmarks);
    let last_read_arg = match last_read.clone() {
        Some(v) => v.into_arg(),
        None => Arg::NullJson,
    };
    if stored_row.is_some() {
        db.execute(
            "UPDATE user_reading_states SET bookmarks = ?, last_read = ?, state_updated_at = ?, \
             updated_at = ? WHERE user_id = ?",
            &[
                (bookmarks_value.clone()).into_arg(),
                last_read_arg,
                (updated_at).into_arg(),
                (now).into_arg(),
                (ctx.user.id).into_arg(),
            ],
        )
        .await?;
    } else {
        db.execute(
            "INSERT INTO user_reading_states (user_id, bookmarks, last_read, state_updated_at, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
            &[
                (ctx.user.id).into_arg(),
                (bookmarks_value.clone()).into_arg(),
                last_read_arg,
                (updated_at).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
    }
    let current = read_state_row(db, ctx.user.id).await?;
    Ok((StatusCode::OK, Json(current)).into_response())
}

// --- tasbih ------------------------------------------------------------------

fn session_out(row: &crate::db::DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let dhikr_text: String = row.try_get("dhikr_text")?;
    let target_count: Option<i32> = row.try_get("target_count")?;
    let count: i32 = row.try_get("count")?;
    let started_at: DateTime<Utc> = row.try_get("started_at")?;
    let ended_at: Option<DateTime<Utc>> = row.try_get("ended_at")?;
    let completed = target_count.map(|t| count >= t).unwrap_or(false);
    Ok(json!({
        "id": id.to_string(),
        "dhikr_text": dhikr_text,
        "target_count": target_count,
        "count": count,
        "started_at": iso_now(started_at),
        "ended_at": ended_at.map(iso_now),
        "completed": completed,
    }))
}

static INCREMENT_KEYS: OnceLock<Mutex<HashMap<String, Instant>>> = OnceLock::new();

fn claim_increment_key(key: &str) -> bool {
    let map = INCREMENT_KEYS.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = map.lock() else {
        return true;
    };
    let now = Instant::now();
    guard.retain(|_, ts| now.duration_since(*ts) <= Duration::from_secs(600));
    if guard.contains_key(key) {
        return false;
    }
    guard.insert(key.to_string(), now);
    true
}

/// `POST /api/v1/tasbih/sessions` (201).
pub async fn tasbih_start(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/tasbih/sessions").await?;
    let parsed = parse_body(&body)?;
    let dhikr_text = crate::validate::req_str_raw(&parsed, "dhikr_text", 1, 255)?;
    let target_count = crate::validate::opt_int_loose(&parsed, "target_count", 1, 1_000_000)?;
    let db = state.db_or_500()?;
    let id = new_id();
    let now = utc_now();
    let text = dhikr_text.trim().to_string();
    let target_arg = match target_count {
        Some(t) => (t).into_arg(),
        None => Arg::NullInt,
    };
    db.execute(
        "INSERT INTO dhikr_sessions (id, user_id, dhikr_text, target_count, count, \
         started_at, ended_at) VALUES (?, ?, ?, ?, 0, ?, NULL)",
        &[
            (id).into_arg(),
            (ctx.user.id).into_arg(),
            (text.clone()).into_arg(),
            target_arg,
            (now).into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "dhikr_session",
        &id.to_string(),
        "tasbih.session_started",
        json!({
            "user_id": ctx.user.id.to_string(),
            "dhikr_text": text,
            "target_count": target_count,
        }),
    )
    .await?;
    let row = db
        .fetch_one(
            "SELECT * FROM dhikr_sessions WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    Ok((StatusCode::CREATED, Json(session_out(&row)?)))
}

async fn get_owned_session(
    db: &Db,
    user_id: Uuid,
    session_id: Uuid,
) -> Result<crate::db::DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM dhikr_sessions WHERE id = ?",
            &[(session_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != user_id {
                return Err(not_found(
                    "session_not_found",
                    "نشست تسبیحی با این شناسه یافت نشد",
                ));
            }
            Ok(r)
        }
        None => Err(not_found(
            "session_not_found",
            "نشست تسبیحی با این شناسه یافت نشد",
        )),
    }
}

/// `POST /api/v1/tasbih/sessions/{session_id}/count`.
pub async fn tasbih_count(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/tasbih/sessions").await?;
    let session_id = parse_path_uuid(&session_id_s, "session_id")?;
    let parsed = parse_body(&body)?;
    let amount = crate::validate::opt_int_loose(&parsed, "amount", 1, 1000)?.unwrap_or(1);
    let idempotency_key = match parsed.get("idempotency_key").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() > 64 {
                return Err(validation_err("idempotency_key", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("idempotency_key", "must be a string")),
        None => None,
    };
    let db = state.db_or_500()?;
    let row = get_owned_session(db, ctx.user.id, session_id).await?;
    let ended_at: Option<DateTime<Utc>> = row.try_get("ended_at")?;
    if ended_at.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "session_ended",
            "این نشست تسبیح پایان یافته است",
        ));
    }
    if let Some(key) = &idempotency_key {
        let claim = format!("{}:{session_id}:{key}", ctx.user.id);
        if !claim_increment_key(&claim) {
            let row = get_owned_session(db, ctx.user.id, session_id).await?;
            return Ok(Json(session_out(&row)?));
        }
    }
    let count: i32 = row.try_get("count")?;
    let target_count: Option<i32> = row.try_get("target_count")?;
    let dhikr_text: String = row.try_get("dhikr_text")?;
    let new_count = count + amount as i32;
    let now = utc_now();
    let completed_now = target_count.map(|t| new_count >= t).unwrap_or(false);
    if completed_now {
        db.execute(
            "UPDATE dhikr_sessions SET count = ?, ended_at = ? WHERE id = ?",
            &[
                (new_count).into_arg(),
                (now).into_arg(),
                (session_id).into_arg(),
            ],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE dhikr_sessions SET count = ? WHERE id = ?",
            &[(new_count).into_arg(), (session_id).into_arg()],
        )
        .await?;
    }
    // Daily aggregate upsert (UTC date).
    let today = now.date_naive();
    let daily = db
        .fetch_optional(
            "SELECT id, total_count, version FROM dhikr_daily_counts \
             WHERE user_id = ? AND date = ? AND dhikr_text = ?",
            &[
                (ctx.user.id).into_arg(),
                (today).into_arg(),
                (dhikr_text.clone()).into_arg(),
            ],
        )
        .await?;
    let (daily_total, daily_version) = match daily {
        None => {
            let did = new_id();
            db.execute(
                "INSERT INTO dhikr_daily_counts (id, user_id, date, dhikr_text, total_count, \
                 version, updated_at) VALUES (?, ?, ?, ?, ?, 1, ?)",
                &[
                    (did).into_arg(),
                    (ctx.user.id).into_arg(),
                    (today).into_arg(),
                    (dhikr_text.clone()).into_arg(),
                    (amount).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
            (amount as i32, 1)
        }
        Some(r) => {
            let total: i32 = r.try_get("total_count")?;
            let version: i32 = r.try_get("version")?;
            let new_total = total + amount as i32;
            let new_version = version + 1;
            db.execute(
                "UPDATE dhikr_daily_counts SET total_count = ?, version = ?, updated_at = ? \
                 WHERE user_id = ? AND date = ? AND dhikr_text = ?",
                &[
                    (new_total).into_arg(),
                    (new_version).into_arg(),
                    (now).into_arg(),
                    (ctx.user.id).into_arg(),
                    (today).into_arg(),
                    (dhikr_text.clone()).into_arg(),
                ],
            )
            .await?;
            (new_total, new_version)
        }
    };
    record_entity_change(
        db,
        ctx.user.id,
        "api",
        "dhikr_daily_count",
        &format!("{}:{dhikr_text}", today.format("%Y-%m-%d")),
        "upsert",
        Some(json!({
            "date": today.format("%Y-%m-%d").to_string(),
            "dhikr_text": dhikr_text,
            "total_count": daily_total,
            "version": daily_version,
        })),
        daily_version as i64,
    )
    .await?;
    if completed_now {
        emit_event(
            db,
            "dhikr_session",
            &session_id.to_string(),
            "tasbih.session_completed",
            json!({
                "user_id": ctx.user.id.to_string(),
                "dhikr_text": dhikr_text,
                "count": new_count,
                "target_count": target_count,
            }),
        )
        .await?;
    }
    let row = get_owned_session(db, ctx.user.id, session_id).await?;
    Ok(Json(session_out(&row)?))
}

/// `GET /api/v1/tasbih/today`.
pub async fn tasbih_today(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/tasbih/today").await?;
    let db = state.db_or_500()?;
    let today = Utc::now().date_naive();
    let rows = db
        .fetch_all(
            "SELECT dhikr_text, total_count FROM dhikr_daily_counts \
             WHERE user_id = ? AND date = ? ORDER BY total_count DESC",
            &[(ctx.user.id).into_arg(), (today).into_arg()],
        )
        .await?;
    let totals: Vec<Value> = rows
        .iter()
        .map(|r| {
            let t: String = r.try_get("dhikr_text").unwrap_or_default();
            let n: i32 = r.try_get("total_count").unwrap_or(0);
            json!({ "dhikr_text": t, "total_count": n })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "date": today.format("%Y-%m-%d").to_string(),
        "totals": totals,
    })))
}

// --- adhan -------------------------------------------------------------------

const ADHAN_METHOD_IDS: &[&str] = &[
    "council", "tehran", "isna", "mwl", "egypt", "karachi", "makkah", "qom", "jafari", "france",
];

fn resolve_location(
    params: &HashMap<String, String>,
) -> Result<(Option<String>, f64, f64), ApiError> {
    if let Some(city) = params.get("city") {
        let Some((lat, lng)) = adhan_city(city) else {
            return Err(bad_request(
                "invalid_city",
                &format!("شهر ناشناخته: {city}"),
            ));
        };
        return Ok((Some(city.clone()), lat, lng));
    }
    let lat = params.get("lat").map(|s| s.as_str());
    let lng = params.get("lng").map(|s| s.as_str());
    match (lat, lng) {
        (Some(la), Some(ln)) => {
            let lat_v: f64 = la
                .trim()
                .parse()
                .map_err(|_| validation_err("lat", "must be a number"))?;
            let lng_v: f64 = ln
                .trim()
                .parse()
                .map_err(|_| validation_err("lng", "must be a number"))?;
            if !(-90.0..=90.0).contains(&lat_v) {
                return Err(validation_err("lat", "out of range"));
            }
            if !(-180.0..=180.0).contains(&lng_v) {
                return Err(validation_err("lng", "out of range"));
            }
            Ok((None, lat_v, lng_v))
        }
        _ => Err(bad_request(
            "missing_location",
            "شهر یا مختصات (lat/lng) لازم است",
        )),
    }
}

/// One day of adhan times: (times, overall source, per-prayer sources).
type AdhanDayTimes = (
    HashMap<&'static str, String>,
    String,
    HashMap<&'static str, &'static str>,
);

fn adhan_times_for_day(
    day: NaiveDate,
    lat: f64,
    lng: f64,
    tz: f64,
    method: &str,
    madhab: &str,
    ramadan: Option<bool>,
) -> Result<AdhanDayTimes, ApiError> {
    if method == "council" && near_torbat(lat, lng) {
        if let Some((times, sources, source)) = table_day_times(day, madhab, tz) {
            return Ok((times, source.to_string(), sources));
        }
    }
    let times = prayer_times(day, lat, lng, tz, method, madhab, ramadan)
        .map_err(|_| bad_request("invalid_adhan_params", "پارامترهای اذان نامعتبر است"))?;
    let mut sources: HashMap<&'static str, &'static str> = HashMap::new();
    for k in ["fajr", "sunrise", "dhuhr", "asr", "maghrib", "isha"] {
        sources.insert(k, "computed");
    }
    sources.insert("imsak", "derived");
    sources.insert("midnight", "derived");
    Ok((times, "computed".to_string(), sources))
}

fn times_json(times: &HashMap<&'static str, String>) -> Value {
    json!({
        "imsak": times["imsak"],
        "fajr": times["fajr"],
        "sunrise": times["sunrise"],
        "dhuhr": times["dhuhr"],
        "asr": times["asr"],
        "maghrib": times["maghrib"],
        "isha": times["isha"],
        "midnight": times["midnight"],
    })
}

/// `GET /api/v1/adhan/times`.
pub async fn adhan_times(uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let (city, lat, lng) = resolve_location(&params)?;
    let method = params
        .get("method")
        .cloned()
        .unwrap_or_else(|| "council".to_string());
    if !ADHAN_METHOD_IDS.contains(&method.as_str()) {
        return Err(validation_err("method", "invalid method"));
    }
    let madhab = params
        .get("madhab")
        .cloned()
        .unwrap_or_else(|| "hanafi".to_string());
    if madhab != "shafi" && madhab != "hanafi" {
        return Err(validation_err("madhab", "invalid madhab"));
    }
    let tz = query_f64(&params, "tz", 0.0, -12.0, 14.0)?;
    let ramadan = query_bool_opt(&params, "ramadan")?;
    let day = query_date(&params, "day")?
        .or(query_date(&params, "date")?)
        .unwrap_or_else(|| Utc::now().date_naive());
    let (times, source, sources) =
        adhan_times_for_day(day, lat, lng, tz, &method, &madhab, ramadan)?;
    let method_label = adhan_method(&method).map(|m| m.label).unwrap_or("");
    Ok(Json(json!({
        "date": day.format("%Y-%m-%d").to_string(),
        "latitude": lat,
        "longitude": lng,
        "timezone_offset": tz,
        "method": method,
        "method_label": method_label,
        "madhab": madhab,
        "times": times_json(&times),
        "city": city,
        "source": source,
        "sources": sources,
    })))
}

/// `GET /api/v1/adhan/range`.
pub async fn adhan_range(uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let start = query_date(&params, "start")?
        .ok_or_else(|| validation_err("start", "field is required"))?;
    let end =
        query_date(&params, "end")?.ok_or_else(|| validation_err("end", "field is required"))?;
    if end < start {
        return Err(bad_request(
            "invalid_range",
            "تاریخ پایان باید پس از تاریخ شروع باشد",
        ));
    }
    if (end - start).num_days() > 365 {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "range_too_long",
            "بازه حداکثر ۳۶۶ روز است",
        ));
    }
    let (city, lat, lng) = resolve_location(&params)?;
    let method = params
        .get("method")
        .cloned()
        .unwrap_or_else(|| "council".to_string());
    if !ADHAN_METHOD_IDS.contains(&method.as_str()) {
        return Err(validation_err("method", "invalid method"));
    }
    let madhab = params
        .get("madhab")
        .cloned()
        .unwrap_or_else(|| "hanafi".to_string());
    if madhab != "shafi" && madhab != "hanafi" {
        return Err(validation_err("madhab", "invalid madhab"));
    }
    let tz = query_f64(&params, "tz", 0.0, -12.0, 14.0)?;
    let ramadan = query_bool_opt(&params, "ramadan")?;
    let mut days: Vec<Value> = Vec::new();
    let mut cur = start;
    while cur <= end {
        let (times, source, _) = adhan_times_for_day(cur, lat, lng, tz, &method, &madhab, ramadan)?;
        days.push(json!({
            "date": cur.format("%Y-%m-%d").to_string(),
            "source": source,
            "times": times_json(&times),
        }));
        cur += chrono::Duration::days(1);
    }
    Ok(Json(json!({
        "start": start.format("%Y-%m-%d").to_string(),
        "end": end.format("%Y-%m-%d").to_string(),
        "city": city,
        "latitude": lat,
        "longitude": lng,
        "timezone_offset": tz,
        "method": method,
        "madhab": madhab,
        "days": days,
    })))
}

/// `GET /api/v1/adhan/options`.
pub async fn adhan_options() -> Json<Value> {
    let methods: Vec<Value> = ADHAN_METHODS
        .iter()
        .map(|m| {
            json!({
                "id": m.id,
                "label": m.label,
                "authority": m.authority,
                "fajr": m.fajr,
                "isha": m.isha,
                "maghrib": m.maghrib,
                "isha_minutes": m.isha_minutes,
                "isha_minutes_ramadan": m.isha_minutes_ramadan,
                "available": true,
                "note": m.note,
            })
        })
        .collect();
    let cities: Vec<Value> = ADHAN_CITIES
        .iter()
        .map(|(slug, lat, lng)| json!({ "slug": slug, "lat": lat, "lng": lng }))
        .collect();
    Json(json!({ "methods": methods, "cities": cities }))
}

// --- geocode (Nominatim proxy) ----------------------------------------------

static GEOCODE_CACHE: OnceLock<Mutex<HashMap<String, (Instant, Value)>>> = OnceLock::new();

fn geocode_cache() -> &'static Mutex<HashMap<String, (Instant, Value)>> {
    GEOCODE_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn geocode_cache_get(key: &str) -> Option<Value> {
    let map = geocode_cache().lock().ok()?;
    let (ts, v) = map.get(key)?;
    if ts.elapsed() > Duration::from_secs(3600) {
        return None;
    }
    Some(v.clone())
}

fn geocode_cache_set(key: &str, value: Value) {
    if let Ok(mut map) = geocode_cache().lock() {
        if map.len() > 1000 {
            map.clear();
        }
        map.insert(key.to_string(), (Instant::now(), value));
    }
}

fn geocode_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent("Maturidi-Academy/1.0 (prayer-times page)")
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn nominatim_get(path: &str, params: &[(&str, String)]) -> Result<Value, ApiError> {
    let upstream = || {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "geocode_upstream_unavailable",
            "سرویس جست‌وجوی مکان در دسترس نیست",
        )
    };
    let resp = geocode_client()
        .get(format!("https://nominatim.openstreetmap.org{path}"))
        .query(params)
        .send()
        .await
        .map_err(|_| upstream())?;
    if !resp.status().is_success() {
        return Err(upstream());
    }
    resp.json::<Value>().await.map_err(|_| upstream())
}

fn short_name(address: &Value, display_name: &str) -> Option<String> {
    for key in ["village", "town", "city", "municipality", "county", "state"] {
        if let Some(s) = address
            .get(key)
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
        {
            return Some(s.trim().to_string());
        }
    }
    let first = display_name.split(',').next()?.trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

fn place_from_item(item: &Value) -> Result<Value, ApiError> {
    let upstream = || {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "geocode_upstream_unavailable",
            "سرویس جست‌وجوی مکان در دسترس نیست",
        )
    };
    let address = item.get("address").cloned().unwrap_or(Value::Null);
    let display_name = item
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let lat: f64 = item
        .get("lat")
        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(upstream)?;
    let lng: f64 = item
        .get("lon")
        .and_then(|v| v.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(upstream)?;
    Ok(json!({
        "name": short_name(&address, &display_name),
        "display_name": display_name,
        "lat": lat,
        "lng": lng,
    }))
}

/// `GET /api/v1/adhan/geocode`.
pub async fn adhan_geocode(uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let q = params
        .get("q")
        .cloned()
        .ok_or_else(|| validation_err("q", "field is required"))?;
    if q.trim().is_empty() || q.trim().chars().count() > 200 {
        return Err(bad_request("invalid_query", "عبارت جست‌وجو نامعتبر است"));
    }
    let q = q.trim().to_string();
    let cache_key = format!("search:{}", q.to_lowercase());
    if let Some(cached) = geocode_cache_get(&cache_key) {
        return Ok(Json(json!({ "results": cached })));
    }
    let data = nominatim_get(
        "/search",
        &[
            ("format", "json".to_string()),
            ("addressdetails", "1".to_string()),
            ("limit", "8".to_string()),
            ("accept-language", "fa".to_string()),
            ("q", q.clone()),
        ],
    )
    .await?;
    let Some(list) = data.as_array() else {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "geocode_upstream_unavailable",
            "سرویس جست‌وجوی مکان در دسترس نیست",
        ));
    };
    let mut results: Vec<Value> = Vec::new();
    for item in list {
        if item.is_object() {
            results.push(place_from_item(item)?);
        }
    }
    geocode_cache_set(&cache_key, Value::Array(results.clone()));
    Ok(Json(json!({ "results": results })))
}

/// `GET /api/v1/adhan/reverse`.
pub async fn adhan_reverse(uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let lat = query_req_f64(&params, "lat", -90.0, 90.0)?;
    let lng = query_req_f64(&params, "lng", -180.0, 180.0)?;
    let cache_key = format!("reverse:{lat:.4}:{lng:.4}");
    if let Some(cached) = geocode_cache_get(&cache_key) {
        return Ok(Json(cached));
    }
    let data = nominatim_get(
        "/reverse",
        &[
            ("format", "json".to_string()),
            ("lat", lat.to_string()),
            ("lon", lng.to_string()),
            ("zoom", "10".to_string()),
            ("addressdetails", "1".to_string()),
            ("accept-language", "fa".to_string()),
        ],
    )
    .await?;
    if !data.is_object() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "geocode_upstream_unavailable",
            "سرویس جست‌وجوی مکان در دسترس نیست",
        ));
    }
    let place = place_from_item(&data)?;
    geocode_cache_set(&cache_key, place.clone());
    Ok(Json(place))
}
