//! Fatwa (Wave 3) — public topics, verify, quiz, admin chunks/topics/
//! positions/categories/trusted-sources/audit, submissions, clusters,
//! saved fatwas. Ported from `apps/api/app/modules/fatwa/`.
//!
//! `POST /api/v1/fatwa/ask` (the AI assistant / mufti) is deliberately
//! NOT registered here — it stays proxied to the Python server.
//!
//! Check order (FastAPI signature order): path UUID → body (422) →
//! auth (401) → permission (403) → service checks (404/409/422).
//! Exceptions are noted per handler (e.g. submissions transitions
//! check the workflow machine before the per-target permission).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Mutex,
    time::{Duration, Instant},
};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    db::{utc_now, Arg, Db, DynRow, IntoArg},
    embeddings::{
        chunk_to_citation, cosine_similarity, hash_embed, hash_embed_many, resolve_provider,
        search_chunks, ScoredChunk, AI_SCOPES, HASH_MODEL, HASH_PROVIDER, PROJECTION_ID,
    },
    errors::{ApiError, ApiResult},
    settings::{get_setting, get_typed_setting},
    validate::{
        as_int, decode_cursor, encode_cursor, opt_bool_loose, opt_dict, opt_int_loose, opt_list,
        opt_str_raw_max, opt_uuid, parse_body, parse_path_uuid, parse_query, query_int, req_dict,
        req_int_loose, req_localized, req_str_raw,
    },
    workflow::{validate_transition, STATUSES},
    AppState,
};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

fn round4(x: f64) -> f64 {
    (x * 10000.0).round() / 10000.0
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Optional auth (`_optional_user`): any auth failure → guest, never an
/// error — including a `2fa_setup` token, which `authenticate` rejects.
async fn optional_user(state: &AppState, headers: &HeaderMap) -> Option<AuthCtx> {
    authenticate(state, headers, "/api/v1/fatwa").await.ok()
}

/// Plain live permission check (no full-scope re-check — mirrors
/// `submissions_service._has_permission`).
async fn has_permission(db: &Db, user_id: Uuid, key: &str) -> Result<bool, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
             JOIN role_permissions rp ON rp.role_id = ur.role_id \
             JOIN permissions p ON p.id = rp.permission_id \
             WHERE ur.user_id = ? AND p.key = ? LIMIT 1",
            &[user_id.into_arg(), key.into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

async fn is_approver(db: &Db, user_id: Uuid) -> Result<bool, ApiError> {
    has_permission(db, user_id, "fatwa.approve").await
}

fn topic_not_found() -> ApiError {
    ApiError::not_found("topic_not_found", "موضوع یافت نشد")
}

fn pages_of(total: i64, per_page: i64) -> i64 {
    if total == 0 {
        0
    } else {
        (total + per_page - 1) / per_page
    }
}

// ---------------------------------------------------------------------------
// Quota (`fatwa/service.py::check_quota`) — in-process sliding windows.
// ---------------------------------------------------------------------------

static QUOTA_MINUTE: std::sync::LazyLock<Mutex<HashMap<String, VecDeque<Instant>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
static QUOTA_DAILY: std::sync::LazyLock<Mutex<HashMap<String, VecDeque<Instant>>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));

fn prune(window: &mut VecDeque<Instant>, max_age: Duration) {
    while let Some(front) = window.front() {
        if front.elapsed() >= max_age {
            window.pop_front();
        } else {
            break;
        }
    }
}

/// `_ai_int`: typed setting → int (JSON number, numeric string, or a
/// float truncated like Python `int()`); anything else → default.
async fn ai_int_setting(state: &AppState, key: &str, default: i64) -> i64 {
    let Ok(db) = state.db_or_500() else {
        return default;
    };
    let Ok(value) = get_typed_setting(db, &state.config, key, json!(default)).await else {
        return default;
    };
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .unwrap_or(default),
        Value::String(s) => s
            .trim()
            .parse::<i64>()
            .or_else(|_| s.trim().parse::<f64>().map(|f| f as i64))
            .unwrap_or(default),
        _ => default,
    }
}

pub(crate) async fn check_quota(
    state: &AppState,
    endpoint: &str,
    user: Option<&AuthCtx>,
    ip: Option<String>,
) -> Result<(), ApiError> {
    let key = match user {
        Some(ctx) => format!("user:{}", ctx.user.id),
        None => format!("ip:{}", ip.unwrap_or_else(|| "unknown".to_string())),
    };
    let now = Instant::now();
    let rpm = ai_int_setting(state, "ai.rate_per_minute", 5).await;
    {
        let mut map = QUOTA_MINUTE.lock().expect("quota minute lock");
        let dq = map.entry(format!("rpm:{endpoint}:{key}")).or_default();
        prune(dq, Duration::from_secs(60));
        if dq.len() as i64 >= rpm {
            return Err(ApiError::with_details(
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "سقف درخواست در دقیقه رد شد؛ لطفاً کمی بعد تلاش کنید",
                json!({ "retry_after_s": 60 }),
            ));
        }
        // The per-minute slot is consumed even when the daily check
        // below then fails (Python order).
        dq.push_back(now);
    }
    let quota = ai_int_setting(state, "ai.daily_quota_per_user", 20).await;
    if let Some(ctx) = user {
        // Daily limit for authenticated users: DB count of today's
        // `ai_usage_log` rows for this endpoint. Timestamps are
        // compared in Rust, never in SQL (Wave 2 guidance).
        let db = state.db_or_500()?;
        let day_start = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .expect("midnight exists")
            .and_utc();
        let rows = db
            .fetch_all(
                "SELECT created_at FROM ai_usage_log WHERE user_id = ? AND endpoint = ?",
                &[ctx.user.id.into_arg(), endpoint.into_arg()],
            )
            .await?;
        let mut count: i64 = 0;
        for row in &rows {
            let created: DateTime<Utc> = row.try_get("created_at")?;
            if created >= day_start {
                count += 1;
            }
        }
        if count >= quota {
            return Err(ApiError::with_details(
                StatusCode::TOO_MANY_REQUESTS,
                "quota_exceeded",
                "سقف روزانهٔ استفاده از هوش مصنوعی تمام شد",
                json!({ "quota": quota }),
            ));
        }
    } else {
        let mut map = QUOTA_DAILY.lock().expect("quota daily lock");
        let dq = map.entry(format!("daily:{endpoint}:{key}")).or_default();
        prune(dq, Duration::from_secs(86400));
        if dq.len() as i64 >= quota {
            return Err(ApiError::with_details(
                StatusCode::TOO_MANY_REQUESTS,
                "quota_exceeded",
                "سقف روزانهٔ استفاده از هوش مصنوعی تمام شد",
                json!({ "quota": quota }),
            ));
        }
        dq.push_back(now);
    }
    Ok(())
}

pub(crate) async fn log_usage(
    db: &Db,
    user_id: Option<Uuid>,
    endpoint: &str,
    model: &str,
) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO ai_usage_log (id, user_id, endpoint, model, tokens_in, tokens_out, cost_estimate, created_at) VALUES (?, ?, ?, ?, 0, 0, 0.0, ?)",
        &[
            Uuid::new_v4().into_arg(),
            user_id.into_arg(),
            endpoint.into_arg(),
            model.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    Ok(())
}

/// `social.service.notify_user` — plain insert, no preference checks.
async fn notify_user(
    db: &Db,
    user_id: Uuid,
    notif_type: &str,
    title: Value,
    body: &str,
    data: Value,
) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO notifications (id, user_id, notif_type, title, body, data, read_at, created_at) VALUES (?, ?, ?, ?, ?, ?, NULL, ?)",
        &[
            Uuid::new_v4().into_arg(),
            user_id.into_arg(),
            notif_type.into_arg(),
            title.into_arg(),
            body.into_arg(),
            data.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Topics — shared shapes
// ---------------------------------------------------------------------------

const TOPIC_COLS: &str = "id, title, description, workflow_status, category_id, author_name, featured_image_url, view_count, share_count, published_at, created_at, updated_at";

fn topic_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let title: Value = row.try_get("title")?;
    let description: Option<Value> = row.try_get("description")?;
    let workflow_status: String = row.try_get("workflow_status")?;
    let category_id: Option<Uuid> = row.try_get("category_id")?;
    let author_name: Option<String> = row.try_get("author_name")?;
    let featured_image_url: Option<String> = row.try_get("featured_image_url")?;
    let view_count: Option<i32> = row.try_get("view_count")?;
    let share_count: Option<i32> = row.try_get("share_count")?;
    let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
    Ok(json!({
        "id": id.to_string(),
        "title": title,
        "description": description,
        "workflow_status": workflow_status,
        "category_id": category_id.map(|u| u.to_string()),
        "author_name": author_name,
        "featured_image_url": featured_image_url,
        "view_count": view_count.unwrap_or(0),
        "share_count": share_count.unwrap_or(0),
        "published_at": published_at,
    }))
}

fn category_dict(cat: Option<&Value>) -> Value {
    match cat {
        Some(c) => json!({ "slug": c["slug"], "title": c["title"] }),
        None => Value::Null,
    }
}

/// `PublicTopicOut`, in the Python field order.
fn public_topic_dict(row: &DynRow, cats: &HashMap<Uuid, Value>) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let title: Value = row.try_get("title")?;
    let description: Option<Value> = row.try_get("description")?;
    let category_id: Option<Uuid> = row.try_get("category_id")?;
    let author_name: Option<String> = row.try_get("author_name")?;
    let featured_image_url: Option<String> = row.try_get("featured_image_url")?;
    let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
    let view_count: Option<i32> = row.try_get("view_count")?;
    let share_count: Option<i32> = row.try_get("share_count")?;
    let cat = category_id.and_then(|id| cats.get(&id));
    Ok(json!({
        "id": id.to_string(),
        "title": title,
        "description": description,
        "category": category_dict(cat),
        "author_name": author_name,
        "featured_image_url": featured_image_url,
        "published_at": published_at,
        "view_count": view_count.unwrap_or(0),
        "share_count": share_count.unwrap_or(0),
    }))
}

async fn load_fatwa_categories(db: &Db, rows: &[DynRow]) -> Result<HashMap<Uuid, Value>, ApiError> {
    let mut ids: Vec<Uuid> = Vec::new();
    for row in rows {
        let id: Option<Uuid> = row.try_get("category_id")?;
        if let Some(id) = id {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let mut map = HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT id, slug, title FROM fatwa_categories WHERE id IN ({})",
        placeholders.join(", ")
    );
    let args: Vec<Arg> = ids.into_iter().map(IntoArg::into_arg).collect();
    for row in db.fetch_all(&sql, &args).await? {
        let id: Uuid = row.try_get("id")?;
        let slug: String = row.try_get("slug")?;
        let title: Value = row.try_get("title")?;
        map.insert(id, json!({ "slug": slug, "title": title }));
    }
    Ok(map)
}

/// Shared topic filters (`_apply_topic_filters`): `q` searches the
/// JSONB text cast of title and description; `category` is a slug
/// (`"__none"` = uncategorized, unknown slug matches nothing);
/// `status` is an exact, unvalidated workflow status; `exclude` is a
/// topic UUID.
fn topic_filters(
    q: &Option<String>,
    category: &Option<String>,
    status: &Option<String>,
    exclude: &Option<Uuid>,
) -> (Vec<String>, Vec<Arg>) {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(q) = q {
        let needle = format!("%{}%", q.to_lowercase());
        conds.push(
            "(LOWER(CAST(title AS TEXT)) LIKE ? OR LOWER(CAST(description AS TEXT)) LIKE ?)"
                .to_string(),
        );
        args.push(needle.clone().into_arg());
        args.push(needle.into_arg());
    }
    if let Some(cat) = category {
        if cat == "__none" {
            conds.push("category_id IS NULL".to_string());
        } else {
            conds.push(
                "category_id IN (SELECT id FROM fatwa_categories WHERE slug = ?)".to_string(),
            );
            args.push(cat.clone().into_arg());
        }
    }
    if let Some(s) = status {
        conds.push("workflow_status = ?".to_string());
        args.push(s.clone().into_arg());
    }
    if let Some(e) = exclude {
        conds.push("id != ?".to_string());
        args.push(e.into_arg());
    }
    (conds, args)
}

async fn fetch_topic_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        &format!("SELECT {TOPIC_COLS} FROM fiqh_topics WHERE id = ?"),
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(topic_not_found)
}

async fn fetch_published_topic(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    let row = fetch_topic_by_id(db, id).await?;
    let status: String = row.try_get("workflow_status")?;
    if status != "published" {
        return Err(topic_not_found());
    }
    Ok(row)
}

fn position_out(madhab: &str, position_text: Value, evidence_refs: Option<Value>) -> Value {
    json!({
        "madhab": madhab,
        "position_text": position_text,
        "evidence_refs": evidence_refs,
    })
}

async fn positions_for_topic(
    db: &Db,
    topic_id: Uuid,
    only_rag_ok: bool,
) -> Result<Vec<Value>, ApiError> {
    let mut sql =
        "SELECT madhab, position_text, evidence_refs FROM madhab_positions WHERE topic_id = ?"
            .to_string();
    if only_rag_ok {
        sql.push_str(" AND workflow_status IN ('approved', 'published')");
    }
    sql.push_str(" ORDER BY madhab ASC");
    let rows = db.fetch_all(&sql, &[topic_id.into_arg()]).await?;
    let mut out = Vec::new();
    for row in &rows {
        let madhab: String = row.try_get("madhab")?;
        let position_text: Value = row.try_get("position_text")?;
        let evidence_refs: Option<Value> = row.try_get("evidence_refs")?;
        out.push(position_out(&madhab, position_text, evidence_refs));
    }
    Ok(out)
}

// --- public: categories -----------------------------------------------------

/// `GET /api/v1/fatwa/categories` — derived from approved/published
/// chunks, NOT from `fatwa_categories`.
pub async fn categories(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT topic_category, COUNT(*) AS c FROM fiqh_knowledge_chunks WHERE workflow_status IN ('approved', 'published') AND topic_category IS NOT NULL GROUP BY topic_category ORDER BY topic_category",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        let key: String = row.try_get("topic_category")?;
        let count: i64 = row.try_get("c")?;
        out.push(json!({ "key": key, "count": count }));
    }
    Ok(Json(Value::Array(out)))
}

// --- public: topics ---------------------------------------------------------

/// `GET /api/v1/fatwa/topics` — published only.
pub async fn public_topic_list(State(state): State<AppState>, uri: Uri) -> ApiResult<Json<Value>> {
    let params = parse_query(&uri);
    let page = query_int(&params, "page", 1, 1, i64::MAX)?;
    let per_page = query_int(&params, "per_page", 10, 1, 100)?;
    let q = params.get("q").filter(|s| !s.is_empty()).cloned();
    let category = params.get("category").filter(|s| !s.is_empty()).cloned();
    let sort = params
        .get("sort")
        .cloned()
        .unwrap_or_else(|| "newest".to_string());
    if sort != "newest" && sort != "views" {
        return Err(validation_err("sort", "invalid sort"));
    }
    let exclude = match params.get("exclude").filter(|s| !s.is_empty()) {
        Some(s) => Some(
            s.parse::<Uuid>()
                .map_err(|_| validation_err("exclude", "must be a UUID"))?,
        ),
        None => None,
    };
    let db = state.db_or_500()?;
    let mut conds = vec!["workflow_status = 'published'".to_string()];
    let (mut fconds, args) = topic_filters(&q, &category, &None, &exclude);
    conds.append(&mut fconds);
    let where_sql = format!(" WHERE {}", conds.join(" AND "));
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS total FROM fiqh_topics{where_sql}"),
            &args,
        )
        .await?
        .try_get("total")?;
    let order = if sort == "views" {
        "view_count DESC, published_at DESC NULLS LAST"
    } else {
        "published_at DESC NULLS LAST, created_at DESC"
    };
    let mut list_args = args.clone();
    list_args.push(per_page.into_arg());
    list_args.push(((page - 1).saturating_mul(per_page)).into_arg());
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {TOPIC_COLS} FROM fiqh_topics{where_sql} ORDER BY {order} LIMIT ? OFFSET ?"
            ),
            &list_args,
        )
        .await?;
    let cats = load_fatwa_categories(db, &rows).await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(public_topic_dict(row, &cats)?);
    }
    Ok(Json(json!({
        "items": items,
        "total": total,
        "page": page,
        "per_page": per_page,
        "pages": pages_of(total, per_page),
    })))
}

/// `GET /api/v1/fatwa/topics/categories` — one row per registered
/// category (slug ASC, zero counts included) + a trailing `"__none"`
/// row when any published topic is uncategorized.
pub async fn public_topic_categories(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let counts: HashMap<String, i64> = db
        .fetch_all(
            "SELECT category_id, COUNT(*) AS c FROM fiqh_topics WHERE workflow_status = 'published' AND category_id IS NOT NULL GROUP BY category_id",
            &[],
        )
        .await?
        .into_iter()
        .filter_map(|r| {
            let id: Uuid = r.try_get("category_id").ok()?;
            let c: i64 = r.try_get("c").ok()?;
            Some((id.to_string(), c))
        })
        .collect();
    let cats = db
        .fetch_all(
            "SELECT id, slug, title, created_at FROM fatwa_categories ORDER BY slug ASC",
            &[],
        )
        .await?;
    let mut out: Vec<Value> = Vec::new();
    for cat in &cats {
        let id: Uuid = cat.try_get("id")?;
        let slug: String = cat.try_get("slug")?;
        let title: Value = cat.try_get("title")?;
        let created_at: Option<DateTime<Utc>> = cat.try_get("created_at")?;
        let count = counts.get(&id.to_string()).copied().unwrap_or(0);
        out.push(json!({
            "id": id.to_string(),
            "slug": slug,
            "title": title,
            "created_at": created_at,
            "published_count": count,
        }));
    }
    let uncategorized: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS total FROM fiqh_topics WHERE workflow_status = 'published' AND category_id IS NULL",
            &[],
        )
        .await?
        .try_get("total")?;
    if uncategorized > 0 {
        out.push(json!({
            "id": Value::Null,
            "slug": "__none",
            "title": { "fa": "بدون دسته", "ar": "غير مصنّف", "en": "Uncategorized" },
            "created_at": Value::Null,
            "published_count": uncategorized,
        }));
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/fatwa/topics/{topic_id}` — detail; every call counts
/// a view. Positions: ALL positions of the topic (any status — the
/// topic's publication is the gate), ordered by madhab ASC.
pub async fn public_topic_detail(
    State(state): State<AppState>,
    Path(topic_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&topic_id, "topic_id")?;
    let db = state.db_or_500()?;
    fetch_published_topic(db, id).await?;
    db.execute(
        "UPDATE fiqh_topics SET view_count = COALESCE(view_count, 0) + 1 WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    let row = fetch_topic_by_id(db, id).await?;
    let cats = load_fatwa_categories(db, std::slice::from_ref(&row)).await?;
    let mut out = public_topic_dict(&row, &cats)?;
    let positions = positions_for_topic(db, id, false).await?;
    out.as_object_mut()
        .expect("public_topic_dict returns an object")
        .insert("positions".to_string(), Value::Array(positions));
    Ok(Json(out))
}

/// `POST /api/v1/fatwa/topics/{topic_id}/share` — unconditional
/// increment; returns the fresh counter.
pub async fn public_topic_share(
    State(state): State<AppState>,
    Path(topic_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&topic_id, "topic_id")?;
    let db = state.db_or_500()?;
    fetch_published_topic(db, id).await?;
    db.execute(
        "UPDATE fiqh_topics SET share_count = COALESCE(share_count, 0) + 1 WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    let row = fetch_topic_by_id(db, id).await?;
    let share_count: Option<i64> = row.try_get("share_count")?;
    Ok(Json(json!({
        "id": id.to_string(),
        "share_count": share_count.unwrap_or(0),
    })))
}

/// `GET /api/v1/fatwa/compare/{topic_id}` — only approved/published
/// topics, only approved/published positions.
pub async fn compare(
    State(state): State<AppState>,
    Path(topic_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&topic_id, "topic_id")?;
    let db = state.db_or_500()?;
    let row = fetch_topic_by_id(db, id).await?;
    let status: String = row.try_get("workflow_status")?;
    if status != "approved" && status != "published" {
        return Err(topic_not_found());
    }
    let title: Value = row.try_get("title")?;
    let description: Option<Value> = row.try_get("description")?;
    let positions = positions_for_topic(db, id, true).await?;
    Ok(Json(json!({
        "topic_id": id.to_string(),
        "title": title,
        "description": description,
        "positions": positions,
    })))
}

// --- public: quiz -----------------------------------------------------------

/// `POST /api/v1/fatwa/quiz/generate` — optional auth, no body.
/// Quota first; random eligible questions; creates the attempt and
/// logs usage (model `"none"`).
pub async fn quiz_generate(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    let params = parse_query(&uri);
    let count = query_int(&params, "count", 5, 1, 20)?;
    check_quota(&state, "fatwa.quiz", user.as_ref(), ip).await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT q.id AS qid, q.question_text, q.options, q.difficulty, c.source_citation \
             FROM quiz_questions q JOIN fiqh_knowledge_chunks c ON q.source_chunk_id = c.id \
             WHERE q.workflow_status IN ('approved', 'published') \
             AND c.workflow_status IN ('approved', 'published') \
             ORDER BY random() LIMIT ?",
            &[count.into_arg()],
        )
        .await?;
    if rows.is_empty() {
        return Err(ApiError::not_found(
            "no_quiz_questions",
            "سؤال تأییدشده‌ای برای آزمون موجود نیست",
        ));
    }
    let mut question_ids: Vec<String> = Vec::new();
    let mut questions: Vec<Value> = Vec::new();
    for row in &rows {
        let qid: Uuid = row.try_get("qid")?;
        let question_text: Value = row.try_get("question_text")?;
        let options: Value = row.try_get("options")?;
        let difficulty: String = row.try_get("difficulty")?;
        let source_citation: Option<Value> = row.try_get("source_citation")?;
        question_ids.push(qid.to_string());
        questions.push(json!({
            "id": qid.to_string(),
            "question_text": question_text,
            "options": options,
            "difficulty": difficulty,
            "source_citation": source_citation,
        }));
    }
    let attempt_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO quiz_attempts (id, user_id, lesson_id, question_ids, answers, score, created_at) VALUES (?, ?, NULL, ?, ?, 0.0, ?)",
        &[
            attempt_id.into_arg(),
            user.as_ref().map(|c| c.user.id).into_arg(),
            Value::Array(question_ids.iter().map(|s| Value::String(s.clone())).collect())
                .into_arg(),
            json!({}).into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    log_usage(db, user.as_ref().map(|c| c.user.id), "fatwa.quiz", "none").await?;
    Ok(Json(json!({
        "attempt_id": attempt_id.to_string(),
        "questions": questions,
    })))
}

/// `POST /api/v1/fatwa/quiz/submit` — optional auth. Scores the
/// attempt in its stored question order; missing questions are
/// skipped, not counted.
pub async fn quiz_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let parsed = parse_body(&body)?;
    let attempt_id = match parsed.get("attempt_id") {
        Some(Value::String(s)) => s
            .parse::<Uuid>()
            .map_err(|_| validation_err("attempt_id", "must be a UUID"))?,
        _ => return Err(validation_err("attempt_id", "field is required")),
    };
    let answers_raw = match parsed.get("answers") {
        Some(v) if v.is_object() => v.as_object().unwrap().clone(),
        _ => return Err(validation_err("answers", "field is required")),
    };
    let mut answers: HashMap<String, i64> = HashMap::new();
    for (k, v) in &answers_raw {
        let n = as_int(v).ok_or_else(|| validation_err("answers", "values must be integers"))?;
        answers.insert(k.clone(), n);
    }
    let db = state.db_or_500()?;
    let attempt = db
        .fetch_optional(
            "SELECT id, user_id, question_ids, answers, score FROM quiz_attempts WHERE id = ?",
            &[attempt_id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("attempt_not_found", "تلاش آزمون یافت نشد"))?;
    let attempt_user: Option<Uuid> = attempt.try_get("user_id")?;
    if let (Some(ctx), Some(owner)) = (user.as_ref(), attempt_user) {
        if owner != ctx.user.id {
            return Err(ApiError::forbidden(
                "forbidden",
                "این تلاش متعلق به شما نیست",
            ));
        }
    }
    let question_ids: Vec<String> = match attempt.try_get::<Option<Value>, _>("question_ids")? {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let mut details: Vec<Value> = Vec::new();
    let mut correct: i64 = 0;
    if !question_ids.is_empty() {
        let placeholders: Vec<&str> = question_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT id, correct_option FROM quiz_questions WHERE id IN ({})",
            placeholders.join(", ")
        );
        let args: Vec<Arg> = question_ids
            .iter()
            .filter_map(|s| s.parse::<Uuid>().ok())
            .map(IntoArg::into_arg)
            .collect();
        let mut by_id: HashMap<String, i64> = HashMap::new();
        if !args.is_empty() {
            for row in db.fetch_all(&sql, &args).await? {
                let id: Uuid = row.try_get("id")?;
                let correct_option: i32 = row.try_get("correct_option")?;
                by_id.insert(id.to_string(), correct_option as i64);
            }
        }
        for qid in &question_ids {
            let Some(correct_option) = by_id.get(qid) else {
                continue;
            };
            let given = answers.get(qid).copied();
            let is_correct = given == Some(*correct_option);
            if is_correct {
                correct += 1;
            }
            details.push(json!({
                "question_id": qid,
                "given": given,
                "correct_option": correct_option,
                "is_correct": is_correct,
            }));
        }
    }
    let total = details.len() as i64;
    let score = if total == 0 {
        0.0
    } else {
        round2(correct as f64 / total as f64 * 100.0)
    };
    let answers_json = Value::Object(answers.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
    db.execute(
        "UPDATE quiz_attempts SET answers = ?, score = ? WHERE id = ?",
        &[
            answers_json.into_arg(),
            score.into_arg(),
            attempt_id.into_arg(),
        ],
    )
    .await?;
    Ok(Json(json!({
        "score": score,
        "total": total,
        "correct": correct,
        "details": details,
    })))
}

// --- public: verify ---------------------------------------------------------

/// `POST /api/v1/verify/check` — public, no auth, no rate limit.
/// Cache first (no expiry); then hybrid retrieval with the 0.5 /
/// 0.95 thresholds and the `conflicting` heuristic.
pub async fn verify_check(
    State(state): State<AppState>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let parsed = parse_body(&body)?;
    let text = req_str_raw(&parsed, "text", 1, 5000)?;
    let db = state.db_or_500()?;
    let input_hash = format!("{:x}", Sha256::digest(text.as_bytes()));
    let cached = db
        .fetch_optional(
            "SELECT id, verdict, matched_chunk_ids FROM verification_checks WHERE input_hash = ? ORDER BY created_at DESC LIMIT 1",
            &[input_hash.clone().into_arg()],
        )
        .await?;
    if let Some(cached) = cached {
        let verdict: String = cached.try_get("verdict")?;
        let matched_ids: Vec<String> =
            match cached.try_get::<Option<Value>, _>("matched_chunk_ids")? {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect(),
                _ => Vec::new(),
            };
        let mut matched: Vec<Value> = Vec::new();
        for cid in &matched_ids {
            let Ok(cid) = cid.parse::<Uuid>() else {
                continue;
            };
            let Some(chunk) = db
                .fetch_optional(
                    "SELECT id, chunk_text, source_citation, topic_category, madhab_scope FROM fiqh_knowledge_chunks WHERE id = ?",
                    &[cid.into_arg()],
                )
                .await?
            else {
                continue;
            };
            let scored = ScoredChunk {
                chunk_id: chunk.try_get("id")?,
                chunk_text: chunk.try_get("chunk_text")?,
                source_citation: chunk.try_get("source_citation")?,
                topic_category: chunk.try_get("topic_category")?,
                madhab_scope: chunk.try_get("madhab_scope")?,
                score: 0.0,
                lexical: 0.0,
                semantic: 0.0,
            };
            matched.push(chunk_to_citation(&scored));
        }
        return Ok(Json(json!({
            "verdict": verdict,
            "matched": matched,
            "note": "نتیجهٔ کش‌شده",
        })));
    }

    let chunks = search_chunks(&state, &text, 3, "verification", None, None).await?;
    let (verdict, matched_chunks, note): (&str, Vec<&ScoredChunk>, &str) = if chunks.is_empty()
        || chunks[0].score < 0.5
    {
        (
            "not_found",
            chunks.iter().take(1).collect(),
            "در محتوای منتشرشده مورد مشابهی یافت نشد",
        )
    } else if chunks[0].score >= 0.95 {
        (
            "verified",
            chunks.iter().take(1).collect(),
            "متن با محتوای منتشرشده و منبع‌دار مطابقت دارد",
        )
    } else {
        let mut verdict = "not_found";
        let mut note = "مورد مشابهی یافت نشد";
        if chunks.len() >= 2
            && chunks[0].topic_category.is_some()
            && chunks[0].topic_category == chunks[1].topic_category
        {
            let vecs =
                hash_embed_many(&[chunks[0].chunk_text.clone(), chunks[1].chunk_text.clone()]);
            if cosine_similarity(&vecs[0], &vecs[1]) < 0.6 {
                verdict = "conflicting";
                note = "متن ورودی به دو منبع هم‌موضوع ولی ناهمسو نزدیک است؛ نیازمند بررسی انسانی";
            }
        }
        (verdict, chunks.iter().take(2).collect(), note)
    };
    let matched_ids: Vec<Value> = matched_chunks
        .iter()
        .map(|c| Value::String(c.chunk_id.to_string()))
        .collect();
    let excerpt: String = text.chars().take(512).collect();
    db.execute(
        "INSERT INTO verification_checks (id, input_hash, input_excerpt, verdict, matched_chunk_ids, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            Uuid::new_v4().into_arg(),
            input_hash.into_arg(),
            excerpt.into_arg(),
            verdict.into_arg(),
            Value::Array(matched_ids).into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    let matched: Vec<Value> = matched_chunks
        .iter()
        .map(|c| chunk_to_citation(c))
        .collect();
    Ok(Json(json!({
        "verdict": verdict,
        "matched": matched,
        "note": note,
    })))
}

// ---------------------------------------------------------------------------
// Admin: chunks
// ---------------------------------------------------------------------------

const CHUNK_COLS: &str = "id, chunk_text, source_type, source_ref, source_citation, topic_category, madhab_scope, workflow_status, ai_scopes, created_at";

fn chunk_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let chunk_text: String = row.try_get("chunk_text")?;
    let source_type: String = row.try_get("source_type")?;
    let source_ref: Option<Value> = row.try_get("source_ref")?;
    let source_citation: Option<Value> = row.try_get("source_citation")?;
    let topic_category: Option<String> = row.try_get("topic_category")?;
    let madhab_scope: Option<String> = row.try_get("madhab_scope")?;
    let workflow_status: String = row.try_get("workflow_status")?;
    let ai_scopes: Vec<String> = match row.try_get::<Option<Value>, _>("ai_scopes")? {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let created_at: Option<DateTime<Utc>> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "chunk_text": chunk_text,
        "source_type": source_type,
        "source_ref": source_ref,
        "source_citation": source_citation,
        "topic_category": topic_category,
        "madhab_scope": madhab_scope,
        "workflow_status": workflow_status,
        "ai_scopes": ai_scopes,
        "created_at": created_at,
    }))
}

async fn fetch_chunk_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        &format!("SELECT {CHUNK_COLS} FROM fiqh_knowledge_chunks WHERE id = ?"),
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("chunk_not_found", "chunk یافت نشد"))
}

fn check_ai_scopes(scopes: &[String]) -> Result<(), ApiError> {
    let bad: Vec<&String> = scopes
        .iter()
        .filter(|s| !AI_SCOPES.contains(&s.as_str()))
        .collect();
    if !bad.is_empty() {
        let rendered: Vec<String> = bad.iter().map(|s| format!("'{s}'")).collect();
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_ai_scopes",
            format!("scope نامعتبر: [{}]", rendered.join(", ")),
        ));
    }
    Ok(())
}

fn parse_ai_scopes(body: &Value) -> Result<Vec<String>, ApiError> {
    match body.get("ai_scopes") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(validation_err("ai_scopes", "must be a list of strings")),
                }
            }
            Ok(out)
        }
        Some(_) => Err(validation_err("ai_scopes", "must be a list of strings")),
        None => Ok(Vec::new()),
    }
}

fn parse_status_literal(body: &Value) -> Result<String, ApiError> {
    let status = match body.get("status") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("status", "field is required")),
    };
    if !STATUSES.contains(&status.as_str()) {
        return Err(validation_err("status", "invalid status"));
    }
    Ok(status)
}

/// `GET /api/v1/admin/fatwa/chunks` — `knowledge.manage`. UUID
/// cursor; an invalid UUID reaches the generic handler as a 500,
/// exactly like Python's unguarded `UUID(cursor)`.
pub async fn admin_chunk_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    let topic_category = params
        .get("topic_category")
        .filter(|s| !s.is_empty())
        .cloned();
    let after = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut sql = format!("SELECT {CHUNK_COLS} FROM fiqh_knowledge_chunks");
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(s) = &status {
        conds.push("workflow_status = ?".to_string());
        args.push(s.clone().into_arg());
    }
    if let Some(tc) = &topic_category {
        conds.push("topic_category = ?".to_string());
        args.push(tc.clone().into_arg());
    }
    if let Some(after) = &after {
        let after_id = after.parse::<Uuid>().map_err(|_| ApiError::internal())?;
        conds.push("id < ?".to_string());
        args.push(after_id.into_arg());
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let fetched = db.fetch_all(&sql, &args).await?;
    let next_cursor = if (fetched.len() as i64) > limit {
        let last = &fetched[(limit as usize) - 1];
        let id: Uuid = last.try_get("id")?;
        Some(id.to_string())
    } else {
        None
    };
    let mut items = Vec::new();
    for row in fetched.iter().take(limit as usize) {
        items.push(chunk_out(row)?);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `POST /api/v1/admin/fatwa/chunks` — `knowledge.manage`, 201.
pub async fn admin_chunk_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let chunk_text = req_str_raw(&parsed, "chunk_text", 1, usize::MAX)?;
    let source_type =
        opt_str_raw_max(&parsed, "source_type", 32)?.unwrap_or_else(|| "manual".to_string());
    let source_ref = opt_dict(&parsed, "source_ref")?;
    let source_citation = opt_dict(&parsed, "source_citation")?;
    let topic_category = opt_str_raw_max(&parsed, "topic_category", 64)?;
    let madhab_scope = opt_str_raw_max(&parsed, "madhab_scope", 16)?;
    let ai_scopes = parse_ai_scopes(&parsed)?;
    check_ai_scopes(&ai_scopes)?;
    resolve_provider(&state).await?;
    let vector = hash_embed(&chunk_text);
    let db = state.db_or_500()?;
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO fiqh_knowledge_chunks (id, chunk_text, embedding, embedding_provider, embedding_model, embedding_projection, source_type, source_ref, source_library_item_ref, source_citation, topic_category, madhab_scope, workflow_status, approved_by, ai_scopes, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, 'draft', NULL, ?, ?, ?)",
        &[
            id.into_arg(),
            chunk_text.into_arg(),
            Value::Array(vector.iter().map(|v| json!(v)).collect()).into_arg(),
            HASH_PROVIDER.into_arg(),
            HASH_MODEL.into_arg(),
            PROJECTION_ID.into_arg(),
            source_type.into_arg(),
            source_ref.into_arg(),
            source_citation.into_arg(),
            topic_category.clone().into_arg(),
            madhab_scope.into_arg(),
            Value::Array(ai_scopes.iter().map(|s| Value::String(s.clone())).collect())
                .into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "fatwa.chunk_created",
        "fiqh_knowledge_chunk",
        Some(&id.to_string()),
        None,
        Some(json!({ "topic_category": topic_category, "ai_scopes": ai_scopes })),
        None,
        None,
    )
    .await?;
    emit_event(
        db,
        "fiqh_knowledge_chunk",
        &id.to_string(),
        "fatwa.chunk_created",
        json!({ "topic_category": topic_category }),
    )
    .await?;
    let row = fetch_chunk_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(chunk_out(&row)?)))
}

/// `PATCH /api/v1/admin/fatwa/chunks/{chunk_id}/status` —
/// `fatwa.approve`.
pub async fn admin_chunk_set_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(chunk_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&chunk_id, "chunk_id")?;
    let parsed = parse_body(&body)?;
    let target = parse_status_literal(&parsed)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "fatwa.approve").await?;
    let db = state.db_or_500()?;
    let row = fetch_chunk_by_id(db, id).await?;
    let old: String = row.try_get("workflow_status")?;
    validate_transition(&old, &target)?;
    let now = utc_now();
    if target == "approved" || target == "published" {
        db.execute(
            "UPDATE fiqh_knowledge_chunks SET workflow_status = ?, approved_by = ?, updated_at = ? WHERE id = ?",
            &[target.clone().into_arg(), ctx.user.id.into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE fiqh_knowledge_chunks SET workflow_status = ?, updated_at = ? WHERE id = ?",
            &[target.clone().into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
    }
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "fatwa.chunk_status_changed",
        "fiqh_knowledge_chunk",
        Some(&id.to_string()),
        Some(json!({ "workflow_status": old })),
        Some(json!({ "workflow_status": target })),
        None,
        None,
    )
    .await?;
    let event_type = if target == "approved" || target == "published" {
        "fatwa.chunk_approved"
    } else {
        "fatwa.chunk_status_changed"
    };
    emit_event(
        db,
        "fiqh_knowledge_chunk",
        &id.to_string(),
        event_type,
        json!({ "from": old, "to": target }),
    )
    .await?;
    let row = fetch_chunk_by_id(db, id).await?;
    Ok(Json(chunk_out(&row)?))
}

// ---------------------------------------------------------------------------
// Admin: ingestion
// ---------------------------------------------------------------------------

/// `POST /api/v1/admin/fatwa/ingest` — `knowledge.manage`, 202.
/// `enqueued=false` always in Wave 3 (no ARQ enqueue from Rust; the
/// run stays `queued` — a legitimate Python code path when Redis is
/// down).
pub async fn admin_ingest_trigger(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let source_type = req_str_raw(&parsed, "source_type", 1, 32)?;
    if source_type != "text" && source_type != "file" {
        return Err(validation_err("source_type", "must be text or file"));
    }
    let payload = req_dict(&parsed, "payload")?;
    let topic_category = opt_str_raw_max(&parsed, "topic_category", 64)?;
    let madhab_scope = opt_str_raw_max(&parsed, "madhab_scope", 16)?;
    let ai_scopes = parse_ai_scopes(&parsed)?;
    check_ai_scopes(&ai_scopes)?;
    let source_citation = opt_dict(&parsed, "source_citation")?;
    let db = state.db_or_500()?;
    let run_id = Uuid::new_v4();
    let stats = json!({
        "run_config": {
            "payload": payload,
            "topic_category": topic_category,
            "madhab_scope": madhab_scope,
            "ai_scopes": ai_scopes,
            "source_citation": source_citation,
            "triggered_by": ctx.user.id.to_string(),
        },
        "enqueued": false,
    });
    db.execute(
        "INSERT INTO ingestion_runs (id, source_type, status, stats, started_at, finished_at, created_at) VALUES (?, ?, 'queued', ?, NULL, NULL, ?)",
        &[
            run_id.into_arg(),
            source_type.clone().into_arg(),
            stats.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "ingestion_run",
        &run_id.to_string(),
        "ingestion.run_triggered",
        json!({ "source_type": source_type, "enqueued": false }),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "run_id": run_id.to_string(), "status": "queued" })),
    ))
}

/// `GET /api/v1/admin/fatwa/ingest/{run_id}` — `knowledge.manage`.
pub async fn admin_ingest_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(run_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&run_id, "run_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, source_type, status, stats, started_at, finished_at FROM ingestion_runs WHERE id = ?",
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("run_not_found", "اجرای ingestion یافت نشد"))?;
    let source_type: String = row.try_get("source_type")?;
    let status: String = row.try_get("status")?;
    let stats: Option<Value> = row.try_get("stats")?;
    let started_at: Option<DateTime<Utc>> = row.try_get("started_at")?;
    let finished_at: Option<DateTime<Utc>> = row.try_get("finished_at")?;
    Ok(Json(json!({
        "id": id.to_string(),
        "source_type": source_type,
        "status": status,
        "stats": stats.unwrap_or_else(|| json!({})),
        "started_at": started_at,
        "finished_at": finished_at,
    })))
}

// ---------------------------------------------------------------------------
// Admin: topics
// ---------------------------------------------------------------------------

struct TopicInput {
    title: Value,
    description: Option<Value>,
    category_id: Option<Uuid>,
    author_name: Option<String>,
    featured_image_url: Option<String>,
}

fn validate_topic_input(body: &Value) -> Result<TopicInput, ApiError> {
    Ok(TopicInput {
        title: req_dict(body, "title")?,
        description: opt_dict(body, "description")?,
        category_id: opt_uuid(body, "category_id")?,
        author_name: opt_str_raw_max(body, "author_name", 160)?,
        featured_image_url: opt_str_raw_max(body, "featured_image_url", 1024)?,
    })
}

async fn check_fatwa_category_exists(db: &Db, category_id: Uuid) -> Result<(), ApiError> {
    let exists = db
        .fetch_optional(
            "SELECT id FROM fatwa_categories WHERE id = ?",
            &[category_id.into_arg()],
        )
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("category_not_found", "دسته یافت نشد"));
    }
    Ok(())
}

/// `POST /api/v1/admin/fatwa/topics` — `knowledge.manage`, 201.
pub async fn admin_topic_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let input = validate_topic_input(&parsed)?;
    let db = state.db_or_500()?;
    if let Some(cat_id) = input.category_id {
        check_fatwa_category_exists(db, cat_id).await?;
    }
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO fiqh_topics (id, title, description, workflow_status, category_id, author_name, featured_image_url, view_count, share_count, published_at, created_at, updated_at) VALUES (?, ?, ?, 'draft', ?, ?, ?, 0, 0, NULL, ?, ?)",
        &[
            id.into_arg(),
            input.title.into_arg(),
            input.description.into_arg(),
            input.category_id.into_arg(),
            input.author_name.into_arg(),
            input.featured_image_url.into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let row = fetch_topic_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(topic_out(&row)?)))
}

/// `PATCH /api/v1/admin/fatwa/topics/{topic_id}/status` —
/// `fatwa.approve`. On entering `published`, `published_at = now`
/// (never cleared on archive).
pub async fn admin_topic_set_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(topic_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&topic_id, "topic_id")?;
    let parsed = parse_body(&body)?;
    let target = parse_status_literal(&parsed)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "fatwa.approve").await?;
    let db = state.db_or_500()?;
    let row = fetch_topic_by_id(db, id).await?;
    let current: String = row.try_get("workflow_status")?;
    validate_transition(&current, &target)?;
    let now = utc_now();
    if target == "published" {
        db.execute(
            "UPDATE fiqh_topics SET workflow_status = ?, published_at = ?, updated_at = ? WHERE id = ?",
            &[target.into_arg(), now.into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE fiqh_topics SET workflow_status = ?, updated_at = ? WHERE id = ?",
            &[target.into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
    }
    let row = fetch_topic_by_id(db, id).await?;
    Ok(Json(topic_out(&row)?))
}

/// `GET /api/v1/admin/fatwa/topics` — `knowledge.manage`.
pub async fn admin_topic_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let params = parse_query(&uri);
    let page = query_int(&params, "page", 1, 1, i64::MAX)?;
    let per_page = query_int(&params, "per_page", 50, 1, 100)?;
    let q = params.get("q").filter(|s| !s.is_empty()).cloned();
    let category = params.get("category").filter(|s| !s.is_empty()).cloned();
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let (conds, args) = topic_filters(&q, &category, &status, &None);
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS total FROM fiqh_topics{where_sql}"),
            &args,
        )
        .await?
        .try_get("total")?;
    let mut list_args = args.clone();
    list_args.push(per_page.into_arg());
    list_args.push(((page - 1).saturating_mul(per_page)).into_arg());
    let rows = db
        .fetch_all(
            &format!("SELECT {TOPIC_COLS} FROM fiqh_topics{where_sql} ORDER BY created_at DESC LIMIT ? OFFSET ?"),
            &list_args,
        )
        .await?;
    let cats = load_fatwa_categories(db, &rows).await?;
    let mut items = Vec::new();
    for row in &rows {
        let mut d = public_topic_dict(row, &cats)?;
        let workflow_status: String = row.try_get("workflow_status")?;
        d.as_object_mut()
            .expect("public_topic_dict returns an object")
            .insert("workflow_status".to_string(), json!(workflow_status));
        items.push(d);
    }
    Ok(Json(json!({
        "items": items,
        "total": total,
        "page": page,
        "per_page": per_page,
        "pages": pages_of(total, per_page),
    })))
}

/// `PUT /api/v1/admin/fatwa/topics/{topic_id}` — full overwrite;
/// does NOT touch `workflow_status`/`published_at`/counters.
pub async fn admin_topic_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(topic_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&topic_id, "topic_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let input = validate_topic_input(&parsed)?;
    let db = state.db_or_500()?;
    fetch_topic_by_id(db, id).await?;
    if let Some(cat_id) = input.category_id {
        check_fatwa_category_exists(db, cat_id).await?;
    }
    let now = utc_now();
    db.execute(
        "UPDATE fiqh_topics SET title = ?, description = ?, category_id = ?, author_name = ?, featured_image_url = ?, updated_at = ? WHERE id = ?",
        &[
            input.title.into_arg(),
            input.description.into_arg(),
            input.category_id.into_arg(),
            input.author_name.into_arg(),
            input.featured_image_url.into_arg(),
            now.into_arg(),
            id.into_arg(),
        ],
    )
    .await?;
    let row = fetch_topic_by_id(db, id).await?;
    Ok(Json(topic_out(&row)?))
}

// ---------------------------------------------------------------------------
// Admin: fatwa categories
// ---------------------------------------------------------------------------

fn validate_fatwa_category_slug(slug: &str) -> Result<(), ApiError> {
    let mut chars = slug.chars();
    let first_ok = chars
        .next()
        .map(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        .unwrap_or(false);
    if !first_ok
        || slug.chars().count() > 80
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(validation_err("slug", "invalid slug"));
    }
    Ok(())
}

fn fatwa_category_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let slug: String = row.try_get("slug")?;
    let title: Value = row.try_get("title")?;
    let created_at: Option<DateTime<Utc>> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "slug": slug,
        "title": title,
        "created_at": created_at,
    }))
}

async fn fetch_fatwa_category_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        "SELECT id, slug, title, created_at FROM fatwa_categories WHERE id = ?",
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("category_not_found", "دسته یافت نشد"))
}

/// `GET /api/v1/admin/fatwa/categories` — slug ASC.
pub async fn admin_category_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, slug, title, created_at FROM fatwa_categories ORDER BY slug ASC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        out.push(fatwa_category_out(row)?);
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/fatwa/categories` — 201.
pub async fn admin_category_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let slug = req_str_raw(&parsed, "slug", 1, 80)?;
    validate_fatwa_category_slug(&slug)?;
    let title = req_dict(&parsed, "title")?;
    let db = state.db_or_500()?;
    let dup = db
        .fetch_optional(
            "SELECT id FROM fatwa_categories WHERE slug = ?",
            &[slug.clone().into_arg()],
        )
        .await?;
    if dup.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "category_slug_exists",
            "دسته‌ای با این slug هست",
        ));
    }
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO fatwa_categories (id, slug, title, created_at) VALUES (?, ?, ?, ?)",
        &[
            id.into_arg(),
            slug.into_arg(),
            title.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let row = fetch_fatwa_category_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(fatwa_category_out(&row)?)))
}

/// `PUT /api/v1/admin/fatwa/categories/{category_id}` — no dup check
/// when the slug is unchanged.
pub async fn admin_category_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(category_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&category_id, "category_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let slug = req_str_raw(&parsed, "slug", 1, 80)?;
    validate_fatwa_category_slug(&slug)?;
    let title = req_dict(&parsed, "title")?;
    let db = state.db_or_500()?;
    let row = fetch_fatwa_category_by_id(db, id).await?;
    let current_slug: String = row.try_get("slug")?;
    if slug != current_slug {
        let dup = db
            .fetch_optional(
                "SELECT id FROM fatwa_categories WHERE slug = ?",
                &[slug.clone().into_arg()],
            )
            .await?;
        if dup.is_some() {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "category_slug_exists",
                "دسته‌ای با این slug هست",
            ));
        }
    }
    db.execute(
        "UPDATE fatwa_categories SET slug = ?, title = ? WHERE id = ?",
        &[slug.into_arg(), title.into_arg(), id.into_arg()],
    )
    .await?;
    let row = fetch_fatwa_category_by_id(db, id).await?;
    Ok(Json(fatwa_category_out(&row)?))
}

/// `DELETE /api/v1/admin/fatwa/categories/{category_id}` — 204;
/// blocked while any topic uses the category.
pub async fn admin_category_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(category_id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_path_uuid(&category_id, "category_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let db = state.db_or_500()?;
    fetch_fatwa_category_by_id(db, id).await?;
    let used: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS total FROM fiqh_topics WHERE category_id = ?",
            &[id.into_arg()],
        )
        .await?
        .try_get("total")?;
    if used > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "category_in_use",
            "این دسته روی فتواهایی استفاده شده است؛ اول دستهٔ آن‌ها را عوض کنید",
        ));
    }
    db.execute(
        "DELETE FROM fatwa_categories WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Admin: positions
// ---------------------------------------------------------------------------

fn validate_position_input(body: &Value) -> Result<(String, Value, Option<Value>), ApiError> {
    let madhab = req_str_raw(body, "madhab", 1, 16)?;
    if !["hanafi", "maliki", "shafii", "hanbali"].contains(&madhab.as_str()) {
        return Err(validation_err("madhab", "invalid madhab"));
    }
    let position_text = req_dict(body, "position_text")?;
    let evidence_refs = opt_list(body, "evidence_refs")?.map(Value::Array);
    Ok((madhab, position_text, evidence_refs))
}

async fn create_position(
    db: &Db,
    ctx: &AuthCtx,
    topic_id: Uuid,
    madhab: &str,
    position_text: Value,
    evidence_refs: Option<Value>,
) -> Result<Uuid, ApiError> {
    fetch_topic_by_id(db, topic_id).await?;
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO madhab_positions (id, topic_id, madhab, position_text, evidence_refs, workflow_status, created_at) VALUES (?, ?, ?, ?, ?, 'draft', ?)",
        &[
            id.into_arg(),
            topic_id.into_arg(),
            madhab.into_arg(),
            position_text.into_arg(),
            evidence_refs.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "fatwa.position_created",
        "madhab_position",
        Some(&id.to_string()),
        None,
        Some(json!({ "topic_id": topic_id.to_string(), "madhab": madhab })),
        None,
        None,
    )
    .await?;
    Ok(id)
}

/// `POST /api/v1/admin/fatwa/topics/{topic_id}/positions` — 201.
/// The path `topic_id` wins over any body `topic_id`.
pub async fn admin_position_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(topic_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let id = parse_path_uuid(&topic_id, "topic_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let (madhab, position_text, evidence_refs) = validate_position_input(&parsed)?;
    let db = state.db_or_500()?;
    let pos_id = create_position(db, &ctx, id, &madhab, position_text, evidence_refs).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": pos_id.to_string(),
            "madhab": madhab,
            "workflow_status": "draft",
        })),
    ))
}

/// `POST /api/v1/admin/fatwa/positions` — flat variant, 201.
/// Missing/null body `topic_id` → 422 `topic_id_required`.
pub async fn admin_position_create_flat(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let topic_id = match opt_uuid(&parsed, "topic_id")? {
        Some(id) => id,
        None => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "topic_id_required",
                "topic_id در body الزامی است",
            ));
        }
    };
    let (madhab, position_text, evidence_refs) = validate_position_input(&parsed)?;
    let db = state.db_or_500()?;
    let pos_id = create_position(db, &ctx, topic_id, &madhab, position_text, evidence_refs).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": pos_id.to_string(),
            "topic_id": topic_id.to_string(),
            "madhab": madhab,
            "workflow_status": "draft",
        })),
    ))
}

/// `PATCH /api/v1/admin/fatwa/positions/{position_id}/status` —
/// `fatwa.approve`.
pub async fn admin_position_set_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(position_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&position_id, "position_id")?;
    let parsed = parse_body(&body)?;
    let target = parse_status_literal(&parsed)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "fatwa.approve").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, workflow_status FROM madhab_positions WHERE id = ?",
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("position_not_found", "موضع یافت نشد"))?;
    let current: String = row.try_get("workflow_status")?;
    validate_transition(&current, &target)?;
    db.execute(
        "UPDATE madhab_positions SET workflow_status = ? WHERE id = ?",
        &[target.clone().into_arg(), id.into_arg()],
    )
    .await?;
    Ok(Json(json!({
        "id": id.to_string(),
        "workflow_status": target,
    })))
}

// ---------------------------------------------------------------------------
// Admin: quiz questions
// ---------------------------------------------------------------------------

/// `POST /api/v1/admin/fatwa/quiz/questions` — `knowledge.manage`,
/// 201. The source chunk must be approved/published with the `quiz`
/// scope, else 422 `chunk_not_quiz_eligible`.
pub async fn admin_quiz_question_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let question_text = req_dict(&parsed, "question_text")?;
    let options = match parsed.get("options") {
        Some(Value::Array(items)) if items.len() >= 2 => {
            for item in items {
                if !item.is_object() {
                    return Err(validation_err(
                        "options",
                        "options must be a list of objects",
                    ));
                }
            }
            Value::Array(items.clone())
        }
        _ => {
            return Err(validation_err(
                "options",
                "options must be a list of at least 2 objects",
            ));
        }
    };
    let options_len = options.as_array().map(|a| a.len()).unwrap_or(0);
    let correct_option = req_int_loose(&parsed, "correct_option", 0, i64::MAX)?;
    let source_chunk_id = match opt_uuid(&parsed, "source_chunk_id")? {
        Some(id) => id,
        None => return Err(validation_err("source_chunk_id", "field is required")),
    };
    let difficulty =
        opt_str_raw_max(&parsed, "difficulty", 16)?.unwrap_or_else(|| "medium".to_string());
    if !["easy", "medium", "hard"].contains(&difficulty.as_str()) {
        return Err(validation_err("difficulty", "invalid difficulty"));
    }
    let db = state.db_or_500()?;
    let chunk = db
        .fetch_optional(
            "SELECT id, workflow_status, ai_scopes FROM fiqh_knowledge_chunks WHERE id = ?",
            &[source_chunk_id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("chunk_not_found", "chunk مرجع یافت نشد"))?;
    let chunk_status: String = chunk.try_get("workflow_status")?;
    let chunk_scopes: Vec<String> = match chunk.try_get::<Option<Value>, _>("ai_scopes")? {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    if (chunk_status != "approved" && chunk_status != "published")
        || !chunk_scopes.iter().any(|s| s == "quiz")
    {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "chunk_not_quiz_eligible",
            "سؤال فقط می‌تواند به chunk تأییدشده با scope=quiz بچسبد",
        ));
    }
    if correct_option as usize >= options_len {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_correct_option",
            "گزینهٔ صحیح خارج از بازه است",
        ));
    }
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO quiz_questions (id, question_text, options, correct_option, source_chunk_id, difficulty, workflow_status, created_at) VALUES (?, ?, ?, ?, ?, ?, 'draft', ?)",
        &[
            id.into_arg(),
            question_text.into_arg(),
            options.into_arg(),
            correct_option.into_arg(),
            source_chunk_id.into_arg(),
            difficulty.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "quiz.question_created",
        "quiz_question",
        Some(&id.to_string()),
        None,
        Some(json!({ "source_chunk_id": source_chunk_id.to_string() })),
        None,
        None,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id.to_string(), "workflow_status": "draft" })),
    ))
}

// ---------------------------------------------------------------------------
// Admin: trusted sources
// ---------------------------------------------------------------------------

fn trusted_source_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let name: String = row.try_get("name")?;
    let base_url: String = row.try_get("base_url")?;
    let domain: Option<String> = row.try_get("domain")?;
    let enabled: bool = row.try_get("enabled")?;
    let allowed_paths: Vec<String> = match row.try_get::<Option<Value>, _>("allowed_paths")? {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let priority: i32 = row.try_get("priority")?;
    let citation_required: bool = row.try_get("citation_required")?;
    let last_checked_at: Option<DateTime<Utc>> = row.try_get("last_checked_at")?;
    Ok(json!({
        "id": id.to_string(),
        "name": name,
        "base_url": base_url,
        "domain": domain,
        "enabled": enabled,
        "allowed_paths": allowed_paths,
        "priority": priority,
        "citation_required": citation_required,
        "last_checked_at": last_checked_at,
    }))
}

/// `urlparse(base_url).hostname` — null on parse failure.
fn derive_domain(base_url: &str) -> Option<String> {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
}

struct TrustedSourceInput {
    name: String,
    base_url: String,
    enabled: bool,
    allowed_paths: Vec<String>,
    priority: i64,
    citation_required: bool,
}

fn validate_trusted_source_input(body: &Value) -> Result<TrustedSourceInput, ApiError> {
    let name = req_str_raw(body, "name", 1, 160)?;
    let base_url = req_str_raw(body, "base_url", 1, 512)?;
    let enabled = opt_bool_loose(body, "enabled")?.unwrap_or(false);
    let allowed_paths = match body.get("allowed_paths") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(validation_err("allowed_paths", "must be a list of strings"));
                    }
                }
            }
            out
        }
        Some(_) => {
            return Err(validation_err("allowed_paths", "must be a list of strings"));
        }
        None => Vec::new(),
    };
    let priority = opt_int_loose(body, "priority", i64::MIN, i64::MAX)?.unwrap_or(0);
    let citation_required = opt_bool_loose(body, "citation_required")?.unwrap_or(true);
    Ok(TrustedSourceInput {
        name,
        base_url,
        enabled,
        allowed_paths,
        priority,
        citation_required,
    })
}

async fn fetch_trusted_source_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        "SELECT id, name, base_url, domain, enabled, allowed_paths, priority, citation_required, last_checked_at FROM trusted_external_sources WHERE id = ?",
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("source_not_found", "منبع یافت نشد"))
}

/// `GET /api/v1/admin/fatwa/trusted-sources` — `platform.settings`,
/// ordered `priority DESC, name ASC`.
pub async fn admin_trusted_source_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, name, base_url, domain, enabled, allowed_paths, priority, citation_required, last_checked_at FROM trusted_external_sources ORDER BY priority DESC, name ASC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        out.push(trusted_source_out(row)?);
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/fatwa/trusted-sources` — 201.
pub async fn admin_trusted_source_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let input = validate_trusted_source_input(&parsed)?;
    let domain = derive_domain(&input.base_url);
    let db = state.db_or_500()?;
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO trusted_external_sources (id, name, base_url, domain, enabled, allowed_paths, priority, citation_required, last_checked_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
        &[
            id.into_arg(),
            input.name.clone().into_arg(),
            input.base_url.clone().into_arg(),
            domain.into_arg(),
            input.enabled.into_arg(),
            Value::Array(input.allowed_paths.iter().map(|s| Value::String(s.clone())).collect())
                .into_arg(),
            input.priority.into_arg(),
            input.citation_required.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "fatwa.trusted_source_created",
        "trusted_external_source",
        Some(&id.to_string()),
        None,
        Some(json!({ "name": input.name, "base_url": input.base_url })),
        None,
        None,
    )
    .await?;
    let row = fetch_trusted_source_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(trusted_source_out(&row)?)))
}

/// `PATCH /api/v1/admin/fatwa/trusted-sources/{source_id}` — full
/// replacement (omitted fields reset to defaults). Every PATCH sets
/// `last_checked_at = now`, even though nothing was "checked".
pub async fn admin_trusted_source_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(source_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&source_id, "source_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let input = validate_trusted_source_input(&parsed)?;
    let db = state.db_or_500()?;
    let old = fetch_trusted_source_by_id(db, id).await?;
    let old_name: String = old.try_get("name")?;
    let old_base_url: String = old.try_get("base_url")?;
    let old_enabled: bool = old.try_get("enabled")?;
    let old_paths: Vec<String> = match old.try_get::<Option<Value>, _>("allowed_paths")? {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    };
    let old_priority: i64 = old.try_get("priority")?;
    let old_citation: bool = old.try_get("citation_required")?;
    let domain = derive_domain(&input.base_url);
    let now = utc_now();
    db.execute(
        "UPDATE trusted_external_sources SET name = ?, base_url = ?, domain = ?, enabled = ?, allowed_paths = ?, priority = ?, citation_required = ?, last_checked_at = ? WHERE id = ?",
        &[
            input.name.clone().into_arg(),
            input.base_url.clone().into_arg(),
            domain.into_arg(),
            input.enabled.into_arg(),
            Value::Array(input.allowed_paths.iter().map(|s| Value::String(s.clone())).collect())
                .into_arg(),
            input.priority.into_arg(),
            input.citation_required.into_arg(),
            now.into_arg(),
            id.into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "fatwa.trusted_source_updated",
        "trusted_external_source",
        Some(&id.to_string()),
        Some(json!({
            "name": old_name,
            "base_url": old_base_url,
            "enabled": old_enabled,
            "allowed_paths": old_paths,
            "priority": old_priority,
            "citation_required": old_citation,
        })),
        Some(json!({
            "name": input.name,
            "base_url": input.base_url,
            "enabled": input.enabled,
            "allowed_paths": input.allowed_paths,
            "priority": input.priority,
            "citation_required": input.citation_required,
        })),
        None,
        None,
    )
    .await?;
    let row = fetch_trusted_source_by_id(db, id).await?;
    Ok(Json(trusted_source_out(&row)?))
}

/// `POST /api/v1/admin/fatwa/trusted-sources/{source_id}/check` —
/// live `HEAD base_url` (10 s, follow redirects); on status ≥ 400
/// retry with `GET`. Network errors return 200 with the exception
/// class name — never a 5xx.
pub async fn admin_trusted_source_check(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(source_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&source_id, "source_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let db = state.db_or_500()?;
    let row = fetch_trusted_source_by_id(db, id).await?;
    let base_url: String = row.try_get("base_url")?;
    let started = Instant::now();
    let head = state
        .client
        .head(&base_url)
        .timeout(Duration::from_secs(10))
        .send()
        .await;
    let result = match head {
        Ok(resp) => {
            if resp.status().as_u16() >= 400 {
                state
                    .client
                    .get(&base_url)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
            } else {
                Ok(resp)
            }
        }
        Err(e) => Err(e),
    };
    let latency_ms = started.elapsed().as_millis() as i64;
    match result {
        Err(e) => {
            let class = if e.is_timeout() {
                "TimeoutException"
            } else if e.is_connect() {
                "ConnectError"
            } else {
                "RequestError"
            };
            Ok(Json(json!({
                "ok": false,
                "latency_ms": latency_ms,
                "error": class,
            })))
        }
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            let now = utc_now();
            db.execute(
                "UPDATE trusted_external_sources SET last_checked_at = ? WHERE id = ?",
                &[now.into_arg(), id.into_arg()],
            )
            .await?;
            Ok(Json(json!({
                "ok": status_code < 400,
                "status_code": status_code,
                "latency_ms": latency_ms,
                "checked_at": now.to_rfc3339(),
            })))
        }
    }
}

/// `DELETE /api/v1/admin/fatwa/trusted-sources/{source_id}` — 204.
pub async fn admin_trusted_source_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(source_id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_path_uuid(&source_id, "source_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let db = state.db_or_500()?;
    let row = fetch_trusted_source_by_id(db, id).await?;
    let name: String = row.try_get("name")?;
    let base_url: String = row.try_get("base_url")?;
    db.execute(
        "DELETE FROM trusted_external_sources WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "user",
        "fatwa.trusted_source_deleted",
        "trusted_external_source",
        Some(&id.to_string()),
        Some(json!({ "name": name, "base_url": base_url })),
        None,
        None,
        None,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Admin: audit
// ---------------------------------------------------------------------------

/// `GET /api/v1/admin/fatwa/audit` — `knowledge.manage`. UUID cursor
/// (invalid → 500, like Python's unguarded `UUID(after)`). Note:
/// `fatwa_submission` is NOT in the entity set.
pub async fn admin_fatwa_audit(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/fatwa").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let after = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut sql = "SELECT id, actor_id, action, entity_type, entity_id, created_at FROM audit_logs WHERE entity_type IN ('fiqh_knowledge_chunk', 'fiqh_topic', 'madhab_position', 'quiz_question', 'trusted_external_source', 'fatwa_session')".to_string();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(after) = &after {
        let after_id = after.parse::<Uuid>().map_err(|_| ApiError::internal())?;
        sql.push_str(" AND id < ?");
        args.push(after_id.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let fetched = db.fetch_all(&sql, &args).await?;
    let next_cursor = if (fetched.len() as i64) > limit {
        let last = &fetched[(limit as usize) - 1];
        let id: Uuid = last.try_get("id")?;
        Some(id.to_string())
    } else {
        None
    };
    let mut items = Vec::new();
    for row in fetched.iter().take(limit as usize) {
        let id: Uuid = row.try_get("id")?;
        let actor_id: Option<Uuid> = row.try_get("actor_id")?;
        let action: String = row.try_get("action")?;
        let entity_type: String = row.try_get("entity_type")?;
        let entity_id: Option<String> = row.try_get("entity_id")?;
        let created_at: Option<DateTime<Utc>> = row.try_get("created_at")?;
        items.push(json!({
            "id": id.to_string(),
            "actor_id": actor_id.map(|u| u.to_string()),
            "action": action,
            "entity_type": entity_type,
            "entity_id": entity_id,
            "created_at": created_at,
        }));
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

// ---------------------------------------------------------------------------
// Submissions (author panel)
// ---------------------------------------------------------------------------

const SUBMISSION_COLS: &str = "id, title, body, madhab, evidence, author_id, workflow_status, version, reviewed_by, reviewed_at, published_at, created_at, updated_at";

fn submission_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let title: Value = row.try_get("title")?;
    let body: String = row.try_get("body")?;
    let madhab: Option<String> = row.try_get("madhab")?;
    let evidence: Option<String> = row.try_get("evidence")?;
    let author_id: Uuid = row.try_get("author_id")?;
    let workflow_status: String = row.try_get("workflow_status")?;
    let version: i32 = row.try_get("version")?;
    let reviewed_by: Option<Uuid> = row.try_get("reviewed_by")?;
    let reviewed_at: Option<DateTime<Utc>> = row.try_get("reviewed_at")?;
    let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
    let created_at: Option<DateTime<Utc>> = row.try_get("created_at")?;
    let updated_at: Option<DateTime<Utc>> = row.try_get("updated_at")?;
    Ok(json!({
        "id": id.to_string(),
        "title": title,
        "body": body,
        "madhab": madhab,
        "evidence": evidence,
        "author_id": author_id.to_string(),
        "workflow_status": workflow_status,
        "version": version,
        "reviewed_by": reviewed_by.map(|u| u.to_string()),
        "reviewed_at": reviewed_at,
        "published_at": published_at,
        "created_at": created_at,
        "updated_at": updated_at,
    }))
}

async fn fetch_submission_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        &format!("SELECT {SUBMISSION_COLS} FROM fatwa_submissions WHERE id = ?"),
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("submission_not_found", "ثبت فتوا یافت نشد"))
}

/// `_can_participate`: `fatwa.submit` OR `fatwa.approve` (plain live
/// DB check, no full-scope re-check).
async fn can_participate(state: &AppState, ctx: &AuthCtx) -> Result<(), ApiError> {
    let db = state.db_or_500()?;
    if has_permission(db, ctx.user.id, "fatwa.submit").await? {
        return Ok(());
    }
    if has_permission(db, ctx.user.id, "fatwa.approve").await? {
        return Ok(());
    }
    Err(ApiError::forbidden("forbidden", "دسترسی کافی نیست"))
}

/// `POST /api/v1/fatwa/submissions` — 201.
pub async fn submission_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa/submissions").await?;
    can_participate(&state, &ctx).await?;
    let title = req_localized(&parsed, "title")?;
    let body_text = req_str_raw(&parsed, "body", 1, 20000)?;
    let madhab = opt_str_raw_max(&parsed, "madhab", 16)?;
    let evidence = opt_str_raw_max(&parsed, "evidence", 20000)?;
    if body_text.trim().is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_body",
            "متن فتوا نمی‌تواند خالی باشد",
        ));
    }
    let db = state.db_or_500()?;
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO fatwa_submissions (id, title, body, madhab, evidence, author_id, workflow_status, version, reviewed_by, reviewed_at, published_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'draft', 1, NULL, NULL, NULL, ?, ?)",
        &[
            id.into_arg(),
            title.into_arg(),
            body_text.into_arg(),
            madhab.into_arg(),
            evidence.into_arg(),
            ctx.user.id.into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "fatwa_submission",
        &id.to_string(),
        "fatwa.submission_created",
        json!({ "author_id": ctx.user.id.to_string() }),
    )
    .await?;
    let row = fetch_submission_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(submission_out(&row)?)))
}

/// `GET /api/v1/fatwa/submissions` — non-approvers see only their
/// own rows. Base64 cursor.
pub async fn submission_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa/submissions").await?;
    can_participate(&state, &ctx).await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    if let Some(s) = &status {
        if !STATUSES.contains(&s.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_status",
                format!("وضعیت ناشناخته: {s}"),
            ));
        }
    }
    let after = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let approver = is_approver(db, ctx.user.id).await?;
    let mut sql = format!("SELECT {SUBMISSION_COLS} FROM fatwa_submissions");
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if !approver {
        conds.push("author_id = ?".to_string());
        args.push(ctx.user.id.into_arg());
    }
    if let Some(s) = &status {
        conds.push("workflow_status = ?".to_string());
        args.push(s.clone().into_arg());
    }
    if let Some(cursor) = &after {
        let (ts, lid) = decode_cursor(cursor)?;
        conds.push("((created_at < ?) OR (created_at = ? AND id < ?))".to_string());
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let fetched = db.fetch_all(&sql, &args).await?;
    let next_cursor = if (fetched.len() as i64) > limit {
        let last = &fetched[(limit as usize) - 1];
        let ts: DateTime<Utc> = last.try_get("created_at")?;
        let id: Uuid = last.try_get("id")?;
        Some(encode_cursor(ts, id))
    } else {
        None
    };
    let mut items = Vec::new();
    for row in fetched.iter().take(limit as usize) {
        items.push(submission_out(row)?);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/fatwa/submissions/{submission_id}` — author or
/// approver only.
pub async fn submission_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(submission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&submission_id, "submission_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa/submissions").await?;
    can_participate(&state, &ctx).await?;
    let db = state.db_or_500()?;
    let row = fetch_submission_by_id(db, id).await?;
    let author_id: Uuid = row.try_get("author_id")?;
    if author_id != ctx.user.id && !is_approver(db, ctx.user.id).await? {
        return Err(ApiError::forbidden("forbidden", "دسترسی کافی نیست"));
    }
    Ok(Json(submission_out(&row)?))
}

async fn submission_transition(
    state: &AppState,
    headers: &HeaderMap,
    submission_id: &str,
    target: &str,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(submission_id, "submission_id")?;
    let ctx = authenticate(state, headers, "/api/v1/fatwa/submissions").await?;
    can_participate(state, &ctx).await?;
    let db = state.db_or_500()?;
    let row = fetch_submission_by_id(db, id).await?;
    let current: String = row.try_get("workflow_status")?;
    // Workflow validation BEFORE the per-target permission (Python
    // service order: an illegal transition yields 422 first).
    validate_transition(&current, target)?;
    let author_id: Uuid = row.try_get("author_id")?;
    let approver = is_approver(db, ctx.user.id).await?;
    let is_author = author_id == ctx.user.id;
    let allowed = match target {
        "in_review" | "archived" => is_author || approver,
        "approved" | "published" | "rejected" => approver,
        _ => approver,
    };
    if !allowed {
        let msg = if target == "approved" || target == "published" || target == "rejected" {
            "فقط تأییدکنندهٔ فتوا مجاز است"
        } else {
            "دسترسی کافی نیست"
        };
        return Err(ApiError::forbidden("forbidden", msg));
    }
    let old_version: i32 = row.try_get("version")?;
    let now = utc_now();
    let mut sets = vec![
        "workflow_status = ?".to_string(),
        "updated_at = ?".to_string(),
    ];
    let mut args: Vec<Arg> = vec![target.into_arg(), now.into_arg()];
    if target == "approved" {
        sets.push("reviewed_by = ?".to_string());
        sets.push("reviewed_at = ?".to_string());
        args.push(ctx.user.id.into_arg());
        args.push(now.into_arg());
    }
    if target == "published" {
        sets.push("published_at = ?".to_string());
        sets.push("version = ?".to_string());
        args.push(now.into_arg());
        args.push((old_version + 1).into_arg());
    }
    args.push(id.into_arg());
    let sql = format!(
        "UPDATE fatwa_submissions SET {} WHERE id = ?",
        sets.join(", ")
    );
    db.execute(&sql, &args).await?;
    let event_type = match target {
        "in_review" => Some("fatwa.submission_submitted"),
        "approved" => Some("fatwa.submission_approved"),
        "published" => Some("fatwa.submission_published"),
        "rejected" => Some("fatwa.submission_rejected"),
        "archived" => Some("fatwa.submission_archived"),
        // Python `_TRANSITION_EVENTS` has no entry for `draft` (or any other
        // unmapped target): no domain event is emitted. The content-center
        // path mirrors this too (ACC-04, resolved at the Wave 7 merge).
        _ => None,
    };
    if let Some(event_type) = event_type {
        emit_event(
            db,
            "fatwa_submission",
            &id.to_string(),
            event_type,
            json!({ "actor_id": ctx.user.id.to_string(), "to": target }),
        )
        .await?;
    }
    // Telegram hooks are best-effort and swallowed in Python too —
    // skipped in Rust (see module docs).
    let row = fetch_submission_by_id(db, id).await?;
    Ok(Json(submission_out(&row)?))
}

/// `POST /api/v1/fatwa/submissions/{id}/submit` → `in_review`.
pub async fn submission_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(submission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    submission_transition(&state, &headers, &submission_id, "in_review").await
}

/// `POST /api/v1/fatwa/submissions/{id}/approve` → `approved`.
pub async fn submission_approve(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(submission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    submission_transition(&state, &headers, &submission_id, "approved").await
}

/// `POST /api/v1/fatwa/submissions/{id}/publish` → `published`.
pub async fn submission_publish(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(submission_id): Path<String>,
) -> ApiResult<Json<Value>> {
    submission_transition(&state, &headers, &submission_id, "published").await
}

/// `POST /api/v1/fatwa/submissions/{id}/reject` → `rejected`. The
/// body is required (an absent/empty body → 422); the `note` is
/// accepted and never stored.
pub async fn submission_reject(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(submission_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let parsed = parse_body(&body)?;
    let _note = opt_str_raw_max(&parsed, "note", 2000)?;
    submission_transition(&state, &headers, &submission_id, "rejected").await
}

/// `POST /api/v1/fatwa/submissions/{submission_id}/add-to-rag` —
/// `knowledge.manage` (checked BEFORE loading the submission, so
/// 403 precedes 404). 201 `{chunk_id}`.
pub async fn submission_add_to_rag(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(submission_id): Path<String>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let id = parse_path_uuid(&submission_id, "submission_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa/submissions").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let db = state.db_or_500()?;
    let row = fetch_submission_by_id(db, id).await?;
    let status: String = row.try_get("workflow_status")?;
    if status != "published" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "submission_not_published",
            "فقط فتوای منتشرشده را می‌توان به RAG افزود",
        ));
    }
    // Duplicate guard: scan chunks with source_type='fatwa_submission'
    // for a source_ref.id == this submission id.
    let existing = db
        .fetch_all(
            "SELECT source_ref FROM fiqh_knowledge_chunks WHERE source_type = 'fatwa_submission'",
            &[],
        )
        .await?;
    for chunk in &existing {
        if let Some(Value::Object(refmap)) = chunk.try_get::<Option<Value>, _>("source_ref")? {
            if refmap.get("id").and_then(|v| v.as_str()) == Some(&id.to_string()) {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "already_in_rag",
                    "این فتوا قبلاً به RAG افزوده شده است",
                ));
            }
        }
    }
    let title: Value = row.try_get("title")?;
    let body_text: String = row.try_get("body")?;
    let evidence: Option<String> = row.try_get("evidence")?;
    let madhab: Option<String> = row.try_get("madhab")?;
    let version: i32 = row.try_get("version")?;
    let title_fa = title
        .get("fa")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            title
                .get("en")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if !title_fa.is_empty() {
        parts.push(title_fa);
    }
    if !body_text.is_empty() {
        parts.push(body_text);
    }
    if let Some(ev) = evidence.as_deref().filter(|s| !s.is_empty()) {
        parts.push(ev.to_string());
    }
    let chunk_text = parts.join("\n\n");
    resolve_provider(&state).await?;
    let vector = hash_embed(&chunk_text);
    let chunk_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO fiqh_knowledge_chunks (id, chunk_text, embedding, embedding_provider, embedding_model, embedding_projection, source_type, source_ref, source_library_item_ref, source_citation, topic_category, madhab_scope, workflow_status, approved_by, ai_scopes, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'fatwa_submission', ?, NULL, ?, 'fatwa', ?, 'approved', ?, ?, ?, ?)",
        &[
            chunk_id.into_arg(),
            chunk_text.into_arg(),
            Value::Array(vector.iter().map(|v| json!(v)).collect()).into_arg(),
            HASH_PROVIDER.into_arg(),
            HASH_MODEL.into_arg(),
            PROJECTION_ID.into_arg(),
            json!({ "type": "fatwa_submission", "id": id.to_string(), "version": version }).into_arg(),
            title.into_arg(),
            madhab.into_arg(),
            ctx.user.id.into_arg(),
            Value::Array(vec![Value::String("fatwa".to_string()), Value::String("assistant".to_string())]).into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "fatwa_submission",
        &id.to_string(),
        "fatwa.submission_added_to_rag",
        json!({ "chunk_id": chunk_id.to_string(), "actor_id": ctx.user.id.to_string() }),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "chunk_id": chunk_id.to_string() })),
    ))
}

// ---------------------------------------------------------------------------
// Clusters
// ---------------------------------------------------------------------------

/// `users.service.ensure_profile_for_section(db, user, "fatwa_question")`.
async fn ensure_profile_for_fatwa_question(
    state: &AppState,
    ctx: &AuthCtx,
) -> Result<(), ApiError> {
    let db = state.db_or_500()?;
    let required = get_typed_setting(
        db,
        &state.config,
        "profile.require_for_fatwa_question",
        json!(false),
    )
    .await?;
    if required != Value::Bool(true) {
        return Ok(());
    }
    let user = &ctx.user;
    let mut missing: Vec<&str> = Vec::new();
    if user.full_name.trim().is_empty() {
        missing.push("full_name");
    }
    if user.avatar_media_id.is_none() {
        missing.push("avatar_media_id");
    }
    if user.date_of_birth.is_none() {
        missing.push("date_of_birth");
    }
    if user.gender.is_none() {
        missing.push("gender");
    }
    if user.country_code.is_none() {
        missing.push("country_code");
    }
    if !missing.is_empty() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "profile_incomplete",
            format!(
                "برای استفاده از این بخش، اول پروفایل‌تان را کامل کنید: {}",
                missing.join("، ")
            ),
        ));
    }
    Ok(())
}

/// `fatwa.cluster_match_threshold` — raw string setting, `float(raw)`
/// with default 0.82, clamped to [0, 1].
async fn cluster_threshold(state: &AppState) -> f64 {
    let Ok(db) = state.db_or_500() else {
        return 0.82;
    };
    let raw = get_setting(db, &state.config, "fatwa.cluster_match_threshold").await;
    let value = match raw {
        Ok(Some(s)) if !s.is_empty() => s.trim().parse::<f64>().unwrap_or(0.82),
        _ => 0.82,
    };
    value.clamp(0.0, 1.0)
}

const CLUSTER_COLS: &str =
    "id, representative_question, embedding, status, answer, answered_by, answered_at, created_at";

fn cluster_out(row: &DynRow, member_count: i64, members: Vec<Value>) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let representative_question: String = row.try_get("representative_question")?;
    let status: String = row.try_get("status")?;
    let answer: Option<String> = row.try_get("answer")?;
    let answered_by: Option<Uuid> = row.try_get("answered_by")?;
    let answered_at: Option<DateTime<Utc>> = row.try_get("answered_at")?;
    let created_at: Option<DateTime<Utc>> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "representative_question": representative_question,
        "status": status,
        "answer": answer,
        "answered_by": answered_by.map(|u| u.to_string()),
        "answered_at": answered_at,
        "member_count": member_count,
        "members": members,
        "created_at": created_at,
    }))
}

async fn count_cluster_members(db: &Db, cluster_id: Uuid) -> Result<i64, ApiError> {
    let row = db
        .fetch_one(
            "SELECT COUNT(*) AS total FROM cluster_members WHERE cluster_id = ?",
            &[cluster_id.into_arg()],
        )
        .await?;
    row.try_get("total")
}

/// `POST /api/v1/fatwa/questions` — 201. Embeds the question and
/// matches it against open/answered clusters (cosine, full scan).
pub async fn submit_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let text = req_str_raw(&parsed, "text", 1, 5000)?;
    let source = opt_str_raw_max(&parsed, "source", 16)?.unwrap_or_else(|| "direct".to_string());
    if source != "chat" && source != "direct" {
        return Err(validation_err("source", "invalid source"));
    }
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa").await?;
    ensure_profile_for_fatwa_question(&state, &ctx).await?;
    let cleaned = text.trim().to_string();
    if cleaned.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_question",
            "متن سوال نمی‌تواند خالی باشد",
        ));
    }
    resolve_provider(&state).await?;
    let query_vec = hash_embed(&cleaned);
    let threshold = cluster_threshold(&state).await;
    let db = state.db_or_500()?;
    let clusters = db
        .fetch_all(
            "SELECT id, status, answer, answered_at, embedding FROM question_clusters WHERE status IN ('open', 'answered') AND embedding IS NOT NULL",
            &[],
        )
        .await?;
    let mut best_id: Option<Uuid> = None;
    let mut best_status: Option<String> = None;
    let mut best_answer: Option<String> = None;
    let mut best_answered_at: Option<DateTime<Utc>> = None;
    let mut best_score: f64 = -1.0;
    for cluster in &clusters {
        let embedding = stored_embedding_for_cluster(cluster)?;
        let score = cosine_similarity(&query_vec, &embedding);
        if score > best_score {
            best_score = score;
            best_id = Some(cluster.try_get("id")?);
            best_status = Some(cluster.try_get("status")?);
            best_answer = cluster.try_get("answer")?;
            best_answered_at = cluster.try_get("answered_at")?;
        }
    }
    let now = utc_now();
    if let Some(cluster_id) = best_id {
        if best_score >= threshold {
            let member_id = Uuid::new_v4();
            db.execute(
                "INSERT INTO cluster_members (id, cluster_id, question_text, user_id, source, created_at) VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    member_id.into_arg(),
                    cluster_id.into_arg(),
                    cleaned.into_arg(),
                    ctx.user.id.into_arg(),
                    source.clone().into_arg(),
                    now.into_arg(),
                ],
            )
            .await?;
            emit_event(
                db,
                "question_cluster",
                &cluster_id.to_string(),
                "fatwa.cluster_question_added",
                json!({
                    "user_id": ctx.user.id.to_string(),
                    "source": source,
                    "similarity": round4(best_score),
                }),
            )
            .await?;
            let suggestion = if best_status.as_deref() == Some("answered")
                && best_answer.as_ref().map(|s| !s.is_empty()).unwrap_or(false)
            {
                json!({ "answer": best_answer, "answered_at": best_answered_at })
            } else {
                Value::Null
            };
            return Ok((
                StatusCode::CREATED,
                Json(json!({
                    "cluster_id": cluster_id.to_string(),
                    "matched": true,
                    "similarity": round4(best_score),
                    "suggestion": suggestion,
                })),
            ));
        }
    }
    let cluster_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO question_clusters (id, representative_question, embedding, status, answer, answered_by, answered_at, created_at) VALUES (?, ?, ?, 'open', NULL, NULL, NULL, ?)",
        &[
            cluster_id.into_arg(),
            cleaned.clone().into_arg(),
            Value::Array(query_vec.iter().map(|v| json!(v)).collect()).into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let member_id = Uuid::new_v4();
    db.execute(
        "INSERT INTO cluster_members (id, cluster_id, question_text, user_id, source, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        &[
            member_id.into_arg(),
            cluster_id.into_arg(),
            cleaned.into_arg(),
            ctx.user.id.into_arg(),
            source.clone().into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "question_cluster",
        &cluster_id.to_string(),
        "fatwa.cluster_created",
        json!({ "user_id": ctx.user.id.to_string(), "source": source }),
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "cluster_id": cluster_id.to_string(),
            "matched": false,
            "similarity": Value::Null,
            "suggestion": Value::Null,
        })),
    ))
}

fn stored_embedding_for_cluster(row: &DynRow) -> Result<Vec<f64>, ApiError> {
    let value: Option<Value> = row.try_get("embedding")?;
    Ok(crate::embeddings::stored_embedding(value))
}

/// `GET /api/v1/fatwa/clusters` — `fatwa.answer_cluster`. Base64
/// cursor; `members` is empty in the list.
pub async fn cluster_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa").await?;
    require_permission(&state, &ctx, "fatwa.answer_cluster").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    if let Some(s) = &status {
        if s != "open" && s != "answered" && s != "archived" {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_status",
                "وضعیت خوشه نامعتبر است",
            ));
        }
    }
    let after = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut sql = format!("SELECT {CLUSTER_COLS} FROM question_clusters");
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(s) = &status {
        conds.push("status = ?".to_string());
        args.push(s.clone().into_arg());
    }
    if let Some(cursor) = &after {
        let (ts, lid) = decode_cursor(cursor)?;
        conds.push("((created_at < ?) OR (created_at = ? AND id < ?))".to_string());
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let fetched = db.fetch_all(&sql, &args).await?;
    let next_cursor = if (fetched.len() as i64) > limit {
        let last = &fetched[(limit as usize) - 1];
        let ts: DateTime<Utc> = last.try_get("created_at")?;
        let id: Uuid = last.try_get("id")?;
        Some(encode_cursor(ts, id))
    } else {
        None
    };
    let mut items = Vec::new();
    for row in fetched.iter().take(limit as usize) {
        let id: Uuid = row.try_get("id")?;
        let count = count_cluster_members(db, id).await?;
        items.push(cluster_out(row, count, Vec::new())?);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/fatwa/clusters/{cluster_id}` — members populated
/// (`created_at` ASC); `member_count` = `len(members)`.
pub async fn cluster_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cluster_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&cluster_id, "cluster_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa").await?;
    require_permission(&state, &ctx, "fatwa.answer_cluster").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            &format!("SELECT {CLUSTER_COLS} FROM question_clusters WHERE id = ?"),
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("cluster_not_found", "خوشه یافت نشد"))?;
    let member_rows = db
        .fetch_all(
            "SELECT id, question_text, user_id, source, created_at FROM cluster_members WHERE cluster_id = ? ORDER BY created_at ASC",
            &[id.into_arg()],
        )
        .await?;
    let mut members = Vec::new();
    for m in &member_rows {
        let mid: Uuid = m.try_get("id")?;
        let question_text: String = m.try_get("question_text")?;
        let user_id: Uuid = m.try_get("user_id")?;
        let source: String = m.try_get("source")?;
        let created_at: Option<DateTime<Utc>> = m.try_get("created_at")?;
        members.push(json!({
            "id": mid.to_string(),
            "question_text": question_text,
            "user_id": user_id.to_string(),
            "source": source,
            "created_at": created_at,
        }));
    }
    let count = members.len() as i64;
    Ok(Json(cluster_out(&row, count, members)?))
}

struct ClusterPublishInput {
    title: String,
    evidence: Option<String>,
    positions: HashMap<String, String>,
    category_id: Option<Uuid>,
    featured_image_url: Option<String>,
    author_name: Option<String>,
}

fn validate_cluster_publish(body: &Value) -> Result<ClusterPublishInput, ApiError> {
    let title = req_str_raw(body, "title", 1, 500)?;
    let evidence = opt_str_raw_max(body, "evidence", 20000)?;
    let mut positions: HashMap<String, String> = HashMap::new();
    if let Some(Value::Object(map)) = body.get("positions") {
        for (k, v) in map {
            match v.as_str() {
                Some(s) => {
                    positions.insert(k.clone(), s.to_string());
                }
                None => {
                    return Err(validation_err(
                        "positions",
                        "positions values must be strings",
                    ));
                }
            }
        }
    } else if body.get("positions").is_some() {
        return Err(validation_err("positions", "positions must be an object"));
    }
    let category_id = opt_uuid(body, "category_id")?;
    let featured_image_url = opt_str_raw_max(body, "featured_image_url", 1024)?;
    let author_name = opt_str_raw_max(body, "author_name", 160)?;
    Ok(ClusterPublishInput {
        title,
        evidence,
        positions,
        category_id,
        featured_image_url,
        author_name,
    })
}

/// `POST /api/v1/fatwa/clusters/{cluster_id}/answer` —
/// `fatwa.answer_cluster`. Re-answering is allowed. When `publish`
/// is given, the answer is simultaneously published as a public
/// fatwa (hanafi position = the answer).
pub async fn cluster_answer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cluster_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&cluster_id, "cluster_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwa").await?;
    require_permission(&state, &ctx, "fatwa.answer_cluster").await?;
    let answer = req_str_raw(&parsed, "answer", 1, 20000)?;
    let publish = match parsed.get("publish") {
        Some(Value::Null) | None => None,
        Some(v) if v.is_object() => Some(validate_cluster_publish(v)?),
        Some(_) => return Err(validation_err("publish", "publish must be an object")),
    };
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            &format!("SELECT {CLUSTER_COLS} FROM question_clusters WHERE id = ?"),
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("cluster_not_found", "خوشه یافت نشد"))?;
    let _ = row;
    let cleaned = answer.trim().to_string();
    if cleaned.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "empty_answer",
            "متن پاسخ نمی‌تواند خالی باشد",
        ));
    }
    // Publish validation happens before the cluster update (Python
    // raises inside the same transaction; Rust auto-commits per
    // statement, so validate first to keep the observable order).
    if let Some(publish) = &publish {
        if publish.title.trim().is_empty() {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "empty_title",
                "عنوان فتوا نمی‌تواند خالی باشد",
            ));
        }
        if let Some(cat_id) = publish.category_id {
            let exists = db
                .fetch_optional(
                    "SELECT id FROM fatwa_categories WHERE id = ?",
                    &[cat_id.into_arg()],
                )
                .await?;
            if exists.is_none() {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "category_not_found",
                    "دستهٔ فتوا یافت نشد",
                ));
            }
        }
    }
    let now = utc_now();
    db.execute(
        "UPDATE question_clusters SET answer = ?, answered_by = ?, answered_at = ?, status = 'answered' WHERE id = ?",
        &[
            cleaned.clone().into_arg(),
            ctx.user.id.into_arg(),
            now.into_arg(),
            id.into_arg(),
        ],
    )
    .await?;
    let mut published_topic_id: Option<Uuid> = None;
    if let Some(publish) = &publish {
        let title = publish.title.trim().to_string();
        let topic_id = Uuid::new_v4();
        db.execute(
            "INSERT INTO fiqh_topics (id, title, description, workflow_status, category_id, author_name, featured_image_url, view_count, share_count, published_at, created_at, updated_at) VALUES (?, ?, NULL, 'published', ?, ?, ?, 0, 0, ?, ?, ?)",
            &[
                topic_id.into_arg(),
                json!({ "fa": title }).into_arg(),
                publish.category_id.into_arg(),
                publish.author_name.clone().into_arg(),
                publish.featured_image_url.clone().into_arg(),
                now.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
        let evidence = publish.evidence.as_deref().unwrap_or("").trim().to_string();
        let evidence_refs: Option<Value> = if evidence.is_empty() {
            None
        } else {
            Some(json!([evidence]))
        };
        db.execute(
            "INSERT INTO madhab_positions (id, topic_id, madhab, position_text, evidence_refs, workflow_status, created_at) VALUES (?, ?, 'hanafi', ?, ?, 'published', ?)",
            &[
                Uuid::new_v4().into_arg(),
                topic_id.into_arg(),
                json!({ "fa": cleaned }).into_arg(),
                evidence_refs.into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
        for madhab in ["shafii", "maliki", "hanbali"] {
            if let Some(text) = publish.positions.get(madhab) {
                let text = text.trim();
                if !text.is_empty() {
                    db.execute(
                        "INSERT INTO madhab_positions (id, topic_id, madhab, position_text, evidence_refs, workflow_status, created_at) VALUES (?, ?, ?, ?, NULL, 'published', ?)",
                        &[
                            Uuid::new_v4().into_arg(),
                            topic_id.into_arg(),
                            madhab.into_arg(),
                            json!({ "fa": text }).into_arg(),
                            now.into_arg(),
                        ],
                    )
                    .await?;
                }
            }
        }
        published_topic_id = Some(topic_id);
    }
    let member_rows = db
        .fetch_all(
            "SELECT user_id FROM cluster_members WHERE cluster_id = ? ORDER BY created_at ASC",
            &[id.into_arg()],
        )
        .await?;
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut user_ids: Vec<Uuid> = Vec::new();
    for m in &member_rows {
        let uid: Uuid = m.try_get("user_id")?;
        if seen.insert(uid) {
            user_ids.push(uid);
        }
    }
    for uid in &user_ids {
        notify_user(
            db,
            *uid,
            "fatwa.cluster_answered",
            json!({
                "fa": "سوال شما پاسخ داده شد",
                "ar": "تمت الإجابة على سؤالك",
                "en": "Your question was answered",
            }),
            &cleaned,
            json!({ "cluster_id": id.to_string() }),
        )
        .await?;
    }
    emit_event(
        db,
        "question_cluster",
        &id.to_string(),
        "fatwa.cluster_answered",
        json!({
            "answered_by": ctx.user.id.to_string(),
            "member_count": user_ids.len(),
            "notified_count": user_ids.len(),
            "published_topic_id": published_topic_id.map(|u| u.to_string()),
        }),
    )
    .await?;
    Ok(Json(json!({
        "id": id.to_string(),
        "status": "answered",
        "answer": cleaned,
        "answered_at": now,
        "notified_count": user_ids.len(),
        "published_topic_id": published_topic_id.map(|u| u.to_string()),
    })))
}

// ---------------------------------------------------------------------------
// Saved fatwas (bookmarks)
// ---------------------------------------------------------------------------

/// `GET /api/v1/fatwas/saved` — most recently saved first; topics
/// that are no longer published are silently dropped.
pub async fn saved_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/fatwas").await?;
    let db = state.db_or_500()?;
    let saved = db
        .fetch_all(
            "SELECT fatwa_id FROM user_saved_fatwas WHERE user_id = ? ORDER BY created_at DESC, id DESC",
            &[ctx.user.id.into_arg()],
        )
        .await?;
    let mut items = Vec::new();
    let mut rows: Vec<DynRow> = Vec::new();
    for s in &saved {
        let fatwa_id: Uuid = s.try_get("fatwa_id")?;
        if let Ok(row) = fetch_published_topic(db, fatwa_id).await {
            rows.push(row);
        }
    }
    let cats = load_fatwa_categories(db, &rows).await?;
    for row in &rows {
        items.push(public_topic_dict(row, &cats)?);
    }
    Ok(Json(json!({ "items": items })))
}

/// `GET /api/v1/fatwas/{fatwa_id}/saved` — optional auth; guests
/// always get `{saved: false}`. No topic lookup at all.
pub async fn saved_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(fatwa_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&fatwa_id, "fatwa_id")?;
    let Some(ctx) = optional_user(&state, &headers).await else {
        return Ok(Json(json!({
            "fatwa_id": id.to_string(),
            "saved": false,
        })));
    };
    let db = state.db_or_500()?;
    let exists = db
        .fetch_optional(
            "SELECT id FROM user_saved_fatwas WHERE user_id = ? AND fatwa_id = ?",
            &[ctx.user.id.into_arg(), id.into_arg()],
        )
        .await?;
    Ok(Json(json!({
        "fatwa_id": id.to_string(),
        "saved": exists.is_some(),
    })))
}

/// `POST /api/v1/fatwas/{fatwa_id}/saved` — idempotent save.
pub async fn saved_save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(fatwa_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&fatwa_id, "fatwa_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwas").await?;
    let db = state.db_or_500()?;
    fetch_published_topic(db, id).await?;
    let exists = db
        .fetch_optional(
            "SELECT id FROM user_saved_fatwas WHERE user_id = ? AND fatwa_id = ?",
            &[ctx.user.id.into_arg(), id.into_arg()],
        )
        .await?;
    if exists.is_none() {
        db.execute(
            "INSERT INTO user_saved_fatwas (id, user_id, fatwa_id, created_at) VALUES (?, ?, ?, ?)",
            &[
                Uuid::new_v4().into_arg(),
                ctx.user.id.into_arg(),
                id.into_arg(),
                utc_now().into_arg(),
            ],
        )
        .await?;
    }
    Ok(Json(json!({
        "fatwa_id": id.to_string(),
        "saved": true,
    })))
}

/// `DELETE /api/v1/fatwas/{fatwa_id}/saved` — idempotent unsave.
pub async fn saved_unsave(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(fatwa_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&fatwa_id, "fatwa_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/fatwas").await?;
    let db = state.db_or_500()?;
    fetch_published_topic(db, id).await?;
    db.execute(
        "DELETE FROM user_saved_fatwas WHERE user_id = ? AND fatwa_id = ?",
        &[ctx.user.id.into_arg(), id.into_arg()],
    )
    .await?;
    Ok(Json(json!({
        "fatwa_id": id.to_string(),
        "saved": false,
    })))
}
