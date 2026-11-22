//! Religious places + widgets-adjacent admin — mirrors
//! `app/modules/places/{router,service,schemas,types}.py` (Wave 7).
//!
//! Public `GET /api/v1/places` (published only, bounding-box filter),
//! `POST /api/v1/places/submissions` (anonymous, draft-only) and
//! `GET /api/v1/places/{place_id}` (published only); admin
//! `POST|GET /api/v1/admin/places`, `GET|PATCH /api/v1/admin/places/{id}`,
//! `POST .../transition`, `DELETE ...` (draft only) behind
//! `places.manage`.
//!
//! Workflow is the shared §9 machine (`workflow.rs`); place types come
//! from the live `places.types` setting with the `types.py` defaults as
//! fallback (a bad setting never breaks registration). The 360-video
//! reference must point at a `video_360` library item. Telegram hooks
//! on transitions are skipped (ADR-025 class: Wave 7 telegram work).

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission},
    db::{new_id, utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    settings::get_setting,
    validate::{parse_body, parse_path_uuid, parse_query, query_int},
    workflow::validate_transition,
    AppState, Config,
};

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn not_found() -> ApiError {
    ApiError::not_found("place_not_found", "مکان یافت نشد")
}

/// `LocalizedText` — `schemas.py`: `fa`/`ar`/`en` each default `""`.
/// Missing keys default to `""`; a present non-string (or null) is a
/// 422; unknown keys are dropped (Pydantic `extra='ignore'`).
fn parse_localized(v: &Value, field: &str) -> Result<Value, ApiError> {
    let Some(map) = v.as_object() else {
        return Err(validation_err(field, "must be an object"));
    };
    let mut out = Map::new();
    for loc in ["fa", "ar", "en"] {
        match map.get(loc) {
            None => {
                out.insert(loc.to_string(), json!(""));
            }
            Some(Value::String(s)) => {
                out.insert(loc.to_string(), json!(s));
            }
            Some(_) => return Err(validation_err(field, "must be a string")),
        }
    }
    Ok(Value::Object(out))
}

fn req_localized(body: &Value, field: &str) -> Result<Value, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) => parse_localized(v, field),
        None => Err(validation_err(field, "field is required")),
    }
}

fn opt_localized(body: &Value, field: &str) -> Result<Option<Value>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) => Ok(Some(parse_localized(v, field)?)),
        None => Ok(None),
    }
}

/// Required float: JSON number or numeric string (Pydantic lax
/// coercion); bool / anything else is a 422. Range-checked.
fn body_f64(v: &Value, field: &str, min: f64, max: f64) -> Result<f64, ApiError> {
    let n = match v {
        Value::Number(n) => n
            .as_f64()
            .ok_or_else(|| validation_err(field, "must be a number"))?,
        Value::String(s) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| validation_err(field, "must be a number"))?,
        _ => return Err(validation_err(field, "must be a number")),
    };
    if !n.is_finite() || n < min || n > max {
        return Err(validation_err(field, "out of range"));
    }
    Ok(n)
}

fn req_f64(body: &Value, field: &str, min: f64, max: f64) -> Result<f64, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) => body_f64(v, field, min, max),
        None => Err(validation_err(field, "field is required")),
    }
}

/// Raw (untrimmed) optional string, max `max` chars; absent/null → None.
/// Python does not strip `city`/`type` (only the URL fields are
/// normalized), so neither do we.
fn opt_raw_str(body: &Value, field: &str, max: usize) -> Result<Option<String>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() > max {
                return Err(validation_err(field, "length out of range"));
            }
            Ok(Some(s.clone()))
        }
        Some(_) => Err(validation_err(field, "must be a string")),
        None => Ok(None),
    }
}

/// URL field: `_normalize_url` — strip, empty → None — then the
/// `^https?://` pattern + 1024 max_length from `schemas.py`.
fn opt_url(body: &Value, field: &str) -> Result<Option<String>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            let t = s.trim();
            if t.is_empty() {
                return Ok(None);
            }
            if t.chars().count() > 1024 {
                return Err(validation_err(field, "length out of range"));
            }
            if !(t.starts_with("http://") || t.starts_with("https://")) {
                return Err(validation_err(field, "must be an http(s) URL"));
            }
            Ok(Some(t.to_string()))
        }
        Some(_) => Err(validation_err(field, "must be a string")),
        None => Ok(None),
    }
}

/// Gallery images: at most 20, each `{url, caption?}`; the URL is
/// required and normalized like the other URL fields (empty → 422,
/// because the field itself is required), caption defaults to `""`
/// and is capped at 200 chars.
fn opt_images(body: &Value) -> Result<Option<Value>, ApiError> {
    match body.get("images").filter(|v| !v.is_null()) {
        Some(Value::Array(items)) => {
            if items.len() > 20 {
                return Err(validation_err("images", "at most 20 images"));
            }
            let mut out = Vec::new();
            for item in items {
                let Some(obj) = item.as_object() else {
                    return Err(validation_err("images", "must be objects"));
                };
                let url = match obj.get("url").filter(|v| !v.is_null()) {
                    Some(Value::String(s)) => {
                        let t = s.trim();
                        if t.is_empty() || t.chars().count() > 1024 {
                            return Err(validation_err("url", "length out of range"));
                        }
                        if !(t.starts_with("http://") || t.starts_with("https://")) {
                            return Err(validation_err("url", "must be an http(s) URL"));
                        }
                        t.to_string()
                    }
                    _ => return Err(validation_err("url", "field is required")),
                };
                let caption = match obj.get("caption").filter(|v| !v.is_null()) {
                    Some(Value::String(s)) => {
                        if s.chars().count() > 200 {
                            return Err(validation_err("caption", "length out of range"));
                        }
                        s.clone()
                    }
                    Some(_) => return Err(validation_err("caption", "must be a string")),
                    None => String::new(),
                };
                out.push(json!({ "url": url, "caption": caption }));
            }
            Ok(Some(Value::Array(out)))
        }
        Some(_) => Err(validation_err("images", "must be an array")),
        None => Ok(None),
    }
}

fn opt_uuid(body: &Value, field: &str) -> Result<Option<Uuid>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s
            .parse::<Uuid>()
            .map(Some)
            .map_err(|_| validation_err(field, "must be a UUID")),
        Some(_) => Err(validation_err(field, "must be a UUID")),
        None => Ok(None),
    }
}

/// Optional float query param: absent → None; present but unparsable
/// or out of range → 422 (FastAPI `Query` validation).
fn query_opt_f64(
    params: &HashMap<String, String>,
    name: &str,
    min: f64,
    max: f64,
) -> Result<Option<f64>, ApiError> {
    match params.get(name) {
        Some(v) => match v.trim().parse::<f64>() {
            Ok(n) if n.is_finite() && (min..=max).contains(&n) => Ok(Some(n)),
            _ => Err(validation_err(name, "out of range")),
        },
        None => Ok(None),
    }
}

// --- place types (`types.py`) ------------------------------------------------

const DEFAULT_PLACE_TYPE_KEYS: &[&str] = &[
    "mosque",
    "haramain",
    "library",
    "museum",
    "pilgrimage",
    "other",
];

/// `parse_place_types` — defensive: anything unparseable yields the
/// defaults, never an exception.
fn parse_place_types(raw: &Value) -> HashSet<String> {
    let Some(list) = raw.as_array().filter(|a| !a.is_empty()) else {
        return DEFAULT_PLACE_TYPE_KEYS
            .iter()
            .map(|s| s.to_string())
            .collect();
    };
    let mut out: HashSet<String> = HashSet::new();
    for item in list {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let Some(key) = obj.get("key").and_then(|k| k.as_str()) else {
            continue;
        };
        let key = key.trim();
        if key.is_empty() || key.chars().count() > 32 {
            continue;
        }
        out.insert(key.to_string());
    }
    if out.is_empty() {
        return DEFAULT_PLACE_TYPE_KEYS
            .iter()
            .map(|s| s.to_string())
            .collect();
    }
    out
}

/// `_allowed_types` — any settings failure falls back to the defaults.
async fn allowed_types(db: &Db, config: &Config) -> HashSet<String> {
    let raw = get_setting(db, config, "places.types").await.ok().flatten();
    match raw {
        None => DEFAULT_PLACE_TYPE_KEYS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        Some(s) => match serde_json::from_str::<Value>(&s) {
            Ok(v) => parse_place_types(&v),
            Err(_) => DEFAULT_PLACE_TYPE_KEYS
                .iter()
                .map(|s| s.to_string())
                .collect(),
        },
    }
}

async fn check_type(
    db: &Db,
    config: &Config,
    place_type: Option<&str>,
    keep: Option<&str>,
) -> Result<(), ApiError> {
    let Some(t) = place_type else {
        return Ok(());
    };
    if keep == Some(t) {
        return Ok(());
    }
    if !allowed_types(db, config).await.contains(t) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_place_type",
            format!("نوع مکان ناشناخته است: {t}"),
        ));
    }
    Ok(())
}

async fn check_360(db: &Db, video_360_id: Option<Uuid>) -> Result<(), ApiError> {
    let Some(id) = video_360_id else {
        return Ok(());
    };
    let row = db
        .fetch_optional(
            "SELECT kind FROM library_items WHERE id = ?",
            &[id.into_arg()],
        )
        .await?;
    let ok = row
        .as_ref()
        .and_then(|r| r.try_get::<String, _>("kind").ok())
        .is_some_and(|k| k == "video_360");
    if !ok {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_360_ref",
            "ارجاع ویدیوی ۳۶۰ نامعتبر است",
            json!({ "video_360_id": id.to_string() }),
        ));
    }
    Ok(())
}

// --- row mapping -------------------------------------------------------------

fn place_from_row(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let name: Value = row.try_get("name")?;
    let description: Option<Value> = row.try_get("description")?;
    let lat: f64 = row.try_get("lat")?;
    let lng: f64 = row.try_get("lng")?;
    let place_type: String = row.try_get("type")?;
    let city: Option<String> = row.try_get("city")?;
    let image_url: Option<String> = row.try_get("image_url")?;
    let images: Option<Value> = row.try_get("images")?;
    let video_360_id: Option<Uuid> = row.try_get("video_360_id")?;
    let video_url: Option<String> = row.try_get("video_url")?;
    let workflow_status: String = row.try_get("workflow_status")?;
    let version: i32 = row.try_get("version")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: DateTime<Utc> = row.try_get("updated_at")?;
    Ok(json!({
        "id": id.to_string(),
        "name": name,
        "description": description,
        "lat": lat,
        "lng": lng,
        "type": place_type,
        "city": city,
        "image_url": image_url,
        "images": images,
        "video_360_id": video_360_id.map(|u| u.to_string()),
        "video_url": video_url,
        "workflow_status": workflow_status,
        "version": version,
        "created_at": created_at.to_rfc3339(),
        "updated_at": updated_at.to_rfc3339(),
    }))
}

const PLACE_COLS: &str = "id, name, description, lat, lng, type, city, image_url, images, \
     video_360_id, video_url, workflow_status, version, created_at, updated_at";

async fn get_place_row(db: &Db, place_id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        &format!("SELECT {PLACE_COLS} FROM religious_places WHERE id = ?"),
        &[place_id.into_arg()],
    )
    .await?
    .ok_or_else(not_found)
}

/// Filters for the shared places list query (public + admin).
struct ListPlacesFilter<'a> {
    published_only: bool,
    min_lat: Option<f64>,
    max_lat: Option<f64>,
    min_lng: Option<f64>,
    max_lng: Option<f64>,
    place_type: Option<&'a str>,
    limit: i64,
}

async fn list_places(db: &Db, f: ListPlacesFilter<'_>) -> Result<(Vec<Value>, i64), ApiError> {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if f.published_only {
        conds.push("workflow_status = ?".to_string());
        args.push("published".into_arg());
    }
    if let Some(v) = f.min_lat {
        conds.push("lat >= ?".to_string());
        args.push(v.into_arg());
    }
    if let Some(v) = f.max_lat {
        conds.push("lat <= ?".to_string());
        args.push(v.into_arg());
    }
    if let Some(v) = f.min_lng {
        conds.push("lng >= ?".to_string());
        args.push(v.into_arg());
    }
    if let Some(v) = f.max_lng {
        conds.push("lng <= ?".to_string());
        args.push(v.into_arg());
    }
    if let Some(t) = f.place_type.filter(|s| !s.is_empty()) {
        conds.push("type = ?".to_string());
        args.push(t.into_arg());
    }
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let total_row = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS n FROM religious_places{where_sql}"),
            &args,
        )
        .await?;
    let total: i64 = total_row.try_get("n")?;
    let mut list_args = args.clone();
    list_args.push(f.limit.into_arg());
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {PLACE_COLS} FROM religious_places{where_sql} \
                 ORDER BY created_at ASC LIMIT ?"
            ),
            &list_args,
        )
        .await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(place_from_row(row)?);
    }
    Ok((items, total))
}

fn ip_of(headers: &HeaderMap, peer: SocketAddr, state: &AppState) -> Option<String> {
    client_ip(headers, Some(peer), &state.config.trusted_proxies)
}

fn user_agent_of(headers: &HeaderMap) -> Option<String> {
    headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

async fn admin_ctx(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<crate::authn::AuthCtx, ApiError> {
    let ctx = authenticate(state, headers, "/api/v1/admin/places").await?;
    require_permission(state, &ctx, "places.manage").await?;
    Ok(ctx)
}

// --- public ------------------------------------------------------------------

/// `GET /api/v1/places` — published only, bounding-box + type filter.
pub async fn list_public_places(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let min_lat = query_opt_f64(&params, "min_lat", -90.0, 90.0)?;
    let max_lat = query_opt_f64(&params, "max_lat", -90.0, 90.0)?;
    let min_lng = query_opt_f64(&params, "min_lng", -180.0, 180.0)?;
    let max_lng = query_opt_f64(&params, "max_lng", -180.0, 180.0)?;
    let place_type = params.get("type").filter(|s| !s.is_empty()).cloned();
    let limit = query_int(&params, "limit", 100, 1, 500)?;
    let db = state.db_or_500()?;
    let (items, total) = list_places(
        db,
        ListPlacesFilter {
            published_only: true,
            min_lat,
            max_lat,
            min_lng,
            max_lng,
            place_type: place_type.as_deref(),
            limit,
        },
    )
    .await?;
    Ok(Json(json!({ "items": items, "total": total })))
}

/// `POST /api/v1/places/submissions` — anonymous, draft-only (201).
pub async fn submit_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let name = req_localized(&parsed, "name")?;
    let description = opt_localized(&parsed, "description")?;
    let lat = req_f64(&parsed, "lat", -90.0, 90.0)?;
    let lng = req_f64(&parsed, "lng", -180.0, 180.0)?;
    let place_type = opt_raw_str(&parsed, "type", 32)?.unwrap_or_else(|| "mosque".to_string());
    let city = opt_raw_str(&parsed, "city", 128)?.filter(|s| !s.is_empty());
    let image_url = opt_url(&parsed, "image_url")?;
    let db = state.db_or_500()?;
    check_type(db, &state.config, Some(&place_type), None).await?;
    let id = new_id();
    let now = utc_now();
    db.execute(
        "INSERT INTO religious_places (id, name, description, lat, lng, type, city, \
         image_url, images, video_360_id, video_url, workflow_status, version, \
         created_by, submitted_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft', 1, ?, ?, ?, ?)",
        &[
            id.into_arg(),
            name.into_arg(),
            description.into_arg(),
            lat.into_arg(),
            lng.into_arg(),
            place_type.clone().into_arg(),
            city.into_arg(),
            image_url.into_arg(),
            (Option::<Value>::None).into_arg(),
            (Option::<Uuid>::None).into_arg(),
            (Option::<String>::None).into_arg(),
            (Option::<Uuid>::None).into_arg(),
            (Option::<Uuid>::None).into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "religious_place",
        &id.to_string(),
        "place.submitted",
        json!({ "type": place_type, "lat": lat, "lng": lng }),
    )
    .await?;
    let ip = ip_of(&headers, peer.0, &state);
    let ua = user_agent_of(&headers);
    log_audit(
        db,
        None,
        "public",
        "places.submit",
        "religious_place",
        Some(&id.to_string()),
        None,
        Some(json!({ "type": place_type, "lat": lat, "lng": lng })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let row = get_place_row(db, id).await?;
    Ok((StatusCode::CREATED, Json(place_from_row(&row)?)))
}

/// `GET /api/v1/places/{place_id}` — published only (else 404).
pub async fn get_public_place(
    State(state): State<AppState>,
    Path(place_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&place_id, "place_id")?;
    let db = state.db_or_500()?;
    let row = get_place_row(db, id).await?;
    let out = place_from_row(&row)?;
    if out["workflow_status"] != "published" {
        return Err(not_found());
    }
    Ok(Json(out))
}

// --- admin -------------------------------------------------------------------

/// `POST /api/v1/admin/places` (201).
pub async fn admin_create_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let name = req_localized(&parsed, "name")?;
    let description = opt_localized(&parsed, "description")?;
    let lat = req_f64(&parsed, "lat", -90.0, 90.0)?;
    let lng = req_f64(&parsed, "lng", -180.0, 180.0)?;
    let place_type = opt_raw_str(&parsed, "type", 32)?.unwrap_or_else(|| "mosque".to_string());
    let city = opt_raw_str(&parsed, "city", 128)?.filter(|s| !s.is_empty());
    let image_url = opt_url(&parsed, "image_url")?;
    let images = opt_images(&parsed)?;
    let video_360_id = opt_uuid(&parsed, "video_360_id")?;
    let video_url = opt_url(&parsed, "video_url")?;
    let db = state.db_or_500()?;
    check_360(db, video_360_id).await?;
    check_type(db, &state.config, Some(&place_type), None).await?;
    let id = new_id();
    let now = utc_now();
    db.execute(
        "INSERT INTO religious_places (id, name, description, lat, lng, type, city, \
         image_url, images, video_360_id, video_url, workflow_status, version, \
         created_by, submitted_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft', 1, ?, ?, ?, ?)",
        &[
            id.into_arg(),
            name.into_arg(),
            description.into_arg(),
            lat.into_arg(),
            lng.into_arg(),
            place_type.clone().into_arg(),
            city.into_arg(),
            image_url.into_arg(),
            images.into_arg(),
            video_360_id.into_arg(),
            video_url.into_arg(),
            Some(ctx.user.id).into_arg(),
            (Option::<Uuid>::None).into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "religious_place",
        &id.to_string(),
        "place.created",
        json!({ "type": place_type, "lat": lat, "lng": lng }),
    )
    .await?;
    let ip = ip_of(&headers, peer.0, &state);
    let ua = user_agent_of(&headers);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "places.create",
        "religious_place",
        Some(&id.to_string()),
        None,
        Some(json!({ "type": place_type, "lat": lat, "lng": lng })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let row = get_place_row(db, id).await?;
    Ok((StatusCode::CREATED, Json(place_from_row(&row)?)))
}

/// `GET /api/v1/admin/places` — all statuses; the `status` filter is
/// applied AFTER the limit, exactly like Python (router filters the
/// fetched page in memory).
pub async fn admin_list_places(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    let place_type = params.get("type").filter(|s| !s.is_empty()).cloned();
    let limit = query_int(&params, "limit", 100, 1, 500)?;
    let db = state.db_or_500()?;
    let (mut items, mut total) = list_places(
        db,
        ListPlacesFilter {
            published_only: false,
            min_lat: None,
            max_lat: None,
            min_lng: None,
            max_lng: None,
            place_type: place_type.as_deref(),
            limit,
        },
    )
    .await?;
    if let Some(s) = &status {
        items.retain(|i| i["workflow_status"].as_str() == Some(s.as_str()));
        total = items.len() as i64;
    }
    Ok(Json(json!({ "items": items, "total": total })))
}

/// `GET /api/v1/admin/places/{place_id}` — any status.
pub async fn admin_get_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(place_id): Path<String>,
) -> ApiResult<Json<Value>> {
    admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&place_id, "place_id")?;
    let db = state.db_or_500()?;
    let row = get_place_row(db, id).await?;
    Ok(Json(place_from_row(&row)?))
}

/// `PATCH /api/v1/admin/places/{place_id}` — `exclude_unset` semantics:
/// absent fields are untouched; a present null clears only
/// `video_360_id` / `city` / `image_url` / `images` / `video_url`
/// (name/description/lat/lng/type nulls are ignored, like Python).
pub async fn admin_update_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(place_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&place_id, "place_id")?;
    let parsed = parse_body(&body)?;
    let db = state.db_or_500()?;
    let row = get_place_row(db, id).await?;
    let old = place_from_row(&row)?;
    let old_type = old["type"].as_str().unwrap_or("").to_string();

    // Validate everything before writing anything.
    let mut sets: Vec<(String, Arg)> = Vec::new();
    if let Some(v) = parsed.get("video_360_id") {
        let vid = match v {
            Value::Null => None,
            Value::String(s) => Some(
                s.parse::<Uuid>()
                    .map_err(|_| validation_err("video_360_id", "must be a UUID"))?,
            ),
            _ => return Err(validation_err("video_360_id", "must be a UUID")),
        };
        check_360(db, vid).await?;
        sets.push(("video_360_id = ?".to_string(), vid.into_arg()));
    }
    if let Some(v) = parsed.get("type").filter(|v| !v.is_null()) {
        let Some(t) = v.as_str() else {
            return Err(validation_err("type", "must be a string"));
        };
        if t.chars().count() > 32 {
            return Err(validation_err("type", "length out of range"));
        }
        check_type(db, &state.config, Some(t), Some(&old_type)).await?;
        sets.push(("type = ?".to_string(), t.to_string().into_arg()));
    }
    if let Some(v) = parsed.get("name").filter(|v| !v.is_null()) {
        let name = parse_localized(v, "name")?;
        sets.push(("name = ?".to_string(), name.into_arg()));
    }
    if let Some(v) = parsed.get("description").filter(|v| !v.is_null()) {
        let desc = parse_localized(v, "description")?;
        sets.push(("description = ?".to_string(), desc.into_arg()));
    }
    if let Some(v) = parsed.get("lat").filter(|v| !v.is_null()) {
        let lat = body_f64(v, "lat", -90.0, 90.0)?;
        sets.push(("lat = ?".to_string(), lat.into_arg()));
    }
    if let Some(v) = parsed.get("lng").filter(|v| !v.is_null()) {
        let lng = body_f64(v, "lng", -180.0, 180.0)?;
        sets.push(("lng = ?".to_string(), lng.into_arg()));
    }
    if let Some(v) = parsed.get("city") {
        let city = match v {
            Value::Null => None,
            Value::String(s) => {
                if s.chars().count() > 128 {
                    return Err(validation_err("city", "length out of range"));
                }
                // Unlike create (`city or None`), update stores "" as-is.
                Some(s.clone())
            }
            _ => return Err(validation_err("city", "must be a string")),
        };
        sets.push(("city = ?".to_string(), city.into_arg()));
    }
    if let Some(v) = parsed.get("image_url") {
        let url = match v {
            Value::Null => None,
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    if t.chars().count() > 1024 {
                        return Err(validation_err("image_url", "length out of range"));
                    }
                    if !(t.starts_with("http://") || t.starts_with("https://")) {
                        return Err(validation_err("image_url", "must be an http(s) URL"));
                    }
                    Some(t.to_string())
                }
            }
            _ => return Err(validation_err("image_url", "must be a string")),
        };
        sets.push(("image_url = ?".to_string(), url.into_arg()));
    }
    if let Some(v) = parsed.get("video_url") {
        let url = match v {
            Value::Null => None,
            Value::String(s) => {
                let t = s.trim();
                if t.is_empty() {
                    None
                } else {
                    if t.chars().count() > 1024 {
                        return Err(validation_err("video_url", "length out of range"));
                    }
                    if !(t.starts_with("http://") || t.starts_with("https://")) {
                        return Err(validation_err("video_url", "must be an http(s) URL"));
                    }
                    Some(t.to_string())
                }
            }
            _ => return Err(validation_err("video_url", "must be a string")),
        };
        sets.push(("video_url = ?".to_string(), url.into_arg()));
    }
    if parsed.get("images").is_some() {
        let images = opt_images(&parsed)?;
        sets.push(("images = ?".to_string(), images.into_arg()));
    }

    let old_version: i32 = old["version"].as_i64().unwrap_or(1) as i32;
    let now = utc_now();
    sets.push(("version = ?".to_string(), (old_version + 1).into_arg()));
    sets.push(("updated_at = ?".to_string(), now.into_arg()));
    let (frags, args): (Vec<String>, Vec<Arg>) = sets.into_iter().unzip();
    let mut all_args = args;
    all_args.push(id.into_arg());
    db.execute(
        &format!(
            "UPDATE religious_places SET {} WHERE id = ?",
            frags.join(", ")
        ),
        &all_args,
    )
    .await?;
    let new_row = get_place_row(db, id).await?;
    let new_out = place_from_row(&new_row)?;
    let ip = ip_of(&headers, peer.0, &state);
    let ua = user_agent_of(&headers);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "places.update",
        "religious_place",
        Some(&id.to_string()),
        Some(json!({
            "name": old["name"],
            "lat": old["lat"],
            "lng": old["lng"],
            "type": old["type"],
            "workflow_status": old["workflow_status"],
        })),
        Some(json!({
            "name": new_out["name"],
            "lat": new_out["lat"],
            "lng": new_out["lng"],
            "type": new_out["type"],
            "workflow_status": new_out["workflow_status"],
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(new_out))
}

/// `POST /api/v1/admin/places/{place_id}/transition` — shared §9 machine.
pub async fn admin_transition_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(place_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&place_id, "place_id")?;
    let parsed = parse_body(&body)?;
    let target = match parsed.get("target").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("target", "field is required")),
    };
    let db = state.db_or_500()?;
    let row = get_place_row(db, id).await?;
    let old = place_from_row(&row)?;
    let current = old["workflow_status"].as_str().unwrap_or("").to_string();
    validate_transition(&current, &target)?;
    let old_version: i32 = old["version"].as_i64().unwrap_or(1) as i32;
    let now = utc_now();
    db.execute(
        "UPDATE religious_places SET workflow_status = ?, version = ?, updated_at = ? \
         WHERE id = ?",
        &[
            target.clone().into_arg(),
            (old_version + 1).into_arg(),
            now.into_arg(),
            id.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "religious_place",
        &id.to_string(),
        &format!("place.{target}"),
        json!({ "actor_id": ctx.user.id.to_string(), "from": current, "to": target }),
    )
    .await?;
    let ip = ip_of(&headers, peer.0, &state);
    let ua = user_agent_of(&headers);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "places.transition",
        "religious_place",
        Some(&id.to_string()),
        Some(json!({ "workflow_status": current })),
        Some(json!({ "workflow_status": target })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let new_row = get_place_row(db, id).await?;
    Ok(Json(place_from_row(&new_row)?))
}

/// `DELETE /api/v1/admin/places/{place_id}` — draft only (204).
pub async fn admin_delete_place(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(place_id): Path<String>,
) -> ApiResult<StatusCode> {
    let ctx = admin_ctx(&state, &headers).await?;
    let id = parse_path_uuid(&place_id, "place_id")?;
    let db = state.db_or_500()?;
    let row = get_place_row(db, id).await?;
    let out = place_from_row(&row)?;
    if out["workflow_status"] != "draft" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "place_not_deletable",
            "فقط مکان‌های پیش‌نویس قابل حذف‌اند؛ بقیه اول باید بایگانی شوند",
        ));
    }
    let ip = ip_of(&headers, peer.0, &state);
    let ua = user_agent_of(&headers);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "places.delete",
        "religious_place",
        Some(&id.to_string()),
        Some(json!({ "name": out["name"] })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    db.execute(
        "DELETE FROM religious_places WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
