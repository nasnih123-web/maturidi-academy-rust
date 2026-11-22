//! Request validation helpers.
//!
//! Bodies are parsed as `serde_json::Value` and validated by hand so the
//! failure mode matches Python: any bad field → 422 with the unified
//! envelope, code `validation_error`, message «خطای اعتبارسنجی ورودی»
//! (the per-field messages differ from Pydantic's wording — ADR-006).
//! Length/range bounds are copied from the Pydantic schemas.

use serde_json::{json, Value};

use crate::errors::ApiError;

pub type VResult<T> = Result<T, ApiError>;

fn err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Parse a request body as JSON. Non-object bodies and invalid JSON are
/// 422 validation errors (Python never returns a bare 400 here).
pub fn parse_body(bytes: &[u8]) -> VResult<Value> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| ApiError::validation(vec![json!({ "msg": "invalid JSON body" })]))?;
    if !v.is_object() {
        return Err(ApiError::validation(vec![
            json!({ "msg": "body must be a JSON object" }),
        ]));
    }
    Ok(v)
}

fn get<'a>(body: &'a Value, field: &str) -> Option<&'a Value> {
    body.get(field).filter(|v| !v.is_null())
}

pub fn req_str(body: &Value, field: &str, min: usize, max: usize) -> VResult<String> {
    let Some(v) = get(body, field) else {
        return Err(err(field, "field is required"));
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    let s = s.trim().to_string();
    if s.chars().count() < min || s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(s)
}

pub fn opt_str(body: &Value, field: &str, min: usize, max: usize) -> VResult<Option<String>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    let s = s.trim().to_string();
    if s.chars().count() < min || s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(Some(s))
}

/// Optional string with no minimum (empty string is significant for
/// phone-clearing in `PATCH /users/me`).
pub fn opt_str_raw(body: &Value, field: &str, max: usize) -> VResult<Option<String>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    if s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(Some(s.to_string()))
}

#[allow(dead_code)] // Later waves: boolean body fields
pub fn req_bool(body: &Value, field: &str) -> VResult<bool> {
    match get(body, field) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(err(field, "must be a boolean")),
        None => Err(err(field, "field is required")),
    }
}

pub fn opt_bool(body: &Value, field: &str) -> VResult<Option<bool>> {
    match get(body, field) {
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(err(field, "must be a boolean")),
        None => Ok(None),
    }
}

pub fn opt_int(body: &Value, field: &str, min: i64, max: i64) -> VResult<Option<i64>> {
    match get(body, field) {
        Some(v) => {
            let Some(n) = v.as_i64() else {
                return Err(err(field, "must be an integer"));
            };
            if n < min || n > max {
                return Err(err(field, "out of range"));
            }
            Ok(Some(n))
        }
        None => Ok(None),
    }
}

pub fn req_int(body: &Value, field: &str, min: i64, max: i64, default: i64) -> VResult<i64> {
    Ok(opt_int(body, field, min, max)?.unwrap_or(default))
}

pub fn opt_str_list(body: &Value, field: &str) -> VResult<Option<Vec<String>>> {
    match get(body, field) {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(err(field, "must be a list of strings"));
                };
                out.push(s.to_string());
            }
            Ok(Some(out))
        }
        Some(_) => Err(err(field, "must be a list of strings")),
        None => Ok(None),
    }
}

/// Minimal email check — mirrors `EmailStr` for realistic addresses.
/// (Pydantic's full RFC validation is not reproduced; see ADR-006.)
pub fn is_valid_email(s: &str) -> bool {
    if s.len() > 320 || s.contains(char::is_whitespace) {
        return false;
    }
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && domain.contains('.') && !domain.starts_with('.')
}

pub fn opt_uuid(body: &Value, field: &str) -> VResult<Option<uuid::Uuid>> {
    match get(body, field) {
        Some(Value::String(s)) => s
            .parse::<uuid::Uuid>()
            .map(Some)
            .map_err(|_| err(field, "must be a UUID")),
        Some(_) => Err(err(field, "must be a UUID")),
        None => Ok(None),
    }
}

// --- Wave 3 helpers -----------------------------------------------------

use std::collections::HashMap;

use axum::http::Uri;
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};

/// Parse a query string into a map (first value wins), percent-decoding
/// `+` → space and `%XX` byte-wise so multi-byte (Persian) sequences
/// survive. Same decoder as `routes_home::parse_query`.
pub fn parse_query(uri: &Uri) -> HashMap<String, String> {
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
            out.entry(url_decode(k)).or_insert_with(|| url_decode(v));
        }
    }
    out
}

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

/// Required integer query param (FastAPI `Query(ge=, le=)`): missing or
/// unparsable or out of range → 422.
pub fn query_int(
    params: &HashMap<String, String>,
    name: &str,
    default: i64,
    min: i64,
    max: i64,
) -> VResult<i64> {
    match params.get(name) {
        Some(v) => match v.trim().parse::<i64>() {
            Ok(n) if (min..=max).contains(&n) => Ok(n),
            _ => Err(err(name, "out of range")),
        },
        None => Ok(default),
    }
}

/// Optional integer query param: absent → None; present but unparsable
/// or out of range → 422.
pub fn query_opt_int(
    params: &HashMap<String, String>,
    name: &str,
    min: i64,
    max: i64,
) -> VResult<Option<i64>> {
    match params.get(name) {
        Some(v) => match v.trim().parse::<i64>() {
            Ok(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(err(name, "out of range")),
        },
        None => Ok(None),
    }
}

/// Pydantic v2 lax `int`: JSON integer, integral float, or a digit
/// string (whitespace tolerated, like Python `int(" 5 ")`). Bool is
/// rejected (Pydantic v2 does not coerce bool → int).
pub fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<i64>().ok().or_else(|| {
                t.parse::<f64>()
                    .ok()
                    .filter(|f| f.fract() == 0.0)
                    .map(|f| f as i64)
            })
        }
        _ => None,
    }
}

/// Pydantic v2 lax `bool`: JSON bool, 0/1, or one of
/// true/false/yes/no/on/off/y/n/1/0 (case-insensitive).
pub fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "yes" | "on" | "y" | "1" => Some(true),
            "false" | "no" | "off" | "n" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

/// Optional body boolean with Pydantic lax coercion; absent/null → None.
pub fn opt_bool_loose(body: &Value, field: &str) -> VResult<Option<bool>> {
    match get(body, field) {
        Some(v) => as_bool(v)
            .map(Some)
            .ok_or_else(|| err(field, "must be a boolean")),
        None => Ok(None),
    }
}

/// Optional body integer with Pydantic lax coercion; absent/null → None.
pub fn opt_int_loose(body: &Value, field: &str, min: i64, max: i64) -> VResult<Option<i64>> {
    match get(body, field) {
        Some(v) => match as_int(v) {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(err(field, "out of range")),
        },
        None => Ok(None),
    }
}

/// Required body integer with Pydantic lax coercion.
pub fn req_int_loose(body: &Value, field: &str, min: i64, max: i64) -> VResult<i64> {
    match opt_int_loose(body, field, min, max)? {
        Some(n) => Ok(n),
        None => Err(err(field, "field is required")),
    }
}

/// Required body string, raw (no trimming — Pydantic does not trim),
/// with a character-length bound.
pub fn req_str_raw(body: &Value, field: &str, min: usize, max: usize) -> VResult<String> {
    let Some(v) = get(body, field) else {
        return Err(err(field, "field is required"));
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    if s.chars().count() < min || s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(s.to_string())
}

/// Optional body string, raw (no trimming), with a character-length bound.
pub fn opt_str_raw_max(body: &Value, field: &str, max: usize) -> VResult<Option<String>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    if s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(Some(s.to_string()))
}

/// Required body object (Pydantic `dict`): must be a JSON object.
pub fn req_dict(body: &Value, field: &str) -> VResult<Value> {
    match get(body, field) {
        Some(v) if v.is_object() => Ok(v.clone()),
        Some(_) => Err(err(field, "must be an object")),
        None => Err(err(field, "field is required")),
    }
}

/// Optional body object: absent/null → None; otherwise must be an object.
pub fn opt_dict(body: &Value, field: &str) -> VResult<Option<Value>> {
    match get(body, field) {
        Some(v) if v.is_object() => Ok(Some(v.clone())),
        Some(_) => Err(err(field, "must be an object")),
        None => Ok(None),
    }
}

/// Optional field that must be a JSON array of any items (Python `list`,
// e.g. `evidence_refs: list | None` — items need not be objects).
pub fn opt_list(body: &Value, field: &str) -> VResult<Option<Vec<Value>>> {
    match get(body, field) {
        Some(Value::Array(items)) => Ok(Some(items.clone())),
        Some(_) => Err(err(field, "must be a list")),
        None => Ok(None),
    }
}

/// Strict `LocalizedText` (`{fa, ar, en}`, all required strings) —
/// the `home.schemas.LocalizedText` model. Returns the object unchanged.
pub fn req_localized(body: &Value, field: &str) -> VResult<Value> {
    let Some(v) = get(body, field) else {
        return Err(err(field, "field is required"));
    };
    validate_localized(v, field)
}

/// Optional strict `LocalizedText`: absent/null → None.
/// (Unused in Wave 3 — kept for later waves' optional localized fields.)
#[allow(dead_code)]
pub fn opt_localized(body: &Value, field: &str) -> VResult<Option<Value>> {
    match get(body, field) {
        Some(v) => Ok(Some(validate_localized(v, field)?)),
        None => Ok(None),
    }
}

fn validate_localized(v: &Value, field: &str) -> VResult<Value> {
    localized_value(v, field)
}

/// Strict `LocalizedText` value validator (object with fa/ar/en strings).
pub fn localized_value(v: &Value, field: &str) -> VResult<Value> {
    let Some(map) = v.as_object() else {
        return Err(err(field, "must be an object"));
    };
    for loc in ["fa", "ar", "en"] {
        match map.get(loc) {
            Some(Value::String(_)) => {}
            _ => return Err(err(field, "field is required")),
        }
    }
    Ok(v.clone())
}

/// A single JSON value must be an object.
pub fn dict_value(v: &Value, field: &str) -> VResult<Value> {
    if v.is_object() {
        Ok(v.clone())
    } else {
        Err(err(field, "must be an object"))
    }
}

/// A single JSON value must be a datetime (string or Unix timestamp).
pub fn datetime_value(v: &Value, field: &str) -> VResult<DateTime<Utc>> {
    match v {
        Value::String(s) => parse_datetime_str(s).ok_or_else(|| err(field, "must be a datetime")),
        Value::Number(n) => {
            let secs = n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64));
            secs.and_then(|s| DateTime::from_timestamp(s, 0))
                .ok_or_else(|| err(field, "must be a datetime"))
        }
        _ => Err(err(field, "must be a datetime")),
    }
}

/// A single JSON value must be a date (`YYYY-MM-DD` or a datetime string).
pub fn date_value(v: &Value, field: &str) -> VResult<NaiveDate> {
    match v {
        Value::String(s) => {
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                return Ok(d);
            }
            if let Some(dt) = parse_datetime_str(s) {
                return Ok(dt.date_naive());
            }
            Err(err(field, "must be a date"))
        }
        Value::Number(n) => {
            let secs = n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64));
            secs.and_then(|s| DateTime::from_timestamp(s, 0))
                .map(|dt| dt.date_naive())
                .ok_or_else(|| err(field, "must be a date"))
        }
        _ => Err(err(field, "must be a date")),
    }
}

/// A single JSON value must be a UUID string.
pub fn uuid_value(v: &Value, field: &str) -> VResult<uuid::Uuid> {
    match v {
        Value::String(s) => s
            .parse::<uuid::Uuid>()
            .map_err(|_| err(field, "must be a UUID")),
        _ => Err(err(field, "must be a UUID")),
    }
}

/// A single JSON value must be a string within the length bound (raw,
/// no trimming).
pub fn str_value(v: &Value, field: &str, max: usize) -> VResult<String> {
    match v {
        Value::String(s) if s.chars().count() <= max => Ok(s.clone()),
        Value::String(_) => Err(err(field, "length out of range")),
        _ => Err(err(field, "must be a string")),
    }
}

/// A single JSON value must be a list of strings.
pub fn str_list_value(v: &Value, field: &str) -> VResult<Vec<String>> {
    match v {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(err(field, "must be a list of strings")),
                }
            }
            Ok(out)
        }
        _ => Err(err(field, "must be a list of strings")),
    }
}

/// A `DistributionIn` value: an object with the five surface toggles,
/// each optional and defaulting to `false` (Pydantic `DistributionIn`).
/// Unknown keys are ignored (Pydantic `extra='ignore'`).
pub fn distribution_value(v: &Value, field: &str) -> VResult<Value> {
    let Some(map) = v.as_object() else {
        return Err(err(field, "must be an object"));
    };
    let mut out = serde_json::Map::new();
    for key in ["web", "mobile", "desktop", "telegram", "ai_knowledge"] {
        match map.get(key) {
            Some(raw) => {
                let b = as_bool(raw).ok_or_else(|| err(field, "must be a boolean"))?;
                out.insert(key.to_string(), Value::Bool(b));
            }
            None => {
                out.insert(key.to_string(), Value::Bool(false));
            }
        }
    }
    Ok(Value::Object(out))
}

/// Parse an ISO-8601 datetime string the way Pydantic does: RFC 3339
/// (aware) first, then naive `YYYY-MM-DD[T| ]HH:MM:SS[.f]` (assumed UTC),
/// then a bare date (midnight UTC). Returns `None` when unparsable.
pub fn parse_datetime_str(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?));
    }
    None
}

/// Optional body datetime: absent/null → None; a string is parsed as
/// ISO-8601 (naive = UTC, like `_as_utc`); a number is a Unix timestamp
/// (Pydantic accepts both). Anything else → 422.
pub fn opt_datetime(body: &Value, field: &str) -> VResult<Option<DateTime<Utc>>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    match v {
        Value::String(s) => parse_datetime_str(s)
            .map(Some)
            .ok_or_else(|| err(field, "must be a datetime")),
        Value::Number(n) => {
            let secs = n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64));
            match secs.and_then(|s| DateTime::from_timestamp(s, 0)) {
                Some(dt) => Ok(Some(dt)),
                None => Err(err(field, "must be a datetime")),
            }
        }
        _ => Err(err(field, "must be a datetime")),
    }
}

/// Optional body date (`YYYY-MM-DD`, or a datetime string whose date
/// part is taken — like Pydantic's `date` field): absent/null → None.
pub fn opt_date(body: &Value, field: &str) -> VResult<Option<NaiveDate>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    match v {
        Value::String(s) => {
            if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
                return Ok(Some(d));
            }
            if let Some(dt) = parse_datetime_str(s) {
                return Ok(Some(dt.date_naive()));
            }
            Err(err(field, "must be a date"))
        }
        Value::Number(n) => {
            let secs = n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64));
            match secs
                .and_then(|s| DateTime::from_timestamp(s, 0))
                .map(|dt| dt.date_naive())
            {
                Some(d) => Ok(Some(d)),
                None => Err(err(field, "must be a date")),
            }
        }
        _ => Err(err(field, "must be a date")),
    }
}

/// Base64url cursor `base64url(JSON([created_at.isoformat(), str(id)]))`
/// — the same format as `cms_service._encode_cursor`,
/// `submissions_service._encode_cursor` and `cluster_service._encode_cursor`
/// (standard `urlsafe_b64encode`, padding included).
pub fn encode_cursor(created_at: DateTime<Utc>, id: uuid::Uuid) -> String {
    let raw = serde_json::to_string(&[
        serde_json::Value::String(created_at.to_rfc3339()),
        serde_json::Value::String(id.to_string()),
    ])
    .unwrap_or_default();
    URL_SAFE.encode(raw.as_bytes())
}

/// Decode a base64url cursor; anything malformed → 400 `invalid_cursor`
/// (the Python services raise `APIError(400, "invalid_cursor", ...)`).
pub fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, uuid::Uuid), ApiError> {
    let invalid = || {
        ApiError::new(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "کرسر صفحه‌بندی نامعتبر است",
        )
    };
    let bytes = URL_SAFE.decode(cursor.as_bytes()).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let parsed: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    let arr = parsed.as_array().ok_or_else(invalid)?;
    if arr.len() != 2 {
        return Err(invalid());
    }
    let ts = arr[0]
        .as_str()
        .and_then(parse_datetime_str)
        .ok_or_else(invalid)?;
    let id = arr[1]
        .as_str()
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
        .ok_or_else(invalid)?;
    Ok((ts, id))
}

/// Parse a path UUID (FastAPI parses path UUIDs as Pydantic fields —
/// malformed → 422 `validation_error`, not axum's plain 400).
pub fn parse_path_uuid(s: &str, field: &str) -> VResult<uuid::Uuid> {
    s.parse::<uuid::Uuid>()
        .map_err(|_| err(field, "must be a UUID"))
}
