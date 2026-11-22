//! Social routes — mirrors `app/modules/social/router.py` + `service.py`
//! (Wave 7).
//!
//! Groups, feed/posts, comments, likes, follows, reports + moderation
//! queue, banned words, and the social notifications surface
//! (`GET /api/v1/social/notifications`, mark-read, unread-count).
//!
//! Notes on fidelity (see the SOC-* ADR drafts in the wave report):
//! - Cursor pagination reuses `validate::{encode_cursor, decode_cursor}`
//!   — the same base64url `JSON([iso, id])` format as Python's
//!   `_encode_cursor` / `_decode_cursor`.
//! - The banned-word filter runs before insert on posts and comments
//!   (422 `content_policy_violation`, never echoing the matched word)
//!   with Python's `_normalize_for_match` (diacritics stripped,
//!   Arabic Yeh/Kaf folded to Farsi, whitespace collapsed).
//! - `POST /notifications/{id}/read` persists the read (an `UPDATE`):
//!   Python's social `mark_notification_read` only flushes and never
//!   commits, so the read is silently lost there; the sibling users
//!   endpoint (`/users/me/notifications/{id}/read`, native since Wave 2)
//!   commits, and the social `unread-count` endpoint only makes sense
//!   if reads stick. SOC-02.
//! - `create_group` requires `social.moderate`, the report queue and
//!   report actions require `social.moderate`, banned words require
//!   `moderation.words` — exactly like the Python router's deps.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, Duration, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::emit_event,
    authn::{authenticate, require_permission, AuthCtx},
    db::{Arg, Db, IntoArg},
    errors::{ApiError, ApiResult},
    localized::localized_json,
    settings::get_setting,
    validate::{
        decode_cursor, encode_cursor, opt_str_raw_max, opt_uuid, parse_body, parse_path_uuid,
        parse_query, query_int, req_localized, req_str_raw, uuid_value,
    },
    AppState,
};

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

// --- helpers -----------------------------------------------------------------

fn not_found(code: &'static str, msg: &str) -> ApiError {
    ApiError::not_found(code, msg)
}

/// Python `service._normalize_for_match`: strip diacritics
/// (U+064B–U+0652, U+0670), fold Arabic Yeh/Kaf to Farsi, lowercase,
/// collapse whitespace.
fn normalize_for_match(text: &str) -> String {
    let lowered = text.trim().to_lowercase();
    let stripped: String = lowered
        .chars()
        .filter(|c| !('\u{064b}'..='\u{0652}').contains(c) && *c != '\u{0670}')
        .collect();
    let folded = stripped
        .replace('\u{064a}', "\u{06cc}")
        .replace('\u{0643}', "\u{06a9}");
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Python `service._slugify`: fa name, else en name, else `"group"`;
/// non-word runs become `-`, edges stripped, truncated to 100 chars.
fn slugify(name: &Value) -> String {
    let base = name
        .get("fa")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            name.get("en")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or("group");
    let mut slug = String::new();
    let mut last_dash = true;
    for ch in base.chars() {
        let keep = ch == '_' || ch.is_alphanumeric() || ('\u{0600}'..='\u{06ff}').contains(&ch);
        if keep {
            slug.push(ch);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    let slug: String = slug.chars().take(100).collect();
    if slug.is_empty() {
        "group".to_string()
    } else {
        slug
    }
}

/// Python `service._sla_hours`: setting `social.moderation.sla_hours`,
/// default 24, minimum 1, unparsable → 24.
async fn sla_hours(state: &AppState, db: &Db) -> Result<i64, ApiError> {
    match get_setting(db, &state.config, "social.moderation.sla_hours").await? {
        Some(raw) if !raw.trim().is_empty() => match raw.trim().parse::<i64>() {
            Ok(n) => Ok(n.max(1)),
            Err(_) => Ok(24),
        },
        _ => Ok(24),
    }
}

/// Python `service.check_content_policy` — 422, never echoing the word.
async fn check_content_policy(db: &Db, text: &str) -> Result<(), ApiError> {
    if text.is_empty() {
        return Ok(());
    }
    let normalized = normalize_for_match(text);
    let rows = db
        .fetch_all("SELECT word FROM moderation_banned_words", &[])
        .await?;
    for row in rows {
        let word: String = row.try_get("word")?;
        let needle = normalize_for_match(&word);
        if !needle.is_empty() && normalized.contains(&needle) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "content_policy_violation",
                "متن با سیاست محتوای انجمن سازگار نیست",
            ));
        }
    }
    Ok(())
}

async fn is_member(db: &Db, group_id: Uuid, user_id: Uuid) -> Result<bool, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT id FROM group_memberships WHERE group_id = ? AND user_id = ? LIMIT 1",
            &[group_id.into_arg(), user_id.into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

async fn get_group_row(db: &Db, group_id: Uuid) -> Result<crate::db::DynRow, ApiError> {
    db.fetch_optional(
        "SELECT id, name, description, slug, is_public, member_count, created_at \
         FROM social_groups WHERE id = ?",
        &[group_id.into_arg()],
    )
    .await?
    .ok_or_else(|| not_found("group_not_found", "گروه یافت نشد"))
}

async fn get_post_row(db: &Db, post_id: Uuid) -> Result<crate::db::DynRow, ApiError> {
    db.fetch_optional(
        "SELECT id, group_id, author_id, body, status, comment_count, likes_count, created_at \
         FROM social_posts WHERE id = ?",
        &[post_id.into_arg()],
    )
    .await?
    .ok_or_else(|| not_found("post_not_found", "پست یافت نشد"))
}

async fn get_report_row(db: &Db, report_id: Uuid) -> Result<crate::db::DynRow, ApiError> {
    db.fetch_optional(
        "SELECT id, target_type, target_id, reason, reported_by, status, reported_at, \
         triaged_at, actioned_at, sla_due_at, action, note, actioned_by \
         FROM social_reports WHERE id = ?",
        &[report_id.into_arg()],
    )
    .await?
    .ok_or_else(|| not_found("report_not_found", "گزارش یافت نشد"))
}

fn group_out(row: &crate::db::DynRow, is_member: bool) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let name: Value = row.try_get("name")?;
    let description: Option<Value> = row.try_get("description")?;
    let slug: String = row.try_get("slug")?;
    let is_public: bool = row.try_get("is_public")?;
    let member_count: i32 = row.try_get("member_count")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "name": name,
        "description": description,
        "slug": slug,
        "is_public": is_public,
        "member_count": member_count,
        "is_member": is_member,
        "created_at": created_at.to_rfc3339(),
    }))
}

fn post_out(row: &crate::db::DynRow, liked: bool) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let group_id: Option<Uuid> = row.try_get("group_id")?;
    let author_id: Uuid = row.try_get("author_id")?;
    let body: String = row.try_get("body")?;
    let status: String = row.try_get("status")?;
    let comment_count: i32 = row.try_get("comment_count")?;
    let likes_count: i32 = row.try_get("likes_count")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "group_id": group_id.map(|g| g.to_string()),
        "author_id": author_id.to_string(),
        "body": body,
        "status": status,
        "comment_count": comment_count,
        "likes_count": likes_count,
        "liked": liked,
        "created_at": created_at.to_rfc3339(),
    }))
}

fn comment_out(row: &crate::db::DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let post_id: Uuid = row.try_get("post_id")?;
    let author_id: Uuid = row.try_get("author_id")?;
    let body: String = row.try_get("body")?;
    let status: String = row.try_get("status")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "post_id": post_id.to_string(),
        "author_id": author_id.to_string(),
        "body": body,
        "status": status,
        "created_at": created_at.to_rfc3339(),
    }))
}

fn report_out(row: &crate::db::DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let target_type: String = row.try_get("target_type")?;
    let target_id: Uuid = row.try_get("target_id")?;
    let reason: String = row.try_get("reason")?;
    let reported_by: Uuid = row.try_get("reported_by")?;
    let status: String = row.try_get("status")?;
    let reported_at: DateTime<Utc> = row.try_get("reported_at")?;
    let triaged_at: Option<DateTime<Utc>> = row.try_get("triaged_at")?;
    let actioned_at: Option<DateTime<Utc>> = row.try_get("actioned_at")?;
    let sla_due_at: DateTime<Utc> = row.try_get("sla_due_at")?;
    let action: Option<String> = row.try_get("action")?;
    let note: Option<String> = row.try_get("note")?;
    Ok(json!({
        "id": id.to_string(),
        "target_type": target_type,
        "target_id": target_id.to_string(),
        "reason": reason,
        "reported_by": reported_by.to_string(),
        "status": status,
        "reported_at": reported_at.to_rfc3339(),
        "triaged_at": triaged_at.map(|t| t.to_rfc3339()),
        "actioned_at": actioned_at.map(|t| t.to_rfc3339()),
        "sla_due_at": sla_due_at.to_rfc3339(),
        "action": action,
        "note": note,
    }))
}

fn notification_out(row: &crate::db::DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let notif_type: String = row.try_get("notif_type")?;
    let title: Value = row.try_get("title")?;
    let body: Option<String> = row.try_get("body")?;
    let data: Option<Value> = row.try_get("data")?;
    let read_at: Option<DateTime<Utc>> = row.try_get("read_at")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "notif_type": notif_type,
        "title": title,
        "body": body,
        "data": data,
        "read_at": read_at.map(|t| t.to_rfc3339()),
        "created_at": created_at.to_rfc3339(),
    }))
}

/// `SELECT post_id FROM post_likes WHERE user_id = ? AND post_id IN (...)`
/// — one query, like Python's `liked_post_ids`.
async fn liked_post_ids(db: &Db, user_id: Uuid, post_ids: &[Uuid]) -> Result<Vec<Uuid>, ApiError> {
    if post_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = post_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql =
        format!("SELECT post_id FROM post_likes WHERE user_id = ? AND post_id IN ({placeholders})");
    let mut args: Vec<Arg> = vec![user_id.into_arg()];
    args.extend(post_ids.iter().map(|id| id.into_arg()));
    let mut out = Vec::new();
    for row in db.fetch_all(&sql, &args).await? {
        let id: Uuid = row.try_get("post_id")?;
        out.push(id);
    }
    Ok(out)
}

#[allow(clippy::type_complexity)]
fn page_args(
    params: &std::collections::HashMap<String, String>,
) -> Result<(Option<(DateTime<Utc>, Uuid)>, i64), ApiError> {
    let cursor = params.get("after").filter(|s| !s.is_empty());
    let decoded = match cursor {
        Some(c) => Some(decode_cursor(c)?),
        None => None,
    };
    let limit = query_int(params, "limit", DEFAULT_LIMIT, 1, MAX_LIMIT)?;
    Ok((decoded, limit))
}

/// Append a `(created_at, id)` cursor condition for a DESC page to `conds`.
fn desc_cursor_cond(
    conds: &mut Vec<String>,
    args: &mut Vec<Arg>,
    cursor: Option<(DateTime<Utc>, Uuid)>,
    col: &str,
) {
    if let Some((ts, lid)) = cursor {
        conds.push(format!("({col} < ? OR ({col} = ? AND id < ?))"));
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
}

/// Paginate `rows` fetched with `limit + 1`: returns `(page, next_cursor)`.
fn paginate<T>(
    mut items: Vec<T>,
    keys: &[(DateTime<Utc>, Uuid)],
    limit: i64,
) -> (Vec<T>, Option<String>) {
    let mut next_cursor = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    (items, next_cursor)
}

async fn auth(state: &AppState, headers: &HeaderMap) -> Result<AuthCtx, ApiError> {
    authenticate(state, headers, "/api/v1/social").await
}

// --- groups ------------------------------------------------------------------

/// `POST /api/v1/social/groups` — 201. Requires `social.moderate`.
pub async fn create_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = auth(&state, &headers).await?;
    require_permission(&state, &ctx, "social.moderate").await?;
    let parsed = parse_body(&body)?;
    let name = req_localized(&parsed, "name")?;
    let description = match parsed.get("description").filter(|v| !v.is_null()) {
        Some(v) => Some(crate::validate::localized_value(v, "description")?),
        None => None,
    };
    let slug = opt_str_raw_max(&parsed, "slug", 128)?.filter(|s| !s.is_empty());
    let is_public = crate::validate::opt_bool_loose(&parsed, "is_public")?.unwrap_or(true);
    let db = state.db_or_500()?;
    let final_slug = slug.unwrap_or_else(|| slugify(&name));
    let dup = db
        .fetch_optional(
            "SELECT id FROM social_groups WHERE slug = ?",
            &[final_slug.clone().into_arg()],
        )
        .await?;
    if dup.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "duplicate_group_slug",
            "نامک گروه تکراری است",
        ));
    }
    let now = Utc::now();
    let group_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO social_groups (id, name, description, slug, is_public, member_count, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            group_id.into_arg(),
            name.clone().into_arg(),
            description.clone().into_arg(),
            final_slug.into_arg(),
            is_public.into_arg(),
            0i32.into_arg(),
            ctx.user.id.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    db.execute(
        "INSERT INTO group_memberships (id, group_id, user_id, role, joined_at) VALUES (?, ?, ?, ?, ?)",
        &[
            Uuid::new_v4().into_arg(),
            group_id.into_arg(),
            ctx.user.id.into_arg(),
            "moderator".into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    db.execute(
        "UPDATE social_groups SET member_count = ? WHERE id = ?",
        &[1i32.into_arg(), group_id.into_arg()],
    )
    .await?;
    let row = get_group_row(db, group_id).await?;
    Ok((StatusCode::CREATED, Json(group_out(&row, true)?)))
}

/// `GET /api/v1/social/groups` — cursor page, `after` + `limit` 1–100.
pub async fn list_groups(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let params = parse_query(&uri);
    let (cursor, limit) = page_args(&params)?;
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, name, description, slug, is_public, member_count, created_at FROM social_groups",
    );
    let mut args: Vec<Arg> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    desc_cursor_cond(&mut conds, &mut args, cursor, "created_at");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    let mut keys = Vec::new();
    for row in &rows {
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let id: Uuid = row.try_get("id")?;
        keys.push((created_at, id));
        let member = is_member(db, id, ctx.user.id).await?;
        items.push(group_out(row, member)?);
    }
    let (items, next_cursor) = paginate(items, &keys, limit);
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/social/groups/{group_id}`.
pub async fn get_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(group_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let group_id = parse_path_uuid(&group_id, "group_id")?;
    let db = state.db_or_500()?;
    let row = get_group_row(db, group_id).await?;
    let member = is_member(db, group_id, ctx.user.id).await?;
    Ok(Json(group_out(&row, member)?))
}

/// `POST /api/v1/social/groups/{group_id}/join` — idempotent.
pub async fn join_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(group_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let group_id = parse_path_uuid(&group_id, "group_id")?;
    let db = state.db_or_500()?;
    let row = get_group_row(db, group_id).await?;
    if !is_member(db, group_id, ctx.user.id).await? {
        let now = Utc::now();
        db.execute(
            "INSERT INTO group_memberships (id, group_id, user_id, role, joined_at) VALUES (?, ?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                group_id.into_arg(),
                ctx.user.id.into_arg(),
                "member".into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
        let count: i32 = row.try_get("member_count")?;
        db.execute(
            "UPDATE social_groups SET member_count = ? WHERE id = ?",
            &[(count + 1).into_arg(), group_id.into_arg()],
        )
        .await?;
    }
    let row = get_group_row(db, group_id).await?;
    Ok(Json(group_out(&row, true)?))
}

/// `POST /api/v1/social/groups/{group_id}/leave` — idempotent.
pub async fn leave_group(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(group_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let group_id = parse_path_uuid(&group_id, "group_id")?;
    let db = state.db_or_500()?;
    let row = get_group_row(db, group_id).await?;
    let membership = db
        .fetch_optional(
            "SELECT id FROM group_memberships WHERE group_id = ? AND user_id = ?",
            &[group_id.into_arg(), ctx.user.id.into_arg()],
        )
        .await?;
    if let Some(m) = membership {
        let mid: Uuid = m.try_get("id")?;
        db.execute(
            "DELETE FROM group_memberships WHERE id = ?",
            &[mid.into_arg()],
        )
        .await?;
        let count: i32 = row.try_get("member_count")?;
        db.execute(
            "UPDATE social_groups SET member_count = ? WHERE id = ?",
            &[
                count.saturating_sub(1).max(0).into_arg(),
                group_id.into_arg(),
            ],
        )
        .await?;
    }
    let row = get_group_row(db, group_id).await?;
    Ok(Json(group_out(&row, false)?))
}

// --- posts, feed, comments -----------------------------------------------------

async fn feed_page(
    db: &Db,
    user_id: Uuid,
    group_id: Option<Uuid>,
    cursor: Option<(DateTime<Utc>, Uuid)>,
    limit: i64,
) -> Result<Value, ApiError> {
    let mut sql = String::from(
        "SELECT id, group_id, author_id, body, status, comment_count, likes_count, created_at \
         FROM social_posts WHERE status = 'published'",
    );
    let mut args: Vec<Arg> = Vec::new();
    if let Some(g) = group_id {
        sql.push_str(" AND group_id = ?");
        args.push(g.into_arg());
    }
    let mut conds: Vec<String> = Vec::new();
    desc_cursor_cond(&mut conds, &mut args, cursor, "created_at");
    if !conds.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let ids: Vec<Uuid> = rows
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<_, _>>()?;
    let liked = liked_post_ids(db, user_id, &ids).await?;
    let mut items = Vec::new();
    let mut keys = Vec::new();
    for row in &rows {
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let id: Uuid = row.try_get("id")?;
        keys.push((created_at, id));
        items.push(post_out(row, liked.contains(&id))?);
    }
    let (items, next_cursor) = paginate(items, &keys, limit);
    Ok(json!({ "items": items, "next_cursor": next_cursor }))
}

/// `GET /api/v1/social/feed` — optional `group_id` filter.
pub async fn feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let params = parse_query(&uri);
    let (cursor, limit) = page_args(&params)?;
    let group_id = match params.get("group_id").filter(|s| !s.is_empty()) {
        Some(g) => Some(g.parse::<Uuid>().map_err(|_| {
            ApiError::validation(vec![
                json!({ "field": "group_id", "msg": "must be a UUID" }),
            ])
        })?),
        None => None,
    };
    let db = state.db_or_500()?;
    Ok(Json(
        feed_page(db, ctx.user.id, group_id, cursor, limit).await?,
    ))
}

/// `GET /api/v1/social/groups/{group_id}/posts` — 404 when the group
/// does not exist, even though the feed itself is group-filtered.
pub async fn group_posts(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(group_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let group_id = parse_path_uuid(&group_id, "group_id")?;
    let params = parse_query(&uri);
    let (cursor, limit) = page_args(&params)?;
    let db = state.db_or_500()?;
    get_group_row(db, group_id).await?;
    Ok(Json(
        feed_page(db, ctx.user.id, Some(group_id), cursor, limit).await?,
    ))
}

/// `POST /api/v1/social/posts` — 201.
pub async fn create_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = auth(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let body_text = req_str_raw(&parsed, "body", 1, 5000)?;
    let group_id = opt_uuid(&parsed, "group_id")?;
    let db = state.db_or_500()?;
    check_content_policy(db, &body_text).await?;
    if let Some(g) = group_id {
        let group = get_group_row(db, g).await?;
        let is_public: bool = group.try_get("is_public")?;
        if !is_public && !is_member(db, g, ctx.user.id).await? {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "group_not_joined",
                "برای انتشار در گروه خصوصی باید عضو باشید",
            ));
        }
    }
    let now = Utc::now();
    let post_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO social_posts (id, group_id, author_id, body, status, comment_count, likes_count, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            post_id.into_arg(),
            group_id.into_arg(),
            ctx.user.id.into_arg(),
            body_text.into_arg(),
            "published".into_arg(),
            0i32.into_arg(),
            0i32.into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "social_post",
        &post_id.to_string(),
        "social.post_created",
        json!({
            "author_id": ctx.user.id.to_string(),
            "group_id": group_id.map(|g| g.to_string()),
        }),
    )
    .await?;
    let row = get_post_row(db, post_id).await?;
    Ok((StatusCode::CREATED, Json(post_out(&row, false)?)))
}

/// `GET /api/v1/social/posts/{post_id}`.
pub async fn get_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(post_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let post_id = parse_path_uuid(&post_id, "post_id")?;
    let db = state.db_or_500()?;
    let row = get_post_row(db, post_id).await?;
    let liked = liked_post_ids(db, ctx.user.id, &[post_id]).await?;
    Ok(Json(post_out(&row, liked.contains(&post_id))?))
}

/// `POST /api/v1/social/posts/{post_id}/like` — idempotent; liking a
/// non-published post is 404, exactly like Python.
pub async fn like_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(post_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let post_id = parse_path_uuid(&post_id, "post_id")?;
    let db = state.db_or_500()?;
    let row = get_post_row(db, post_id).await?;
    let status: String = row.try_get("status")?;
    if status != "published" {
        return Err(not_found("post_not_found", "پست یافت نشد"));
    }
    let count: i32 = row.try_get("likes_count")?;
    let existing = db
        .fetch_optional(
            "SELECT id FROM post_likes WHERE post_id = ? AND user_id = ?",
            &[post_id.into_arg(), ctx.user.id.into_arg()],
        )
        .await?;
    if existing.is_some() {
        return Ok(Json(json!({ "liked": true, "likes_count": count })));
    }
    db.execute(
        "INSERT INTO post_likes (id, post_id, user_id, created_at) VALUES (?, ?, ?, ?)",
        &[
            Uuid::new_v4().into_arg(),
            post_id.into_arg(),
            ctx.user.id.into_arg(),
            Utc::now().into_arg(),
        ],
    )
    .await?;
    let new_count = count + 1;
    db.execute(
        "UPDATE social_posts SET likes_count = ? WHERE id = ?",
        &[new_count.into_arg(), post_id.into_arg()],
    )
    .await?;
    emit_event(
        db,
        "social_post",
        &post_id.to_string(),
        "social.post_liked",
        json!({ "user_id": ctx.user.id.to_string() }),
    )
    .await?;
    Ok(Json(json!({ "liked": true, "likes_count": new_count })))
}

/// `DELETE /api/v1/social/posts/{post_id}/like` — idempotent.
pub async fn unlike_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(post_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let post_id = parse_path_uuid(&post_id, "post_id")?;
    let db = state.db_or_500()?;
    let row = get_post_row(db, post_id).await?;
    let mut count: i32 = row.try_get("likes_count")?;
    let existing = db
        .fetch_optional(
            "SELECT id FROM post_likes WHERE post_id = ? AND user_id = ?",
            &[post_id.into_arg(), ctx.user.id.into_arg()],
        )
        .await?;
    if let Some(like) = existing {
        let like_id: Uuid = like.try_get("id")?;
        db.execute("DELETE FROM post_likes WHERE id = ?", &[like_id.into_arg()])
            .await?;
        count = (count - 1).max(0);
        db.execute(
            "UPDATE social_posts SET likes_count = ? WHERE id = ?",
            &[count.into_arg(), post_id.into_arg()],
        )
        .await?;
    }
    Ok(Json(json!({ "liked": false, "likes_count": count })))
}

/// `POST /api/v1/social/posts/{post_id}/comments` — 201.
pub async fn create_comment(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(post_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = auth(&state, &headers).await?;
    let post_id = parse_path_uuid(&post_id, "post_id")?;
    let parsed = parse_body(&body)?;
    let body_text = req_str_raw(&parsed, "body", 1, 2000)?;
    let db = state.db_or_500()?;
    let post = get_post_row(db, post_id).await?;
    let status: String = post.try_get("status")?;
    if status != "published" {
        return Err(not_found("post_not_found", "پست یافت نشد"));
    }
    check_content_policy(db, &body_text).await?;
    let now = Utc::now();
    let comment_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO social_comments (id, post_id, author_id, body, status, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
        &[
            comment_id.into_arg(),
            post_id.into_arg(),
            ctx.user.id.into_arg(),
            body_text.into_arg(),
            "published".into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let count: i32 = post.try_get("comment_count")?;
    db.execute(
        "UPDATE social_posts SET comment_count = ? WHERE id = ?",
        &[(count + 1).into_arg(), post_id.into_arg()],
    )
    .await?;
    let row = db
        .fetch_optional(
            "SELECT id, post_id, author_id, body, status, created_at FROM social_comments WHERE id = ?",
            &[comment_id.into_arg()],
        )
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok((StatusCode::CREATED, Json(comment_out(&row)?)))
}

/// `GET /api/v1/social/posts/{post_id}/comments` — oldest first.
pub async fn list_comments(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Path(post_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _ctx = auth(&state, &headers).await?;
    let post_id = parse_path_uuid(&post_id, "post_id")?;
    let params = parse_query(&uri);
    let (cursor, limit) = page_args(&params)?;
    let db = state.db_or_500()?;
    get_post_row(db, post_id).await?;
    let mut sql = String::from(
        "SELECT id, post_id, author_id, body, status, created_at FROM social_comments \
         WHERE post_id = ? AND status = 'published'",
    );
    let mut args: Vec<Arg> = vec![post_id.into_arg()];
    if let Some((ts, lid)) = cursor {
        sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    sql.push_str(" ORDER BY created_at ASC, id ASC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    let mut keys = Vec::new();
    for row in &rows {
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let id: Uuid = row.try_get("id")?;
        keys.push((created_at, id));
        items.push(comment_out(row)?);
    }
    let (items, next_cursor) = paginate(items, &keys, limit);
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

// --- follows -------------------------------------------------------------------

/// `POST /api/v1/social/follows` — 201. Idempotent: following twice
/// returns the existing row.
pub async fn follow(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = auth(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let followed_id = match parsed.get("followed_id").filter(|v| !v.is_null()) {
        Some(v) => uuid_value(v, "followed_id")?,
        None => {
            return Err(ApiError::validation(vec![
                json!({ "field": "followed_id", "msg": "field is required" }),
            ]));
        }
    };
    if followed_id == ctx.user.id {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "self_follow",
            "نمی‌توانید خودتان را دنبال کنید",
        ));
    }
    let db = state.db_or_500()?;
    let target = db
        .fetch_optional(
            "SELECT id FROM users WHERE id = ?",
            &[followed_id.into_arg()],
        )
        .await?;
    if target.is_none() {
        return Err(not_found("user_not_found", "کاربر یافت نشد"));
    }
    let existing = db
        .fetch_optional(
            "SELECT id, follower_id, followed_id, created_at FROM social_follows \
             WHERE follower_id = ? AND followed_id = ?",
            &[ctx.user.id.into_arg(), followed_id.into_arg()],
        )
        .await?;
    if let Some(row) = existing {
        let id: Uuid = row.try_get("id")?;
        let follower_id: Uuid = row.try_get("follower_id")?;
        let followed_id: Uuid = row.try_get("followed_id")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        return Ok((
            StatusCode::CREATED,
            Json(json!({
                "id": id.to_string(),
                "follower_id": follower_id.to_string(),
                "followed_id": followed_id.to_string(),
                "created_at": created_at.to_rfc3339(),
            })),
        ));
    }
    let now = Utc::now();
    let follow_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO social_follows (id, follower_id, followed_id, created_at) VALUES (?, ?, ?, ?)",
        &[
            follow_id.into_arg(),
            ctx.user.id.into_arg(),
            followed_id.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": follow_id.to_string(),
            "follower_id": ctx.user.id.to_string(),
            "followed_id": followed_id.to_string(),
            "created_at": now.to_rfc3339(),
        })),
    ))
}

/// `DELETE /api/v1/social/follows/{followed_id}` — idempotent,
/// `{"unfollowed": bool}`.
pub async fn unfollow(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(followed_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let followed_id = parse_path_uuid(&followed_id, "followed_id")?;
    let db = state.db_or_500()?;
    let existing = db
        .fetch_optional(
            "SELECT id FROM social_follows WHERE follower_id = ? AND followed_id = ?",
            &[ctx.user.id.into_arg(), followed_id.into_arg()],
        )
        .await?;
    let mut removed = false;
    if let Some(row) = existing {
        let id: Uuid = row.try_get("id")?;
        db.execute("DELETE FROM social_follows WHERE id = ?", &[id.into_arg()])
            .await?;
        removed = true;
    }
    Ok(Json(json!({ "unfollowed": removed })))
}

// --- reports + moderation queue -------------------------------------------------

const REPORT_TARGETS: [&str; 3] = ["post", "comment", "user"];

async fn target_exists(db: &Db, target_type: &str, target_id: Uuid) -> Result<bool, ApiError> {
    let table = match target_type {
        "post" => "social_posts",
        "comment" => "social_comments",
        "user" => "users",
        _ => return Ok(false),
    };
    let row = db
        .fetch_optional(
            &format!("SELECT id FROM {table} WHERE id = ?"),
            &[target_id.into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

/// `POST /api/v1/social/report` — 201. SLA from
/// `social.moderation.sla_hours` (default 24 h).
pub async fn create_report(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = auth(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let target_type = req_str_raw(&parsed, "target_type", 1, 16)?;
    if !REPORT_TARGETS.contains(&target_type.as_str()) {
        return Err(ApiError::validation(vec![
            json!({ "field": "target_type", "msg": "must be one of post, comment, user" }),
        ]));
    }
    let target_id = match parsed.get("target_id").filter(|v| !v.is_null()) {
        Some(v) => uuid_value(v, "target_id")?,
        None => {
            return Err(ApiError::validation(vec![
                json!({ "field": "target_id", "msg": "field is required" }),
            ]));
        }
    };
    let reason = req_str_raw(&parsed, "reason", 1, 2000)?;
    let db = state.db_or_500()?;
    if !target_exists(db, &target_type, target_id).await? {
        return Err(not_found("report_target_not_found", "هدف گزارش یافت نشد"));
    }
    let now = Utc::now();
    let hours = sla_hours(&state, db).await?;
    let sla_due = now + Duration::hours(hours);
    let report_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO social_reports (id, target_type, target_id, reason, reported_by, status, \
         reported_at, triaged_at, actioned_at, sla_due_at, action, note, actioned_by) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            report_id.into_arg(),
            target_type.clone().into_arg(),
            target_id.into_arg(),
            reason.into_arg(),
            ctx.user.id.into_arg(),
            "new".into_arg(),
            now.into_arg(),
            Arg::NullTime,
            Arg::NullTime,
            sla_due.into_arg(),
            Arg::NullText,
            Arg::NullText,
            Arg::NullUuid,
        ],
    )
    .await?;
    emit_event(
        db,
        "social_report",
        &report_id.to_string(),
        "social.report_created",
        json!({
            "target_type": target_type,
            "target_id": target_id.to_string(),
            "reported_by": ctx.user.id.to_string(),
            "sla_due_at": sla_due.to_rfc3339(),
        }),
    )
    .await?;
    let row = get_report_row(db, report_id).await?;
    Ok((StatusCode::CREATED, Json(report_out(&row)?)))
}

/// `GET /api/v1/admin/social/reports` — requires `social.moderate`.
pub async fn report_queue(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    require_permission(&state, &ctx, "social.moderate").await?;
    let params = parse_query(&uri);
    let (cursor, limit) = page_args(&params)?;
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, target_type, target_id, reason, reported_by, status, reported_at, \
         triaged_at, actioned_at, sla_due_at, action, note, actioned_by FROM social_reports",
    );
    let mut args: Vec<Arg> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    if let Some(s) = &status {
        conds.push("status = ?".to_string());
        args.push(s.clone().into_arg());
    }
    desc_cursor_cond(&mut conds, &mut args, cursor, "reported_at");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY reported_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    let mut keys = Vec::new();
    for row in &rows {
        let reported_at: DateTime<Utc> = row.try_get("reported_at")?;
        let id: Uuid = row.try_get("id")?;
        keys.push((reported_at, id));
        items.push(report_out(row)?);
    }
    let (items, next_cursor) = paginate(items, &keys, limit);
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// Resolve the user a report action targets: the author of a reported
/// post/comment, or the reported user themselves.
async fn report_target_user(
    db: &Db,
    target_type: &str,
    target_id: Uuid,
) -> Result<Option<Uuid>, ApiError> {
    match target_type {
        "post" => {
            let row = db
                .fetch_optional(
                    "SELECT author_id FROM social_posts WHERE id = ?",
                    &[target_id.into_arg()],
                )
                .await?;
            Ok(match row {
                Some(r) => {
                    let author_id: Uuid = r.try_get("author_id")?;
                    Some(author_id)
                }
                None => None,
            })
        }
        "comment" => {
            let row = db
                .fetch_optional(
                    "SELECT author_id FROM social_comments WHERE id = ?",
                    &[target_id.into_arg()],
                )
                .await?;
            Ok(match row {
                Some(r) => {
                    let author_id: Uuid = r.try_get("author_id")?;
                    Some(author_id)
                }
                None => None,
            })
        }
        "user" => Ok(Some(target_id)),
        _ => Ok(None),
    }
}

async fn is_protected(db: &Db, user_id: Uuid) -> Result<bool, ApiError> {
    let rows = db
        .fetch_all(
            "SELECT r.name FROM roles r JOIN user_roles ur ON ur.role_id = r.id WHERE ur.user_id = ?",
            &[user_id.into_arg()],
        )
        .await?;
    for row in rows {
        let name: String = row.try_get("name")?;
        if name == "super_admin" {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn notify_user(
    db: &Db,
    user_id: Uuid,
    notif_type: &str,
    title: Value,
    body: Option<String>,
    data: Option<Value>,
) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO notifications (id, user_id, notif_type, title, body, data, read_at, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            Uuid::new_v4().into_arg(),
            user_id.into_arg(),
            notif_type.into_arg(),
            title.into_arg(),
            body.into_arg(),
            data.into_arg(),
            Arg::NullTime,
            Utc::now().into_arg(),
        ],
    )
    .await?;
    Ok(())
}

/// `POST /api/v1/admin/social/reports/{report_id}/action` — requires
/// `social.moderate`. Actions: triage (only from `new`), dismiss,
/// remove_content, warn_user, ban_user.
pub async fn report_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(report_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    require_permission(&state, &ctx, "social.moderate").await?;
    let report_id = parse_path_uuid(&report_id, "report_id")?;
    let parsed = parse_body(&body)?;
    let action = req_str_raw(&parsed, "action", 1, 32)?;
    const ACTIONS: [&str; 5] = [
        "triage",
        "dismiss",
        "remove_content",
        "warn_user",
        "ban_user",
    ];
    if !ACTIONS.contains(&action.as_str()) {
        return Err(ApiError::validation(vec![
            json!({ "field": "action", "msg": "must be one of triage, dismiss, remove_content, warn_user, ban_user" }),
        ]));
    }
    let note = opt_str_raw_max(&parsed, "note", 2000)?;
    let db = state.db_or_500()?;
    let row = get_report_row(db, report_id).await?;
    let status: String = row.try_get("status")?;
    let target_type: String = row.try_get("target_type")?;
    let target_id: Uuid = row.try_get("target_id")?;
    let now = Utc::now();
    match action.as_str() {
        "triage" => {
            if status != "new" {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "invalid_report_state",
                    "فقط گزارش تازه را می‌توان triage کرد",
                ));
            }
            db.execute(
                "UPDATE social_reports SET status = ?, triaged_at = ?, action = ?, note = ?, actioned_by = ? WHERE id = ?",
                &[
                    "triaged".into_arg(),
                    now.into_arg(),
                    action.clone().into_arg(),
                    note.clone().into_arg(),
                    ctx.user.id.into_arg(),
                    report_id.into_arg(),
                ],
            )
            .await?;
        }
        "dismiss" => {
            db.execute(
                "UPDATE social_reports SET status = ?, actioned_at = ?, action = ?, note = ?, actioned_by = ? WHERE id = ?",
                &[
                    "dismissed".into_arg(),
                    now.into_arg(),
                    action.clone().into_arg(),
                    note.clone().into_arg(),
                    ctx.user.id.into_arg(),
                    report_id.into_arg(),
                ],
            )
            .await?;
        }
        "remove_content" => {
            match target_type.as_str() {
                "post" => {
                    db.execute(
                        "UPDATE social_posts SET status = 'removed' WHERE id = ?",
                        &[target_id.into_arg()],
                    )
                    .await?;
                }
                "comment" => {
                    db.execute(
                        "UPDATE social_comments SET status = 'removed' WHERE id = ?",
                        &[target_id.into_arg()],
                    )
                    .await?;
                }
                _ => {}
            }
            db.execute(
                "UPDATE social_reports SET status = ?, actioned_at = ?, action = ?, note = ?, actioned_by = ? WHERE id = ?",
                &[
                    "actioned".into_arg(),
                    now.into_arg(),
                    action.clone().into_arg(),
                    note.clone().into_arg(),
                    ctx.user.id.into_arg(),
                    report_id.into_arg(),
                ],
            )
            .await?;
        }
        "warn_user" => {
            if let Some(warned_id) = report_target_user(db, &target_type, target_id).await? {
                notify_user(
                    db,
                    warned_id,
                    "social.warning",
                    localized_json("اخطار نظارت", "تحذير الإشراف", "Moderation warning"),
                    note.clone(),
                    Some(json!({ "report_id": report_id.to_string() })),
                )
                .await?;
            }
            db.execute(
                "UPDATE social_reports SET status = ?, actioned_at = ?, action = ?, note = ?, actioned_by = ? WHERE id = ?",
                &[
                    "actioned".into_arg(),
                    now.into_arg(),
                    action.clone().into_arg(),
                    note.clone().into_arg(),
                    ctx.user.id.into_arg(),
                    report_id.into_arg(),
                ],
            )
            .await?;
        }
        "ban_user" => {
            if let Some(ban_id) = report_target_user(db, &target_type, target_id).await? {
                let target = db
                    .fetch_optional("SELECT id FROM users WHERE id = ?", &[ban_id.into_arg()])
                    .await?;
                if target.is_some() && !is_protected(db, ban_id).await? {
                    db.execute(
                        "UPDATE users SET is_active = FALSE WHERE id = ?",
                        &[ban_id.into_arg()],
                    )
                    .await?;
                    db.execute(
                        "UPDATE sessions SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
                        &[ban_id.into_arg()],
                    )
                    .await?;
                    db.execute(
                        "UPDATE refresh_tokens SET revoked = TRUE WHERE user_id = ? AND revoked = FALSE",
                        &[ban_id.into_arg()],
                    )
                    .await?;
                }
            }
            db.execute(
                "UPDATE social_reports SET status = ?, actioned_at = ?, action = ?, note = ?, actioned_by = ? WHERE id = ?",
                &[
                    "actioned".into_arg(),
                    now.into_arg(),
                    action.clone().into_arg(),
                    note.clone().into_arg(),
                    ctx.user.id.into_arg(),
                    report_id.into_arg(),
                ],
            )
            .await?;
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_action",
                "اقدام نامعتبر است",
            ));
        }
    }
    let row = get_report_row(db, report_id).await?;
    Ok(Json(report_out(&row)?))
}

// --- banned words ----------------------------------------------------------------

/// `GET /api/v1/moderation/banned-words` — requires `moderation.words`.
pub async fn list_banned_words(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    require_permission(&state, &ctx, "moderation.words").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, word, created_at FROM moderation_banned_words ORDER BY created_at ASC",
            &[],
        )
        .await?;
    let mut items = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let word: String = row.try_get("word")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        items.push(json!({
            "id": id.to_string(),
            "word": word,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    Ok(Json(Value::Array(items)))
}

/// `POST /api/v1/moderation/banned-words` — 201, requires
/// `moderation.words`.
pub async fn add_banned_word(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = auth(&state, &headers).await?;
    require_permission(&state, &ctx, "moderation.words").await?;
    let parsed = parse_body(&body)?;
    let word = req_str_raw(&parsed, "word", 1, 128)?;
    let cleaned = word.trim().to_string();
    if cleaned.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_word",
            "واژه نمی‌تواند خالی باشد",
        ));
    }
    let db = state.db_or_500()?;
    let dup = db
        .fetch_optional(
            "SELECT id FROM moderation_banned_words WHERE word = ?",
            &[cleaned.clone().into_arg()],
        )
        .await?;
    if dup.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "duplicate_banned_word",
            "این واژه قبلاً ثبت شده است",
        ));
    }
    let now = Utc::now();
    let word_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO moderation_banned_words (id, word, added_by, created_at) VALUES (?, ?, ?, ?)",
        &[
            word_id.into_arg(),
            cleaned.clone().into_arg(),
            ctx.user.id.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": word_id.to_string(),
            "word": cleaned,
            "created_at": now.to_rfc3339(),
        })),
    ))
}

/// `DELETE /api/v1/moderation/banned-words/{word_id}` — 204, requires
/// `moderation.words`.
pub async fn delete_banned_word(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(word_id): Path<String>,
) -> ApiResult<StatusCode> {
    let ctx = auth(&state, &headers).await?;
    require_permission(&state, &ctx, "moderation.words").await?;
    let word_id = parse_path_uuid(&word_id, "word_id")?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id FROM moderation_banned_words WHERE id = ?",
            &[word_id.into_arg()],
        )
        .await?;
    if row.is_none() {
        return Err(not_found("banned_word_not_found", "واژه یافت نشد"));
    }
    db.execute(
        "DELETE FROM moderation_banned_words WHERE id = ?",
        &[word_id.into_arg()],
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- notifications ---------------------------------------------------------------

/// `GET /api/v1/social/notifications` — the caller's own, newest first.
pub async fn list_notifications(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let params = parse_query(&uri);
    let (cursor, limit) = page_args(&params)?;
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, notif_type, title, body, data, read_at, created_at FROM notifications \
         WHERE user_id = ?",
    );
    let mut args: Vec<Arg> = vec![ctx.user.id.into_arg()];
    let mut conds: Vec<String> = Vec::new();
    desc_cursor_cond(&mut conds, &mut args, cursor, "created_at");
    if !conds.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    let mut keys = Vec::new();
    for row in &rows {
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let id: Uuid = row.try_get("id")?;
        keys.push((created_at, id));
        items.push(notification_out(row)?);
    }
    let (items, next_cursor) = paginate(items, &keys, limit);
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `POST /api/v1/social/notifications/{notif_id}/read` — only the
/// owner's notification; idempotent. The read is persisted (SOC-02).
pub async fn notification_read(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(notif_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let notif_id = parse_path_uuid(&notif_id, "notif_id")?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, user_id, notif_type, title, body, data, read_at, created_at \
             FROM notifications WHERE id = ?",
            &[notif_id.into_arg()],
        )
        .await?
        .ok_or_else(|| not_found("notification_not_found", "اعلان یافت نشد"))?;
    let owner: Uuid = row.try_get("user_id")?;
    if owner != ctx.user.id {
        return Err(not_found("notification_not_found", "اعلان یافت نشد"));
    }
    let read_at: Option<DateTime<Utc>> = row.try_get("read_at")?;
    if read_at.is_none() {
        let now = Utc::now();
        db.execute(
            "UPDATE notifications SET read_at = ? WHERE id = ?",
            &[now.into_arg(), notif_id.into_arg()],
        )
        .await?;
    }
    let row = db
        .fetch_optional(
            "SELECT id, notif_type, title, body, data, read_at, created_at \
             FROM notifications WHERE id = ?",
            &[notif_id.into_arg()],
        )
        .await?
        .ok_or_else(ApiError::internal)?;
    Ok(Json(notification_out(&row)?))
}

/// `GET /api/v1/social/notifications/unread-count` — `{"unread": n}`.
pub async fn notifications_unread_count(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = auth(&state, &headers).await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM notifications WHERE user_id = ? AND read_at IS NULL",
            &[ctx.user.id.into_arg()],
        )
        .await?;
    let n: i64 = row.try_get("n")?;
    Ok(Json(json!({ "unread": n })))
}
