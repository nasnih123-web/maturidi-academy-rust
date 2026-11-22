//! Home + global search routes — mirrors
//! `app/modules/home/{router,admin_router,schemas,service}.py`.
//!
//! `GET /api/v1/home/sections` (public): visible sections in
//! `sort_order` + theme overrides from `platform_settings` (`theme.*`).
//! `GET /api/v1/search` (public): lexical search over sections, themes,
//! published fatwas, public library items, active public courses and
//! published CMS pages — scored in Rust exactly like `service.search`
//! (prefix > substring, title > body, recency ≤0.15 over 180 days,
//! course popularity ≤0.15; the fatwa semantic boost stays inactive:
//! it only fires when the embedding provider is not `"hash"`, and no
//! real provider is ported — see ADR-018).
//! `PUT /api/v1/admin/home/sections/{key}/data`
//! (`platform.settings`): replace a section's `data` payload.

use std::collections::HashMap;

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{
    authn::{authenticate, require_permission},
    db::{utc_now, IntoArg},
    errors::{ApiError, ApiResult},
    localized::{as_localized, locale_texts, localized_json},
    settings::get_setting,
    validate::parse_body,
    AppState,
};

const SCAN_CAP: i64 = 500;
const RECENCY_MAX: f64 = 0.15;
const POPULARITY_MAX: f64 = 0.15;

const TYPE_ORDER: &[(&str, i64)] = &[
    ("section", 0),
    ("fatwa", 1),
    ("library", 2),
    ("course", 3),
    ("page", 4),
    ("theme", 5),
];

fn type_order(t: &str) -> i64 {
    TYPE_ORDER
        .iter()
        .find(|(name, _)| *name == t)
        .map(|(_, n)| *n)
        .unwrap_or(9)
}

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// `GET /api/v1/home/sections`.
pub async fn home_sections(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT key, sort_order, visible, title, subtitle, data, empty_title, empty_hint \
             FROM home_sections WHERE visible IS TRUE ORDER BY sort_order ASC",
            &[],
        )
        .await?;
    let mut sections = Vec::new();
    for row in rows {
        let key: String = row.try_get("key")?;
        let order: i32 = row.try_get("sort_order")?;
        let visible: bool = row.try_get("visible")?;
        let title_raw: Value = row.try_get("title")?;
        let Some((fa, ar, en)) = as_localized(&title_raw) else {
            continue; // corrupt data never breaks the response
        };
        let subtitle = row
            .try_get::<Option<Value>, _>("subtitle")?
            .and_then(|v| as_localized(&v))
            .map(|(fa, ar, en)| localized_json(&fa, &ar, &en));
        let data: Option<Value> = row.try_get("data")?;
        let empty = match (
            row.try_get::<Option<Value>, _>("empty_title")?
                .and_then(|v| as_localized(&v)),
            row.try_get::<Option<Value>, _>("empty_hint")?
                .and_then(|v| as_localized(&v)),
        ) {
            (Some((tfa, tar, ten)), Some((hfa, har, hen))) => Some(json!({
                "title": localized_json(&tfa, &tar, &ten),
                "hint": localized_json(&hfa, &har, &hen),
            })),
            _ => None,
        };
        sections.push(json!({
            "key": key,
            "order": order,
            "visible": visible,
            "title": localized_json(&fa, &ar, &en),
            "subtitle": subtitle,
            "data": data,
            "empty": empty,
        }));
    }
    Ok(Json(json!({
        "theme": load_theme_overrides(&state).await?,
        "sections": sections,
    })))
}

/// `_load_theme_overrides`: `theme.palette.<token>` / `theme.fonts.<slot>`
/// from decrypted `platform_settings`; `None` when both are empty.
async fn load_theme_overrides(state: &AppState) -> ApiResult<Value> {
    let db = state.db_or_500()?;
    let keys = db
        .fetch_all(
            "SELECT key FROM platform_settings WHERE key LIKE 'theme.%'",
            &[],
        )
        .await?;
    let mut palette = Map::new();
    let mut fonts = Map::new();
    for row in keys {
        let key: String = row.try_get("key")?;
        let Some(value) = get_setting(db, &state.config, &key).await? else {
            continue;
        };
        let rest = &key["theme.".len()..];
        if let Some(token) = rest.strip_prefix("palette.").filter(|t| !t.is_empty()) {
            palette.insert(token.to_string(), Value::String(value));
        } else if let Some(slot) = rest.strip_prefix("fonts.").filter(|t| !t.is_empty()) {
            fonts.insert(slot.to_string(), Value::String(value));
        }
    }
    if palette.is_empty() && fonts.is_empty() {
        return Ok(Value::Null);
    }
    Ok(json!({ "palette": palette, "fonts": fonts }))
}

// --- search scoring (verbatim from home/service.py) ---

fn match_score(texts: &[String], needle: &str, prefix_w: f64, substr_w: f64) -> f64 {
    let mut best = 0.0f64;
    for text in texts {
        let t = text.to_lowercase();
        if t.starts_with(needle) {
            best = best.max(prefix_w);
        } else if t.contains(needle) {
            best = best.max(substr_w);
        }
    }
    best
}

fn recency_boost(ts: Option<DateTime<Utc>>, now: DateTime<Utc>) -> f64 {
    let Some(ts) = ts else { return 0.0 };
    let age_days = ((now - ts).num_seconds() as f64 / 86400.0).max(0.0);
    RECENCY_MAX * (1.0 - age_days / 180.0).max(0.0)
}

fn popularity_boost(count: i64, max_count: i64) -> f64 {
    if count <= 0 || max_count <= 0 {
        return 0.0;
    }
    POPULARITY_MAX * ((count as f64 + 1.0).ln() / (max_count as f64 + 1.0).ln())
}

/// `_score_hit` → (score, matched text). `None` score means "drop".
fn score_hit(
    title_texts: &[String],
    body_texts: &[String],
    needle: &str,
    recency_ts: Option<DateTime<Utc>>,
    popularity: f64,
    now: DateTime<Utc>,
) -> (f64, Option<String>) {
    let mut score = match_score(title_texts, needle, 2.0, 1.0);
    let mut matched = title_texts
        .iter()
        .find(|t| t.to_lowercase().contains(needle))
        .cloned();
    if !body_texts.is_empty() {
        let body_score = match_score(body_texts, needle, 1.5, 0.5);
        if body_score > score {
            score = body_score;
            matched = body_texts
                .iter()
                .find(|t| t.to_lowercase().contains(needle))
                .cloned()
                .or(matched);
        }
    }
    if score <= 0.0 {
        return (0.0, None);
    }
    let total = score + recency_boost(recency_ts, now) + popularity;
    ((total * 10000.0).round() / 10000.0, matched)
}

fn snippet(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = collapsed.chars().collect();
    if chars.len() <= 160 {
        collapsed
    } else {
        let head: String = chars[..160].iter().collect();
        format!("{}…", head.trim_end())
    }
}

/// Recursively collect every string in a JSON value (`_block_texts`).
fn block_texts(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => {
            for item in items {
                block_texts(item, out);
            }
        }
        Value::Object(map) => {
            for v in map.values() {
                block_texts(v, out);
            }
        }
        _ => {}
    }
}

struct Hit {
    hit_type: &'static str,
    id: String,
    title: Value,
    snippet: String,
    url: Option<String>,
    score: f64,
}

impl Hit {
    fn to_json(&self) -> Value {
        json!({
            "type": self.hit_type,
            "id": self.id,
            "title": self.title,
            "snippet": self.snippet,
            "url": self.url,
            "score": self.score,
        })
    }
}

/// `GET /api/v1/search?q&...&type&limit`.
pub async fn search(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let q = match params.get("q") {
        Some(v) => v.clone(),
        None => return Err(validation_err("q", "field is required")),
    };
    if q.chars().count() < 1 || q.chars().count() > 200 {
        return Err(validation_err("q", "length out of range"));
    }
    let limit = match params.get("limit") {
        Some(v) => match v.parse::<i64>() {
            Ok(n) if (1..=50).contains(&n) => n,
            _ => return Err(validation_err("limit", "out of range")),
        },
        None => 10,
    };
    let result_type = params.get("type").map(String::as_str);
    let needle = q.trim().to_lowercase();
    if needle.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_query",
            "متن جست‌وجو نمی‌تواند خالی باشد",
        ));
    }
    let db = state.db_or_500()?;
    let now = utc_now();
    let mut hits: Vec<Hit> = Vec::new();

    // sections
    if result_type.is_none() || result_type == Some("section") {
        let rows = db
            .fetch_all(
                "SELECT key, title, subtitle FROM home_sections \
                 WHERE visible IS TRUE ORDER BY sort_order ASC",
                &[],
            )
            .await?;
        for row in rows {
            let key: String = row.try_get("key")?;
            let title_raw: Value = row.try_get("title")?;
            let Some((fa, ar, en)) = as_localized(&title_raw) else {
                continue;
            };
            let title_texts = vec![fa.clone(), ar.clone(), en.clone()];
            let mut score = match_score(&title_texts, &needle, 2.0, 1.0);
            let mut matched_text: Option<String> = None;
            let subtitle = row
                .try_get::<Option<Value>, _>("subtitle")?
                .and_then(|v| as_localized(&v));
            if let Some((sfa, sar, sen)) = &subtitle {
                let sub_texts = vec![sfa.clone(), sar.clone(), sen.clone()];
                let sub_score = match_score(&sub_texts, &needle, 1.5, 0.5);
                if sub_score > score {
                    score = sub_score;
                    matched_text = sub_texts
                        .iter()
                        .find(|t| t.to_lowercase().contains(&needle))
                        .cloned();
                }
            }
            if score <= 0.0 {
                continue;
            }
            let matched = matched_text.unwrap_or_else(|| {
                title_texts
                    .iter()
                    .find(|t| t.to_lowercase().contains(&needle))
                    .cloned()
                    .unwrap_or_else(|| title_texts[0].clone())
            });
            hits.push(Hit {
                hit_type: "section",
                id: key,
                title: localized_json(&fa, &ar, &en),
                snippet: snippet(&matched),
                url: None,
                score,
            });
        }
    }

    // themes
    if result_type.is_none() || result_type == Some("theme") {
        let rows = db
            .fetch_all("SELECT id, name FROM themes WHERE is_active IS TRUE", &[])
            .await?;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let name_raw: Value = row.try_get("name")?;
            let Some((fa, ar, en)) = as_localized(&name_raw) else {
                continue;
            };
            let name_texts = vec![fa.clone(), ar.clone(), en.clone()];
            let score = match_score(&name_texts, &needle, 2.0, 1.0);
            if score <= 0.0 {
                continue;
            }
            let matched = name_texts
                .iter()
                .find(|t| t.to_lowercase().contains(&needle))
                .cloned()
                .unwrap_or_else(|| name_texts[0].clone());
            hits.push(Hit {
                hit_type: "theme",
                id: id.to_string(),
                title: localized_json(&fa, &ar, &en),
                snippet: snippet(&matched),
                url: None,
                score,
            });
        }
    }

    // fatwas (published)
    if result_type.is_none() || result_type == Some("fatwa") {
        let rows = db
            .fetch_all(
                "SELECT id, title, body, published_at, created_at FROM fatwa_submissions \
                 WHERE workflow_status = 'published' ORDER BY published_at DESC LIMIT ?",
                &[SCAN_CAP.into_arg()],
            )
            .await?;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let title_raw: Value = row.try_get("title")?;
            let Some((fa, ar, en)) = as_localized(&title_raw) else {
                continue;
            };
            let body: String = row
                .try_get::<Option<String>, _>("body")?
                .unwrap_or_default();
            let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let (score, matched) = score_hit(
                &locale_texts(&title_raw),
                std::slice::from_ref(&body),
                &needle,
                published_at.or(Some(created_at)),
                0.0,
                now,
            );
            if score <= 0.0 {
                continue;
            }
            hits.push(Hit {
                hit_type: "fatwa",
                id: id.to_string(),
                title: localized_json(&fa, &ar, &en),
                snippet: snippet(matched.as_deref().unwrap_or(&body)),
                url: Some("/fatwa".to_string()),
                score,
            });
        }
        // Semantic boost: inactive — no real embedding provider is
        // ported (the Python default provider is "hash", which the
        // Python code itself skips). See ADR-018.
    }

    // library (published, public)
    if result_type.is_none() || result_type == Some("library") {
        let rows = db
            .fetch_all(
                "SELECT id, title, description, transcript, created_at FROM library_items \
                 WHERE workflow_status = 'published' AND visibility = 'public' \
                 ORDER BY created_at DESC LIMIT ?",
                &[SCAN_CAP.into_arg()],
            )
            .await?;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let title_raw: Value = row.try_get("title")?;
            let Some((fa, ar, en)) = as_localized(&title_raw) else {
                continue;
            };
            let mut body_texts = locale_texts(
                &row.try_get::<Option<Value>, _>("description")?
                    .unwrap_or(Value::Null),
            );
            if let Some(t) = row.try_get::<Option<String>, _>("transcript")? {
                body_texts.push(t);
            }
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let (score, matched) = score_hit(
                &locale_texts(&title_raw),
                &body_texts,
                &needle,
                Some(created_at),
                0.0,
                now,
            );
            if score <= 0.0 {
                continue;
            }
            hits.push(Hit {
                hit_type: "library",
                id: id.to_string(),
                title: localized_json(&fa, &ar, &en),
                snippet: snippet(matched.as_deref().unwrap_or("")),
                url: Some(format!("/library/{id}")),
                score,
            });
        }
    }

    // courses (active, public) + enrollment popularity
    if result_type.is_none() || result_type == Some("course") {
        let rows = db
            .fetch_all(
                "SELECT id, title, description, created_at FROM courses \
                 WHERE is_active IS TRUE AND visibility = 'public' \
                 ORDER BY created_at DESC LIMIT ?",
                &[SCAN_CAP.into_arg()],
            )
            .await?;
        if !rows.is_empty() {
            let counts = db
                .fetch_all(
                    "SELECT course_cohorts.course_id, count(cohort_enrollments.id) AS n \
                     FROM course_cohorts JOIN cohort_enrollments \
                       ON cohort_enrollments.cohort_id = course_cohorts.id \
                     WHERE cohort_enrollments.status = 'active' \
                     GROUP BY course_cohorts.course_id",
                    &[],
                )
                .await?;
            let mut enrollments: HashMap<String, i64> = HashMap::new();
            for r in counts {
                let cid: Uuid = r.try_get("course_id")?;
                let n: i64 = r.try_get("n")?;
                enrollments.insert(cid.to_string(), n);
            }
            let max_enroll = enrollments.values().copied().max().unwrap_or(0);
            for row in rows {
                let id: Uuid = row.try_get("id")?;
                let title_raw: Value = row.try_get("title")?;
                let Some((fa, ar, en)) = as_localized(&title_raw) else {
                    continue;
                };
                let body_texts = locale_texts(
                    &row.try_get::<Option<Value>, _>("description")?
                        .unwrap_or(Value::Null),
                );
                let created_at: DateTime<Utc> = row.try_get("created_at")?;
                let (score, matched) = score_hit(
                    &locale_texts(&title_raw),
                    &body_texts,
                    &needle,
                    Some(created_at),
                    popularity_boost(
                        enrollments.get(&id.to_string()).copied().unwrap_or(0),
                        max_enroll,
                    ),
                    now,
                );
                if score <= 0.0 {
                    continue;
                }
                hits.push(Hit {
                    hit_type: "course",
                    id: id.to_string(),
                    title: localized_json(&fa, &ar, &en),
                    snippet: snippet(matched.as_deref().unwrap_or("")),
                    url: Some(format!("/learn/courses/{id}")),
                    score,
                });
            }
        }
    }

    // CMS pages (published)
    if result_type.is_none() || result_type == Some("page") {
        let rows = db
            .fetch_all(
                "SELECT id, slug, title, blocks, published_at, created_at FROM pages \
                 WHERE workflow_status = 'published' ORDER BY created_at DESC LIMIT ?",
                &[SCAN_CAP.into_arg()],
            )
            .await?;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let slug: String = row.try_get("slug")?;
            let title_raw: Value = row.try_get("title")?;
            let Some((fa, ar, en)) = as_localized(&title_raw) else {
                continue;
            };
            let mut body_texts = Vec::new();
            block_texts(
                &row.try_get::<Option<Value>, _>("blocks")?
                    .unwrap_or(Value::Null),
                &mut body_texts,
            );
            let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            let (score, matched) = score_hit(
                &locale_texts(&title_raw),
                &body_texts,
                &needle,
                published_at.or(Some(created_at)),
                0.0,
                now,
            );
            if score <= 0.0 {
                continue;
            }
            hits.push(Hit {
                hit_type: "page",
                id: id.to_string(),
                title: localized_json(&fa, &ar, &en),
                snippet: snippet(matched.as_deref().unwrap_or("")),
                url: Some(format!("/pages/{slug}")),
                score,
            });
        }
    }

    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(type_order(a.hit_type).cmp(&type_order(b.hit_type)))
            .then(a.id.cmp(&b.id))
    });
    let total = hits.len() as i64;
    let results: Vec<Value> = hits
        .into_iter()
        .take(limit as usize)
        .map(|h| h.to_json())
        .collect();
    Ok(Json(json!({
        "query": q.trim(),
        "results": results,
        "total": total,
    })))
}

fn parse_query(uri: &Uri) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(query) = uri.query() {
        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (k, v) = match pair.split_once('=') {
                Some((k, v)) => (k, v),
                None => (pair, ""),
            };
            let key = url_decode(k);
            let val = url_decode(v);
            out.entry(key).or_insert(val);
        }
    }
    out
}

/// Percent-decode a query component (`+` → space). Bytes are
/// collected first and decoded as UTF-8, so multi-byte (e.g. Persian)
/// sequences survive — decoding byte-by-byte into `char` would mangle
/// them into Latin-1.
fn url_decode(s: &str) -> String {
    let mut bytes: Vec<u8> = Vec::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '+' => bytes.push(b' '),
            '%' => {
                let h1 = chars.next().and_then(|c| c.to_digit(16));
                let h2 = chars.next().and_then(|c| c.to_digit(16));
                match (h1, h2) {
                    (Some(a), Some(b)) => bytes.push(((a << 4) | b) as u8),
                    _ => bytes.push(b'%'),
                }
            }
            _ => {
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

// --- admin: section data ---

/// Validate a LocalizedText-ish value: a string (kept as-is) or a dict
/// with only fa/ar/en string values. Required fields need at least one
/// non-empty value. Returns the value unchanged (nulls already stripped
/// by the caller) or a 422 with the Python error code as `msg`.
fn validate_localized_text(value: &Value, required: bool, field: &str) -> Result<Value, ApiError> {
    match value {
        Value::Null => Ok(Value::Null),
        Value::String(s) => {
            if required && s.trim().is_empty() {
                return Err(validation_err(field, "empty_text"));
            }
            Ok(value.clone())
        }
        Value::Object(map) => {
            for (k, v) in map {
                if !["fa", "ar", "en"].contains(&k.as_str()) {
                    return Err(validation_err(field, "unknown_locale"));
                }
                if !v.is_string() {
                    return Err(validation_err(field, "bad_localized_value"));
                }
            }
            if required
                && !map
                    .values()
                    .any(|v| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
            {
                return Err(validation_err(field, "empty_text"));
            }
            Ok(value.clone())
        }
        _ => Err(validation_err(field, "bad_localized_text")),
    }
}

/// Validate a safe href: empty/whitespace → null; `/...` (not `//...`)
/// or `http(s)://` kept; anything else → 422 `unsafe_url`.
fn validate_safe_href(value: &Value, field: &str) -> Result<Option<String>, ApiError> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => {
            let url = s.trim();
            if url.is_empty() {
                return Ok(None);
            }
            if (url.starts_with('/') && !url.starts_with("//"))
                || url.starts_with("https://")
                || url.starts_with("http://")
            {
                return Ok(Some(url.to_string()));
            }
            Err(validation_err(field, "unsafe_url"))
        }
        _ => Err(validation_err(field, "bad_url")),
    }
}

fn validate_section_item(item: &Value, idx: usize) -> Result<Value, ApiError> {
    let obj = item
        .as_object()
        .ok_or_else(|| validation_err(&format!("items[{idx}]"), "must be an object"))?;
    let mut out = Map::new();
    if let Some(id) = obj.get("id") {
        if !id.is_null() {
            let s = id
                .as_str()
                .ok_or_else(|| validation_err(&format!("items[{idx}].id"), "must be a string"))?;
            out.insert("id".to_string(), Value::String(s.to_string()));
        }
    }
    let title = obj
        .get("title")
        .ok_or_else(|| validation_err(&format!("items[{idx}].title"), "field is required"))?;
    out.insert(
        "title".to_string(),
        validate_localized_text(title, true, &format!("items[{idx}].title"))?,
    );
    for key in [
        "subtitle", "snippet", "author", "category", "duration", "badge",
    ] {
        if let Some(v) = obj.get(key) {
            if !v.is_null() {
                out.insert(
                    key.to_string(),
                    validate_localized_text(v, false, &format!("items[{idx}].{key}"))?,
                );
            }
        }
    }
    if let Some(v) = obj.get("image") {
        if !v.is_null() {
            let s = v.as_str().ok_or_else(|| {
                validation_err(&format!("items[{idx}].image"), "must be a string")
            })?;
            out.insert("image".to_string(), Value::String(s.to_string()));
        }
    }
    if let Some(v) = obj.get("url") {
        if let Some(safe) = validate_safe_href(v, &format!("items[{idx}].url"))? {
            out.insert("url".to_string(), Value::String(safe));
        }
    }
    Ok(Value::Object(out))
}

fn validate_footer_link(link: &Value, col: usize, idx: usize) -> Result<Value, ApiError> {
    let obj = link.as_object().ok_or_else(|| {
        validation_err(&format!("columns[{col}].links[{idx}]"), "must be an object")
    })?;
    let label = obj.get("label").ok_or_else(|| {
        validation_err(
            &format!("columns[{col}].links[{idx}].label"),
            "field is required",
        )
    })?;
    let label =
        validate_localized_text(label, true, &format!("columns[{col}].links[{idx}].label"))?;
    let href = obj.get("href").ok_or_else(|| {
        validation_err(
            &format!("columns[{col}].links[{idx}].href"),
            "field is required",
        )
    })?;
    let safe = validate_safe_href(href, &format!("columns[{col}].links[{idx}].href"))?.ok_or_else(
        || validation_err(&format!("columns[{col}].links[{idx}].href"), "unsafe_url"),
    )?;
    Ok(json!({ "label": label, "href": safe }))
}

fn validate_footer_column(col: &Value, idx: usize) -> Result<Value, ApiError> {
    let obj = col
        .as_object()
        .ok_or_else(|| validation_err(&format!("columns[{idx}]"), "must be an object"))?;
    let heading = obj
        .get("heading")
        .ok_or_else(|| validation_err(&format!("columns[{idx}].heading"), "field is required"))?;
    let heading = validate_localized_text(heading, true, &format!("columns[{idx}].heading"))?;
    let mut links = Vec::new();
    if let Some(v) = obj.get("links") {
        let arr = v
            .as_array()
            .ok_or_else(|| validation_err(&format!("columns[{idx}].links"), "must be a list"))?;
        if arr.len() > 12 {
            return Err(validation_err(
                &format!("columns[{idx}].links"),
                "length out of range",
            ));
        }
        for (i, link) in arr.iter().enumerate() {
            links.push(validate_footer_link(link, idx, i)?);
        }
    }
    Ok(json!({ "heading": heading, "links": links }))
}

fn validate_footer_brand(brand: &Value) -> Result<Value, ApiError> {
    let obj = brand
        .as_object()
        .ok_or_else(|| validation_err("brand", "must be an object"))?;
    let mut out = Map::new();
    for key in ["tagline", "rights"] {
        if let Some(v) = obj.get(key) {
            if !v.is_null() {
                out.insert(
                    key.to_string(),
                    validate_localized_text(v, false, &format!("brand.{key}"))?,
                );
            }
        }
    }
    Ok(Value::Object(out))
}

/// `PUT /api/v1/admin/home/sections/{key}/data` (`platform.settings`).
pub async fn put_section_data(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(key): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/home/sections").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let parsed = parse_body(&body)?;
    let items_v = parsed.get("items").filter(|v| !v.is_null());
    let layout_v = parsed.get("layout").filter(|v| !v.is_null());
    let columns_v = parsed.get("columns").filter(|v| !v.is_null());
    let brand_v = parsed.get("brand").filter(|v| !v.is_null());
    if items_v.is_none() && layout_v.is_none() && columns_v.is_none() && brand_v.is_none() {
        return Err(validation_err("body", "empty_payload"));
    }
    let mut data = Map::new();
    if let Some(items) = items_v {
        let arr = items
            .as_array()
            .ok_or_else(|| validation_err("items", "must be a list"))?;
        if arr.len() > 12 {
            return Err(validation_err("items", "length out of range"));
        }
        let mut out = Vec::new();
        for (i, item) in arr.iter().enumerate() {
            out.push(validate_section_item(item, i)?);
        }
        data.insert("items".to_string(), Value::Array(out));
    }
    if let Some(layout) = layout_v {
        let s = layout
            .as_str()
            .ok_or_else(|| validation_err("layout", "must be a string"))?;
        if !["slider", "grid", "featured-rail", "shelf", "mosaic"].contains(&s) {
            return Err(validation_err("layout", "invalid layout"));
        }
        data.insert("layout".to_string(), Value::String(s.to_string()));
    }
    if let Some(columns) = columns_v {
        let arr = columns
            .as_array()
            .ok_or_else(|| validation_err("columns", "must be a list"))?;
        if arr.len() > 6 {
            return Err(validation_err("columns", "length out of range"));
        }
        let mut out = Vec::new();
        for (i, col) in arr.iter().enumerate() {
            out.push(validate_footer_column(col, i)?);
        }
        data.insert("columns".to_string(), Value::Array(out));
    }
    if let Some(brand) = brand_v {
        data.insert("brand".to_string(), validate_footer_brand(brand)?);
    }
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT key FROM home_sections WHERE key = ?",
            &[(key.as_str()).into_arg()],
        )
        .await?;
    if row.is_none() {
        return Err(ApiError::not_found(
            "section_not_found",
            "بخش خانه یافت نشد",
        ));
    }
    let data_value = Value::Object(data);
    db.execute(
        "UPDATE home_sections SET data = ? WHERE key = ?",
        &[data_value.clone().into_arg(), (key.as_str()).into_arg()],
    )
    .await?;
    Ok(Json(json!({ "key": key, "data": data_value })))
}
