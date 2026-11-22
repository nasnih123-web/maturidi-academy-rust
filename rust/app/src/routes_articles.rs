//! Articles (Wave 3) — public reads + admin CRUD, ported from
//! `apps/api/app/modules/articles/{router,service,schemas}.py`.
//!
//! WordPress model: SEO fields + featured image on grid cards, grid
//! 10/page (public) / 50/page (admin), detail with author + publish
//! date + share count, category sidebar with counts (incl. the
//! `"__none"` row for uncategorized published articles). There is NO
//! server-side "same-category slider" or "sidebar" endpoint — the
//! client composes those from the list endpoint (`category=` +
//! `exclude=`); do not invent one. Video inside an article is embedded
//! content inside the opaque Tiptap `body` JSON — stored and returned
//! verbatim, never handled server-side.
//!
//! Check order (FastAPI signature order): path UUID → body (422) →
//! auth (401) → permission (403) → service checks (404/409/422).
//! Public routes have no auth. View/share counters increment
//! unconditionally on every call, unauthenticated, un-deduped.

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    authn::{authenticate, require_permission},
    db::{Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    validate::{
        opt_str_raw_max, opt_uuid, parse_body, parse_path_uuid, parse_query, query_int, req_dict,
        req_str_raw,
    },
    workflow::validate_transition,
    AppState,
};

const ARTICLE_COLS: &str = "id, title, body, slug, seo_title, seo_description, workflow_status, category_id, author_name, featured_image_url, view_count, share_count, published_at, created_at, updated_at";

fn not_found() -> ApiError {
    ApiError::not_found("article_not_found", "مقاله یافت نشد")
}

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// `slug`: 1–200 chars, `^[^\s/]+$` (Unicode allowed — Persian slugs
/// included; no whitespace, no `/`).
fn validate_article_slug(slug: &str) -> Result<(), ApiError> {
    if slug.is_empty()
        || slug.chars().count() > 200
        || slug.chars().any(|c| c.is_whitespace() || c == '/')
    {
        return Err(validation_err("slug", "invalid slug"));
    }
    Ok(())
}

/// Category slug: 1–80 chars, `^[a-z0-9][a-z0-9_-]*$` (lowercase ASCII
/// only, must start with a letter or digit).
fn validate_category_slug(slug: &str) -> Result<(), ApiError> {
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

struct ArticleInput {
    title: Value,
    body: Value,
    slug: String,
    seo_title: Option<String>,
    seo_description: Option<String>,
    category_id: Option<Uuid>,
    author_name: Option<String>,
    featured_image_url: Option<String>,
}

fn validate_article_input(body: &Value) -> Result<ArticleInput, ApiError> {
    let title = req_dict(body, "title")?;
    let article_body = req_dict(body, "body")?;
    let slug = req_str_raw(body, "slug", 1, 200)?;
    validate_article_slug(&slug)?;
    let seo_title = opt_str_raw_max(body, "seo_title", 200)?;
    let seo_description = opt_str_raw_max(body, "seo_description", 500)?;
    let category_id = opt_uuid(body, "category_id")?;
    let author_name = opt_str_raw_max(body, "author_name", 160)?;
    let featured_image_url = opt_str_raw_max(body, "featured_image_url", 1024)?;
    Ok(ArticleInput {
        title,
        body: article_body,
        slug,
        seo_title,
        seo_description,
        category_id,
        author_name,
        featured_image_url,
    })
}

/// `ArticleOut` — the admin shape, in the Python field order.
fn article_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let title: Value = row.try_get("title")?;
    let body: Value = row.try_get("body")?;
    let slug: String = row.try_get("slug")?;
    let seo_title: Option<String> = row.try_get("seo_title")?;
    let seo_description: Option<String> = row.try_get("seo_description")?;
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
        "body": body,
        "slug": slug,
        "seo_title": seo_title,
        "seo_description": seo_description,
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

/// `PublicArticleOut`, in the Python field order.
fn public_dict(
    row: &DynRow,
    cats: &std::collections::HashMap<Uuid, Value>,
) -> Result<Value, ApiError> {
    let slug: String = row.try_get("slug")?;
    let title: Value = row.try_get("title")?;
    let category_id: Option<Uuid> = row.try_get("category_id")?;
    let author_name: Option<String> = row.try_get("author_name")?;
    let featured_image_url: Option<String> = row.try_get("featured_image_url")?;
    let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
    let view_count: Option<i32> = row.try_get("view_count")?;
    let share_count: Option<i32> = row.try_get("share_count")?;
    let cat = category_id.and_then(|id| cats.get(&id));
    Ok(json!({
        "slug": slug,
        "title": title,
        "category": category_dict(cat),
        "author_name": author_name,
        "featured_image_url": featured_image_url,
        "published_at": published_at,
        "view_count": view_count.unwrap_or(0),
        "share_count": share_count.unwrap_or(0),
    }))
}

/// Batch-load the categories referenced by `rows` (one query).
async fn load_categories(
    db: &Db,
    rows: &[DynRow],
) -> Result<std::collections::HashMap<Uuid, Value>, ApiError> {
    let mut ids: Vec<Uuid> = Vec::new();
    for row in rows {
        let id: Option<Uuid> = row.try_get("category_id")?;
        if let Some(id) = id {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    let mut map = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(map);
    }
    let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT id, slug, title FROM article_categories WHERE id IN ({})",
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

/// Shared list filters (`_apply_article_filters`): `q` searches the
/// JSONB text cast of title and body (case-insensitive); `category`
/// is a slug (`"__none"` = uncategorized, unknown slug matches
/// nothing); `status` is an exact, unvalidated workflow status;
/// `exclude` is a slug. Empty strings behave like absent, except
/// `exclude=""` which Python applies (`slug != ''` — matches every
/// real slug, so equivalent to absent).
fn article_filters(
    q: &Option<String>,
    category: &Option<String>,
    status: &Option<String>,
    exclude: &Option<String>,
) -> (Vec<String>, Vec<Arg>) {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(q) = q {
        let needle = format!("%{}%", q.to_lowercase());
        conds.push(
            "(LOWER(CAST(title AS TEXT)) LIKE ? OR LOWER(CAST(body AS TEXT)) LIKE ?)".to_string(),
        );
        args.push(needle.clone().into_arg());
        args.push(needle.into_arg());
    }
    if let Some(cat) = category {
        if cat == "__none" {
            conds.push("category_id IS NULL".to_string());
        } else {
            conds.push(
                "category_id IN (SELECT id FROM article_categories WHERE slug = ?)".to_string(),
            );
            args.push(cat.clone().into_arg());
        }
    }
    if let Some(s) = status {
        conds.push("workflow_status = ?".to_string());
        args.push(s.clone().into_arg());
    }
    if let Some(e) = exclude {
        conds.push("slug != ?".to_string());
        args.push(e.clone().into_arg());
    }
    (conds, args)
}

fn pages_of(total: i64, per_page: i64) -> i64 {
    if total == 0 {
        0
    } else {
        (total + per_page - 1) / per_page
    }
}

async fn fetch_article_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        &format!("SELECT {ARTICLE_COLS} FROM articles WHERE id = ?"),
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(not_found)
}

async fn fetch_published_by_slug(db: &Db, slug: &str) -> Result<DynRow, ApiError> {
    let row = db
        .fetch_optional(
            &format!("SELECT {ARTICLE_COLS} FROM articles WHERE slug = ?"),
            &[slug.to_string().into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(not_found());
    };
    let status: String = row.try_get("workflow_status")?;
    if status != "published" {
        return Err(not_found());
    }
    Ok(row)
}

// --- public ---------------------------------------------------------------

/// `GET /api/v1/articles` — published only.
pub async fn public_list(
    State(state): State<AppState>,
    uri: axum::http::Uri,
) -> ApiResult<Json<Value>> {
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
    let exclude = params.get("exclude").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut conds = vec!["workflow_status = 'published'".to_string()];
    let (mut fconds, args) = article_filters(&q, &category, &None, &exclude);
    conds.append(&mut fconds);
    let where_sql = format!(" WHERE {}", conds.join(" AND "));
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS total FROM articles{where_sql}"),
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
                "SELECT {ARTICLE_COLS} FROM articles{where_sql} ORDER BY {order} LIMIT ? OFFSET ?"
            ),
            &list_args,
        )
        .await?;
    let cats = load_categories(db, &rows).await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(public_dict(row, &cats)?);
    }
    Ok(Json(json!({
        "items": items,
        "total": total,
        "page": page,
        "per_page": per_page,
        "pages": pages_of(total, per_page),
    })))
}

/// `GET /api/v1/articles/categories` — one row per category (slug
/// ASC, zero counts included) + a trailing `"__none"` row when any
/// published article is uncategorized.
pub async fn public_categories(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let counts: std::collections::HashMap<String, i64> = db
        .fetch_all(
            "SELECT category_id, COUNT(*) AS c FROM articles WHERE workflow_status = 'published' AND category_id IS NOT NULL GROUP BY category_id",
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
            "SELECT id, slug, title, created_at FROM article_categories ORDER BY slug ASC",
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
            "SELECT COUNT(*) AS total FROM articles WHERE workflow_status = 'published' AND category_id IS NULL",
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

/// `GET /api/v1/articles/{slug}/meta` — no view counted.
pub async fn public_meta(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let row = fetch_published_by_slug(db, &slug).await?;
    let cats = load_categories(db, std::slice::from_ref(&row)).await?;
    let category_id: Option<Uuid> = row.try_get("category_id")?;
    let cat = category_id.and_then(|id| cats.get(&id));
    let title: Value = row.try_get("title")?;
    let seo_title: Option<String> = row.try_get("seo_title")?;
    let seo_description: Option<String> = row.try_get("seo_description")?;
    let featured_image_url: Option<String> = row.try_get("featured_image_url")?;
    Ok(Json(json!({
        "slug": slug,
        "title": title,
        "seo_title": seo_title,
        "seo_description": seo_description,
        "featured_image_url": featured_image_url,
        "category": category_dict(cat),
    })))
}

/// `GET /api/v1/articles/{slug}` — detail; every call counts a view.
pub async fn public_detail(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let row = fetch_published_by_slug(db, &slug).await?;
    let id: Uuid = row.try_get("id")?;
    db.execute(
        "UPDATE articles SET view_count = COALESCE(view_count, 0) + 1 WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    let row = fetch_article_by_id(db, id).await?;
    let cats = load_categories(db, std::slice::from_ref(&row)).await?;
    let mut out = public_dict(&row, &cats)?;
    let body: Value = row.try_get("body")?;
    out.as_object_mut()
        .expect("public_dict returns an object")
        .insert("body".to_string(), body);
    Ok(Json(out))
}

/// `POST /api/v1/articles/{slug}/share` — unconditional increment.
pub async fn public_share(
    State(state): State<AppState>,
    Path(slug): Path<String>,
) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let row = fetch_published_by_slug(db, &slug).await?;
    let id: Uuid = row.try_get("id")?;
    db.execute(
        "UPDATE articles SET share_count = COALESCE(share_count, 0) + 1 WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    let row = fetch_article_by_id(db, id).await?;
    let share_count: Option<i64> = row.try_get("share_count")?;
    Ok(Json(json!({
        "slug": slug,
        "share_count": share_count.unwrap_or(0),
    })))
}

// --- admin ----------------------------------------------------------------

/// `GET /api/v1/admin/articles` — all statuses, `knowledge.manage`.
pub async fn admin_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: axum::http::Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let params = parse_query(&uri);
    let page = query_int(&params, "page", 1, 1, i64::MAX)?;
    let per_page = query_int(&params, "per_page", 50, 1, 100)?;
    let q = params.get("q").filter(|s| !s.is_empty()).cloned();
    let category = params.get("category").filter(|s| !s.is_empty()).cloned();
    let status = params.get("status").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let (conds, args) = article_filters(&q, &category, &status, &None);
    let where_sql = if conds.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conds.join(" AND "))
    };
    let total: i64 = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS total FROM articles{where_sql}"),
            &args,
        )
        .await?
        .try_get("total")?;
    let mut list_args = args.clone();
    list_args.push(per_page.into_arg());
    list_args.push(((page - 1).saturating_mul(per_page)).into_arg());
    let rows = db
        .fetch_all(
            &format!("SELECT {ARTICLE_COLS} FROM articles{where_sql} ORDER BY created_at DESC LIMIT ? OFFSET ?"),
            &list_args,
        )
        .await?;
    let cats = load_categories(db, &rows).await?;
    let mut items = Vec::new();
    for row in &rows {
        let mut d = public_dict(row, &cats)?;
        let id: Uuid = row.try_get("id")?;
        let body: Value = row.try_get("body")?;
        let seo_title: Option<String> = row.try_get("seo_title")?;
        let seo_description: Option<String> = row.try_get("seo_description")?;
        let workflow_status: String = row.try_get("workflow_status")?;
        let obj = d.as_object_mut().expect("public_dict returns an object");
        obj.insert("id".to_string(), json!(id.to_string()));
        obj.insert("body".to_string(), body);
        obj.insert("seo_title".to_string(), json!(seo_title));
        obj.insert("seo_description".to_string(), json!(seo_description));
        obj.insert("workflow_status".to_string(), json!(workflow_status));
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

async fn check_category_exists(db: &Db, category_id: Uuid) -> Result<(), ApiError> {
    let exists = db
        .fetch_optional(
            "SELECT id FROM article_categories WHERE id = ?",
            &[category_id.into_arg()],
        )
        .await?;
    if exists.is_none() {
        return Err(ApiError::not_found("category_not_found", "دسته یافت نشد"));
    }
    Ok(())
}

async fn check_slug_free(db: &Db, slug: &str, exclude_id: Option<Uuid>) -> Result<(), ApiError> {
    let mut sql = "SELECT id FROM articles WHERE slug = ?".to_string();
    let mut args: Vec<Arg> = vec![slug.to_string().into_arg()];
    if let Some(id) = exclude_id {
        sql.push_str(" AND id != ?");
        args.push(id.into_arg());
    }
    let dup = db.fetch_optional(&sql, &args).await?;
    if dup.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "article_slug_exists",
            "مقاله‌ای با این نامک هست",
        ));
    }
    Ok(())
}

/// `POST /api/v1/admin/articles` — `knowledge.manage`, 201.
/// Category check BEFORE slug check; `workflow_status` forced `draft`.
pub async fn admin_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let input = validate_article_input(&parsed)?;
    let db = state.db_or_500()?;
    if let Some(cat_id) = input.category_id {
        check_category_exists(db, cat_id).await?;
    }
    check_slug_free(db, &input.slug, None).await?;
    let id = Uuid::new_v4();
    let now = chrono::Utc::now();
    db.execute(
        "INSERT INTO articles (id, title, body, slug, seo_title, seo_description, workflow_status, category_id, author_name, featured_image_url, view_count, share_count, published_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'draft', ?, ?, ?, 0, 0, NULL, ?, ?)",
        &[
            id.into_arg(),
            input.title.into_arg(),
            input.body.into_arg(),
            input.slug.into_arg(),
            input.seo_title.into_arg(),
            input.seo_description.into_arg(),
            input.category_id.into_arg(),
            input.author_name.into_arg(),
            input.featured_image_url.into_arg(),
            now.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let row = fetch_article_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(article_out(&row)?)))
}

/// `PUT /api/v1/admin/articles/{article_id}` — full overwrite;
/// does NOT touch `workflow_status` or `published_at`.
pub async fn admin_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(article_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&article_id, "article_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let input = validate_article_input(&parsed)?;
    let db = state.db_or_500()?;
    fetch_article_by_id(db, id).await?;
    if let Some(cat_id) = input.category_id {
        check_category_exists(db, cat_id).await?;
    }
    check_slug_free(db, &input.slug, Some(id)).await?;
    let now = chrono::Utc::now();
    db.execute(
        "UPDATE articles SET title = ?, body = ?, slug = ?, seo_title = ?, seo_description = ?, category_id = ?, author_name = ?, featured_image_url = ?, updated_at = ? WHERE id = ?",
        &[
            input.title.into_arg(),
            input.body.into_arg(),
            input.slug.into_arg(),
            input.seo_title.into_arg(),
            input.seo_description.into_arg(),
            input.category_id.into_arg(),
            input.author_name.into_arg(),
            input.featured_image_url.into_arg(),
            now.into_arg(),
            id.into_arg(),
        ],
    )
    .await?;
    let row = fetch_article_by_id(db, id).await?;
    Ok(Json(article_out(&row)?))
}

/// `PATCH /api/v1/admin/articles/{article_id}/status` —
/// `fatwa.approve`. On entering `published`, `published_at = now`
/// (set at entry, overwritten on re-publish, never cleared).
pub async fn admin_set_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(article_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&article_id, "article_id")?;
    let parsed = parse_body(&body)?;
    let target = match parsed.get("status") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("status", "field is required")),
    };
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "fatwa.approve").await?;
    let db = state.db_or_500()?;
    let row = fetch_article_by_id(db, id).await?;
    let current: String = row.try_get("workflow_status")?;
    validate_transition(&current, &target)?;
    let now = chrono::Utc::now();
    if target == "published" {
        db.execute(
            "UPDATE articles SET workflow_status = ?, published_at = ?, updated_at = ? WHERE id = ?",
            &[target.into_arg(), now.into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE articles SET workflow_status = ?, updated_at = ? WHERE id = ?",
            &[target.into_arg(), now.into_arg(), id.into_arg()],
        )
        .await?;
    }
    let row = fetch_article_by_id(db, id).await?;
    Ok(Json(article_out(&row)?))
}

// --- admin: categories ------------------------------------------------------

fn category_out(row: &DynRow) -> Result<Value, ApiError> {
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

struct CategoryInput {
    slug: String,
    title: Value,
}

fn validate_category_input(body: &Value) -> Result<CategoryInput, ApiError> {
    let slug = req_str_raw(body, "slug", 1, 80)?;
    validate_category_slug(&slug)?;
    let title = req_dict(body, "title")?;
    Ok(CategoryInput { slug, title })
}

async fn fetch_category_by_id(db: &Db, id: Uuid) -> Result<DynRow, ApiError> {
    db.fetch_optional(
        "SELECT id, slug, title, created_at FROM article_categories WHERE id = ?",
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("category_not_found", "دسته یافت نشد"))
}

/// `GET /api/v1/admin/articles/categories` — slug ASC, no counts.
pub async fn admin_category_list(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, slug, title, created_at FROM article_categories ORDER BY slug ASC",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for row in &rows {
        out.push(category_out(row)?);
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/articles/categories` — 201.
pub async fn admin_category_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let input = validate_category_input(&parsed)?;
    let db = state.db_or_500()?;
    let dup = db
        .fetch_optional(
            "SELECT id FROM article_categories WHERE slug = ?",
            &[input.slug.clone().into_arg()],
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
    let now = chrono::Utc::now();
    db.execute(
        "INSERT INTO article_categories (id, slug, title, created_at) VALUES (?, ?, ?, ?)",
        &[
            id.into_arg(),
            input.slug.into_arg(),
            input.title.into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let row = fetch_category_by_id(db, id).await?;
    Ok((StatusCode::CREATED, Json(category_out(&row)?)))
}

/// `PUT /api/v1/admin/articles/categories/{category_id}` — no dup
/// check when the slug is unchanged.
pub async fn admin_category_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(category_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&category_id, "category_id")?;
    let parsed = parse_body(&body)?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let input = validate_category_input(&parsed)?;
    let db = state.db_or_500()?;
    let row = fetch_category_by_id(db, id).await?;
    let current_slug: String = row.try_get("slug")?;
    if input.slug != current_slug {
        let dup = db
            .fetch_optional(
                "SELECT id FROM article_categories WHERE slug = ?",
                &[input.slug.clone().into_arg()],
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
        "UPDATE article_categories SET slug = ?, title = ? WHERE id = ?",
        &[input.slug.into_arg(), input.title.into_arg(), id.into_arg()],
    )
    .await?;
    let row = fetch_category_by_id(db, id).await?;
    Ok(Json(category_out(&row)?))
}

/// `DELETE /api/v1/admin/articles/categories/{category_id}` — 204;
/// blocked while any article uses the category.
pub async fn admin_category_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(category_id): Path<String>,
) -> ApiResult<StatusCode> {
    let id = parse_path_uuid(&category_id, "category_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/admin/articles").await?;
    require_permission(&state, &ctx, "knowledge.manage").await?;
    let db = state.db_or_500()?;
    fetch_category_by_id(db, id).await?;
    let used: i64 = db
        .fetch_one(
            "SELECT COUNT(*) AS total FROM articles WHERE category_id = ?",
            &[id.into_arg()],
        )
        .await?
        .try_get("total")?;
    if used > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "category_in_use",
            "این دسته روی مقاله‌هایی استفاده شده است؛ اول دستهٔ آن‌ها را عوض کنید",
        ));
    }
    db.execute(
        "DELETE FROM article_categories WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}
