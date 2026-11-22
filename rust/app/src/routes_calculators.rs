//! Calculator routes — zakat + inheritance (miras) + mosaref
//! (مُصَرِّف, the Arabic conjugator) + nisab prices + rule configs.
//!
//! A faithful port of `app/modules/calculators/` (router.py,
//! service.py, engine.py, zakat_engine.py, miras_engine.py,
//! mosaref_engine.py, prices.py, source_rules.py). The engines live in
//! the sibling modules loaded below; this file is only the HTTP layer
//! (validation, auth, the approved-rule gate, persistence, audit).
//!
//! Compatibility notes (see also the ADR drafts in the wave report):
//! * Money/weight amounts are exact `Dec` values, never `f64`.
//! * `estate_value` is stored and returned at 4dp and nisab prices at
//!   6dp — the production `NUMERIC(20,4)` / `NUMERIC(20,6)` scales —
//!   formatted in the app so sqlite and Postgres agree.
//! * `get_active_rule` filters in Rust (effective window, madhab,
//!   required `rule_data` key), like the Python loop.
//! * The zakat price settings are read RAW (`get_setting`): the keys
//!   are not in Python's `SETTING_DEFS`, so `get_typed_setting` would
//!   try to JSON-parse a URL and fail.
//! * Fiqh rule tables are never invented here: every rule comes from
//!   an approved `rule_config` row (or the verbatim source-rule data
//!   seeded through the admin API in tests).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::log_audit,
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    db::{new_id, utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    settings::get_setting,
    validate::{
        as_int, opt_bool_loose, opt_datetime, opt_dict, opt_int_loose, parse_body, parse_path_uuid,
        parse_query, query_int, req_str_raw,
    },
    AppState,
};

use crate::calc_decimal::{dec_from_json, py_repr, CalcError, Dec};
use crate::calc_engines::{
    compute_inheritance, compute_miras, compute_zakat, compute_zakat_extended, ExtendedSections,
    MirasEstate, MirasFail, MirasFamily, ZakatRates,
};
use crate::calc_mosaref::conjugate_verb;

const VALID_MADHABS: &[&str] = &["hanafi", "maliki", "shafii", "hanbali"];
const RULE_STATUSES: &[&str] = &["draft", "pending_review", "approved", "rejected"];
const PRICE_TTL_DEFAULT: i64 = 21_600;
const CACHE_SOURCE: &str = "default";

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Optional auth (like the Python `_optional_user`): any auth failure
/// → guest, never an error.
async fn optional_user(state: &AppState, headers: &HeaderMap) -> Option<AuthCtx> {
    authenticate(state, headers, "/api/v1/calculators")
        .await
        .ok()
}

fn idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A required body string, raw (no trimming — Pydantic does not trim).
fn req_string(body: &Value, field: &str) -> Result<String, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(validation_err(field, "must be a string")),
        None => Err(validation_err(field, "field is required")),
    }
}

/// An optional body string, raw; absent/null → None.
fn opt_string(body: &Value, field: &str) -> Result<Option<String>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(validation_err(field, "must be a string")),
        None => Ok(None),
    }
}

fn req_dec(body: &Value, field: &str) -> Result<Dec, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) => dec_from_json(v).ok_or_else(|| validation_err(field, "must be a number")),
        None => Err(validation_err(field, "field is required")),
    }
}

fn opt_dec(body: &Value, field: &str) -> Result<Option<Dec>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) => dec_from_json(v)
            .map(Some)
            .ok_or_else(|| validation_err(field, "must be a number")),
        None => Ok(None),
    }
}

fn calc_api(e: CalcError) -> ApiError {
    e.to_api()
}

// ------------------------------------------------------------------ gate ---

struct ActiveRule {
    id: Uuid,
    version: i64,
    rule_data: Value,
}

fn rules_not_approved(kind: &str, madhab: Option<&str>) -> ApiError {
    ApiError::with_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        "rules_not_approved",
        "هنوز هیچ پیکربندی تأییدشده‌ای برای این محاسبه وجود ندارد؛ قواعد باید ابتدا توسط عالم بازبینی و تأیید شوند",
        json!({ "kind": kind, "madhab": madhab }),
    )
}

/// The newest approved in-window `rule_config` whose `rule_data`
/// contains `require_key` — `service.get_active_rule`, with the
/// filtering done in Rust (ADR-014: expiry comparisons in Rust).
async fn get_active_rule(
    db: &Db,
    kind: &str,
    madhab: Option<&str>,
    require_key: &str,
) -> Result<ActiveRule, ApiError> {
    if kind == "inheritance" && !matches!(madhab, Some(m) if VALID_MADHABS.contains(&m)) {
        let shown = match madhab {
            Some(m) => py_repr(m),
            None => "None".to_string(),
        };
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_madhab",
            format!("مذهب نامعتبر: {shown}"),
        ));
    }
    let (sql, is_zakat) = if kind == "zakat" {
        (
            "SELECT id, version, madhab, rule_data, effective_from, effective_to \
             FROM zakat_rule_configs WHERE status = 'approved' ORDER BY version DESC",
            true,
        )
    } else {
        (
            "SELECT id, version, madhab, rule_data, effective_from, effective_to \
             FROM inheritance_rule_configs WHERE status = 'approved' ORDER BY version DESC",
            false,
        )
    };
    let rows = db.fetch_all(sql, &[]).await?;
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
        let madhab_ok = if is_zakat {
            match madhab {
                None => row_madhab.is_none(),
                Some(m) => row_madhab.as_deref() == Some(m),
            }
        } else {
            row_madhab.as_deref() == madhab
        };
        if !madhab_ok {
            continue;
        }
        let rule_data: Value = row.try_get("rule_data")?;
        let has_key = rule_data
            .as_object()
            .map(|o| o.contains_key(require_key))
            .unwrap_or(false);
        if !has_key {
            continue;
        }
        return Ok(ActiveRule {
            id: row.try_get("id")?,
            // INTEGER columns read as i32 on Postgres (ADR-028).
            version: row.try_get::<i32, _>("version")? as i64,
            rule_data,
        });
    }
    Err(rules_not_approved(kind, madhab))
}

// ---------------------------------------------------------------- prices ---

struct PriceInfo {
    status: String,
    gold_per_gram: Option<String>,
    silver_per_gram: Option<String>,
    currency: Option<String>,
    source: String,
    fetched_at: Option<String>,
    stale: bool,
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
    // `_s(v)` in prices.py: `str(Decimal(str(v)))` on the DB value —
    // the NUMERIC(20,6) scale, so always 6dp here.
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
    // Python swallows any settings error → graceful `None`.
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

async fn read_cache(db: &Db) -> Result<Option<CacheRow>, ApiError> {
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

/// `prices.get_nisab_prices` — cache + TTL + live fetch + stale fallback.
async fn get_nisab_prices(db: &Db, config: &crate::Config) -> Result<PriceInfo, ApiError> {
    let source_url = price_source_url(db, config).await;
    let ttl = price_ttl_seconds(db, config).await;
    let cached = read_cache(db).await?;
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

// ------------------------------------------------------- public: inheritance ---

fn case_to_out(row: &DynRow) -> Result<Value, ApiError> {
    let estate_raw: String = row.try_get("estate_value")?;
    let estate = Dec::parse(&estate_raw)
        .and_then(|d| d.fmt_fixed(4).ok())
        .unwrap_or(estate_raw);
    Ok(json!({
        "id": row.try_get::<Uuid, _>("id")?,
        "madhab": row.try_get::<String, _>("madhab")?,
        "estate_value": estate,
        "heirs": row.try_get::<Value, _>("heirs")?,
        "result": row.try_get::<Value, _>("result")?,
        "rule_config_id": row.try_get::<Option<Uuid>, _>("rule_config_id")?,
        "created_at": row.try_get::<DateTime<Utc>, _>("created_at")?,
    }))
}

pub async fn inheritance_calculate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let body = parse_body(&body)?;
    let madhab = req_string(&body, "madhab")?;
    let estate = req_dec(&body, "estate_value")?;
    if estate <= Dec::zero() {
        return Err(validation_err("estate_value", "must be greater than 0"));
    }
    let heirs_raw = match body.get("heirs") {
        Some(Value::Array(items)) if !items.is_empty() => items,
        Some(_) => return Err(validation_err("heirs", "must be a non-empty list")),
        None => return Err(validation_err("heirs", "field is required")),
    };
    let mut heirs: Vec<(String, i64)> = Vec::new();
    let mut heirs_json: Vec<Value> = Vec::new();
    for item in heirs_raw {
        let Some(obj) = item.as_object() else {
            return Err(validation_err("heirs", "must be a list of objects"));
        };
        let heir_type = match obj.get("type").filter(|v| !v.is_null()) {
            Some(Value::String(s)) if !s.is_empty() => s.clone(),
            _ => return Err(validation_err("heirs.type", "must be a non-empty string")),
        };
        let count = match obj.get("count").filter(|v| !v.is_null()) {
            Some(v) => as_int(v)
                .filter(|n| *n >= 1)
                .ok_or_else(|| validation_err("heirs.count", "must be an integer >= 1"))?,
            None => 1,
        };
        heirs.push((heir_type.clone(), count));
        heirs_json.push(json!({ "type": heir_type, "count": count }));
    }
    let idem = idempotency_key(&headers);
    let user = optional_user(&state, &headers).await;

    if let Some(key) = &idem {
        let existing = db
            .fetch_optional(
                "SELECT id, madhab, CAST(estate_value AS TEXT) AS estate_value, heirs, \
                 result, rule_config_id, created_at FROM inheritance_cases \
                 WHERE idempotency_key = ?",
                &[key.into_arg()],
            )
            .await?;
        if let Some(row) = existing {
            return Ok(Json(case_to_out(&row)?));
        }
    }

    let rule = get_active_rule(db, "inheritance", Some(&madhab), "shares").await?;
    let estate_4dp = estate.fmt_fixed(4).map_err(calc_api)?;
    let result =
        compute_inheritance(&heirs, estate, &rule.rule_data, rule.version).map_err(calc_api)?;
    let id = new_id();
    let created_at = utc_now();
    db.execute(
        "INSERT INTO inheritance_cases (id, user_id, estate_value, madhab, heirs, \
         result, rule_config_id, idempotency_key, created_at) \
         VALUES (?, ?, CAST(? AS NUMERIC(20,4)), ?, ?, ?, ?, ?, ?)",
        &[
            (id).into_arg(),
            (user.as_ref().map(|u| u.user.id)).into_arg(),
            (estate_4dp.clone()).into_arg(),
            (madhab.clone()).into_arg(),
            (Value::Array(heirs_json.clone())).into_arg(),
            (result.clone()).into_arg(),
            (rule.id).into_arg(),
            (idem.clone()).into_arg(),
            (created_at).into_arg(),
        ],
    )
    .await?;
    Ok(Json(json!({
        "id": id,
        "madhab": madhab,
        "estate_value": estate_4dp,
        "heirs": heirs_json,
        "result": result,
        "rule_config_id": rule.id,
        "created_at": created_at,
    })))
}

pub async fn inheritance_cases_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/calculators/inheritance/cases").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let after = params.get("after").cloned();
    let after_uuid = match &after {
        Some(c) => Some(c.parse::<Uuid>().map_err(|_| ApiError::internal())?),
        None => None,
    };
    let (sql, args): (String, Vec<Arg>) = match after_uuid {
        Some(id) => (
            "SELECT id, madhab, CAST(estate_value AS TEXT) AS estate_value, heirs, \
             result, rule_config_id, created_at FROM inheritance_cases \
             WHERE user_id = ? AND id < ? ORDER BY created_at DESC LIMIT ?"
                .to_string(),
            vec![
                (ctx.user.id).into_arg(),
                (id).into_arg(),
                ((limit + 1) as i64).into_arg(),
            ],
        ),
        None => (
            "SELECT id, madhab, CAST(estate_value AS TEXT) AS estate_value, heirs, \
             result, rule_config_id, created_at FROM inheritance_cases \
             WHERE user_id = ? ORDER BY created_at DESC LIMIT ?"
                .to_string(),
            vec![(ctx.user.id).into_arg(), ((limit + 1) as i64).into_arg()],
        ),
    };
    let rows = db.fetch_all(&sql, &args).await?;
    let next_cursor = if rows.len() as i64 > limit {
        rows.get(limit as usize)
            .and_then(|r| r.try_get::<Uuid, _>("id").ok())
            .map(|id| id.to_string())
    } else {
        None
    };
    let mut items = Vec::new();
    for row in rows.iter().take(limit as usize) {
        items.push(case_to_out(row)?);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

// ------------------------------------------------------------ public: zakat ---

fn zakat_to_out(row: &DynRow) -> Result<Value, ApiError> {
    Ok(json!({
        "id": row.try_get::<Uuid, _>("id")?,
        "inputs": row.try_get::<Value, _>("inputs")?,
        "result": row.try_get::<Value, _>("result")?,
        "nisab_snapshot": row.try_get::<Value, _>("nisab_snapshot")?,
        "rule_config_id": row.try_get::<Option<Uuid>, _>("rule_config_id")?,
        "created_at": row.try_get::<DateTime<Utc>, _>("created_at")?,
    }))
}

pub async fn zakat_calculate(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let body = parse_body(&body)?;
    let assets_raw = match body.get("assets") {
        Some(Value::Object(map)) => map,
        Some(_) => return Err(validation_err("assets", "must be an object")),
        None => return Err(validation_err("assets", "field is required")),
    };
    let mut assets: Vec<(String, Dec)> = Vec::new();
    let mut assets_json = serde_json::Map::new();
    for (k, v) in assets_raw {
        let dec = dec_from_json(v).ok_or_else(|| validation_err("assets", "must be numbers"))?;
        assets_json.insert(k.clone(), json!(dec.to_py_string()));
        assets.push((k.clone(), dec));
    }
    let debts = opt_dec(&body, "debts")?.unwrap_or_else(Dec::zero);
    if debts < Dec::zero() {
        return Err(validation_err(
            "debts",
            "must be greater than or equal to 0",
        ));
    }
    let madhab = opt_string(&body, "madhab")?;
    let idem = idempotency_key(&headers);
    let user = optional_user(&state, &headers).await;

    if let Some(key) = &idem {
        let existing = db
            .fetch_optional(
                "SELECT id, inputs, result, nisab_snapshot, rule_config_id, created_at \
                 FROM zakat_calculations WHERE idempotency_key = ?",
                &[key.into_arg()],
            )
            .await?;
        if let Some(row) = existing {
            return Ok(Json(zakat_to_out(&row)?));
        }
    }

    let rule = get_active_rule(db, "zakat", madhab.as_deref(), "asset_categories").await?;
    let price_info = get_nisab_prices(db, &state.config).await?;
    let rates = ZakatRates {
        gold_per_gram: price_info.gold_per_gram.as_deref().and_then(Dec::parse),
        silver_per_gram: price_info.silver_per_gram.as_deref().and_then(Dec::parse),
        currency: price_info.currency.clone(),
        stale: price_info.stale,
    };
    let result =
        compute_zakat(&assets, debts, &rule.rule_data, &rates, rule.version).map_err(calc_api)?;
    let id = new_id();
    let created_at = utc_now();
    let inputs = json!({
        "assets": assets_json,
        "debts": debts.to_py_string(),
        "madhab": madhab,
    });
    let nisab_snapshot = price_info_to_json(&price_info);
    db.execute(
        "INSERT INTO zakat_calculations (id, user_id, inputs, result, nisab_snapshot, \
         rule_config_id, idempotency_key, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            (id).into_arg(),
            (user.as_ref().map(|u| u.user.id)).into_arg(),
            (inputs.clone()).into_arg(),
            (result.clone()).into_arg(),
            (nisab_snapshot.clone()).into_arg(),
            (rule.id).into_arg(),
            (idem.clone()).into_arg(),
            (created_at).into_arg(),
        ],
    )
    .await?;
    Ok(Json(json!({
        "id": id,
        "inputs": inputs,
        "result": result,
        "nisab_snapshot": nisab_snapshot,
        "rule_config_id": rule.id,
        "created_at": created_at,
    })))
}

pub async fn nisab_rate(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let info = get_nisab_prices(db, &state.config).await?;
    Ok(Json(price_info_to_json(&info)))
}

// ---------------------------------------------- public: extended / miras ---

fn req_int_field(body: &Value, field: &str, min: i64, max: i64) -> Result<i64, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) => as_int(v)
            .filter(|n| (min..=max).contains(n))
            .ok_or_else(|| validation_err(field, "out of range")),
        None => Err(validation_err(field, "field is required")),
    }
}

fn opt_int_field(body: &Value, field: &str, min: i64, max: i64) -> Result<Option<i64>, ApiError> {
    opt_int_loose(body, field, min, max)
}

pub async fn zakat_extended(State(state): State<AppState>, body: Bytes) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let body = parse_body(&body)?;
    let mut sections = ExtendedSections::default();

    if let Some(money) = body.get("money").filter(|v| !v.is_null()) {
        if !money.is_object() {
            return Err(validation_err("money", "must be an object"));
        }
        let amount = req_dec(money, "amount")?;
        if amount < Dec::zero() {
            return Err(validation_err(
                "money.amount",
                "must be greater than or equal to 0",
            ));
        }
        let coin_price = req_dec(money, "coin_price")?;
        if coin_price <= Dec::zero() {
            return Err(validation_err("money.coin_price", "must be greater than 0"));
        }
        sections.money = Some((amount, coin_price));
    }
    if let Some(v) = opt_dec(&body, "gold_grams")? {
        if v < Dec::zero() {
            return Err(validation_err(
                "gold_grams",
                "must be greater than or equal to 0",
            ));
        }
        sections.gold_grams = Some(v);
    }
    if let Some(v) = opt_dec(&body, "silver_grams")? {
        if v < Dec::zero() {
            return Err(validation_err(
                "silver_grams",
                "must be greater than or equal to 0",
            ));
        }
        sections.silver_grams = Some(v);
    }
    if let Some(n) = opt_int_field(&body, "sheep", 0, 1_000_000)? {
        sections.sheep = Some(n);
    }
    if let Some(cows) = body.get("cows").filter(|v| !v.is_null()) {
        if !cows.is_object() {
            return Err(validation_err("cows", "must be an object"));
        }
        let count = req_int_field(cows, "count", 0, 1_000_000)?;
        let musinna_price = opt_dec(cows, "musinna_price")?;
        if let Some(p) = &musinna_price {
            if *p <= Dec::zero() {
                return Err(validation_err("cows", "must be greater than 0"));
            }
        }
        sections.cows = Some((count, musinna_price));
    }
    if let Some(n) = opt_int_field(&body, "camels", 0, 1_000_000)? {
        sections.camels = Some(n);
    }
    if let Some(horses) = body.get("horses").filter(|v| !v.is_null()) {
        if !horses.is_object() {
            return Err(validation_err("horses", "must be an object"));
        }
        let count = req_int_field(horses, "count", 0, 1_000_000)?;
        let total_value = opt_dec(horses, "total_value")?;
        if let Some(v) = &total_value {
            if *v < Dec::zero() {
                return Err(validation_err(
                    "horses.total_value",
                    "must be greater than or equal to 0",
                ));
            }
        }
        sections.horses = Some((count, total_value));
    }
    if let Some(oshr) = body.get("oshr").filter(|v| !v.is_null()) {
        if !oshr.is_object() {
            return Err(validation_err("oshr", "must be an object"));
        }
        let amount_kg = req_dec(oshr, "amount_kg")?;
        if amount_kg < Dec::zero() {
            return Err(validation_err(
                "oshr.amount_kg",
                "must be greater than or equal to 0",
            ));
        }
        let irrigation = req_string(oshr, "irrigation")?;
        if irrigation != "rain_fed" && irrigation != "irrigated" {
            return Err(validation_err(
                "oshr.irrigation",
                "must be 'rain_fed' or 'irrigated'",
            ));
        }
        sections.oshr = Some((amount_kg, irrigation));
    }
    if let Some(khoms) = body.get("khoms").filter(|v| !v.is_null()) {
        if !khoms.is_object() {
            return Err(validation_err("khoms", "must be an object"));
        }
        let amount = req_dec(khoms, "amount")?;
        if amount < Dec::zero() {
            return Err(validation_err(
                "khoms.amount",
                "must be greater than or equal to 0",
            ));
        }
        sections.khoms = Some(amount);
    }

    let rule = get_active_rule(db, "zakat", None, "extended").await?;
    let extended = rule
        .rule_data
        .get("extended")
        .cloned()
        .filter(|v| v.is_object())
        .filter(|v| v.get("format").and_then(|f| f.as_str()) == Some("source_extended_v1"))
        .ok_or_else(|| rules_not_approved("zakat", None))?;
    let result = compute_zakat_extended(&sections, &extended).map_err(calc_api)?;
    Ok(Json(json!({
        "sections": result,
        "applied_rule_version": rule.version,
    })))
}
pub async fn inheritance_miras(
    State(state): State<AppState>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let body = parse_body(&body)?;
    let gender = req_string(&body, "gender")?;
    if gender != "male" && gender != "female" {
        return Err(validation_err("gender", "must be 'male' or 'female'"));
    }
    let b = |f: &str| opt_bool_loose(&body, f).map(|v| v.unwrap_or(false));
    let n = |f: &str, max: i64| opt_int_field(&body, f, 0, max).map(|v| v.unwrap_or(0));
    let family = MirasFamily {
        gender,
        husband: b("husband")?,
        father: b("father")?,
        mother: b("mother")?,
        grandfather: b("grandfather")?,
        mgm: b("mgm")?,
        pgm: b("pgm")?,
        wives: n("wives", 4)?,
        sons: n("sons", 99)?,
        daughters: n("daughters", 99)?,
        son_sons: n("son_sons", 99)?,
        son_daughters: n("son_daughters", 99)?,
        brothers: n("brothers", 99)?,
        sisters: n("sisters", 99)?,
        paternal_brothers: n("paternal_brothers", 99)?,
        paternal_sisters: n("paternal_sisters", 99)?,
        uterine_brothers: n("uterine_brothers", 99)?,
        uterine_sisters: n("uterine_sisters", 99)?,
        full_brother_sons: n("full_brother_sons", 99)?,
        paternal_brother_sons: n("paternal_brother_sons", 99)?,
        full_uncles: n("full_uncles", 99)?,
        paternal_uncles: n("paternal_uncles", 99)?,
        full_uncle_sons: n("full_uncle_sons", 99)?,
        paternal_uncle_sons: n("paternal_uncle_sons", 99)?,
        daughter_sons: n("daughter_sons", 99)?,
        daughter_daughters: n("daughter_daughters", 99)?,
        son_daughter_sons: n("son_daughter_sons", 99)?,
        son_daughter_daughters: n("son_daughter_daughters", 99)?,
        maternal_grandfather: n("maternal_grandfather", 99)?,
        remote_grandmothers: n("remote_grandmothers", 99)?,
        sister_sons: n("sister_sons", 99)?,
        sister_daughters: n("sister_daughters", 99)?,
        full_brother_daughters: n("full_brother_daughters", 99)?,
        paternal_brother_daughters: n("paternal_brother_daughters", 99)?,
        uterine_brother_children: n("uterine_brother_children", 99)?,
        uterine_sister_children: n("uterine_sister_children", 99)?,
        maternal_uncles: n("maternal_uncles", 99)?,
        maternal_aunts: n("maternal_aunts", 99)?,
        maternal_paternal_uncles: n("maternal_paternal_uncles", 99)?,
        paternal_aunts: n("paternal_aunts", 99)?,
        paternal_uncle_daughters: n("paternal_uncle_daughters", 99)?,
    };
    let estate_value = req_dec(&body, "estate_value")?;
    if estate_value < Dec::zero() {
        return Err(validation_err(
            "estate_value",
            "must be greater than or equal to 0",
        ));
    }
    let mut estate = MirasEstate {
        estate_value,
        funeral: Dec::zero(),
        debt: Dec::zero(),
        will: Dec::zero(),
        will_for_heir: false,
        will_approved: false,
        religion_mixed: false,
        killer: false,
        simultaneous_death: false,
        slavery: false,
    };
    estate.funeral = opt_dec(&body, "funeral")?.unwrap_or_else(Dec::zero);
    estate.debt = opt_dec(&body, "debt")?.unwrap_or_else(Dec::zero);
    estate.will = opt_dec(&body, "will")?.unwrap_or_else(Dec::zero);
    if estate.funeral < Dec::zero() || estate.debt < Dec::zero() || estate.will < Dec::zero() {
        return Err(validation_err(
            "estate",
            "funeral, debt and will must be greater than or equal to 0",
        ));
    }
    estate.will_for_heir = opt_bool_loose(&body, "will_for_heir")?.unwrap_or(false);
    estate.will_approved = opt_bool_loose(&body, "will_approved")?.unwrap_or(false);
    if let Some(imp) = body.get("impediments").filter(|v| !v.is_null()) {
        if !imp.is_object() {
            return Err(validation_err("impediments", "must be an object"));
        }
        estate.religion_mixed = opt_bool_loose(imp, "religion_mixed")?.unwrap_or(false);
        estate.killer = opt_bool_loose(imp, "killer")?.unwrap_or(false);
        estate.simultaneous_death = opt_bool_loose(imp, "simultaneous_death")?.unwrap_or(false);
        estate.slavery = opt_bool_loose(imp, "slavery")?.unwrap_or(false);
    }

    let rule = get_active_rule(db, "inheritance", Some("hanafi"), "miras").await?;
    let miras = rule
        .rule_data
        .get("miras")
        .cloned()
        .filter(|v| v.is_object())
        .filter(|v| v.get("format").and_then(|f| f.as_str()) == Some("miras_v1"))
        .ok_or_else(|| rules_not_approved("inheritance", Some("hanafi")))?;
    match compute_miras(&family, &estate, &miras) {
        Ok(mut result) => {
            result["applied_rule_version"] = json!(rule.version);
            Ok(Json(result))
        }
        Err(MirasFail::Stopped(code)) => Ok(Json(json!({
            "status": "stopped",
            "stop_code": code,
            "applied_rule_version": rule.version,
        }))),
        Err(MirasFail::Err(e)) => Err(calc_api(e)),
    }
}

pub async fn mosaref_conjugate(body: Bytes) -> ApiResult<Json<Value>> {
    let body = parse_body(&body)?;
    let root = req_str_raw(&body, "root", 1, 12)?;
    let bab = req_str_raw(&body, "bab", 1, 32)?;
    let transitivity = req_string(&body, "transitivity")?;
    if transitivity != "mutaadi" && transitivity != "laazim" {
        return Err(validation_err(
            "transitivity",
            "must be 'mutaadi' or 'laazim'",
        ));
    }
    let harf_jar = opt_string(&body, "harf_jar")?.unwrap_or_else(|| "bi".to_string());
    if !["bi", "li", "an", "min", "ila", "ala", "fi"].contains(&harf_jar.as_str()) {
        return Err(validation_err("harf_jar", "unknown harf_jar"));
    }
    let out = conjugate_verb(&root, &bab, &transitivity, &harf_jar).map_err(calc_api)?;
    Ok(Json(out))
}

// ------------------------------------------------------------------ admin ---

const RULE_COLS: &str = "id, madhab, source, version, status, effective_from, \
    effective_to, approved_by, approved_at, change_reason, rule_data, created_at, updated_at";

fn rule_table(kind: &str) -> &'static str {
    if kind == "zakat" {
        "zakat_rule_configs"
    } else {
        "inheritance_rule_configs"
    }
}

fn entity_type(kind: &str) -> &'static str {
    if kind == "zakat" {
        "zakat_rule_config"
    } else {
        "inheritance_rule_config"
    }
}

fn bad_rule_kind(kind: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad_rule_kind",
        format!("kind نامعتبر: {}", py_repr(kind)),
    )
}

/// `kind` from a query string: required, then `_model_for` validation.
fn query_kind(params: &HashMap<String, String>) -> Result<String, ApiError> {
    match params.get("kind") {
        Some(k) if k == "inheritance" || k == "zakat" => Ok(k.clone()),
        Some(k) => Err(bad_rule_kind(k)),
        None => Err(validation_err("kind", "field is required")),
    }
}

fn rule_to_out(kind: &str, row: &DynRow) -> Result<Value, ApiError> {
    let asset_categories: Value = if kind == "zakat" {
        row.try_get::<Option<Value>, _>("asset_categories")?
            .unwrap_or(json!({}))
    } else {
        json!({})
    };
    Ok(json!({
        "id": row.try_get::<Uuid, _>("id")?,
        "kind": kind,
        "madhab": row.try_get::<Option<String>, _>("madhab")?,
        "source": row.try_get::<String, _>("source")?,
        "version": row.try_get::<i32, _>("version")? as i64,
        "status": row.try_get::<String, _>("status")?,
        "effective_from": row.try_get::<DateTime<Utc>, _>("effective_from")?,
        "effective_to": row.try_get::<Option<DateTime<Utc>>, _>("effective_to")?,
        "approved_by": row.try_get::<Option<Uuid>, _>("approved_by")?,
        "approved_at": row.try_get::<Option<DateTime<Utc>>, _>("approved_at")?,
        "change_reason": row.try_get::<Option<String>, _>("change_reason")?,
        "rule_data": row.try_get::<Value, _>("rule_data")?,
        "asset_categories": asset_categories,
        "created_at": row.try_get::<DateTime<Utc>, _>("created_at")?,
        "updated_at": row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?,
    }))
}

async fn fetch_rule(db: &Db, kind: &str, id: Uuid) -> Result<DynRow, ApiError> {
    let cols = if kind == "zakat" {
        format!("{RULE_COLS}, asset_categories")
    } else {
        RULE_COLS.to_string()
    };
    let sql = format!("SELECT {cols} FROM {} WHERE id = ?", rule_table(kind));
    let row = db.fetch_optional(&sql, &[(id).into_arg()]).await?;
    match row {
        Some(r) => Ok(r),
        None => Err(ApiError::not_found(
            "rule_not_found",
            "پیکربندی قاعده یافت نشد",
        )),
    }
}

pub async fn rule_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let params = parse_query(&uri);
    let kind = params.get("kind").cloned();
    if let Some(k) = &kind {
        if k != "inheritance" && k != "zakat" {
            return Err(bad_rule_kind(k));
        }
    }
    let madhab = params.get("madhab").cloned();
    let status = params.get("status").cloned();
    if let Some(s) = &status {
        if !RULE_STATUSES.contains(&s.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_rule_status",
                format!("وضعیت نامعتبر: {}", py_repr(s)),
            ));
        }
    }
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let after = params.get("after").cloned();

    let kinds: Vec<&str> = match kind.as_deref() {
        Some(k) => vec![k],
        None => vec!["inheritance", "zakat"],
    };
    let mut merged: Vec<(DateTime<Utc>, Value)> = Vec::new();
    for k in kinds {
        let cols = if k == "zakat" {
            format!("{RULE_COLS}, asset_categories")
        } else {
            RULE_COLS.to_string()
        };
        let mut sql = format!("SELECT {cols} FROM {} ", rule_table(k));
        let mut conds: Vec<String> = Vec::new();
        let mut args: Vec<Arg> = Vec::new();
        if let Some(m) = &madhab {
            conds.push("madhab = ?".to_string());
            args.push(m.into_arg());
        }
        if let Some(s) = &status {
            conds.push("status = ?".to_string());
            args.push(s.into_arg());
        }
        if !conds.is_empty() {
            sql.push_str("WHERE ");
            sql.push_str(&conds.join(" AND "));
            sql.push(' ');
        }
        sql.push_str("ORDER BY created_at DESC");
        let rows = db.fetch_all(&sql, &args).await?;
        for row in &rows {
            let created_at: DateTime<Utc> = row.try_get("created_at")?;
            merged.push((created_at, rule_to_out(k, row)?));
        }
    }
    // Stable sort by created_at desc (Python `list.sort` is stable).
    merged.sort_by_key(|a| std::cmp::Reverse(a.0));
    let mut filtered: Vec<Value> = merged.into_iter().map(|(_, v)| v).collect();
    if let Some(cursor) = &after {
        filtered.retain(|d| {
            d.get("id")
                .and_then(|v| v.as_str())
                .map(|id| id < cursor.as_str())
                .unwrap_or(false)
        });
    }
    let next_cursor = if filtered.len() as i64 > limit {
        filtered
            .get(limit as usize)
            .and_then(|d| d.get("id"))
            .and_then(|v| v.as_str())
            .map(str::to_string)
    } else {
        None
    };
    let items: Vec<Value> = filtered.into_iter().take(limit as usize).collect();
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

pub async fn rule_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let body = parse_body(&body)?;
    let kind = req_string(&body, "kind")?;
    if kind != "inheritance" && kind != "zakat" {
        return Err(validation_err("kind", "must be 'inheritance' or 'zakat'"));
    }
    let madhab = opt_string(&body, "madhab")?;
    if kind == "inheritance" {
        if !matches!(madhab.as_deref(), Some(m) if VALID_MADHABS.contains(&m)) {
            let shown = match &madhab {
                Some(m) => py_repr(m),
                None => "None".to_string(),
            };
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_madhab",
                format!("مذهب نامعتبر: {shown}"),
            ));
        }
    } else if let Some(m) = &madhab {
        if !VALID_MADHABS.contains(&m.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_madhab",
                format!("مذهب نامعتبر: {}", py_repr(m)),
            ));
        }
    }
    let source = req_string(&body, "source")?;
    if source.is_empty() {
        return Err(validation_err("source", "must be a non-empty string"));
    }
    let version = match body.get("version").filter(|v| !v.is_null()) {
        Some(v) => as_int(v)
            .filter(|n| *n >= 1)
            .ok_or_else(|| validation_err("version", "must be an integer >= 1"))?,
        None => return Err(validation_err("version", "field is required")),
    };
    let effective_from = match body.get("effective_from").filter(|v| !v.is_null()) {
        Some(v) => crate::validate::datetime_value(v, "effective_from")?,
        None => return Err(validation_err("effective_from", "field is required")),
    };
    let effective_to = opt_datetime(&body, "effective_to")?;
    let change_reason = opt_string(&body, "change_reason")?;
    let rule_data = match opt_dict(&body, "rule_data")? {
        Some(d) => d,
        None => json!({}),
    };
    let asset_categories = opt_dict(&body, "asset_categories")?;

    // Duplicate version → 409 (Python catches the IntegrityError).
    let dup_sql = if kind == "inheritance" {
        "SELECT id FROM inheritance_rule_configs WHERE madhab = ? AND version = ?"
    } else {
        "SELECT id FROM zakat_rule_configs WHERE version = ?"
    };
    let dup_args: Vec<Arg> = if kind == "inheritance" {
        vec![
            (madhab.clone().unwrap_or_default()).into_arg(),
            (version).into_arg(),
        ]
    } else {
        vec![(version).into_arg()]
    };
    if db.fetch_optional(dup_sql, &dup_args).await?.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "rule_version_exists",
            "برای این مذهب نسخه‌ای با همین شماره از قبل وجود دارد",
        ));
    }

    let id = new_id();
    let created_at = utc_now();
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    if kind == "inheritance" {
        db.execute(
            "INSERT INTO inheritance_rule_configs (id, madhab, source, version, \
             effective_from, effective_to, approved_by, approved_at, change_reason, \
             status, rule_data, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft', ?, ?, ?)",
            &[
                (id).into_arg(),
                (madhab.clone()).into_arg(),
                (source.clone()).into_arg(),
                (version).into_arg(),
                (effective_from).into_arg(),
                (effective_to).into_arg(),
                (Uuid::null_arg()),
                (DateTime::<Utc>::null_arg()),
                (change_reason.clone()).into_arg(),
                (rule_data.clone()).into_arg(),
                (created_at).into_arg(),
                (DateTime::<Utc>::null_arg()),
            ],
        )
        .await?;
    } else {
        db.execute(
            "INSERT INTO zakat_rule_configs (id, madhab, source, version, \
             effective_from, effective_to, approved_by, approved_at, change_reason, \
             status, rule_data, asset_categories, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft', ?, ?, ?, ?)",
            &[
                (id).into_arg(),
                (madhab.clone()).into_arg(),
                (source.clone()).into_arg(),
                (version).into_arg(),
                (effective_from).into_arg(),
                (effective_to).into_arg(),
                (Uuid::null_arg()),
                (DateTime::<Utc>::null_arg()),
                (change_reason.clone()).into_arg(),
                (rule_data.clone()).into_arg(),
                (asset_categories.clone().unwrap_or(json!({}))).into_arg(),
                (created_at).into_arg(),
                (DateTime::<Utc>::null_arg()),
            ],
        )
        .await?;
    }
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "calculator.rule_created",
        entity_type(&kind),
        Some(&id.to_string()),
        None,
        Some(json!({
            "kind": kind,
            "madhab": madhab,
            "version": version,
            "status": "draft",
            "source": source,
        })),
        ip.as_deref(),
        None,
    )
    .await?;
    let row = fetch_rule(db, &kind, id).await?;
    Ok((StatusCode::CREATED, Json(rule_to_out(&kind, &row)?)))
}

pub async fn rule_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(rule_id): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let rule_id = parse_path_uuid(&rule_id, "rule_id")?;
    let params = parse_query(&uri);
    let kind = query_kind(&params)?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    Ok(Json(rule_to_out(&kind, &row)?))
}

pub async fn rule_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(rule_id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let rule_id = parse_path_uuid(&rule_id, "rule_id")?;
    let params = parse_query(&uri);
    let kind = query_kind(&params)?;
    let body = parse_body(&body)?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    let status: String = row.try_get("status")?;
    if status != "draft" && status != "rejected" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "rule_locked",
            "فقط پیکربندی‌های draft/rejected قابل ویرایش‌اند؛ برای تغییر نسخهٔ تأییدشده، نسخهٔ جدید بسازید",
        ));
    }
    let old = json!({
        "status": status,
        "version": row.try_get::<i32, _>("version")? as i64,
        "source": row.try_get::<String, _>("source")?,
    });

    let mut sets: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    let mut changed: Vec<&str> = Vec::new();
    if let Some(source) = opt_string(&body, "source")? {
        sets.push("source = ?".to_string());
        args.push(source.into_arg());
        changed.push("source");
    }
    if let Some(effective_from) = opt_datetime(&body, "effective_from")? {
        sets.push("effective_from = ?".to_string());
        args.push(effective_from.into_arg());
        changed.push("effective_from");
    }
    if let Some(effective_to) = opt_datetime(&body, "effective_to")? {
        sets.push("effective_to = ?".to_string());
        args.push(effective_to.into_arg());
        changed.push("effective_to");
    }
    if let Some(change_reason) = opt_string(&body, "change_reason")? {
        sets.push("change_reason = ?".to_string());
        args.push(change_reason.into_arg());
        changed.push("change_reason");
    }
    if let Some(rule_data) = opt_dict(&body, "rule_data")? {
        sets.push("rule_data = ?".to_string());
        args.push(rule_data.into_arg());
        changed.push("rule_data");
    }
    if kind == "zakat" {
        if let Some(asset_categories) = opt_dict(&body, "asset_categories")? {
            sets.push("asset_categories = ?".to_string());
            args.push(asset_categories.into_arg());
            changed.push("asset_categories");
        }
    }
    let now = utc_now();
    if status == "rejected" {
        sets.push("status = 'draft'".to_string());
        changed.push("status");
    }
    sets.push("updated_at = ?".to_string());
    args.push(now.into_arg());
    changed.sort_unstable();
    let sql = format!(
        "UPDATE {} SET {} WHERE id = ?",
        rule_table(&kind),
        sets.join(", ")
    );
    args.push((rule_id).into_arg());
    db.execute(&sql, &args).await?;
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "calculator.rule_updated",
        entity_type(&kind),
        Some(&rule_id.to_string()),
        Some(old),
        Some(json!({ "changed_fields": changed })),
        ip.as_deref(),
        None,
    )
    .await?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    Ok(Json(rule_to_out(&kind, &row)?))
}

pub async fn rule_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(rule_id): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let rule_id = parse_path_uuid(&rule_id, "rule_id")?;
    let params = parse_query(&uri);
    let kind = query_kind(&params)?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    let status: String = row.try_get("status")?;
    if status != "draft" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_rule_transition",
            format!("فقط draft قابل ارسال برای بازبینی است (وضعیت فعلی: {status})"),
        ));
    }
    let now = utc_now();
    db.execute(
        &format!(
            "UPDATE {} SET status = 'pending_review', updated_at = ? WHERE id = ?",
            rule_table(&kind)
        ),
        &[(now).into_arg(), (rule_id).into_arg()],
    )
    .await?;
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "calculator.rule_submitted",
        entity_type(&kind),
        Some(&rule_id.to_string()),
        Some(json!({ "status": "draft" })),
        Some(json!({ "status": "pending_review" })),
        ip.as_deref(),
        None,
    )
    .await?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    Ok(Json(rule_to_out(&kind, &row)?))
}

pub async fn rule_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(rule_id): Path<String>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.approve").await?;
    let rule_id = parse_path_uuid(&rule_id, "rule_id")?;
    let params = parse_query(&uri);
    let kind = query_kind(&params)?;
    let body = parse_body(&body)?;
    let decision = req_string(&body, "decision")?;
    if decision != "approve" && decision != "reject" {
        return Err(validation_err("decision", "must be 'approve' or 'reject'"));
    }
    let note = opt_string(&body, "note")?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    let status: String = row.try_get("status")?;
    if status != "pending_review" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_rule_transition",
            format!("فقط pending_review قابل بازبینی است (وضعیت فعلی: {status})"),
        ));
    }
    let now = utc_now();
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    if decision == "approve" {
        let mut sets =
            "status = 'approved', approved_by = ?, approved_at = ?, updated_at = ?".to_string();
        let mut args: Vec<Arg> = vec![(ctx.user.id).into_arg(), (now).into_arg(), (now).into_arg()];
        if let Some(n) = note.as_deref().filter(|s| !s.is_empty()) {
            sets.push_str(", change_reason = ?");
            args.push(n.into_arg());
        }
        let sql = format!("UPDATE {} SET {sets} WHERE id = ?", rule_table(&kind));
        args.push((rule_id).into_arg());
        db.execute(&sql, &args).await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "user",
            "calculator.rule_approved",
            entity_type(&kind),
            Some(&rule_id.to_string()),
            Some(json!({ "status": "pending_review" })),
            Some(json!({
                "status": "approved",
                "approved_by": ctx.user.id.to_string(),
                "note": note.is_some(),
            })),
            ip.as_deref(),
            None,
        )
        .await?;
    } else {
        let mut sets = "status = 'rejected', updated_at = ?".to_string();
        let mut args: Vec<Arg> = vec![(now).into_arg()];
        if let Some(n) = note.as_deref().filter(|s| !s.is_empty()) {
            sets.push_str(", change_reason = ?");
            args.push(n.into_arg());
        }
        let sql = format!("UPDATE {} SET {sets} WHERE id = ?", rule_table(&kind));
        args.push((rule_id).into_arg());
        db.execute(&sql, &args).await?;
        log_audit(
            db,
            Some(ctx.user.id),
            "user",
            "calculator.rule_rejected",
            entity_type(&kind),
            Some(&rule_id.to_string()),
            Some(json!({ "status": "pending_review" })),
            Some(json!({
                "status": "rejected",
                "approved_by": Value::Null,
                "note": note.is_some(),
            })),
            ip.as_deref(),
            None,
        )
        .await?;
    }
    let row = fetch_rule(db, &kind, rule_id).await?;
    Ok(Json(rule_to_out(&kind, &row)?))
}

pub async fn rule_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(rule_id): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/rules").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let rule_id = parse_path_uuid(&rule_id, "rule_id")?;
    let params = parse_query(&uri);
    let kind = query_kind(&params)?;
    let row = fetch_rule(db, &kind, rule_id).await?;
    let madhab: Option<String> = row.try_get("madhab")?;
    let cols = if kind == "zakat" {
        format!("{RULE_COLS}, asset_categories")
    } else {
        RULE_COLS.to_string()
    };
    let (sql, args): (String, Vec<Arg>) = match &madhab {
        Some(m) => (
            format!(
                "SELECT {cols} FROM {} WHERE madhab = ? ORDER BY version DESC",
                rule_table(&kind)
            ),
            vec![(m).into_arg()],
        ),
        None => (
            format!(
                "SELECT {cols} FROM {} WHERE madhab IS NULL ORDER BY version DESC",
                rule_table(&kind)
            ),
            vec![],
        ),
    };
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(rule_to_out(&kind, row)?);
    }
    Ok(Json(json!({ "items": items, "next_cursor": Value::Null })))
}

pub async fn calculator_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/calculators/audit").await?;
    require_permission(&state, &ctx, "calculators.manage").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let after = params.get("after").cloned();
    let after_uuid = match &after {
        Some(c) => Some(c.parse::<Uuid>().map_err(|_| ApiError::internal())?),
        None => None,
    };
    let (sql, args): (String, Vec<Arg>) = match after_uuid {
        Some(id) => (
            "SELECT id, actor_id, action, entity_type, entity_id, created_at \
             FROM audit_logs WHERE entity_type IN \
             ('inheritance_rule_config', 'zakat_rule_config') AND id < ? \
             ORDER BY created_at DESC LIMIT ?"
                .to_string(),
            vec![(id).into_arg(), ((limit + 1) as i64).into_arg()],
        ),
        None => (
            "SELECT id, actor_id, action, entity_type, entity_id, created_at \
             FROM audit_logs WHERE entity_type IN \
             ('inheritance_rule_config', 'zakat_rule_config') \
             ORDER BY created_at DESC LIMIT ?"
                .to_string(),
            vec![((limit + 1) as i64).into_arg()],
        ),
    };
    let rows = db.fetch_all(&sql, &args).await?;
    let next_cursor = if rows.len() as i64 > limit {
        rows.get(limit as usize)
            .and_then(|r| r.try_get::<Uuid, _>("id").ok())
            .map(|id| id.to_string())
    } else {
        None
    };
    let mut items = Vec::new();
    for row in rows.iter().take(limit as usize) {
        items.push(json!({
            "id": row.try_get::<Uuid, _>("id")?,
            "actor_id": row.try_get::<Option<Uuid>, _>("actor_id")?,
            "action": row.try_get::<String, _>("action")?,
            "entity_type": row.try_get::<String, _>("entity_type")?,
            "entity_id": row.try_get::<Option<String>, _>("entity_id")?,
            "created_at": row.try_get::<DateTime<Utc>, _>("created_at")?,
        }));
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}
