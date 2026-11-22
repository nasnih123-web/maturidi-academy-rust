//! Videos routes — mirrors `app/modules/videos/router.py` +
//! `service.py` (Wave 5).
//!
//! Videos are library items (`kind = video | video_360`); the public
//! visibility rules are exactly the library's
//! (`library_visibility::public_where`). There is no create/upload
//! route here — videos are created through the library admin routes
//! (Wave 6). Subtitle generation is triggered here but the worker
//! (faster-whisper + LLM translation) stays a Python sidecar
//! (ADR-045): the trigger marks rows `pending` and honestly reports
//! `enqueued: false`, the Wave 3 ingest precedent.
//!
//! Route order in the router mirrors Python (`/saved` and
//! `/playlists` before `/{item_id}`); axum's static segments win
//! over captures in any case.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Json, Response},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::log_audit,
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    db::{new_id, utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    library_visibility::{
        is_privileged, library_item_from_row, may_access, public_where, scholar_names, signed_url,
        LibraryItem, LIBRARY_ITEM_COLUMNS,
    },
    subtitles::{
        build_vtt, cue_to_json, is_direct_file_external, is_embed_external, is_subtitle_lang,
        parse_vtt, reset_rows, subtitles_enabled, target_langs, Cue, SUBTITLE_LANGS,
    },
    validate::{
        decode_cursor, encode_cursor, localized_value, opt_bool_loose, opt_int_loose,
        opt_str_raw_max, opt_uuid, parse_body, parse_path_uuid, parse_query, query_int,
        req_localized,
    },
    video_links::{
        aparat_embed_url, parse_aparat_video_hash, parse_youtube_video_id, youtube_embed_url,
        youtube_thumbnail_url,
    },
    AppState,
};

const VIDEO_KINDS_SQL: &str = "kind IN ('video', 'video_360')";

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Optional auth (`_optional_user`): any auth failure → guest.
async fn optional_user(state: &AppState, headers: &HeaderMap) -> Option<AuthCtx> {
    authenticate(state, headers, "/api/v1/videos").await.ok()
}

fn dt_json(dt: Option<DateTime<Utc>>) -> Value {
    match dt {
        Some(d) => Value::String(d.to_rfc3339()),
        None => Value::Null,
    }
}

/// Required float body field (Pydantic lax `float`: JSON number or a
/// numeric string; bool is rejected).
fn req_f64(body: &Value, field: &str) -> Result<f64, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::Number(n)) => n
            .as_f64()
            .ok_or_else(|| validation_err(field, "must be a number")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| validation_err(field, "must be a number")),
        Some(_) => Err(validation_err(field, "must be a number")),
        None => Err(validation_err(field, "field is required")),
    }
}

fn opt_f64(body: &Value, field: &str) -> Result<Option<f64>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::Number(n)) => n
            .as_f64()
            .map(Some)
            .ok_or_else(|| validation_err(field, "must be a number")),
        Some(Value::String(s)) => s
            .trim()
            .parse::<f64>()
            .map(Some)
            .map_err(|_| validation_err(field, "must be a number")),
        Some(_) => Err(validation_err(field, "must be a number")),
        None => Ok(None),
    }
}

fn client_ip_ua(
    headers: &HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    state: &AppState,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer.0), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

// --- shared output builders ------------------------------------------------

fn external_fields(item: &LibraryItem) -> (Option<String>, Option<String>) {
    let Some(url) = item.external_url.clone() else {
        return (None, None);
    };
    if let Some(id) = parse_youtube_video_id(Some(&url)) {
        return (Some(url), Some(youtube_embed_url(&id)));
    }
    if let Some(hash) = parse_aparat_video_hash(Some(&url)) {
        return (Some(url), Some(aparat_embed_url(&hash)));
    }
    (Some(url), None)
}

fn cover_url(state: &AppState, item: &LibraryItem) -> ApiResult<Option<String>> {
    if let Some(url) = signed_url(&state.config, item.cover_media_key.as_deref())? {
        return Ok(Some(url));
    }
    if let Some(url) = &item.external_url {
        if let Some(id) = parse_youtube_video_id(Some(url)) {
            return Ok(Some(youtube_thumbnail_url(&id)));
        }
    }
    Ok(None)
}

/// `videos/service.py::_published_memberships` — published playlists
/// per item, ordered `sort_order ASC, created_at ASC`.
async fn published_memberships(db: &Db, item_ids: &[Uuid]) -> ApiResult<HashMap<Uuid, Vec<Value>>> {
    let mut out: HashMap<Uuid, Vec<Value>> = HashMap::new();
    if item_ids.is_empty() {
        return Ok(out);
    }
    let placeholders = item_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT vpi.item_id, vp.id, vp.title FROM video_playlist_items vpi \
         JOIN video_playlists vp ON vp.id = vpi.playlist_id \
         WHERE vpi.item_id IN ({placeholders}) AND vp.is_published = TRUE \
         ORDER BY vp.sort_order ASC, vp.created_at ASC"
    );
    let args: Vec<Arg> = item_ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&sql, &args).await? {
        let item_id: Uuid = row.try_get("item_id")?;
        let pid: Uuid = row.try_get("id")?;
        let title: Value = row.try_get("title")?;
        out.entry(item_id).or_default().push(json!({
            "id": pid.to_string(),
            "title": title,
        }));
    }
    Ok(out)
}

fn item_out(
    state: &AppState,
    item: &LibraryItem,
    names: &HashMap<Uuid, String>,
    memberships: Option<&Vec<Value>>,
) -> ApiResult<Value> {
    let (external_url, embed_url) = external_fields(item);
    Ok(json!({
        "id": item.id.to_string(),
        "kind": item.kind,
        "title": item.title,
        "description": item.description.clone().unwrap_or(Value::Null),
        "cover_url": cover_url(state, item)?,
        "external_url": external_url,
        "embed_url": embed_url,
        "duration_seconds": item.duration_seconds,
        "scholar_id": item.scholar_id.map(|u| u.to_string()),
        "scholar_name": item.scholar_id.and_then(|id| names.get(&id).cloned()),
        "topic_tags": item.topic_tags,
        "visibility": item.visibility,
        "created_at": item.created_at.to_rfc3339(),
        "published_at": dt_json(item.published_at),
        "playlists": memberships.cloned().unwrap_or_default(),
    }))
}

/// `videos/service.py::_items_out`.
async fn items_out(state: &AppState, db: &Db, items: &[LibraryItem]) -> ApiResult<Vec<Value>> {
    let names = scholar_names(db, &items.iter().map(|i| i.scholar_id).collect::<Vec<_>>()).await?;
    let memberships =
        published_memberships(db, &items.iter().map(|i| i.id).collect::<Vec<_>>()).await?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        out.push(item_out(state, item, &names, memberships.get(&item.id))?);
    }
    Ok(out)
}

/// `videos/service.py::_visible_items_by_ids` — the given ids that
/// are public for this caller.
async fn visible_items_by_ids(
    db: &Db,
    user: Option<&AuthCtx>,
    item_ids: &[Uuid],
) -> ApiResult<HashMap<Uuid, LibraryItem>> {
    let mut out = HashMap::new();
    if item_ids.is_empty() {
        return Ok(out);
    }
    let privileged = is_privileged(db, user).await?;
    let placeholders = item_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!(
        "SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items WHERE {} AND id IN ({placeholders})",
        public_where(privileged, user.is_some())
    );
    let args: Vec<Arg> = item_ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&sql, &args).await? {
        let item = library_item_from_row(&row)?;
        out.insert(item.id, item);
    }
    Ok(out)
}

async fn load_library_item(db: &Db, item_id: Uuid) -> ApiResult<Option<LibraryItem>> {
    let row = db
        .fetch_optional(
            &format!("SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items WHERE id = ?"),
            &[(item_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Some(library_item_from_row(&r)?)),
        None => Ok(None),
    }
}

/// `get_item_detail` visibility gate — unpublished / invisible →
/// 404 `item_not_found`, never 403.
async fn get_watch_item(db: &Db, user: Option<&AuthCtx>, item_id: Uuid) -> ApiResult<LibraryItem> {
    let item = load_library_item(db, item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("item_not_found", "آیتم یافت نشد"))?;
    if item.workflow_status != "published" {
        return Err(ApiError::not_found("item_not_found", "آیتم یافت نشد"));
    }
    let privileged = is_privileged(db, user).await?;
    if !may_access(&item, user.is_some(), privileged) {
        return Err(ApiError::not_found("item_not_found", "آیتم یافت نشد"));
    }
    Ok(item)
}

/// `videos/service.py::_get_video_item_or_error` — must exist (404)
/// and be a video (422 `not_a_video`).
async fn get_video_item(db: &Db, item_id: Uuid) -> ApiResult<LibraryItem> {
    let item = load_library_item(db, item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("item_not_found", "آیتم یافت نشد"))?;
    if item.kind != "video" && item.kind != "video_360" {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_a_video",
            "موقعیت پخش فقط برای آیتم‌های ویدیویی است",
            json!({ "kind": item.kind }),
        ));
    }
    Ok(item)
}

async fn get_library_item_or_404(db: &Db, item_id: Uuid) -> ApiResult<LibraryItem> {
    load_library_item(db, item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("item_not_found", "آیتم یافت نشد"))
}

// --- playlists ---------------------------------------------------------------

const PLAYLIST_COLUMNS: &str = "id, title, description, cover_media_key, scholar_id, \
    is_published, sort_order, created_at, updated_at";

/// Same columns, qualified for the `vp JOIN vpi` query in
/// `playlist_context` — unqualified `id` is ambiguous there
/// (both tables have one) and sqlite rejects the query.
const PLAYLIST_COLUMNS_Q: &str = "vp.id, vp.title, vp.description, vp.cover_media_key, \
    vp.scholar_id, vp.is_published, vp.sort_order, vp.created_at, vp.updated_at";

#[derive(Debug, Clone)]
struct PlaylistRow {
    id: Uuid,
    title: Value,
    description: Option<Value>,
    cover_media_key: Option<String>,
    scholar_id: Option<Uuid>,
    is_published: bool,
    sort_order: i32,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

fn playlist_from_row(row: &DynRow) -> ApiResult<PlaylistRow> {
    Ok(PlaylistRow {
        id: row.try_get("id")?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        cover_media_key: row.try_get("cover_media_key")?,
        scholar_id: row.try_get("scholar_id")?,
        is_published: row.try_get("is_published")?,
        sort_order: row.try_get("sort_order")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

async fn load_playlist(db: &Db, playlist_id: Uuid) -> ApiResult<Option<PlaylistRow>> {
    let row = db
        .fetch_optional(
            &format!("SELECT {PLAYLIST_COLUMNS} FROM video_playlists WHERE id = ?"),
            &[(playlist_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Some(playlist_from_row(&r)?)),
        None => Ok(None),
    }
}

async fn get_playlist_or_404(db: &Db, playlist_id: Uuid) -> ApiResult<PlaylistRow> {
    load_playlist(db, playlist_id)
        .await?
        .ok_or_else(|| ApiError::not_found("playlist_not_found", "لیست پخش یافت نشد"))
}

/// Ordered membership item ids (`position ASC, created_at ASC`).
async fn playlist_items_ordered(db: &Db, playlist_id: Uuid) -> ApiResult<Vec<(Uuid, i32)>> {
    let rows = db
        .fetch_all(
            "SELECT item_id, position FROM video_playlist_items \
             WHERE playlist_id = ? ORDER BY position ASC, created_at ASC",
            &[(playlist_id).into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let item_id: Uuid = row.try_get("item_id")?;
        let position: i32 = row.try_get("position")?;
        out.push((item_id, position));
    }
    Ok(out)
}

/// `videos/service.py::_playlist_out`.
async fn playlist_out(
    state: &AppState,
    db: &Db,
    playlist: &PlaylistRow,
    user: Option<&AuthCtx>,
    item_limit: Option<usize>,
) -> ApiResult<Value> {
    let memberships = playlist_items_ordered(db, playlist.id).await?;
    let ids: Vec<Uuid> = memberships.iter().map(|(id, _)| *id).collect();
    let visible = visible_items_by_ids(db, user, &ids).await?;
    let ordered: Vec<LibraryItem> = memberships
        .iter()
        .filter_map(|(id, _)| visible.get(id))
        .cloned()
        .collect();
    let shown: &[LibraryItem] = match item_limit {
        Some(n) => &ordered[..ordered.len().min(n)],
        None => &ordered,
    };
    let mut cover = signed_url(&state.config, playlist.cover_media_key.as_deref())?;
    if cover.is_none() {
        if let Some(first) = ordered.first() {
            cover = cover_url(state, first)?;
        }
    }
    let names = scholar_names(db, &[playlist.scholar_id]).await?;
    Ok(json!({
        "id": playlist.id.to_string(),
        "title": playlist.title,
        "description": playlist.description.clone().unwrap_or(Value::Null),
        "cover_url": cover,
        "scholar_id": playlist.scholar_id.map(|u| u.to_string()),
        "scholar_name": playlist.scholar_id.and_then(|id| names.get(&id).cloned()),
        "is_published": playlist.is_published,
        "sort_order": playlist.sort_order,
        "item_count": ordered.len(),
        "total_duration_seconds": ordered.iter().map(|i| i.duration_seconds.unwrap_or(0)).sum::<i64>(),
        "items": items_out(state, db, shown).await?,
        "created_at": playlist.created_at.to_rfc3339(),
        "updated_at": playlist.updated_at.to_rfc3339(),
    }))
}

/// `videos/service.py::_admin_playlist_out` — all memberships, any
/// item status, with the raw cover key.
async fn admin_playlist_out(state: &AppState, db: &Db, playlist: &PlaylistRow) -> ApiResult<Value> {
    let _ = state;
    let memberships = playlist_items_ordered(db, playlist.id).await?;
    let ids: Vec<Uuid> = memberships.iter().map(|(id, _)| *id).collect();
    let mut by_id: HashMap<Uuid, LibraryItem> = HashMap::new();
    if !ids.is_empty() {
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items WHERE id IN ({placeholders})"
        );
        let args: Vec<Arg> = ids.iter().map(|id| id.into_arg()).collect();
        for row in db.fetch_all(&sql, &args).await? {
            let item = library_item_from_row(&row)?;
            by_id.insert(item.id, item);
        }
    }
    let mut out_items: Vec<Value> = Vec::new();
    for (item_id, position) in &memberships {
        let Some(item) = by_id.get(item_id) else {
            continue;
        };
        out_items.push(json!({
            "item_id": item.id.to_string(),
            "position": position,
            "kind": item.kind,
            "title": item.title,
            "workflow_status": item.workflow_status,
            "visibility": item.visibility,
            "duration_seconds": item.duration_seconds,
            "created_at": item.created_at.to_rfc3339(),
        }));
    }
    let names = scholar_names(db, &[playlist.scholar_id]).await?;
    Ok(json!({
        "id": playlist.id.to_string(),
        "title": playlist.title,
        "description": playlist.description.clone().unwrap_or(Value::Null),
        "cover_media_key": playlist.cover_media_key,
        "scholar_id": playlist.scholar_id.map(|u| u.to_string()),
        "scholar_name": playlist.scholar_id.and_then(|id| names.get(&id).cloned()),
        "is_published": playlist.is_published,
        "sort_order": playlist.sort_order,
        "item_count": out_items.len(),
        "items": out_items,
        "created_at": playlist.created_at.to_rfc3339(),
        "updated_at": playlist.updated_at.to_rfc3339(),
    }))
}

/// `videos/service.py::_rewrite_positions` — rewrite positions
/// 0-based exactly as the given order (inserting new rows).
async fn rewrite_positions(db: &Db, playlist_id: Uuid, ordered_item_ids: &[Uuid]) -> ApiResult<()> {
    let rows = db
        .fetch_all(
            "SELECT id, item_id FROM video_playlist_items WHERE playlist_id = ?",
            &[(playlist_id).into_arg()],
        )
        .await?;
    let mut existing: HashMap<Uuid, Uuid> = HashMap::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let item_id: Uuid = row.try_get("item_id")?;
        existing.insert(item_id, id);
    }
    let now = utc_now();
    for (idx, item_id) in ordered_item_ids.iter().enumerate() {
        match existing.get(item_id) {
            Some(row_id) => {
                db.execute(
                    "UPDATE video_playlist_items SET position = ? WHERE id = ?",
                    &[(idx as i64).into_arg(), (row_id).into_arg()],
                )
                .await?;
            }
            None => {
                db.execute(
                    "INSERT INTO video_playlist_items (id, playlist_id, item_id, position, created_at) \
                     VALUES (?, ?, ?, ?, ?)",
                    &[
                        (new_id()).into_arg(),
                        (playlist_id).into_arg(),
                        (item_id).into_arg(),
                        (idx as i64).into_arg(),
                        (now).into_arg(),
                    ],
                )
                .await?;
            }
        }
    }
    Ok(())
}

// --- watch -------------------------------------------------------------------

/// `videos/service.py::_playlist_context`.
async fn playlist_context(
    state: &AppState,
    db: &Db,
    item: &LibraryItem,
    user: Option<&AuthCtx>,
    requested_id: Option<Uuid>,
) -> ApiResult<Option<Value>> {
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {PLAYLIST_COLUMNS_Q} FROM video_playlists vp \
                 JOIN video_playlist_items vpi ON vpi.playlist_id = vp.id \
                 WHERE vpi.item_id = ? AND vp.is_published = TRUE \
                 ORDER BY vp.sort_order ASC, vp.created_at ASC"
            ),
            &[(item.id).into_arg()],
        )
        .await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let mut playlists = Vec::new();
    for row in rows {
        playlists.push(playlist_from_row(&row)?);
    }
    let chosen = requested_id
        .and_then(|rid| playlists.iter().find(|p| p.id == rid))
        .unwrap_or(&playlists[0])
        .clone();
    let memberships = playlist_items_ordered(db, chosen.id).await?;
    let ids: Vec<Uuid> = memberships.iter().map(|(id, _)| *id).collect();
    let visible = visible_items_by_ids(db, user, &ids).await?;
    let ordered: Vec<LibraryItem> = memberships
        .iter()
        .filter_map(|(id, _)| visible.get(id))
        .cloned()
        .collect();
    let ordered_ids: Vec<Uuid> = ordered.iter().map(|i| i.id).collect();
    let Some(idx) = ordered_ids.iter().position(|id| *id == item.id) else {
        return Ok(None);
    };
    Ok(Some(json!({
        "playlist": { "id": chosen.id.to_string(), "title": chosen.title },
        "position": idx + 1,
        "total": ordered.len(),
        "prev_item_id": if idx > 0 { Some(ordered_ids[idx - 1].to_string()) } else { None },
        "next_item_id": if idx + 1 < ordered.len() { Some(ordered_ids[idx + 1].to_string()) } else { None },
        "items": items_out(state, db, &ordered).await?,
    })))
}

/// `videos/service.py::_related` — scan the newest 300 visible
/// videos, score by shared topic tags (case-insensitive), stable
/// sort by score desc, take 8.
async fn related(
    state: &AppState,
    db: &Db,
    item: &LibraryItem,
    user: Option<&AuthCtx>,
) -> ApiResult<Vec<Value>> {
    let privileged = is_privileged(db, user).await?;
    let sql = format!(
        "SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items WHERE {} AND {VIDEO_KINDS_SQL} \
         AND id != ? ORDER BY created_at DESC, id DESC LIMIT 300",
        public_where(privileged, user.is_some())
    );
    let rows = db.fetch_all(&sql, &[(item.id).into_arg()]).await?;
    let mut candidates = Vec::new();
    for row in rows {
        candidates.push(library_item_from_row(&row)?);
    }
    let tags: HashSet<String> = item
        .topic_tags
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                .collect()
        })
        .unwrap_or_default();
    let shared = |other: &LibraryItem| -> usize {
        other
            .topic_tags
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_lowercase()))
                    .filter(|t| tags.contains(t))
                    .count()
            })
            .unwrap_or(0)
    };
    // `sort_by_key` with `Reverse`: ties keep the `created_at DESC` order.
    candidates.sort_by_key(|c| std::cmp::Reverse(shared(c)));
    candidates.truncate(8);
    items_out(state, db, &candidates).await
}

/// `videos/service.py::_subtitle_tracks` — ready tracks for the
/// watch payload, in `SUBTITLE_LANGS` order; empty for embed
/// externals and when the `videos.subtitles_enabled` switch is off.
async fn subtitle_tracks(state: &AppState, db: &Db, item: &LibraryItem) -> ApiResult<Vec<Value>> {
    if is_embed_external(item) {
        return Ok(Vec::new());
    }
    if !subtitles_enabled(state).await? {
        return Ok(Vec::new());
    }
    let rows = db
        .fetch_all(
            "SELECT lang FROM item_subtitles WHERE item_id = ? AND status = 'ready'",
            &[(item.id).into_arg()],
        )
        .await?;
    let order = |lang: &str| SUBTITLE_LANGS.iter().position(|l| *l == lang).unwrap_or(99);
    let mut langs: Vec<String> = Vec::new();
    for row in rows {
        let lang: String = row.try_get("lang")?;
        langs.push(lang);
    }
    langs.sort_by_key(|l| order(l));
    Ok(langs
        .iter()
        .map(|lang| {
            json!({
                "lang": lang,
                "url": format!("/api/v1/videos/{}/subtitles/{lang}.vtt", item.id),
            })
        })
        .collect())
}

// --- public ------------------------------------------------------------------

/// `GET /api/v1/videos` — cursor list, `limit` 1–100 default 20.
pub async fn public_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let cursor = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let privileged = is_privileged(db, user.as_ref()).await?;
    let mut sql = format!(
        "SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items WHERE {} AND {VIDEO_KINDS_SQL}",
        public_where(privileged, user.is_some())
    );
    let mut args: Vec<Arg> = Vec::new();
    if let Some(c) = &cursor {
        let (ts, lid) = decode_cursor(c)?;
        sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    for row in rows {
        items.push(library_item_from_row(&row)?);
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let last = &items[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(last.created_at, last.id));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({
        "items": items_out(&state, db, &items).await?,
        "next_cursor": next_cursor,
    })))
}

/// `GET /api/v1/videos/playlists` — published playlists, max 4 items each.
pub async fn public_playlist_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {PLAYLIST_COLUMNS} FROM video_playlists \
                 WHERE is_published = TRUE ORDER BY sort_order ASC, created_at ASC"
            ),
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let playlist = playlist_from_row(&row)?;
        out.push(playlist_out(&state, db, &playlist, user.as_ref(), Some(4)).await?);
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/videos/playlists/{playlist_id}` — unpublished → 404.
pub async fn public_playlist_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(playlist_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let playlist_id = parse_path_uuid(&playlist_id, "playlist_id")?;
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    if !playlist.is_published {
        return Err(ApiError::not_found(
            "playlist_not_found",
            "لیست پخش یافت نشد",
        ));
    }
    Ok(Json(
        playlist_out(&state, db, &playlist, user.as_ref(), None).await?,
    ))
}

/// `GET /api/v1/videos/saved` — the caller's saved items, newest
/// save first; `next_cursor` is always null.
pub async fn saved_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/videos/saved").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT item_id FROM user_saved_videos WHERE user_id = ? \
             ORDER BY created_at DESC, id DESC",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let mut ids = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("item_id")?;
        ids.push(id);
    }
    let visible = visible_items_by_ids(db, Some(&ctx), &ids).await?;
    let ordered: Vec<LibraryItem> = ids
        .iter()
        .filter_map(|id| visible.get(id))
        .cloned()
        .collect();
    Ok(Json(json!({
        "items": items_out(&state, db, &ordered).await?,
        "next_cursor": Value::Null,
    })))
}

/// `GET /api/v1/videos/{item_id}/saved` — guest → `saved: false`,
/// never a 404 even when the item does not exist.
pub async fn saved_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let user = optional_user(&state, &headers).await;
    let saved = match user {
        None => false,
        Some(ctx) => {
            let db = state.db_or_500()?;
            db.fetch_optional(
                "SELECT id FROM user_saved_videos WHERE user_id = ? AND item_id = ? LIMIT 1",
                &[(ctx.user.id).into_arg(), (item_id).into_arg()],
            )
            .await?
            .is_some()
        }
    };
    Ok(Json(
        json!({ "item_id": item_id.to_string(), "saved": saved }),
    ))
}

/// `POST /api/v1/videos/{item_id}/saved` — idempotent; any library
/// kind may be saved.
pub async fn saved_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/videos").await?;
    let db = state.db_or_500()?;
    get_library_item_or_404(db, item_id).await?;
    let existing = db
        .fetch_optional(
            "SELECT id FROM user_saved_videos WHERE user_id = ? AND item_id = ? LIMIT 1",
            &[(ctx.user.id).into_arg(), (item_id).into_arg()],
        )
        .await?;
    if existing.is_none() {
        db.execute(
            "INSERT INTO user_saved_videos (id, user_id, item_id, created_at) VALUES (?, ?, ?, ?)",
            &[
                (new_id()).into_arg(),
                (ctx.user.id).into_arg(),
                (item_id).into_arg(),
                (utc_now()).into_arg(),
            ],
        )
        .await?;
    }
    Ok(Json(
        json!({ "item_id": item_id.to_string(), "saved": true }),
    ))
}

/// `DELETE /api/v1/videos/{item_id}/saved` — idempotent.
pub async fn saved_unsave(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/videos").await?;
    let db = state.db_or_500()?;
    get_library_item_or_404(db, item_id).await?;
    db.execute(
        "DELETE FROM user_saved_videos WHERE user_id = ? AND item_id = ?",
        &[(ctx.user.id).into_arg(), (item_id).into_arg()],
    )
    .await?;
    Ok(Json(
        json!({ "item_id": item_id.to_string(), "saved": false }),
    ))
}

/// `GET /api/v1/videos/{item_id}` — watch payload. Any published,
/// visible library item answers (kind is not checked here).
pub async fn watch_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let user = optional_user(&state, &headers).await;
    let params = parse_query(&uri);
    let playlist_id = match params.get("playlist") {
        Some(raw) => Some(parse_path_uuid(raw, "playlist")?),
        None => None,
    };
    let db = state.db_or_500()?;
    let item = get_watch_item(db, user.as_ref(), item_id).await?;
    let stream_url = if item.external_url.is_none() {
        signed_url(&state.config, item.media_key.as_deref())?
    } else if is_embed_external(&item) {
        None
    } else {
        item.external_url.clone()
    };
    let base = items_out(&state, db, std::slice::from_ref(&item)).await?;
    Ok(Json(json!({
        "item": base.into_iter().next().unwrap_or(Value::Null),
        "stream_url": stream_url,
        "playlist": playlist_context(&state, db, &item, user.as_ref(), playlist_id).await?,
        "related": related(&state, db, &item, user.as_ref()).await?,
        "subtitles": subtitle_tracks(&state, db, &item).await?,
    })))
}

/// `GET /api/v1/videos/{item_id}/subtitles/{lang}.vtt` — raw VTT,
/// `Content-Type: text/vtt`, no JSON envelope. Any non-ready /
/// invisible / embed case is 404 `subtitle_not_found`. (The route is
/// registered as `{lang_vtt}` because matchit allows only one capture
/// per segment; the `.vtt` suffix is stripped here.)
pub async fn subtitle_vtt_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((item_id, lang_vtt)): Path<(String, String)>,
) -> ApiResult<Response<Body>> {
    let lang = match lang_vtt.strip_suffix(".vtt") {
        Some(l) => l.to_string(),
        None => {
            return Err(ApiError::not_found(
                "subtitle_not_found",
                "زیرنویس یافت نشد",
            ));
        }
    };
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let visible = visible_items_by_ids(db, user.as_ref(), &[item_id]).await?;
    let item = visible
        .get(&item_id)
        .ok_or_else(|| ApiError::not_found("subtitle_not_found", "زیرنویس یافت نشد"))?;
    if is_embed_external(item) {
        return Err(ApiError::not_found(
            "subtitle_not_found",
            "زیرنویس یافت نشد",
        ));
    }
    let row = db
        .fetch_optional(
            "SELECT vtt FROM item_subtitles WHERE item_id = ? AND lang = ? AND status = 'ready'",
            &[(item_id).into_arg(), (lang).into_arg()],
        )
        .await?;
    let vtt: Option<String> = match row {
        Some(r) => r.try_get("vtt")?,
        None => None,
    };
    let Some(vtt) = vtt.filter(|v| !v.is_empty()) else {
        return Err(ApiError::not_found(
            "subtitle_not_found",
            "زیرنویس یافت نشد",
        ));
    };
    let res = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/vtt")
        .body(Body::from(vtt))
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}

fn progress_out(
    position: Option<f64>,
    duration: Option<f64>,
    updated_at: Option<DateTime<Utc>>,
    item_id: Uuid,
) -> Value {
    json!({
        "item_id": item_id.to_string(),
        "position_seconds": position,
        "duration_seconds": duration,
        "updated_at": dt_json(updated_at),
    })
}

/// `GET /api/v1/videos/{item_id}/progress` — no row (or an external
/// video) → all three fields null, still 200.
pub async fn progress_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/videos").await?;
    let db = state.db_or_500()?;
    let item = get_video_item(db, item_id).await?;
    if item.external_url.is_some() {
        return Ok(Json(progress_out(None, None, None, item_id)));
    }
    let row = db
        .fetch_optional(
            "SELECT position_seconds, duration_seconds, updated_at \
             FROM user_video_progress WHERE user_id = ? AND item_id = ?",
            &[(ctx.user.id).into_arg(), (item_id).into_arg()],
        )
        .await?;
    let out = match row {
        Some(r) => {
            let position: f64 = r.try_get("position_seconds")?;
            let duration: Option<f64> = r.try_get("duration_seconds")?;
            let updated_at: Option<DateTime<Utc>> = r.try_get("updated_at")?;
            progress_out(Some(position), duration, updated_at, item_id)
        }
        None => progress_out(None, None, None, item_id),
    };
    Ok(Json(out))
}

/// `PUT /api/v1/videos/{item_id}/progress` — upsert; external videos
/// never store progress (422 `external_video`).
pub async fn progress_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/videos").await?;
    let parsed = parse_body(&body)?;
    let position = req_f64(&parsed, "position_seconds")?;
    if position < 0.0 {
        return Err(validation_err(
            "position_seconds",
            "must be greater than or equal to 0",
        ));
    }
    let duration = opt_f64(&parsed, "duration_seconds")?;
    if let Some(d) = duration {
        if d < 0.0 {
            return Err(validation_err(
                "duration_seconds",
                "must be greater than or equal to 0",
            ));
        }
    }
    let db = state.db_or_500()?;
    let item = get_video_item(db, item_id).await?;
    if item.external_url.is_some() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "external_video",
            "ویدیوی خارجی (یوتیوب) موقعیت پخش ذخیره نمی‌کند",
        ));
    }
    let now = utc_now();
    let existing = db
        .fetch_optional(
            "SELECT id FROM user_video_progress WHERE user_id = ? AND item_id = ?",
            &[(ctx.user.id).into_arg(), (item_id).into_arg()],
        )
        .await?;
    match existing {
        None => {
            db.execute(
                "INSERT INTO user_video_progress (id, user_id, item_id, position_seconds, \
                 duration_seconds, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
                &[
                    (new_id()).into_arg(),
                    (ctx.user.id).into_arg(),
                    (item_id).into_arg(),
                    (position).into_arg(),
                    (duration).into_arg(),
                    (now).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
        }
        Some(row) => {
            let row_id: Uuid = row.try_get("id")?;
            if let Some(d) = duration {
                db.execute(
                    "UPDATE user_video_progress SET position_seconds = ?, duration_seconds = ?, \
                     updated_at = ? WHERE id = ?",
                    &[
                        (position).into_arg(),
                        (d).into_arg(),
                        (now).into_arg(),
                        (row_id).into_arg(),
                    ],
                )
                .await?;
            } else {
                db.execute(
                    "UPDATE user_video_progress SET position_seconds = ?, updated_at = ? WHERE id = ?",
                    &[
                        (position).into_arg(),
                        (now).into_arg(),
                        (row_id).into_arg(),
                    ],
                )
                .await?;
            }
        }
    }
    // The response is the stored row: an omitted `duration_seconds`
    // leaves the previous value in place (Python keeps it too).
    let row = db
        .fetch_optional(
            "SELECT position_seconds, duration_seconds, updated_at \
             FROM user_video_progress WHERE user_id = ? AND item_id = ?",
            &[(ctx.user.id).into_arg(), (item_id).into_arg()],
        )
        .await?
        .ok_or_else(ApiError::internal)?;
    let stored_position: f64 = row.try_get("position_seconds")?;
    let stored_duration: Option<f64> = row.try_get("duration_seconds")?;
    let stored_updated: Option<DateTime<Utc>> = row.try_get("updated_at")?;
    Ok(Json(progress_out(
        Some(stored_position),
        stored_duration,
        stored_updated,
        item_id,
    )))
}

// --- admin -------------------------------------------------------------------

/// `GET /api/v1/admin/videos/playlists` — every playlist, drafts included.
pub async fn admin_playlist_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {PLAYLIST_COLUMNS} FROM video_playlists \
                 ORDER BY sort_order ASC, created_at ASC"
            ),
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let playlist = playlist_from_row(&row)?;
        out.push(admin_playlist_out(&state, db, &playlist).await?);
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/videos/playlists` — 201.
pub async fn admin_playlist_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let parsed = parse_body(&body)?;
    let title = req_localized(&parsed, "title")?;
    let description = match parsed.get("description") {
        Some(Value::Null) | None => None,
        Some(v) => Some(localized_value(v, "description")?),
    };
    let cover_media_key = opt_str_raw_max(&parsed, "cover_media_key", 1024)?;
    let scholar_id = opt_uuid(&parsed, "scholar_id")?;
    let is_published = opt_bool_loose(&parsed, "is_published")?.unwrap_or(false);
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?.unwrap_or(0);
    let db = state.db_or_500()?;
    let now = utc_now();
    let playlist_id = new_id();
    db.execute(
        "INSERT INTO video_playlists (id, title, description, cover_media_key, scholar_id, \
         is_published, sort_order, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            (playlist_id).into_arg(),
            (title).into_arg(),
            (description).into_arg(),
            (cover_media_key).into_arg(),
            (scholar_id).into_arg(),
            (is_published).into_arg(),
            (sort_order).into_arg(),
            (now).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.playlist.create",
        "video_playlist",
        Some(&playlist_id.to_string()),
        None,
        Some(json!({ "is_published": is_published })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(admin_playlist_out(&state, db, &playlist).await?),
    ))
}

/// `PATCH /api/v1/admin/videos/playlists/{playlist_id}` — only the
/// fields present in the body are applied (`exclude_unset`).
pub async fn admin_playlist_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(playlist_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let playlist_id = parse_path_uuid(&playlist_id, "playlist_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let parsed = parse_body(&body)?;
    // Validate every present field before the 404 check (FastAPI
    // validates the body before the handler runs).
    let mut sets: Vec<(String, Arg)> = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    if let Some(v) = parsed.get("title") {
        let checked = if v.is_null() {
            Arg::NullJson
        } else {
            localized_value(v, "title")?.into_arg()
        };
        sets.push(("title".to_string(), checked));
        fields.push("title".to_string());
    }
    if let Some(v) = parsed.get("description") {
        let checked = if v.is_null() {
            Arg::NullJson
        } else {
            localized_value(v, "description")?.into_arg()
        };
        sets.push(("description".to_string(), checked));
        fields.push("description".to_string());
    }
    if let Some(v) = parsed.get("cover_media_key") {
        let checked = if v.is_null() {
            Arg::NullText
        } else {
            opt_str_raw_max(&parsed, "cover_media_key", 1024)?
                .unwrap_or_default()
                .into_arg()
        };
        sets.push(("cover_media_key".to_string(), checked));
        fields.push("cover_media_key".to_string());
    }
    if let Some(v) = parsed.get("scholar_id") {
        let checked = if v.is_null() {
            Arg::NullUuid
        } else {
            crate::validate::uuid_value(v, "scholar_id")?.into_arg()
        };
        sets.push(("scholar_id".to_string(), checked));
        fields.push("scholar_id".to_string());
    }
    if let Some(v) = parsed.get("is_published") {
        let checked = if v.is_null() {
            Arg::NullBool
        } else {
            crate::validate::as_bool(v)
                .ok_or_else(|| validation_err("is_published", "must be a boolean"))?
                .into_arg()
        };
        sets.push(("is_published".to_string(), checked));
        fields.push("is_published".to_string());
    }
    if let Some(v) = parsed.get("sort_order") {
        let checked = if v.is_null() {
            Arg::NullInt
        } else {
            crate::validate::as_int(v)
                .ok_or_else(|| validation_err("sort_order", "must be an integer"))?
                .into_arg()
        };
        sets.push(("sort_order".to_string(), checked));
        fields.push("sort_order".to_string());
    }
    let db = state.db_or_500()?;
    get_playlist_or_404(db, playlist_id).await?;
    if !sets.is_empty() {
        let now = utc_now();
        let mut sql = String::from("UPDATE video_playlists SET ");
        sql.push_str(
            &sets
                .iter()
                .map(|(c, _)| format!("{c} = ?"))
                .collect::<Vec<_>>()
                .join(", "),
        );
        sql.push_str(", updated_at = ? WHERE id = ?");
        let mut args: Vec<Arg> = sets.into_iter().map(|(_, a)| a).collect();
        args.push(now.into_arg());
        args.push(playlist_id.into_arg());
        db.execute(&sql, &args).await?;
    } else {
        db.execute(
            "UPDATE video_playlists SET updated_at = ? WHERE id = ?",
            &[(utc_now()).into_arg(), (playlist_id).into_arg()],
        )
        .await?;
    }
    fields.sort();
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.playlist.update",
        "video_playlist",
        Some(&playlist_id.to_string()),
        None,
        Some(json!({ "fields": fields })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    Ok(Json(admin_playlist_out(&state, db, &playlist).await?))
}

/// `DELETE /api/v1/admin/videos/playlists/{playlist_id}` — hard
/// delete; returns the pre-delete snapshot.
pub async fn admin_playlist_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(playlist_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let playlist_id = parse_path_uuid(&playlist_id, "playlist_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let db = state.db_or_500()?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    let snapshot = admin_playlist_out(&state, db, &playlist).await?;
    let memberships = playlist_items_ordered(db, playlist.id).await?;
    let item_count = memberships.len();
    db.execute(
        "DELETE FROM video_playlist_items WHERE playlist_id = ?",
        &[(playlist_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM video_playlists WHERE id = ?",
        &[(playlist_id).into_arg()],
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.playlist.delete",
        "video_playlist",
        Some(&playlist_id.to_string()),
        Some(json!({ "item_count": item_count })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(snapshot))
}

/// `POST /api/v1/admin/videos/playlists/{playlist_id}/items` — 201.
pub async fn admin_playlist_item_add(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(playlist_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let playlist_id = parse_path_uuid(&playlist_id, "playlist_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let parsed = parse_body(&body)?;
    let item_id = match parsed.get("item_id") {
        Some(v) => crate::validate::uuid_value(v, "item_id")?,
        None => return Err(validation_err("item_id", "field is required")),
    };
    let position = opt_int_loose(&parsed, "position", 0, i64::MAX)?;
    let db = state.db_or_500()?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    let item = get_library_item_or_404(db, item_id).await?;
    if item.kind != "video" && item.kind != "video_360" {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_a_video",
            "فقط آیتم‌های ویدیویی می‌توانند عضو لیست پخش شوند",
            json!({ "kind": item.kind }),
        ));
    }
    let memberships = playlist_items_ordered(db, playlist.id).await?;
    let mut ids: Vec<Uuid> = memberships.iter().map(|(id, _)| *id).collect();
    if ids.contains(&item_id) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "already_in_playlist",
            "این ویدیو از قبل در این لیست هست",
        ));
    }
    match position {
        None => ids.push(item_id),
        Some(p) => {
            let at = (p as usize).min(ids.len());
            ids.insert(at, item_id);
        }
    }
    rewrite_positions(db, playlist.id, &ids).await?;
    db.execute(
        "UPDATE video_playlists SET updated_at = ? WHERE id = ?",
        &[(utc_now()).into_arg(), (playlist_id).into_arg()],
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.playlist.item_add",
        "video_playlist",
        Some(&playlist_id.to_string()),
        None,
        Some(json!({ "item_id": item_id.to_string() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(admin_playlist_out(&state, db, &playlist).await?),
    ))
}

/// `DELETE /api/v1/admin/videos/playlists/{playlist_id}/items/{item_id}`.
pub async fn admin_playlist_item_remove(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path((playlist_id, item_id)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let playlist_id = parse_path_uuid(&playlist_id, "playlist_id")?;
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let db = state.db_or_500()?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    let memberships = playlist_items_ordered(db, playlist.id).await?;
    if !memberships.iter().any(|(id, _)| *id == item_id) {
        return Err(ApiError::not_found(
            "playlist_item_not_found",
            "این ویدیو در لیست نیست",
        ));
    }
    db.execute(
        "DELETE FROM video_playlist_items WHERE playlist_id = ? AND item_id = ?",
        &[(playlist_id).into_arg(), (item_id).into_arg()],
    )
    .await?;
    let remaining: Vec<Uuid> = memberships
        .iter()
        .map(|(id, _)| *id)
        .filter(|id| *id != item_id)
        .collect();
    rewrite_positions(db, playlist.id, &remaining).await?;
    db.execute(
        "UPDATE video_playlists SET updated_at = ? WHERE id = ?",
        &[(utc_now()).into_arg(), (playlist_id).into_arg()],
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.playlist.item_remove",
        "video_playlist",
        Some(&playlist_id.to_string()),
        Some(json!({ "item_id": item_id.to_string() })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    Ok(Json(admin_playlist_out(&state, db, &playlist).await?))
}

/// `PUT /api/v1/admin/videos/playlists/{playlist_id}/items/order`.
pub async fn admin_playlist_reorder(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(playlist_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let playlist_id = parse_path_uuid(&playlist_id, "playlist_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let parsed = parse_body(&body)?;
    let item_ids = match parsed.get("item_ids") {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for v in items {
                out.push(crate::validate::uuid_value(v, "item_ids")?);
            }
            out
        }
        Some(_) => return Err(validation_err("item_ids", "must be a list")),
        None => return Err(validation_err("item_ids", "field is required")),
    };
    let db = state.db_or_500()?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    let memberships = playlist_items_ordered(db, playlist.id).await?;
    let current: Vec<Uuid> = memberships.iter().map(|(id, _)| *id).collect();
    let current_set: HashSet<Uuid> = current.iter().cloned().collect();
    let new_set: HashSet<Uuid> = item_ids.iter().cloned().collect();
    if item_ids.len() != current.len() || new_set != current_set {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "order_mismatch",
            "ترتیب ارسالی باید دقیقاً همان مجموعهٔ فعلی آیتم‌های لیست باشد",
        ));
    }
    rewrite_positions(db, playlist.id, &item_ids).await?;
    db.execute(
        "UPDATE video_playlists SET updated_at = ? WHERE id = ?",
        &[(utc_now()).into_arg(), (playlist_id).into_arg()],
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.playlist.reorder",
        "video_playlist",
        Some(&playlist_id.to_string()),
        None,
        Some(json!({ "count": item_ids.len() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let playlist = get_playlist_or_404(db, playlist_id).await?;
    Ok(Json(admin_playlist_out(&state, db, &playlist).await?))
}

/// `GET /api/v1/admin/videos/subtitles` — subtitle status of the
/// newest upload / direct-file videos (fixed cap 30, scanned from
/// the newest 300).
pub async fn admin_subtitle_statuses(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items \
                 WHERE {VIDEO_KINDS_SQL} AND (media_key IS NOT NULL OR external_url IS NOT NULL) \
                 ORDER BY created_at DESC, id DESC LIMIT 300"
            ),
            &[],
        )
        .await?;
    let mut items = Vec::new();
    for row in rows {
        let item = library_item_from_row(&row)?;
        if is_embed_external(&item) {
            continue;
        }
        items.push(item);
        if items.len() >= 30 {
            break;
        }
    }
    let mut by_item: HashMap<Uuid, HashMap<String, String>> = HashMap::new();
    let mut errors_by_item: HashMap<Uuid, HashMap<String, String>> = HashMap::new();
    if !items.is_empty() {
        let ids: Vec<Uuid> = items.iter().map(|i| i.id).collect();
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT item_id, lang, status, error FROM item_subtitles WHERE item_id IN ({placeholders})"
        );
        let args: Vec<Arg> = ids.iter().map(|id| id.into_arg()).collect();
        for row in db.fetch_all(&sql, &args).await? {
            let item_id: Uuid = row.try_get("item_id")?;
            let lang: String = row.try_get("lang")?;
            let status: String = row.try_get("status")?;
            let error: Option<String> = row.try_get("error")?;
            by_item
                .entry(item_id)
                .or_default()
                .insert(lang.clone(), status.clone());
            if status == "failed" {
                if let Some(e) = error.filter(|e| !e.is_empty()) {
                    errors_by_item.entry(item_id).or_default().insert(lang, e);
                }
            }
        }
    }
    let mut out = Vec::new();
    for item in items {
        let have = by_item.get(&item.id);
        let mut subtitles = serde_json::Map::new();
        for lang in SUBTITLE_LANGS {
            let status = have
                .and_then(|m| m.get(*lang))
                .cloned()
                .unwrap_or_else(|| "missing".to_string());
            subtitles.insert(lang.to_string(), Value::String(status));
        }
        out.push(json!({
            "id": item.id.to_string(),
            "kind": item.kind,
            "title": item.title,
            "workflow_status": item.workflow_status,
            "visibility": item.visibility,
            "created_at": item.created_at.to_rfc3339(),
            "subtitles": Value::Object(subtitles),
            "errors": errors_by_item.get(&item.id).cloned().unwrap_or_default(),
        }));
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/videos/{item_id}/subtitles/generate` — 202.
/// Rows go `pending`; `enqueued: false` always (ADR-045 — no ARQ
/// enqueue from Rust, the Wave 3 ingest precedent).
pub async fn admin_subtitle_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let db = state.db_or_500()?;
    let item = get_library_item_or_404(db, item_id).await?;
    if item.kind != "video" && item.kind != "video_360" {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_a_video",
            "زیرنویس خودکار فقط برای آیتم‌های ویدیویی است",
            json!({ "kind": item.kind }),
        ));
    }
    if is_embed_external(&item) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "external_video",
            "ویدیوی خارجی (یوتیوب/آپارات) زیرنویس خودکار ندارد؛ کپشن داخل خود منبع است",
        ));
    }
    if item.external_url.is_none() && item.media_key.is_none() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_media_file",
            "این آیتم فایل رسانه‌ای ندارد",
        ));
    }
    let langs = target_langs(&state).await?;
    let statuses = reset_rows(db, item.id, &langs).await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.subtitles.generate",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({ "langs": langs })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "item_id": item.id.to_string(),
            "statuses": statuses,
            "enqueued": false,
        })),
    ))
}

/// `GET /api/v1/admin/videos/{item_id}/subtitles/{lang}` — the lang
/// check runs before the item check (Python order).
pub async fn admin_subtitle_cues_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((item_id, lang)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    if !is_subtitle_lang(&lang) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "subtitle_lang_unknown",
            "زبان زیرنویس ناشناخته است",
            json!({ "lang": lang }),
        ));
    }
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let db = state.db_or_500()?;
    let item = get_library_item_or_404(db, item_id).await?;
    let row = db
        .fetch_optional(
            "SELECT status, vtt FROM item_subtitles WHERE item_id = ? AND lang = ?",
            &[(item_id).into_arg(), (&lang).into_arg()],
        )
        .await?;
    let (status, cues) = match row {
        Some(r) => {
            let status: String = r.try_get("status")?;
            let vtt: Option<String> = r.try_get("vtt")?;
            let cues: Vec<Value> = parse_vtt(vtt.as_deref()).iter().map(cue_to_json).collect();
            (status, cues)
        }
        None => ("missing".to_string(), Vec::new()),
    };
    Ok(Json(json!({
        "item_id": item.id.to_string(),
        "lang": lang,
        "status": status,
        "preview_url": if is_direct_file_external(&item) { item.external_url.clone() } else { None },
        "cues": cues,
    })))
}

/// `PUT /api/v1/admin/videos/{item_id}/subtitles/{lang}` — manual
/// subtitle edit; the row becomes `ready`.
pub async fn admin_subtitle_cues_put(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path((item_id, lang)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let parsed = parse_body(&body)?;
    // Schema-level validation first (FastAPI validates the body
    // before the handler runs): `cues` required, ≤ 5000, each cue a
    // `{start: number, end: number, text: string}`.
    let raw_cues = match parsed.get("cues") {
        Some(Value::Array(items)) => items.clone(),
        Some(_) => return Err(validation_err("cues", "must be a list")),
        None => return Err(validation_err("cues", "field is required")),
    };
    if raw_cues.len() > 5000 {
        return Err(validation_err("cues", "length out of range"));
    }
    let mut cues: Vec<Cue> = Vec::with_capacity(raw_cues.len());
    for raw in raw_cues.iter() {
        let Some(obj) = raw.as_object() else {
            return Err(validation_err("cues", "must be a list of objects"));
        };
        let start = match obj.get("start") {
            Some(Value::Number(n)) => n
                .as_f64()
                .ok_or_else(|| validation_err("cues", "must be a number"))?,
            Some(Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| validation_err("cues", "must be a number"))?,
            _ => return Err(validation_err("cues", "start is required")),
        };
        let end = match obj.get("end") {
            Some(Value::Number(n)) => n
                .as_f64()
                .ok_or_else(|| validation_err("cues", "must be a number"))?,
            Some(Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| validation_err("cues", "must be a number"))?,
            _ => {
                return Err(validation_err("cues", "must be a number"));
            }
        };
        let text = match obj.get("text") {
            Some(Value::String(s)) => s.clone(),
            _ => {
                return Err(validation_err("cues", "text is required"));
            }
        };
        cues.push(Cue { start, end, text });
    }
    if !is_subtitle_lang(&lang) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "subtitle_lang_unknown",
            "زبان زیرنویس ناشناخته است",
            json!({ "lang": lang }),
        ));
    }
    let db = state.db_or_500()?;
    let item = get_library_item_or_404(db, item_id).await?;
    if item.kind != "video" && item.kind != "video_360" {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "not_a_video",
            "زیرنویس فقط برای آیتم‌های ویدیویی است",
            json!({ "kind": item.kind }),
        ));
    }
    if is_embed_external(&item) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "external_video",
            "ویدیوی خارجی (یوتیوب/آپارات) زیرنویس دستی ندارد؛ کپشن داخل خود منبع است",
        ));
    }
    let row = db
        .fetch_optional(
            "SELECT id, status FROM item_subtitles WHERE item_id = ? AND lang = ?",
            &[(item_id).into_arg(), (&lang).into_arg()],
        )
        .await?;
    if let Some(r) = &row {
        let status: String = r.try_get("status")?;
        if status == "pending" || status == "processing" {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "subtitle_generation_in_progress",
                "تولید خودکار این زیرنویس در جریان است؛ پس از پایانش ویرایش کنید",
            ));
        }
    }
    if cues.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "subtitle_cues_empty",
            "دست‌کم یک سطر زیرنویس لازم است",
        ));
    }
    let mut clean: Vec<Cue> = Vec::with_capacity(cues.len());
    for (i, cue) in cues.into_iter().enumerate() {
        let text = cue.text.trim().to_string();
        if text.is_empty() {
            return Err(ApiError::with_details(
                StatusCode::UNPROCESSABLE_ENTITY,
                "subtitle_cue_empty_text",
                "متن سطر زیرنویس خالی است",
                json!({ "index": i }),
            ));
        }
        if cue.start < 0.0 || cue.end <= cue.start {
            return Err(ApiError::with_details(
                StatusCode::UNPROCESSABLE_ENTITY,
                "subtitle_cue_range",
                "بازهٔ زمانی سطر نامعتبر است (پایان باید بعد از شروع باشد)",
                json!({ "index": i }),
            ));
        }
        clean.push(Cue {
            start: cue.start,
            end: cue.end,
            text,
        });
    }
    clean.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let vtt = build_vtt(&clean);
    let now = utc_now();
    match row {
        Some(r) => {
            let row_id: Uuid = r.try_get("id")?;
            db.execute(
                "UPDATE item_subtitles SET vtt = ?, status = 'ready', error = NULL, updated_at = ? WHERE id = ?",
                &[
                    (vtt.clone()).into_arg(),
                    (now).into_arg(),
                    (row_id).into_arg(),
                ],
            )
            .await?;
        }
        None => {
            db.execute(
                "INSERT INTO item_subtitles (id, item_id, lang, status, vtt, error, created_at, updated_at) \
                 VALUES (?, ?, ?, 'ready', ?, NULL, ?, ?)",
                &[
                    (new_id()).into_arg(),
                    (item_id).into_arg(),
                    (lang.clone()).into_arg(),
                    (vtt.clone()).into_arg(),
                    (now).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
        }
    }
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "videos.subtitles.edit",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({ "lang": lang, "cues": clean.len() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let saved_cues: Vec<Value> = parse_vtt(Some(&vtt)).iter().map(cue_to_json).collect();
    Ok(Json(json!({
        "item_id": item.id.to_string(),
        "lang": lang,
        "status": "ready",
        "preview_url": if is_direct_file_external(&item) { item.external_url.clone() } else { None },
        "cues": saved_cues,
    })))
}

/// `GET /api/v1/admin/videos/candidates` — video items for the CRM
/// picker, all workflow statuses; `q` searches title/description
/// in Rust (Python does the same — JSON LIKE is unreliable on
/// unicode-escaped sqlite text).
pub async fn admin_candidates(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/videos").await?;
    require_permission(&state, &ctx, "library.manage").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let q = params.get("q").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let fetch_limit = if q.is_some() { 500 } else { limit };
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {LIBRARY_ITEM_COLUMNS} FROM library_items \
                 WHERE {VIDEO_KINDS_SQL} ORDER BY created_at DESC, id DESC LIMIT ?"
            ),
            &[(fetch_limit).into_arg()],
        )
        .await?;
    let mut items = Vec::new();
    for row in rows {
        items.push(library_item_from_row(&row)?);
    }
    if let Some(needle) = q {
        let needle = needle.to_lowercase();
        items.retain(|item| {
            // `json.dumps({"t": title, "d": description},
            // ensure_ascii=False)` — Python separators `", "` / `": "`.
            let blob = format!(
                "{{\"t\": {}, \"d\": {}}}",
                serde_json::to_string(&item.title).unwrap_or_default(),
                serde_json::to_string(&item.description.clone().unwrap_or(Value::Null))
                    .unwrap_or_default()
            );
            blob.to_lowercase().contains(&needle)
        });
        items.truncate(limit as usize);
    }
    let out: Vec<Value> = items
        .iter()
        .map(|item| {
            json!({
                "id": item.id.to_string(),
                "kind": item.kind,
                "title": item.title,
                "workflow_status": item.workflow_status,
                "visibility": item.visibility,
                "duration_seconds": item.duration_seconds,
                "created_at": item.created_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}
