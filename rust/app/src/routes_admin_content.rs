//! Admin Content Center — mirrors
//! `app/modules/admin/{content_center_router, content_center_service,
//! control_registry, control_schemas}.py` (phase 14).
//!
//! One inbox over every content domain (§9.5): the adapter registry is
//! a static table (content ×5, fatwa ×2, library, lms, places, social),
//! each adapter mapping one (domain, content_type) pair onto its own
//! table with the shared §9 workflow machine. No generic content table
//! exists, in Python or here.
//!
//! Notes on fidelity (see the ACC drafts in the wave report):
//! - The registry is static and complete: Python registers the same
//!   eleven adapters at import time, so the list is identical.
//! - `GET /items` merges per-adapter lists, sorts by
//!   (-created_at, domain, content_type, id) and pages with the
//!   `{"t","d","ct","id"}` cursor, exactly like `list_items`.
//! - Adapter transitions reuse each domain's own transition (CMS,
//!   fatwa submissions, library, places) including its own domain
//!   event + audit; the center adds its own `content.bulk.transition`
//!   / `content.transition` audit on top, like Python.
//! - Telegram distribution hooks are skipped (ADR-025): transitions
//!   never notify Telegram from the Rust binary.
//! - The AI reindex trigger only marks the ingestion run `queued`
//!   with `enqueued: false` (ADR-022 precedent); no ARQ job exists.
//! - `GET /distribution` never 404s on a missing item — only an
//!   unknown adapter 404s — exactly like Python's `get_distribution`.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    db::{new_id, utc_now, Arg, Db, DynRow, IntoArg},
    embeddings::AI_SCOPES,
    errors::{ApiError, ApiResult},
    validate::{
        as_bool, as_int, opt_datetime, parse_body, parse_datetime_str, parse_path_uuid,
        parse_query, query_int,
    },
    workflow::{allowed_next, validate_transition, STATUSES},
    AppState,
};

/// The seven distribution channels (§9.5).
const CHANNELS: &[&str] = &[
    "web",
    "mobile",
    "desktop",
    "telegram",
    "ai_knowledge",
    "search",
    "embed",
];

const BULK_ACTIONS: &[&str] = &[
    "transition",
    "schedule",
    "placement",
    "distribution",
    "export",
];

/// `{"web": true, "mobile": false, "desktop": false, "telegram": false,
/// "ai_knowledge": false}` — the publish-time distribution default
/// (`DEFAULT_DISTRIBUTION` in `cms_service.py`).
fn default_distribution() -> Value {
    json!({
        "web": true,
        "mobile": false,
        "desktop": false,
        "telegram": false,
        "ai_knowledge": false,
    })
}

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn bad_datetime(value: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad_datetime",
        format!("قالب تاریخ نامعتبر: {value}"),
    )
}

fn adapter_not_found(domain: &str, content_type: &str) -> ApiError {
    ApiError::not_found(
        "adapter_not_found",
        format!("آداپتر محتوا یافت نشد: {domain}/{content_type}"),
    )
}

/// All center endpoints require `content.bulk`.
async fn admin_ctx(state: &AppState, headers: &HeaderMap) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, "/api/v1/admin/content").await?;
    require_permission(state, &ctx, "content.bulk").await?;
    Ok(ctx)
}

fn client_ip_ua(
    headers: &HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    state: &AppState,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer.0), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

/// `_parse_dt` (router): empty → None, unparsable → 422 `bad_datetime`.
fn parse_dt_opt(value: Option<&str>) -> Result<Option<DateTime<Utc>>, ApiError> {
    match value {
        None | Some("") => Ok(None),
        Some(s) => parse_datetime_str(s)
            .map(Some)
            .ok_or_else(|| bad_datetime(s)),
    }
}

/// `_parse_dt` (service, JSON params): a value must be an ISO string;
/// anything else → 422 `bad_datetime` (Python: `fromisoformat(str(v))`).
fn parse_dt_value(value: &Value) -> Result<Option<DateTime<Utc>>, ApiError> {
    match value {
        Value::Null => Ok(None),
        Value::String(s) => parse_datetime_str(s)
            .map(Some)
            .ok_or_else(|| bad_datetime(s)),
        other => Err(bad_datetime(&other.to_string())),
    }
}

/// Python `bool(x)` truthiness for JSON values.
fn py_bool(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(false),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(m) => !m.is_empty(),
    }
}

/// Python `int(x)` for JSON values (truncates floats, parses digit
/// strings, `True` → 1).
fn py_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f.trunc() as i64)),
        Value::String(s) => {
            let t = s.trim();
            t.parse::<i64>()
                .ok()
                .or_else(|| t.parse::<f64>().ok().map(|f| f.trunc() as i64))
        }
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// `_lt_text` — extract text from a `LocalizedText` (`fa` → `ar` →
/// `en` → first truthy value), or the plain string itself.
fn lt_text(v: &Value) -> String {
    if let Some(obj) = v.as_object() {
        for loc in ["fa", "ar", "en"] {
            if let Some(s) = obj.get(loc).and_then(|x| x.as_str()) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        for (_, val) in obj {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        return String::new();
    }
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Python `str[:n]` on characters, not bytes.
fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn dt_json(v: Option<DateTime<Utc>>) -> Value {
    match v {
        Some(dt) => Value::String(dt.to_rfc3339()),
        None => Value::Null,
    }
}

/// Python `str(v)` for audit old/new values: `None` → `"None"`,
/// `True`/`False`, datetimes as `"YYYY-MM-DD HH:MM:SS+00:00"`.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".to_string(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn py_str_dt(v: Option<DateTime<Utc>>) -> String {
    match v {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S+00:00").to_string(),
        None => "None".to_string(),
    }
}

async fn has_permission(db: &Db, user_id: Uuid, key: &str) -> ApiResult<bool> {
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

// --- adapters -------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum AdapterKind {
    CmsEvent,
    CmsQuote,
    CmsPage,
    CmsBanner,
    CmsAnnouncement,
    FatwaSubmission,
    KnowledgeChunk,
    LibraryItem,
    Course,
    SocialPost,
    ReligiousPlace,
}

struct Adapter {
    domain: &'static str,
    content_type: &'static str,
    label: &'static str,
    audit_entity_type: &'static str,
    kind: AdapterKind,
}

impl Adapter {
    /// `supports_transition` — `transition_fn is not None`.
    fn supports_transition(&self) -> bool {
        matches!(
            self.kind,
            AdapterKind::CmsEvent
                | AdapterKind::CmsQuote
                | AdapterKind::CmsPage
                | AdapterKind::CmsBanner
                | AdapterKind::CmsAnnouncement
                | AdapterKind::FatwaSubmission
                | AdapterKind::LibraryItem
                | AdapterKind::ReligiousPlace
        )
    }

    /// The CMS resource name (`events`, `quotes`, …) for the five CMS
    /// adapters; `None` elsewhere.
    fn resource(&self) -> Option<&'static str> {
        match self.kind {
            AdapterKind::CmsEvent => Some("events"),
            AdapterKind::CmsQuote => Some("quotes"),
            AdapterKind::CmsPage => Some("pages"),
            AdapterKind::CmsBanner => Some("banners"),
            AdapterKind::CmsAnnouncement => Some("announcements"),
            _ => None,
        }
    }

    fn table(&self) -> &'static str {
        match self.kind {
            AdapterKind::CmsEvent => "islamic_events",
            AdapterKind::CmsQuote => "scholar_quotes",
            AdapterKind::CmsPage => "pages",
            AdapterKind::CmsBanner => "banners",
            AdapterKind::CmsAnnouncement => "announcements",
            AdapterKind::FatwaSubmission => "fatwa_submissions",
            AdapterKind::KnowledgeChunk => "fiqh_knowledge_chunks",
            AdapterKind::LibraryItem => "library_items",
            AdapterKind::Course => "courses",
            AdapterKind::SocialPost => "social_posts",
            AdapterKind::ReligiousPlace => "religious_places",
        }
    }

    /// The title column (JSONB `LocalizedText` for CMS/library/course/
    /// places, plain text elsewhere).
    fn title_col(&self) -> &'static str {
        match self.kind {
            AdapterKind::CmsEvent => "name",
            AdapterKind::CmsQuote => "text",
            AdapterKind::CmsPage => "title",
            AdapterKind::CmsBanner => "title",
            AdapterKind::CmsAnnouncement => "message",
            AdapterKind::FatwaSubmission => "title",
            AdapterKind::KnowledgeChunk => "chunk_text",
            AdapterKind::LibraryItem => "title",
            AdapterKind::Course => "title",
            AdapterKind::SocialPost => "body",
            AdapterKind::ReligiousPlace => "name",
        }
    }

    /// Columns the `_like` search runs over.
    fn search_cols(&self) -> &'static [&'static str] {
        match self.kind {
            AdapterKind::CmsEvent => &["name"],
            AdapterKind::CmsQuote => &["text"],
            AdapterKind::CmsPage => &["title"],
            AdapterKind::CmsBanner => &["title"],
            AdapterKind::CmsAnnouncement => &["message"],
            AdapterKind::FatwaSubmission => &["title", "body"],
            AdapterKind::KnowledgeChunk => &["chunk_text"],
            AdapterKind::LibraryItem => &["title", "transcript"],
            AdapterKind::Course => &["title"],
            AdapterKind::SocialPost => &["body"],
            AdapterKind::ReligiousPlace => &["name"],
        }
    }
}

/// The registry, sorted by (domain, content_type) like Python's
/// `sorted(_REGISTRY.items())`.
fn adapters() -> &'static [Adapter] {
    const ALL: &[Adapter] = &[
        Adapter {
            domain: "content",
            content_type: "announcement",
            label: "اعلان",
            audit_entity_type: "announcement",
            kind: AdapterKind::CmsAnnouncement,
        },
        Adapter {
            domain: "content",
            content_type: "banner",
            label: "بنر",
            audit_entity_type: "banner",
            kind: AdapterKind::CmsBanner,
        },
        Adapter {
            domain: "content",
            content_type: "islamic_event",
            label: "رویداد اسلامی",
            audit_entity_type: "islamic_event",
            kind: AdapterKind::CmsEvent,
        },
        Adapter {
            domain: "content",
            content_type: "page",
            label: "صفحه",
            audit_entity_type: "page",
            kind: AdapterKind::CmsPage,
        },
        Adapter {
            domain: "content",
            content_type: "scholar_quote",
            label: "نقل‌قول علما",
            audit_entity_type: "scholar_quote",
            kind: AdapterKind::CmsQuote,
        },
        Adapter {
            domain: "fatwa",
            content_type: "fatwa_submission",
            label: "فتوای رسمی",
            audit_entity_type: "fatwa_submission",
            kind: AdapterKind::FatwaSubmission,
        },
        Adapter {
            domain: "fatwa",
            content_type: "knowledge_chunk",
            label: "chunk دانش فقهی",
            audit_entity_type: "fiqh_knowledge_chunk",
            kind: AdapterKind::KnowledgeChunk,
        },
        Adapter {
            domain: "library",
            content_type: "library_item",
            label: "آیتم کتابخانه",
            audit_entity_type: "library_item",
            kind: AdapterKind::LibraryItem,
        },
        Adapter {
            domain: "lms",
            content_type: "course",
            label: "دورهٔ آموزشی",
            audit_entity_type: "course",
            kind: AdapterKind::Course,
        },
        Adapter {
            domain: "places",
            content_type: "religious_place",
            label: "مکان مذهبی",
            audit_entity_type: "religious_place",
            kind: AdapterKind::ReligiousPlace,
        },
        Adapter {
            domain: "social",
            content_type: "social_post",
            label: "پست اجتماعی",
            audit_entity_type: "social_post",
            kind: AdapterKind::SocialPost,
        },
    ];
    ALL
}

fn get_adapter(domain: &str, content_type: &str) -> Result<&'static Adapter, ApiError> {
    adapters()
        .iter()
        .find(|a| a.domain == domain && a.content_type == content_type)
        .ok_or_else(|| adapter_not_found(domain, content_type))
}

fn list_adapters_filtered(
    domain: Option<&str>,
    content_type: Option<&str>,
) -> Vec<&'static Adapter> {
    adapters()
        .iter()
        .filter(|a| {
            (domain.is_none() || domain == Some(a.domain))
                && (content_type.is_none() || content_type == Some(a.content_type))
        })
        .collect()
}

// --- unified item ---------------------------------------------------------

/// One row from any adapter, in the `UnifiedItem` shape of
/// `control_registry.py`.
struct UnifiedItem {
    domain: String,
    content_type: String,
    id: String,
    title: String,
    excerpt: String,
    status: String,
    author_id: Option<String>,
    visibility: Option<String>,
    ai_scopes: Vec<String>,
    version: Option<i64>,
    created_at: Option<DateTime<Utc>>,
    updated_at: Option<DateTime<Utc>>,
    published_at: Option<DateTime<Utc>>,
    /// Native `distribution` JSONB (CMS adapters only).
    distribution: Option<Value>,
    /// `seo_meta` (pages only).
    seo_meta: Option<Value>,
}

struct ListFilters {
    q: Option<String>,
    status: Option<String>,
    author_id: Option<String>,
    visibility: Option<String>,
    ai_scope: Option<String>,
    date_from: Option<DateTime<Utc>>,
    date_to: Option<DateTime<Utc>>,
    cursor_ts: Option<DateTime<Utc>>,
    limit: i64,
}

/// Escape `\`, `%`, `_` for a `LIKE … ESCAPE '\'` pattern
/// (`_like` in `control_registry.py`).
fn escape_like(q: &str) -> String {
    let mut out = String::with_capacity(q.len());
    for c in q.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '%' => out.push_str("\\%"),
            '_' => out.push_str("\\_"),
            _ => out.push(c),
        }
    }
    out
}

/// Columns selected for one adapter's list/get queries.
fn select_cols(adapter: &Adapter) -> String {
    match adapter.kind {
        AdapterKind::CmsEvent
        | AdapterKind::CmsQuote
        | AdapterKind::CmsBanner
        | AdapterKind::CmsAnnouncement => format!(
            "id, {}, workflow_status, version, created_by, created_at, updated_at, distribution",
            adapter.title_col()
        ),
        AdapterKind::CmsPage => {
            "id, title, workflow_status, version, created_by, created_at, updated_at, \
             distribution, published_at, seo_meta"
                .to_string()
        }
        AdapterKind::FatwaSubmission => {
            "id, title, body, author_id, workflow_status, version, published_at, created_at, \
             updated_at"
                .to_string()
        }
        AdapterKind::KnowledgeChunk => {
            "id, chunk_text, workflow_status, approved_by, ai_scopes, created_at, updated_at"
                .to_string()
        }
        AdapterKind::LibraryItem => {
            "id, title, description, workflow_status, version, created_by, visibility, \
             ai_scopes, published_at, created_at, updated_at"
                .to_string()
        }
        AdapterKind::Course => {
            "id, title, description, visibility, is_active, created_at".to_string()
        }
        AdapterKind::SocialPost => {
            "id, body, status, author_id, created_at, updated_at".to_string()
        }
        AdapterKind::ReligiousPlace => {
            "id, name, description, workflow_status, version, created_by, created_at, updated_at"
                .to_string()
        }
    }
}

fn item_from_row(adapter: &Adapter, row: &DynRow) -> ApiResult<UnifiedItem> {
    let id: Uuid = row.try_get("id")?;
    let id = id.to_string();
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let mut item = UnifiedItem {
        domain: adapter.domain.to_string(),
        content_type: adapter.content_type.to_string(),
        id,
        title: String::new(),
        excerpt: String::new(),
        status: String::new(),
        author_id: None,
        visibility: None,
        ai_scopes: Vec::new(),
        version: None,
        created_at: Some(created_at),
        updated_at: None,
        published_at: None,
        distribution: None,
        seo_meta: None,
    };
    match adapter.kind {
        AdapterKind::CmsEvent
        | AdapterKind::CmsQuote
        | AdapterKind::CmsPage
        | AdapterKind::CmsBanner
        | AdapterKind::CmsAnnouncement => {
            let title_raw: Value = row.try_get(adapter.title_col())?;
            item.title = lt_text(&title_raw);
            item.excerpt = String::new();
            item.status = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            item.version = Some(i64::from(version));
            let created_by: Option<Uuid> = row.try_get("created_by")?;
            item.author_id = created_by.map(|u| u.to_string());
            item.updated_at = row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?;
            item.distribution = row.try_get::<Option<Value>, _>("distribution")?;
            if adapter.kind == AdapterKind::CmsPage {
                item.published_at = row.try_get::<Option<DateTime<Utc>>, _>("published_at")?;
                item.seo_meta = row.try_get::<Option<Value>, _>("seo_meta")?;
            }
        }
        AdapterKind::FatwaSubmission => {
            let title_raw: Value = row.try_get("title")?;
            item.title = lt_text(&title_raw);
            let body: String = row.try_get("body")?;
            item.excerpt = truncate(&body, 300);
            item.status = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            item.version = Some(i64::from(version));
            let author_id: Uuid = row.try_get("author_id")?;
            item.author_id = Some(author_id.to_string());
            item.updated_at = row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?;
            item.published_at = row.try_get::<Option<DateTime<Utc>>, _>("published_at")?;
        }
        AdapterKind::KnowledgeChunk => {
            let text: String = row.try_get("chunk_text")?;
            item.title = truncate(&text, 120);
            item.excerpt = truncate(&text, 300);
            item.status = row.try_get("workflow_status")?;
            let approved_by: Option<Uuid> = row.try_get("approved_by")?;
            item.author_id = approved_by.map(|u| u.to_string());
            let scopes: Option<Value> = row.try_get("ai_scopes")?;
            item.ai_scopes = scopes
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                })
                .unwrap_or_default();
            item.updated_at = row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?;
        }
        AdapterKind::LibraryItem => {
            let title_raw: Value = row.try_get("title")?;
            item.title = lt_text(&title_raw);
            let desc: Option<Value> = row.try_get("description")?;
            item.excerpt = truncate(&desc.map(|v| lt_text(&v)).unwrap_or_default(), 300);
            item.status = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            item.version = Some(i64::from(version));
            let created_by: Option<Uuid> = row.try_get("created_by")?;
            item.author_id = created_by.map(|u| u.to_string());
            item.visibility = row.try_get::<Option<String>, _>("visibility")?;
            let scopes: Option<Value> = row.try_get("ai_scopes")?;
            item.ai_scopes = scopes
                .and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                })
                .unwrap_or_default();
            item.updated_at = row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?;
            item.published_at = row.try_get::<Option<DateTime<Utc>>, _>("published_at")?;
        }
        AdapterKind::Course => {
            let title_raw: Value = row.try_get("title")?;
            item.title = lt_text(&title_raw);
            let desc: Option<Value> = row.try_get("description")?;
            item.excerpt = truncate(&desc.map(|v| lt_text(&v)).unwrap_or_default(), 300);
            let is_active: bool = row.try_get("is_active")?;
            item.status = if is_active {
                "published".to_string()
            } else {
                "archived".to_string()
            };
            item.visibility = row.try_get::<Option<String>, _>("visibility")?;
        }
        AdapterKind::SocialPost => {
            let body: String = row.try_get("body")?;
            item.title = truncate(&body, 120);
            item.excerpt = truncate(&body, 300);
            let raw: String = row.try_get("status")?;
            item.status = if raw == "published" {
                "published".to_string()
            } else {
                "archived".to_string()
            };
            let author_id: Uuid = row.try_get("author_id")?;
            item.author_id = Some(author_id.to_string());
            item.updated_at = row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?;
        }
        AdapterKind::ReligiousPlace => {
            let name_raw: Value = row.try_get("name")?;
            item.title = lt_text(&name_raw);
            let desc: Option<Value> = row.try_get("description")?;
            item.excerpt = truncate(&desc.map(|v| lt_text(&v)).unwrap_or_default(), 300);
            item.status = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            item.version = Some(i64::from(version));
            let created_by: Option<Uuid> = row.try_get("created_by")?;
            item.author_id = created_by.map(|u| u.to_string());
            item.updated_at = row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?;
        }
    }
    Ok(item)
}

/// One adapter's list (`list_fn`): filters, `created_at DESC, id ASC`,
/// `limit + 1` rows; the `ai_scope` filter is in-memory afterwards
/// (chunks + library only), like Python.
async fn list_adapter(db: &Db, adapter: &Adapter, f: &ListFilters) -> ApiResult<Vec<UnifiedItem>> {
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(q) = f.q.as_deref().filter(|s| !s.is_empty()) {
        let pattern = format!("%{}%", escape_like(&q.to_lowercase()));
        let parts: Vec<String> = adapter
            .search_cols()
            .iter()
            .map(|c| format!("LOWER(CAST({c} AS TEXT)) LIKE ? ESCAPE '\\'"))
            .collect();
        for _ in &parts {
            args.push(pattern.clone().into_arg());
        }
        conds.push(format!("({})", parts.join(" OR ")));
    }
    if let Some(status) = f.status.as_deref().filter(|s| !s.is_empty()) {
        match adapter.kind {
            AdapterKind::Course => match status {
                "published" => conds.push("is_active = TRUE".to_string()),
                "archived" => conds.push("is_active = FALSE".to_string()),
                _ => conds.push("1 = 0".to_string()),
            },
            AdapterKind::SocialPost => match status {
                "published" => conds.push("status = 'published'".to_string()),
                "archived" => conds.push("status = 'removed'".to_string()),
                _ => conds.push("1 = 0".to_string()),
            },
            _ => {
                conds.push("workflow_status = ?".to_string());
                args.push(status.to_string().into_arg());
            }
        }
    }
    if let Some(author_id) = f.author_id.as_deref().filter(|s| !s.is_empty()) {
        let author_col = match adapter.kind {
            AdapterKind::FatwaSubmission | AdapterKind::SocialPost => Some("author_id"),
            AdapterKind::KnowledgeChunk => Some("approved_by"),
            AdapterKind::Course => None,
            _ => Some("created_by"),
        };
        match author_col {
            None => conds.push("1 = 0".to_string()),
            Some(col) => match author_id.parse::<Uuid>() {
                Ok(u) => {
                    conds.push(format!("{col} = ?"));
                    args.push(u.into_arg());
                }
                Err(_) => conds.push("1 = 0".to_string()),
            },
        }
    }
    if let Some(visibility) = f.visibility.as_deref().filter(|s| !s.is_empty()) {
        match adapter.kind {
            AdapterKind::LibraryItem | AdapterKind::Course => {
                conds.push("visibility = ?".to_string());
                args.push(visibility.to_string().into_arg());
            }
            _ => {}
        }
    }
    if let Some(from) = f.date_from {
        conds.push("created_at >= ?".to_string());
        args.push(from.into_arg());
    }
    if let Some(to) = f.date_to {
        conds.push("created_at <= ?".to_string());
        args.push(to.into_arg());
    }
    if let Some(ts) = f.cursor_ts {
        conds.push("created_at <= ?".to_string());
        args.push(ts.into_arg());
    }
    let mut sql = format!("SELECT {} FROM {}", select_cols(adapter), adapter.table());
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id ASC LIMIT ?");
    args.push((f.limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    for row in &rows {
        items.push(item_from_row(adapter, row)?);
    }
    // ai_scope: in-memory, chunks + library only (`"none"` = no filter).
    if let Some(scope) = f.ai_scope.as_deref().filter(|s| !s.is_empty()) {
        if scope != "none"
            && matches!(
                adapter.kind,
                AdapterKind::KnowledgeChunk | AdapterKind::LibraryItem
            )
        {
            items.retain(|it| it.ai_scopes.iter().any(|s| s == scope));
        }
    }
    Ok(items)
}

/// One adapter's get (`get_fn`): `None` for a missing row or a
/// non-UUID id, like `_uuid_or_none` + `db.get`.
async fn get_adapter_item(
    db: &Db,
    adapter: &Adapter,
    item_id: &str,
) -> ApiResult<Option<UnifiedItem>> {
    let Ok(id) = item_id.parse::<Uuid>() else {
        return Ok(None);
    };
    let sql = format!(
        "SELECT {} FROM {} WHERE id = ?",
        select_cols(adapter),
        adapter.table()
    );
    let row = db.fetch_optional(&sql, &[id.into_arg()]).await?;
    match row {
        Some(r) => Ok(Some(item_from_row(adapter, &r)?)),
        None => Ok(None),
    }
}

/// `resolve_author_names` — one batched query.
async fn resolve_author_names(
    db: &Db,
    author_ids: &[Option<String>],
) -> ApiResult<HashMap<String, String>> {
    let mut ids: Vec<Uuid> = Vec::new();
    for a in author_ids.iter().flatten() {
        if let Ok(u) = a.parse::<Uuid>() {
            if !ids.contains(&u) {
                ids.push(u);
            }
        }
    }
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, full_name FROM users WHERE id IN ({placeholders})");
    let args: Vec<Arg> = ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&sql, &args).await? {
        let id: Uuid = row.try_get("id")?;
        let name: String = row.try_get("full_name")?;
        out.insert(id.to_string(), name);
    }
    Ok(out)
}

/// `ContentItemOut` for one merged item.
fn item_out(item: &UnifiedItem, author_name: Option<&str>) -> Value {
    let mut out = Map::new();
    out.insert("domain".to_string(), json!(item.domain));
    out.insert("content_type".to_string(), json!(item.content_type));
    out.insert("id".to_string(), json!(item.id));
    out.insert("title".to_string(), json!(item.title));
    out.insert("excerpt".to_string(), json!(item.excerpt));
    out.insert("status".to_string(), json!(item.status));
    out.insert(
        "author_id".to_string(),
        item.author_id
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    out.insert(
        "author_name".to_string(),
        author_name
            .map(|s| Value::String(s.to_string()))
            .unwrap_or(Value::Null),
    );
    out.insert(
        "visibility".to_string(),
        item.visibility
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    out.insert("ai_scopes".to_string(), json!(item.ai_scopes));
    out.insert(
        "version".to_string(),
        item.version.map(Value::from).unwrap_or(Value::Null),
    );
    out.insert("created_at".to_string(), dt_json(item.created_at));
    out.insert("updated_at".to_string(), dt_json(item.updated_at));
    out.insert("published_at".to_string(), dt_json(item.published_at));
    Value::Object(out)
}

// --- cursor -----------------------------------------------------------------

/// `_encode_cursor`: base64url of
/// `{"t": iso, "d": domain, "ct": content_type, "id": item_id}`.
fn encode_cursor(
    created_at: DateTime<Utc>,
    domain: &str,
    content_type: &str,
    item_id: &str,
) -> String {
    let mut raw = Map::new();
    raw.insert("t".to_string(), json!(created_at.to_rfc3339()));
    raw.insert("d".to_string(), json!(domain));
    raw.insert("ct".to_string(), json!(content_type));
    raw.insert("id".to_string(), json!(item_id));
    URL_SAFE.encode(
        serde_json::to_string(&Value::Object(raw))
            .unwrap_or_default()
            .as_bytes(),
    )
}

/// `_decode_cursor`: anything malformed → 400 `invalid_cursor`.
fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, String, String, String), ApiError> {
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_cursor",
            "کرسر صفحه‌بندی نامعتبر است",
        )
    };
    let bytes = URL_SAFE.decode(cursor.as_bytes()).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let parsed: Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    let ts = parsed
        .get("t")
        .and_then(|v| v.as_str())
        .and_then(parse_datetime_str)
        .ok_or_else(invalid)?;
    let d = parsed
        .get("d")
        .and_then(|v| v.as_str())
        .ok_or_else(invalid)?
        .to_string();
    let ct = parsed
        .get("ct")
        .and_then(|v| v.as_str())
        .ok_or_else(invalid)?
        .to_string();
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(invalid)?
        .to_string();
    Ok((ts, d, ct, id))
}

// --- placements -------------------------------------------------------------

const PLACEMENT_COLS: &str = "id, domain, content_type, content_id, surface, slot, sort_order, \
     audience, starts_at, ends_at, locale, active, created_at";

/// `_placement_out`.
fn placement_out(row: &DynRow) -> ApiResult<Value> {
    let id: Uuid = row.try_get("id")?;
    let domain: String = row.try_get("domain")?;
    let content_type: String = row.try_get("content_type")?;
    let content_id: String = row.try_get("content_id")?;
    let surface: String = row.try_get("surface")?;
    let slot: Option<String> = row.try_get("slot")?;
    let sort_order: i32 = row.try_get("sort_order")?;
    let audience: Option<Value> = row.try_get("audience")?;
    let starts_at: Option<DateTime<Utc>> = row.try_get("starts_at")?;
    let ends_at: Option<DateTime<Utc>> = row.try_get("ends_at")?;
    let locale: Option<String> = row.try_get("locale")?;
    let active: bool = row.try_get("active")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let mut out = Map::new();
    out.insert("id".to_string(), json!(id.to_string()));
    out.insert("domain".to_string(), json!(domain));
    out.insert("content_type".to_string(), json!(content_type));
    out.insert("content_id".to_string(), json!(content_id));
    out.insert("surface".to_string(), json!(surface));
    out.insert(
        "slot".to_string(),
        slot.map(Value::String).unwrap_or(Value::Null),
    );
    out.insert("sort_order".to_string(), json!(sort_order));
    out.insert("audience".to_string(), audience.unwrap_or(json!({})));
    out.insert("starts_at".to_string(), dt_json(starts_at));
    out.insert("ends_at".to_string(), dt_json(ends_at));
    out.insert(
        "locale".to_string(),
        locale.map(Value::String).unwrap_or(Value::Null),
    );
    out.insert("active".to_string(), json!(active));
    out.insert("created_at".to_string(), json!(created_at.to_rfc3339()));
    Ok(Value::Object(out))
}

async fn fetch_placement(db: &Db, id: Uuid) -> ApiResult<Option<DynRow>> {
    let sql = format!("SELECT {PLACEMENT_COLS} FROM content_placements WHERE id = ?");
    db.fetch_optional(&sql, &[id.into_arg()]).await
}

/// `upsert_placement` — idempotent upsert on
/// (surface, slot, domain, content_type, content_id).
#[allow(clippy::too_many_arguments)]
async fn upsert_placement(
    db: &Db,
    actor: &AuthCtx,
    domain: &str,
    content_type: &str,
    content_id: &str,
    surface: Option<String>,
    slot: Option<String>,
    sort_order: i64,
    audience: Value,
    starts_at: Option<DateTime<Utc>>,
    ends_at: Option<DateTime<Utc>>,
    locale: Option<String>,
    active: bool,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> ApiResult<Value> {
    let Some(surface) = surface.filter(|s| !s.is_empty()) else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "missing_surface",
            "surface الزامی است",
        ));
    };
    get_adapter(domain, content_type)?;
    if let (Some(s), Some(e)) = (starts_at, ends_at) {
        if s >= e {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_window",
                "starts_at باید قبل از ends_at باشد",
            ));
        }
    }
    let (sql, args): (String, Vec<Arg>) = match &slot {
        None => (
            "SELECT id FROM content_placements WHERE surface = ? AND slot IS NULL AND domain = ? \
             AND content_type = ? AND content_id = ?"
                .to_string(),
            vec![
                surface.clone().into_arg(),
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
            ],
        ),
        Some(slot) => (
            "SELECT id FROM content_placements WHERE surface = ? AND slot = ? AND domain = ? \
             AND content_type = ? AND content_id = ?"
                .to_string(),
            vec![
                surface.clone().into_arg(),
                slot.clone().into_arg(),
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
            ],
        ),
    };
    let existing = db.fetch_optional(&sql, &args).await?;
    let now = utc_now();
    let id: Uuid;
    let old: Option<Value>;
    if let Some(row) = existing {
        id = row.try_get("id")?;
        let full = fetch_placement(db, id).await?.expect("placement row");
        old = Some(placement_out(&full)?);
        db.execute(
            "UPDATE content_placements SET sort_order = ?, audience = ?, starts_at = ?, \
             ends_at = ?, locale = ?, active = ?, updated_at = ? WHERE id = ?",
            &[
                sort_order.into_arg(),
                audience.into_arg(),
                starts_at.into_arg(),
                ends_at.into_arg(),
                locale.into_arg(),
                active.into_arg(),
                now.into_arg(),
                id.into_arg(),
            ],
        )
        .await?;
    } else {
        id = new_id();
        old = None;
        db.execute(
            "INSERT INTO content_placements (id, domain, content_type, content_id, surface, \
             slot, sort_order, audience, starts_at, ends_at, locale, active, created_by, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
                surface.into_arg(),
                slot.into_arg(),
                sort_order.into_arg(),
                audience.into_arg(),
                starts_at.into_arg(),
                ends_at.into_arg(),
                locale.into_arg(),
                active.into_arg(),
                actor.user.id.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
    }
    let full = fetch_placement(db, id).await?.expect("placement row");
    let out = placement_out(&full)?;
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "content.placement.upsert",
        "content_placement",
        Some(&id.to_string()),
        old,
        Some(out.clone()),
        ip,
        user_agent,
    )
    .await?;
    Ok(out)
}

// --- distribution -----------------------------------------------------------

/// `get_distribution` — the effective state of every channel plus its
/// source (`explicit` | `native` | `default`). A missing item never
/// 404s here; only an unknown adapter does.
async fn distribution_state(
    db: &Db,
    domain: &str,
    content_type: &str,
    content_id: &str,
) -> ApiResult<Value> {
    let adapter = get_adapter(domain, content_type)?;
    let rows = db
        .fetch_all(
            "SELECT channel, enabled, scheduled_at, schedule_tz, until_at \
             FROM content_distributions WHERE domain = ? AND content_type = ? AND content_id = ?",
            &[
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
            ],
        )
        .await?;
    let mut explicit: HashMap<String, DynRow> = HashMap::new();
    for row in rows {
        let channel: String = row.try_get("channel")?;
        explicit.insert(channel, row);
    }
    // Native distribution: the CMS `distribution` JSONB column.
    let mut native: Option<Value> = None;
    if adapter.resource().is_some() {
        if let Some(item) = get_adapter_item(db, adapter, content_id).await? {
            if let Some(Value::Object(_)) = item.distribution {
                native = item.distribution;
            }
        }
    }
    let mut channels = Map::new();
    for ch in CHANNELS {
        let mut entry = Map::new();
        if let Some(row) = explicit.get(*ch) {
            let enabled: bool = row.try_get("enabled")?;
            let scheduled_at: Option<DateTime<Utc>> = row.try_get("scheduled_at")?;
            let schedule_tz: Option<String> = row.try_get("schedule_tz")?;
            let until_at: Option<DateTime<Utc>> = row.try_get("until_at")?;
            entry.insert("enabled".to_string(), json!(enabled));
            entry.insert("scheduled_at".to_string(), dt_json(scheduled_at));
            entry.insert(
                "schedule_tz".to_string(),
                schedule_tz.map(Value::String).unwrap_or(Value::Null),
            );
            entry.insert("until_at".to_string(), dt_json(until_at));
            entry.insert("source".to_string(), json!("explicit"));
        } else if let Some(Value::Object(map)) = &native {
            if let Some(v) = map.get(*ch) {
                entry.insert("enabled".to_string(), json!(py_bool(v)));
                entry.insert("source".to_string(), json!("native"));
                entry.insert("scheduled_at".to_string(), Value::Null);
                entry.insert("schedule_tz".to_string(), Value::Null);
                entry.insert("until_at".to_string(), Value::Null);
            } else {
                entry.insert("enabled".to_string(), json!(true));
                entry.insert("source".to_string(), json!("default"));
                entry.insert("scheduled_at".to_string(), Value::Null);
                entry.insert("schedule_tz".to_string(), Value::Null);
                entry.insert("until_at".to_string(), Value::Null);
                entry.insert(
                    "note".to_string(),
                    json!("no override — existing domain behavior applies"),
                );
            }
        } else {
            entry.insert("enabled".to_string(), json!(true));
            entry.insert("source".to_string(), json!("default"));
            entry.insert("scheduled_at".to_string(), Value::Null);
            entry.insert("schedule_tz".to_string(), Value::Null);
            entry.insert("until_at".to_string(), Value::Null);
            entry.insert(
                "note".to_string(),
                json!("no override — existing domain behavior applies"),
            );
        }
        channels.insert((*ch).to_string(), Value::Object(entry));
    }
    let mut out = Map::new();
    out.insert("domain".to_string(), json!(domain));
    out.insert("content_type".to_string(), json!(content_type));
    out.insert("id".to_string(), json!(content_id));
    out.insert("channels".to_string(), Value::Object(channels));
    Ok(Value::Object(out))
}

/// `is_channel_allowed` — the serve-time gate: explicit row, then the
/// domain's native distribution, then allowed. An unknown adapter is
/// allowed (Python catches the `APIError` and returns `True`).
async fn is_channel_allowed(
    db: &Db,
    domain: &str,
    content_type: &str,
    content_id: &str,
    channel: &str,
) -> ApiResult<bool> {
    let now = utc_now();
    let row = db
        .fetch_optional(
            "SELECT enabled, scheduled_at, until_at FROM content_distributions \
             WHERE domain = ? AND content_type = ? AND content_id = ? AND channel = ?",
            &[
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
                channel.to_string().into_arg(),
            ],
        )
        .await?;
    if let Some(row) = row {
        let enabled: bool = row.try_get("enabled")?;
        let scheduled_at: Option<DateTime<Utc>> = row.try_get("scheduled_at")?;
        let until_at: Option<DateTime<Utc>> = row.try_get("until_at")?;
        if !enabled {
            return Ok(false);
        }
        if let Some(s) = scheduled_at {
            if s > now {
                return Ok(false);
            }
        }
        if let Some(u) = until_at {
            if u <= now {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    let Ok(adapter) = get_adapter(domain, content_type) else {
        return Ok(true);
    };
    if adapter.resource().is_some() {
        if let Some(item) = get_adapter_item(db, adapter, content_id).await? {
            if let Some(Value::Object(map)) = item.distribution {
                if let Some(v) = map.get(channel) {
                    return Ok(py_bool(v));
                }
            }
        }
    }
    Ok(true)
}

/// `set_distribution` — upsert one channel row + audit. Returns the
/// single-channel shape (`source: "explicit"`).
#[allow(clippy::too_many_arguments)]
async fn apply_distribution(
    db: &Db,
    actor: &AuthCtx,
    domain: &str,
    content_type: &str,
    content_id: &str,
    channel: &str,
    enabled: bool,
    scheduled_at: Option<DateTime<Utc>>,
    schedule_tz: Option<String>,
    until_at: Option<DateTime<Utc>>,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> ApiResult<Value> {
    if !CHANNELS.contains(&channel) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_channel",
            format!("کانال نامعتبر: {channel}"),
        ));
    }
    get_adapter(domain, content_type)?;
    if let (Some(s), Some(u)) = (scheduled_at, until_at) {
        if s >= u {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_window",
                "scheduled_at باید قبل از until_at باشد",
            ));
        }
    }
    let row = db
        .fetch_optional(
            "SELECT id, enabled, scheduled_at, until_at FROM content_distributions \
             WHERE domain = ? AND content_type = ? AND content_id = ? AND channel = ?",
            &[
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
                channel.to_string().into_arg(),
            ],
        )
        .await?;
    let now = utc_now();
    let (id, old) = match &row {
        Some(r) => {
            let id: Uuid = r.try_get("id")?;
            let old_enabled: bool = r.try_get("enabled")?;
            let old_scheduled: Option<DateTime<Utc>> = r.try_get("scheduled_at")?;
            let old_until: Option<DateTime<Utc>> = r.try_get("until_at")?;
            (
                id,
                Some(json!({
                    "enabled": old_enabled,
                    "scheduled_at": dt_json(old_scheduled),
                    "until_at": dt_json(old_until),
                })),
            )
        }
        None => (new_id(), None),
    };
    if row.is_some() {
        db.execute(
            "UPDATE content_distributions SET enabled = ?, scheduled_at = ?, schedule_tz = ?, \
             until_at = ?, updated_by = ?, updated_at = ? WHERE id = ?",
            &[
                enabled.into_arg(),
                scheduled_at.into_arg(),
                schedule_tz.clone().into_arg(),
                until_at.into_arg(),
                actor.user.id.into_arg(),
                now.into_arg(),
                id.into_arg(),
            ],
        )
        .await?;
    } else {
        db.execute(
            "INSERT INTO content_distributions (id, domain, content_type, content_id, channel, \
             enabled, scheduled_at, schedule_tz, until_at, updated_by, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                id.into_arg(),
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                content_id.to_string().into_arg(),
                channel.to_string().into_arg(),
                enabled.into_arg(),
                scheduled_at.into_arg(),
                schedule_tz.clone().into_arg(),
                until_at.into_arg(),
                actor.user.id.into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
    }
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "content.distribution.set",
        "content_distribution",
        Some(&id.to_string()),
        old,
        Some(json!({
            "channel": channel,
            "enabled": enabled,
            "scheduled_at": dt_json(scheduled_at),
            "until_at": dt_json(until_at),
            "content": format!("{domain}/{content_type}/{content_id}"),
        })),
        ip,
        user_agent,
    )
    .await?;
    let mut out = Map::new();
    out.insert("channel".to_string(), json!(channel));
    out.insert("enabled".to_string(), json!(enabled));
    out.insert("scheduled_at".to_string(), dt_json(scheduled_at));
    out.insert(
        "schedule_tz".to_string(),
        schedule_tz.map(Value::String).unwrap_or(Value::Null),
    );
    out.insert("until_at".to_string(), dt_json(until_at));
    out.insert("source".to_string(), json!("explicit"));
    Ok(Value::Object(out))
}

// --- adapter transitions ----------------------------------------------------

/// Run one adapter's own transition (`transition_fn`) and return the new
/// workflow status. Each domain keeps its own side effects (version
/// bumps, `published_at`, domain event + domain audit); the center's
/// own audit is logged by the caller.
async fn adapter_transition(
    db: &Db,
    adapter: &Adapter,
    item_id: &str,
    to: &str,
    actor: &AuthCtx,
) -> ApiResult<String> {
    match adapter.kind {
        AdapterKind::CmsEvent
        | AdapterKind::CmsQuote
        | AdapterKind::CmsPage
        | AdapterKind::CmsBanner
        | AdapterKind::CmsAnnouncement => {
            let id = item_id
                .parse::<Uuid>()
                .map_err(|_| ApiError::not_found("not_found", "آیتم یافت نشد"))?;
            let table = adapter.table();
            let resource = adapter.resource().expect("cms resource");
            let prefix = adapter.audit_entity_type;
            let row = db
                .fetch_optional(
                    &format!(
                        "SELECT workflow_status, version, distribution FROM {table} WHERE id = ?"
                    ),
                    &[id.into_arg()],
                )
                .await?
                .ok_or_else(|| ApiError::not_found("not_found", "آیتم یافت نشد"))?;
            let current: String = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            let new_version = version + 1;
            let distribution: Option<Value> = row.try_get("distribution")?;
            let now = utc_now();
            let mut new_distribution = distribution.clone();
            let mut sets = vec![
                "workflow_status = ?".to_string(),
                "version = ?".to_string(),
                "updated_at = ?".to_string(),
            ];
            let mut args: Vec<Arg> = vec![
                to.to_string().into_arg(),
                new_version.into_arg(),
                now.into_arg(),
            ];
            if to == "published" {
                if adapter.kind == AdapterKind::CmsPage {
                    let pub_row = db
                        .fetch_optional(
                            &format!("SELECT published_at FROM {table} WHERE id = ?"),
                            &[id.into_arg()],
                        )
                        .await?;
                    let published_at: Option<DateTime<Utc>> = pub_row
                        .as_ref()
                        .and_then(|r| r.try_get("published_at").ok())
                        .flatten();
                    if published_at.is_none() {
                        sets.push("published_at = ?".to_string());
                        args.push(now.into_arg());
                    }
                }
                if distribution.is_none() {
                    new_distribution = Some(default_distribution());
                    sets.push("distribution = ?".to_string());
                    args.push(default_distribution().into_arg());
                }
            }
            args.push(id.into_arg());
            let sql = format!("UPDATE {table} SET {} WHERE id = ?", sets.join(", "));
            db.execute(&sql, &args).await?;
            emit_event(
                db,
                prefix,
                &id.to_string(),
                &format!("{prefix}.{to}"),
                json!({
                    "from": current,
                    "to": to,
                    "version": new_version,
                    "distribution": new_distribution,
                }),
            )
            .await?;
            log_audit(
                db,
                Some(actor.user.id),
                "admin",
                &format!("cms.{resource}.transition"),
                prefix,
                Some(&id.to_string()),
                Some(json!({ "workflow_status": current })),
                Some(json!({ "workflow_status": to, "version": new_version })),
                None,
                None,
            )
            .await?;
            Ok(to.to_string())
        }
        AdapterKind::FatwaSubmission => {
            let id = item_id
                .parse::<Uuid>()
                .map_err(|_| ApiError::not_found("not_found", "فتوا یافت نشد"))?;
            let row = db
                .fetch_optional(
                    "SELECT author_id, workflow_status, version FROM fatwa_submissions WHERE id = ?",
                    &[id.into_arg()],
                )
                .await?
                .ok_or_else(|| ApiError::not_found("not_found", "فتوا یافت نشد"))?;
            let _current: String = row.try_get("workflow_status")?;
            let author_id: Uuid = row.try_get("author_id")?;
            let old_version: i32 = row.try_get("version")?;
            let approver = has_permission(db, actor.user.id, "fatwa.approve").await?;
            let is_author = author_id == actor.user.id;
            let allowed = match to {
                "in_review" | "archived" => is_author || approver,
                "approved" | "published" | "rejected" => approver,
                _ => approver,
            };
            if !allowed {
                let msg = if matches!(to, "approved" | "published" | "rejected") {
                    "فقط تأییدکنندهٔ فتوا مجاز است"
                } else {
                    "دسترسی کافی نیست"
                };
                return Err(ApiError::forbidden("forbidden", msg));
            }
            let now = utc_now();
            let mut sets = vec![
                "workflow_status = ?".to_string(),
                "updated_at = ?".to_string(),
            ];
            let mut args: Vec<Arg> = vec![to.to_string().into_arg(), now.into_arg()];
            if to == "approved" {
                sets.push("reviewed_by = ?".to_string());
                sets.push("reviewed_at = ?".to_string());
                args.push(actor.user.id.into_arg());
                args.push(now.into_arg());
            }
            if to == "published" {
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
            if let Some(event_type) = match to {
                "in_review" => Some("fatwa.submission_submitted"),
                "approved" => Some("fatwa.submission_approved"),
                "published" => Some("fatwa.submission_published"),
                "rejected" => Some("fatwa.submission_rejected"),
                "archived" => Some("fatwa.submission_archived"),
                _ => None,
            } {
                emit_event(
                    db,
                    "fatwa_submission",
                    &id.to_string(),
                    event_type,
                    json!({ "actor_id": actor.user.id.to_string(), "to": to }),
                )
                .await?;
            }
            Ok(to.to_string())
        }
        AdapterKind::LibraryItem => {
            let id = item_id
                .parse::<Uuid>()
                .map_err(|_| ApiError::not_found("not_found", "آیتم کتابخانه یافت نشد"))?;
            let row = db
                .fetch_optional(
                    "SELECT workflow_status, version, published_at FROM library_items WHERE id = ?",
                    &[id.into_arg()],
                )
                .await?
                .ok_or_else(|| ApiError::not_found("not_found", "آیتم کتابخانه یافت نشد"))?;
            let current: String = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            let published_at: Option<DateTime<Utc>> = row.try_get("published_at")?;
            let now = utc_now();
            let mut sets = vec![
                "workflow_status = ?".to_string(),
                "updated_at = ?".to_string(),
            ];
            let mut args: Vec<Arg> = vec![to.to_string().into_arg(), now.into_arg()];
            if to == "published" && published_at.is_none() {
                sets.push("published_at = ?".to_string());
                args.push(now.into_arg());
            }
            args.push(id.into_arg());
            let sql = format!("UPDATE library_items SET {} WHERE id = ?", sets.join(", "));
            db.execute(&sql, &args).await?;
            let event_type = if to == "published" {
                "library.item.published"
            } else {
                "library.item.transition"
            };
            emit_event(
                db,
                "library_item",
                &id.to_string(),
                event_type,
                json!({"from": current, "to": to, "version": version, "note": false}),
            )
            .await?;
            log_audit(
                db,
                Some(actor.user.id),
                "admin",
                "library.item.transition",
                "library_item",
                Some(&id.to_string()),
                Some(json!({ "workflow_status": current })),
                Some(json!({ "workflow_status": to, "version": version })),
                None,
                None,
            )
            .await?;
            Ok(to.to_string())
        }
        AdapterKind::ReligiousPlace => {
            let id = item_id
                .parse::<Uuid>()
                .map_err(|_| ApiError::not_found("not_found", "مکان یافت نشد"))?;
            let row = db
                .fetch_optional(
                    "SELECT workflow_status, version FROM religious_places WHERE id = ?",
                    &[id.into_arg()],
                )
                .await?
                .ok_or_else(|| ApiError::not_found("not_found", "مکان یافت نشد"))?;
            let current: String = row.try_get("workflow_status")?;
            let version: i32 = row.try_get("version")?;
            let new_version = version + 1;
            let now = utc_now();
            db.execute(
                "UPDATE religious_places SET workflow_status = ?, version = ?, updated_at = ? \
                 WHERE id = ?",
                &[
                    to.to_string().into_arg(),
                    new_version.into_arg(),
                    now.into_arg(),
                    id.into_arg(),
                ],
            )
            .await?;
            emit_event(
                db,
                "religious_place",
                &id.to_string(),
                &format!("place.{to}"),
                json!({ "actor_id": actor.user.id.to_string(), "from": current, "to": to }),
            )
            .await?;
            log_audit(
                db,
                Some(actor.user.id),
                "admin",
                "places.transition",
                "religious_place",
                Some(&id.to_string()),
                Some(json!({ "workflow_status": current })),
                Some(json!({ "workflow_status": to })),
                None,
                None,
            )
            .await?;
            Ok(to.to_string())
        }
        _ => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "transition_unsupported",
            format!("نوع {} گردش‌کار §۹ ندارد", adapter.content_type),
        )),
    }
}

// --- detail -----------------------------------------------------------------

/// `get_detail` — the unified detail view: preview, workflow, placements,
/// distribution, schedules, related, versions (library only), SEO
/// (pages only), audit.
async fn get_detail(db: &Db, domain: &str, content_type: &str, item_id: &str) -> ApiResult<Value> {
    let adapter = get_adapter(domain, content_type)?;
    let detail = get_adapter_item(db, adapter, item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("not_found", "محتوا یافت نشد"))?;
    let status = detail.status.clone();
    let allowed: Vec<String> = if STATUSES.contains(&status.as_str()) {
        let mut next: Vec<String> = allowed_next(&status)
            .iter()
            .map(|s| s.to_string())
            .collect();
        next.sort();
        next
    } else {
        Vec::new()
    };
    let placement_rows = db
        .fetch_all(
            &format!(
                "SELECT {PLACEMENT_COLS} FROM content_placements WHERE domain = ? AND \
                 content_type = ? AND content_id = ? ORDER BY sort_order ASC"
            ),
            &[
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                item_id.to_string().into_arg(),
            ],
        )
        .await?;
    let mut placements = Vec::new();
    for row in &placement_rows {
        placements.push(placement_out(row)?);
    }
    let distribution = distribution_state(db, domain, content_type, item_id).await?;
    let now = utc_now();
    let schedule_rows = db
        .fetch_all(
            "SELECT channel, enabled, scheduled_at, schedule_tz, until_at FROM \
             content_distributions WHERE domain = ? AND content_type = ? AND content_id = ? \
             AND scheduled_at IS NOT NULL AND scheduled_at > ? ORDER BY scheduled_at ASC",
            &[
                domain.to_string().into_arg(),
                content_type.to_string().into_arg(),
                item_id.to_string().into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
    let mut schedules = Vec::new();
    for row in &schedule_rows {
        let channel: String = row.try_get("channel")?;
        let enabled: bool = row.try_get("enabled")?;
        let scheduled_at: Option<DateTime<Utc>> = row.try_get("scheduled_at")?;
        let schedule_tz: Option<String> = row.try_get("schedule_tz")?;
        let until_at: Option<DateTime<Utc>> = row.try_get("until_at")?;
        schedules.push(json!({
            "channel": channel,
            "enabled": enabled,
            "scheduled_at": dt_json(scheduled_at),
            "schedule_tz": schedule_tz,
            "until_at": dt_json(until_at),
        }));
    }
    // Related: five recent same-type items, except itself; optional.
    let mut related = Vec::new();
    if let Ok(others) = list_adapter(
        db,
        adapter,
        &ListFilters {
            q: None,
            status: None,
            author_id: None,
            visibility: None,
            ai_scope: None,
            date_from: None,
            date_to: None,
            cursor_ts: None,
            limit: 6,
        },
    )
    .await
    {
        for o in others {
            if o.id != item_id {
                related.push(json!({
                    "id": o.id,
                    "title": o.title,
                    "status": o.status,
                    "created_at": dt_json(o.created_at),
                }));
            }
            if related.len() >= 5 {
                break;
            }
        }
    }
    let audit_rows = db
        .fetch_all(
            "SELECT id, actor_id, actor_type, action, old_values, new_values, created_at \
             FROM audit_logs WHERE entity_type = ? AND entity_id = ? \
             ORDER BY created_at DESC LIMIT 50",
            &[
                adapter.audit_entity_type.to_string().into_arg(),
                item_id.to_string().into_arg(),
            ],
        )
        .await?;
    let mut audit = Vec::new();
    for row in &audit_rows {
        let id: Uuid = row.try_get("id")?;
        let actor_id: Option<Uuid> = row.try_get("actor_id")?;
        let actor_type: String = row.try_get("actor_type")?;
        let action: String = row.try_get("action")?;
        let old_values: Option<Value> = row.try_get("old_values")?;
        let new_values: Option<Value> = row.try_get("new_values")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        audit.push(json!({
            "id": id.to_string(),
            "actor_id": actor_id.map(|u| u.to_string()),
            "actor_type": actor_type,
            "action": action,
            "old_values": old_values,
            "new_values": new_values,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    // Version history (library only).
    let mut versions = Vec::new();
    if (domain, content_type) == ("library", "library_item") {
        if let Ok(item_uuid) = item_id.parse::<Uuid>() {
            let vrows = db
                .fetch_all(
                    "SELECT version, created_at, created_by, note FROM library_item_versions \
                     WHERE item_id = ? ORDER BY version DESC LIMIT 20",
                    &[item_uuid.into_arg()],
                )
                .await?;
            for row in &vrows {
                let version: i32 = row.try_get("version")?;
                let created_at: DateTime<Utc> = row.try_get("created_at")?;
                let created_by: Option<Uuid> = row.try_get("created_by")?;
                let note: Option<String> = row.try_get("note")?;
                versions.push(json!({
                    "version": version,
                    "created_at": created_at.to_rfc3339(),
                    "created_by": created_by.map(|u| u.to_string()),
                    "note": note,
                }));
            }
        }
    }
    let seo = match &detail.seo_meta {
        Some(v) => v.clone(),
        None => Value::Null,
    };
    let names = if let Some(author_id) = &detail.author_id {
        resolve_author_names(db, &[Some(author_id.clone())]).await?
    } else {
        HashMap::new()
    };
    let author_name = detail
        .author_id
        .as_ref()
        .and_then(|a| names.get(a).cloned());
    let mut out = Map::new();
    out.insert("domain".to_string(), json!(domain));
    out.insert("content_type".to_string(), json!(content_type));
    out.insert("id".to_string(), json!(item_id));
    out.insert(
        "preview".to_string(),
        json!({ "title": detail.title, "excerpt": detail.excerpt }),
    );
    out.insert(
        "workflow".to_string(),
        json!({
            "status": status,
            "allowed_transitions": allowed,
            "version": detail.version,
        }),
    );
    out.insert(
        "author".to_string(),
        json!({ "id": detail.author_id, "name": author_name }),
    );
    out.insert(
        "timestamps".to_string(),
        json!({
            "created_at": dt_json(detail.created_at),
            "updated_at": dt_json(detail.updated_at),
            "published_at": dt_json(detail.published_at),
        }),
    );
    out.insert(
        "visibility".to_string(),
        detail
            .visibility
            .clone()
            .map(Value::String)
            .unwrap_or(Value::Null),
    );
    out.insert("ai_scopes".to_string(), json!(detail.ai_scopes));
    out.insert("placements".to_string(), json!(placements));
    out.insert("distribution".to_string(), distribution);
    out.insert("schedules".to_string(), json!(schedules));
    out.insert("related".to_string(), json!(related));
    out.insert("versions".to_string(), json!(versions));
    out.insert("seo".to_string(), seo);
    out.insert("audit".to_string(), json!(audit));
    Ok(Value::Object(out))
}

// --- handlers: adapters + items ---------------------------------------------

/// `GET /api/v1/admin/content/adapters` — the registered domains.
pub async fn list_adapters(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let items: Vec<Value> = adapters()
        .iter()
        .map(|a| {
            json!({
                "domain": a.domain,
                "content_type": a.content_type,
                "label": a.label,
                "supports_transition": a.supports_transition(),
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// `GET /api/v1/admin/content/items` — the unified inbox.
pub async fn list_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let date_from = parse_dt_opt(params.get("date_from").map(String::as_str))?;
    let date_to = parse_dt_opt(params.get("date_to").map(String::as_str))?;
    // Cursor first, then the channel check, then the adapter check
    // (Python's order in `list_items`).
    let (cursor_ts, cursor_key) = match params.get("cursor").filter(|s| !s.is_empty()) {
        Some(c) => {
            let (ts, d, ct, id) = decode_cursor(c)?;
            (Some(ts), Some((ts, d, ct, id)))
        }
        None => (None, None),
    };
    let distribution_channel = params
        .get("distribution_channel")
        .filter(|s| !s.is_empty())
        .cloned();
    if let Some(ch) = &distribution_channel {
        if !CHANNELS.contains(&ch.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_channel",
                format!("کانال نامعتبر: {ch}"),
            ));
        }
    }
    // An empty domain/content_type matches no adapter (Python passes
    // the raw value to `list_adapters`); only a non-empty value can
    // 404 via the `(domain or content_type)` check.
    let domain = params.get("domain").cloned();
    let content_type = params.get("content_type").cloned();
    let selected = list_adapters_filtered(domain.as_deref(), content_type.as_deref());
    let has_filter = domain.as_deref().is_some_and(|s| !s.is_empty())
        || content_type.as_deref().is_some_and(|s| !s.is_empty());
    if has_filter && selected.is_empty() {
        return Err(adapter_not_found(
            domain.as_deref().unwrap_or(""),
            content_type.as_deref().unwrap_or(""),
        ));
    }
    let db = state.db_or_500()?;
    let filters = ListFilters {
        q: params.get("q").filter(|s| !s.is_empty()).cloned(),
        status: params.get("status").filter(|s| !s.is_empty()).cloned(),
        author_id: params.get("author_id").filter(|s| !s.is_empty()).cloned(),
        visibility: params.get("visibility").filter(|s| !s.is_empty()).cloned(),
        ai_scope: params.get("ai_scope").filter(|s| !s.is_empty()).cloned(),
        date_from,
        date_to,
        cursor_ts,
        limit,
    };
    let mut merged: Vec<UnifiedItem> = Vec::new();
    for adapter in &selected {
        match list_adapter(db, adapter, &filters).await {
            Ok(items) => merged.extend(items.into_iter().take((limit + 1) as usize)),
            Err(ApiError { .. }) => {
                // One broken domain must not break the whole inbox
                // (Python logs and continues).
                continue;
            }
        }
    }
    // Placement filter: only items with an active placement on the surface.
    if let Some(surface) = params.get("placement").filter(|s| !s.is_empty()) {
        let rows = db
            .fetch_all(
                "SELECT domain, content_type, content_id FROM content_placements \
                 WHERE surface = ? AND active = TRUE",
                &[surface.clone().into_arg()],
            )
            .await?;
        let mut pset: HashSet<(String, String, String)> = HashSet::new();
        for row in &rows {
            let d: String = row.try_get("domain")?;
            let ct: String = row.try_get("content_type")?;
            let cid: String = row.try_get("content_id")?;
            pset.insert((d, ct, cid));
        }
        merged.retain(|it| {
            pset.contains(&(it.domain.clone(), it.content_type.clone(), it.id.clone()))
        });
    }
    // Distribution filter: the serve-time channel gate.
    if let Some(channel) = &distribution_channel {
        let mut kept = Vec::new();
        for it in merged {
            if is_channel_allowed(db, &it.domain, &it.content_type, &it.id, channel).await? {
                kept.push(it);
            }
        }
        merged = kept;
    }
    merged.sort_by(|a, b| {
        let ta = a
            .created_at
            .map(|d| d.timestamp_nanos_opt().unwrap_or(0))
            .unwrap_or(0);
        let tb = b
            .created_at
            .map(|d| d.timestamp_nanos_opt().unwrap_or(0))
            .unwrap_or(0);
        tb.cmp(&ta)
            .then_with(|| a.domain.cmp(&b.domain))
            .then_with(|| a.content_type.cmp(&b.content_type))
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Some((cts, cd, cct, cid)) = &cursor_key {
        merged.retain(|it| {
            let ts = it.created_at;
            match ts {
                Some(t) if t < *cts => true,
                Some(t) if t > *cts => false,
                Some(_) => {
                    (it.domain.as_str(), it.content_type.as_str(), it.id.as_str())
                        > (cd.as_str(), cct.as_str(), cid.as_str())
                }
                None => true,
            }
        });
    }
    let has_more = merged.len() > limit as usize;
    let page: Vec<UnifiedItem> = merged.into_iter().take(limit as usize).collect();
    let next_cursor = if has_more {
        let last = page.last().expect("non-empty page when has_more");
        Some(encode_cursor(
            last.created_at.unwrap_or_else(utc_now),
            &last.domain,
            &last.content_type,
            &last.id,
        ))
    } else {
        None
    };
    let names = resolve_author_names(
        db,
        &page
            .iter()
            .map(|it| it.author_id.clone())
            .collect::<Vec<_>>(),
    )
    .await?;
    let mut items = Vec::new();
    for it in &page {
        let name = it
            .author_id
            .as_ref()
            .and_then(|a| names.get(a).map(String::as_str));
        items.push(item_out(it, name));
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/admin/content/items/{domain}/{content_type}/{item_id}`.
pub async fn get_item_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((domain, content_type, item_id)): Path<(String, String, String)>,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    Ok(Json(
        get_detail(db, &domain, &content_type, &item_id).await?,
    ))
}

// --- bulk -------------------------------------------------------------------

/// One bulk `transition` item. Returns the entry's extra fields
/// (`from`/`to`, or `skipped` + `reason`).
async fn bulk_transition_item(
    db: &Db,
    adapter: &Adapter,
    item_id: &str,
    params: &Value,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Value, ApiError> {
    let to = match params.get("to") {
        Some(Value::String(s)) if !s.is_empty() => s.clone(),
        Some(Value::String(_)) | None => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "missing_target",
                "پارامتر to الزامی است",
            ));
        }
        Some(_) => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_status",
                "وضعیت ناشناخته",
            ));
        }
    };
    if !adapter.supports_transition() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "transition_unsupported",
            format!("نوع {} گردش‌کار §۹ ندارد", adapter.content_type),
        ));
    }
    let detail = get_adapter_item(db, adapter, item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("not_found", "محتوا یافت نشد"))?;
    let current = detail.status.clone();
    if current == to {
        return Ok(json!({
            "ok": true,
            "skipped": true,
            "reason": "already_in_target_status",
        }));
    }
    validate_transition(&current, &to)?;
    let new_status = adapter_transition(db, adapter, item_id, &to, actor).await?;
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "content.bulk.transition",
        adapter.audit_entity_type,
        Some(item_id),
        Some(json!({ "workflow_status": current })),
        Some(json!({ "workflow_status": new_status })),
        ip,
        user_agent,
    )
    .await?;
    Ok(json!({ "ok": true, "from": current, "to": new_status }))
}

async fn bulk_schedule_item(
    db: &Db,
    adapter: &Adapter,
    item_id: &str,
    params: &Value,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Value, ApiError> {
    let channels: Vec<String> = match params.get("channels") {
        None | Some(Value::Null) => vec!["web".to_string()],
        Some(Value::Array(arr)) if arr.is_empty() => vec!["web".to_string()],
        Some(Value::Array(arr)) => arr
            .iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect(),
        Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
        Some(Value::Object(map)) => map.keys().cloned().collect(),
        Some(_) => Vec::new(),
    };
    let bad: Vec<String> = channels
        .iter()
        .filter(|c| !CHANNELS.contains(&c.as_str()))
        .cloned()
        .collect();
    if !bad.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_channel",
            format!("کانال نامعتبر: {bad:?}"),
        ));
    }
    let scheduled_at = parse_dt_value(params.get("scheduled_at").unwrap_or(&Value::Null))?;
    let until_at = parse_dt_value(params.get("until_at").unwrap_or(&Value::Null))?;
    let schedule_tz = params
        .get("schedule_tz")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let enabled = match params.get("enabled") {
        None => true,
        Some(v) => py_bool(v),
    };
    for ch in &channels {
        apply_distribution(
            db,
            actor,
            adapter.domain,
            adapter.content_type,
            item_id,
            ch,
            enabled,
            scheduled_at,
            schedule_tz.clone(),
            until_at,
            ip,
            user_agent,
        )
        .await?;
    }
    Ok(json!({
        "ok": true,
        "channels": channels,
        "scheduled_at": dt_json(scheduled_at),
    }))
}

async fn bulk_placement_item(
    db: &Db,
    adapter: &Adapter,
    item_id: &str,
    params: &Value,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Value, ApiError> {
    let surface = params
        .get("surface")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let slot = params
        .get("slot")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let sort_order = match params.get("sort_order") {
        None | Some(Value::Null) => 0,
        Some(v) => py_int(v).ok_or_else(ApiError::internal)?,
    };
    let audience = match params.get("audience") {
        None | Some(Value::Null) => json!({}),
        Some(v) if !py_bool(v) => json!({}),
        Some(v) => v.clone(),
    };
    let starts_at = parse_dt_value(params.get("starts_at").unwrap_or(&Value::Null))?;
    let ends_at = parse_dt_value(params.get("ends_at").unwrap_or(&Value::Null))?;
    let locale = params
        .get("locale")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let active = match params.get("active") {
        None => true,
        Some(v) => py_bool(v),
    };
    upsert_placement(
        db,
        actor,
        adapter.domain,
        adapter.content_type,
        item_id,
        surface.clone(),
        slot,
        sort_order,
        audience,
        starts_at,
        ends_at,
        locale,
        active,
        ip,
        user_agent,
    )
    .await?;
    Ok(json!({ "ok": true, "surface": surface }))
}

async fn bulk_distribution_item(
    db: &Db,
    adapter: &Adapter,
    item_id: &str,
    params: &Value,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Value, ApiError> {
    let channels = match params.get("channels") {
        Some(Value::Object(map)) if !map.is_empty() => map.clone(),
        _ => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_channels",
                "channels باید دیکشنری غیرخالی باشد",
            ));
        }
    };
    for ch in channels.keys() {
        if !CHANNELS.contains(&ch.as_str()) {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_channel",
                format!("کانال نامعتبر: {ch}"),
            ));
        }
    }
    let mut names: Vec<String> = channels.keys().cloned().collect();
    names.sort();
    for ch in &names {
        let cfg = channels.get(ch).cloned().unwrap_or(Value::Null);
        let cfg = cfg.as_object().cloned().unwrap_or_default();
        let enabled = match cfg.get("enabled") {
            None => false,
            Some(v) => py_bool(v),
        };
        let scheduled_at = parse_dt_value(cfg.get("scheduled_at").unwrap_or(&Value::Null))?;
        let schedule_tz = cfg
            .get("schedule_tz")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let until_at = parse_dt_value(cfg.get("until_at").unwrap_or(&Value::Null))?;
        apply_distribution(
            db,
            actor,
            adapter.domain,
            adapter.content_type,
            item_id,
            ch,
            enabled,
            scheduled_at,
            schedule_tz,
            until_at,
            ip,
            user_agent,
        )
        .await?;
    }
    Ok(json!({ "ok": true, "channels": names }))
}

/// `POST /api/v1/admin/content/bulk` — transition | schedule |
/// placement | distribution | export, idempotent where possible, one
/// audit row per changed item.
pub async fn bulk_action(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    // Pydantic first: `items` 1..=200, each a `{domain, content_type, id}`
    // ref; `action` a required string; `params` an object.
    let items_raw = match parsed.get("items") {
        Some(Value::Array(arr)) if !arr.is_empty() && arr.len() <= 200 => arr.clone(),
        _ => return Err(validation_err("items", "must be a list of 1..200 items")),
    };
    let mut refs: Vec<(String, String, String)> = Vec::new();
    for r in &items_raw {
        let (Some(d), Some(ct), Some(id)) = (
            r.get("domain").and_then(|v| v.as_str()),
            r.get("content_type").and_then(|v| v.as_str()),
            r.get("id").and_then(|v| v.as_str()),
        ) else {
            return Err(validation_err(
                "items",
                "each item needs domain, content_type, id",
            ));
        };
        refs.push((d.to_string(), ct.to_string(), id.to_string()));
    }
    let action_raw = match parsed.get("action") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("action", "field is required")),
    };
    let params = match parsed.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => return Err(validation_err("params", "must be an object")),
    };
    let action = match action_raw.as_str() {
        "set_placement" => "placement".to_string(),
        "set_distribution" => "distribution".to_string(),
        other => other.to_string(),
    };
    if !BULK_ACTIONS.contains(&action.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_bulk_action",
            format!("عملیات نامعتبر: {action}"),
        ));
    }
    let db = state.db_or_500()?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let mut results: Vec<Value> = Vec::new();
    let mut export_rows: Vec<Value> = Vec::new();
    for (domain, content_type, item_id) in &refs {
        let mut entry = Map::new();
        entry.insert("domain".to_string(), json!(domain));
        entry.insert("content_type".to_string(), json!(content_type));
        entry.insert("id".to_string(), json!(item_id));
        entry.insert("ok".to_string(), json!(false));
        let adapter = match get_adapter(domain, content_type) {
            Ok(a) => a,
            Err(e) => {
                entry.insert(
                    "error".to_string(),
                    json!({ "code": e.code, "message": e.message }),
                );
                results.push(Value::Object(entry));
                continue;
            }
        };
        let outcome: Result<Value, ApiError> = match action.as_str() {
            "transition" => {
                bulk_transition_item(
                    db,
                    adapter,
                    item_id,
                    &params,
                    &ctx,
                    ip.as_deref(),
                    ua.as_deref(),
                )
                .await
            }
            "schedule" => {
                bulk_schedule_item(
                    db,
                    adapter,
                    item_id,
                    &params,
                    &ctx,
                    ip.as_deref(),
                    ua.as_deref(),
                )
                .await
            }
            "placement" => {
                bulk_placement_item(
                    db,
                    adapter,
                    item_id,
                    &params,
                    &ctx,
                    ip.as_deref(),
                    ua.as_deref(),
                )
                .await
            }
            "distribution" => {
                bulk_distribution_item(
                    db,
                    adapter,
                    item_id,
                    &params,
                    &ctx,
                    ip.as_deref(),
                    ua.as_deref(),
                )
                .await
            }
            "export" => match get_adapter_item(db, adapter, item_id).await? {
                None => Err(ApiError::not_found("not_found", "محتوا یافت نشد")),
                Some(detail) => {
                    export_rows.push(json!({
                        "domain": domain,
                        "content_type": content_type,
                        "id": item_id,
                        "title": detail.title,
                        "status": detail.status,
                        "author_id": detail.author_id,
                        "created_at": dt_json(detail.created_at),
                    }));
                    Ok(json!({ "ok": true }))
                }
            },
            _ => unreachable!(),
        };
        match outcome {
            Ok(extra) => {
                if let Value::Object(map) = extra {
                    for (k, v) in map {
                        entry.insert(k, v);
                    }
                }
            }
            Err(e) => {
                entry.insert(
                    "error".to_string(),
                    json!({ "code": e.code, "message": e.message }),
                );
            }
        }
        results.push(Value::Object(entry));
    }
    if action == "export" {
        log_audit(
            db,
            Some(ctx.user.id),
            "admin",
            "content.bulk.export",
            "content_bulk",
            None,
            None,
            Some(json!({ "count": export_rows.len() })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
    }
    let succeeded = results
        .iter()
        .filter(|r| r.get("ok").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    let skipped = results
        .iter()
        .filter(|r| r.get("skipped").and_then(|v| v.as_bool()).unwrap_or(false))
        .count();
    let failed = results.iter().filter(|r| r.get("error").is_some()).count();
    let mut out = Map::new();
    out.insert("action".to_string(), json!(action));
    out.insert("total".to_string(), json!(results.len()));
    out.insert("succeeded".to_string(), json!(succeeded));
    out.insert("skipped".to_string(), json!(skipped));
    out.insert("failed".to_string(), json!(failed));
    out.insert("results".to_string(), json!(results));
    if action == "export" {
        out.insert("export".to_string(), json!(export_rows));
    }
    Ok(Json(Value::Object(out)))
}

// --- handlers: placements ---------------------------------------------------

/// `GET /api/v1/admin/content/placements`.
pub async fn list_placements(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 100, 1, 500)?.min(500);
    let surface = params.get("surface").filter(|s| !s.is_empty()).cloned();
    let domain = params.get("domain").filter(|s| !s.is_empty()).cloned();
    let content_type = params
        .get("content_type")
        .filter(|s| !s.is_empty())
        .cloned();
    let content_id = params.get("content_id").filter(|s| !s.is_empty()).cloned();
    let active = match params.get("active") {
        None => None,
        Some(v) => match v.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => return Err(validation_err("active", "must be a boolean")),
        },
    };
    let db = state.db_or_500()?;
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(s) = &surface {
        conds.push("surface = ?".to_string());
        args.push(s.clone().into_arg());
    }
    if let Some(d) = &domain {
        conds.push("domain = ?".to_string());
        args.push(d.clone().into_arg());
    }
    if let Some(ct) = &content_type {
        conds.push("content_type = ?".to_string());
        args.push(ct.clone().into_arg());
    }
    if let Some(cid) = &content_id {
        conds.push("content_id = ?".to_string());
        args.push(cid.clone().into_arg());
    }
    if let Some(a) = active {
        conds.push("active = ?".to_string());
        args.push(a.into_arg());
    }
    let mut sql = format!("SELECT {PLACEMENT_COLS} FROM content_placements");
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY surface ASC, sort_order ASC LIMIT ?");
    args.push(limit.into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut out = Vec::new();
    for row in &rows {
        out.push(placement_out(row)?);
    }
    Ok(Json(json!(out)))
}

/// `PUT /api/v1/admin/content/placements` — upsert.
pub async fn upsert_placement_route(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let domain = match parsed.get("domain").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Err(validation_err("domain", "field is required")),
    };
    if domain.chars().count() > 64 {
        return Err(validation_err("domain", "length out of range"));
    }
    let content_type = match parsed.get("content_type").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Err(validation_err("content_type", "field is required")),
    };
    if content_type.chars().count() > 64 {
        return Err(validation_err("content_type", "length out of range"));
    }
    let content_id = match parsed.get("content_id").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Err(validation_err("content_id", "field is required")),
    };
    if content_id.chars().count() > 128 {
        return Err(validation_err("content_id", "length out of range"));
    }
    let surface = match parsed.get("surface") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("surface", "field is required")),
    };
    if surface.chars().count() > 64 {
        return Err(validation_err("surface", "length out of range"));
    }
    let slot = match parsed.get("slot") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.chars().count() > 64 {
                return Err(validation_err("slot", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("slot", "must be a string")),
    };
    let sort_order = match parsed.get("sort_order") {
        None | Some(Value::Null) => 0,
        Some(v) => match as_int(v) {
            Some(n) if (-2_147_483_648..=2_147_483_647).contains(&n) => n,
            _ => return Err(validation_err("sort_order", "out of range")),
        },
    };
    let audience = match parsed.get("audience") {
        None | Some(Value::Null) => json!({}),
        Some(v) if v.is_object() => v.clone(),
        Some(_) => return Err(validation_err("audience", "must be an object")),
    };
    let starts_at = opt_datetime(&parsed, "starts_at")?;
    let ends_at = opt_datetime(&parsed, "ends_at")?;
    let locale = match parsed.get("locale") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.chars().count() > 8 {
                return Err(validation_err("locale", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("locale", "must be a string")),
    };
    let active = match parsed.get("active") {
        None | Some(Value::Null) => true,
        Some(v) => as_bool(v).ok_or_else(|| validation_err("active", "must be a boolean"))?,
    };
    let db = state.db_or_500()?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let out = upsert_placement(
        db,
        &ctx,
        &domain,
        &content_type,
        &content_id,
        if surface.is_empty() {
            None
        } else {
            Some(surface)
        },
        slot,
        sort_order,
        audience,
        starts_at,
        ends_at,
        locale,
        active,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(out))
}

/// `DELETE /api/v1/admin/content/placements/{placement_id}`.
pub async fn delete_placement(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(placement_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let placement_id = parse_path_uuid(&placement_id, "placement_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let row = fetch_placement(db, placement_id)
        .await?
        .ok_or_else(|| ApiError::not_found("not_found", "placement یافت نشد"))?;
    let old = placement_out(&row)?;
    db.execute(
        "DELETE FROM content_placements WHERE id = ?",
        &[placement_id.into_arg()],
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "content.placement.delete",
        "content_placement",
        Some(&placement_id.to_string()),
        Some(old),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(json!({ "message": "placement حذف شد" })))
}

/// `PATCH /api/v1/admin/content/placements/{placement_id}` — partial
/// update of the patchable fields.
pub async fn patch_placement(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(placement_id): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let placement_id = parse_path_uuid(&placement_id, "placement_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let patchable = [
        "surface",
        "slot",
        "sort_order",
        "audience",
        "starts_at",
        "ends_at",
        "locale",
        "active",
    ];
    let bad: Vec<String> = parsed
        .as_object()
        .map(|m| {
            m.keys()
                .filter(|k| !patchable.contains(&k.as_str()))
                .cloned()
                .collect()
        })
        .unwrap_or_default();
    if !bad.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_patch_field",
            format!("فیلد نامعتبر: {bad:?}"),
        ));
    }
    let db = state.db_or_500()?;
    let row = fetch_placement(db, placement_id)
        .await?
        .ok_or_else(|| ApiError::not_found("placement_not_found", "placement یافت نشد"))?;
    // Old values, rendered with Python `str()` (audit only).
    let mut old_values = Map::new();
    let mut sets: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(v) = parsed.get("surface") {
        let old: String = row.try_get("surface")?;
        old_values.insert("surface".to_string(), json!(old));
        match v {
            Value::Null => {
                sets.push("surface = ?".to_string());
                args.push(Arg::NullText);
            }
            Value::String(s) => {
                sets.push("surface = ?".to_string());
                args.push(s.clone().into_arg());
            }
            _ => return Err(ApiError::internal()),
        }
    }
    if let Some(v) = parsed.get("slot") {
        let old: Option<String> = row.try_get("slot")?;
        old_values.insert(
            "slot".to_string(),
            json!(old
                .map(|s| py_str(&json!(s)))
                .unwrap_or_else(|| "None".to_string())),
        );
        match v {
            Value::Null => {
                sets.push("slot = ?".to_string());
                args.push(Arg::NullText);
            }
            Value::String(s) => {
                sets.push("slot = ?".to_string());
                args.push(s.clone().into_arg());
            }
            _ => return Err(ApiError::internal()),
        }
    }
    if let Some(v) = parsed.get("sort_order") {
        let old: i32 = row.try_get("sort_order")?;
        old_values.insert("sort_order".to_string(), json!(old.to_string()));
        match v {
            Value::Null => {
                sets.push("sort_order = ?".to_string());
                args.push(Arg::NullInt);
            }
            _ => match as_int(v) {
                Some(n) if (-2_147_483_648..=2_147_483_647).contains(&n) => {
                    sets.push("sort_order = ?".to_string());
                    args.push(n.into_arg());
                }
                _ => return Err(ApiError::internal()),
            },
        }
    }
    if let Some(v) = parsed.get("audience") {
        let old: Option<Value> = row.try_get("audience")?;
        old_values.insert(
            "audience".to_string(),
            json!(old
                .map(|o| o.to_string())
                .unwrap_or_else(|| "None".to_string())),
        );
        sets.push("audience = ?".to_string());
        args.push(v.clone().into_arg());
    }
    if let Some(v) = parsed.get("starts_at") {
        let old: Option<DateTime<Utc>> = row.try_get("starts_at")?;
        old_values.insert("starts_at".to_string(), json!(py_str_dt(old)));
        match v {
            Value::Null => {
                sets.push("starts_at = ?".to_string());
                args.push(Arg::NullTime);
            }
            Value::String(s) => match parse_datetime_str(s) {
                Some(dt) => {
                    sets.push("starts_at = ?".to_string());
                    args.push(dt.into_arg());
                }
                None => return Err(ApiError::internal()),
            },
            _ => return Err(ApiError::internal()),
        }
    }
    if let Some(v) = parsed.get("ends_at") {
        let old: Option<DateTime<Utc>> = row.try_get("ends_at")?;
        old_values.insert("ends_at".to_string(), json!(py_str_dt(old)));
        match v {
            Value::Null => {
                sets.push("ends_at = ?".to_string());
                args.push(Arg::NullTime);
            }
            Value::String(s) => match parse_datetime_str(s) {
                Some(dt) => {
                    sets.push("ends_at = ?".to_string());
                    args.push(dt.into_arg());
                }
                None => return Err(ApiError::internal()),
            },
            _ => return Err(ApiError::internal()),
        }
    }
    if let Some(v) = parsed.get("locale") {
        let old: Option<String> = row.try_get("locale")?;
        old_values.insert(
            "locale".to_string(),
            json!(old.unwrap_or_else(|| "None".to_string())),
        );
        match v {
            Value::Null => {
                sets.push("locale = ?".to_string());
                args.push(Arg::NullText);
            }
            Value::String(s) => {
                sets.push("locale = ?".to_string());
                args.push(s.clone().into_arg());
            }
            _ => return Err(ApiError::internal()),
        }
    }
    if let Some(v) = parsed.get("active") {
        let old: bool = row.try_get("active")?;
        old_values.insert(
            "active".to_string(),
            json!(if old { "True" } else { "False" }),
        );
        match v {
            Value::Null => {
                sets.push("active = ?".to_string());
                args.push(Arg::NullBool);
            }
            _ => match as_bool(v) {
                Some(b) => {
                    sets.push("active = ?".to_string());
                    args.push(b.into_arg());
                }
                None => return Err(ApiError::internal()),
            },
        }
    }
    // New values, rendered with Python `str()` (audit only).
    let mut new_values = Map::new();
    for (k, v) in parsed.as_object().expect("object") {
        if patchable.contains(&k.as_str()) {
            new_values.insert(k.clone(), json!(py_str(v)));
        }
    }
    if !sets.is_empty() {
        sets.push("updated_at = ?".to_string());
        args.push(utc_now().into_arg());
        args.push(placement_id.into_arg());
        let sql = format!(
            "UPDATE content_placements SET {} WHERE id = ?",
            sets.join(", ")
        );
        db.execute(&sql, &args).await?;
    }
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "content.placement.patch",
        "content_placement",
        Some(&placement_id.to_string()),
        Some(Value::Object(old_values)),
        Some(Value::Object(new_values)),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let row = fetch_placement(db, placement_id)
        .await?
        .ok_or_else(|| ApiError::not_found("placement_not_found", "placement یافت نشد"))?;
    placement_out(&row).map(Json)
}

/// `POST /api/v1/admin/content/placements/reorder`.
pub async fn reorder_placements(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let ordered_ids = match parsed.get("ordered_ids") {
        Some(Value::Array(arr)) if !arr.is_empty() && arr.len() <= 200 => arr.clone(),
        _ => {
            return Err(validation_err(
                "ordered_ids",
                "must be a list of 1..200 ids",
            ));
        }
    };
    // Validate everything first: Python raises before committing, so a
    // bad id must not leave earlier ids reordered.
    let mut ids: Vec<Uuid> = Vec::new();
    for raw in &ordered_ids {
        let s = raw.as_str().unwrap_or("");
        let pid = s.parse::<Uuid>().map_err(|_| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_placement_id",
                format!("شناسه نامعتبر: {s}"),
            )
        })?;
        ids.push(pid);
    }
    let db = state.db_or_500()?;
    for pid in &ids {
        if fetch_placement(db, *pid).await?.is_none() {
            return Err(ApiError::not_found(
                "placement_not_found",
                format!("یافت نشد: {pid}"),
            ));
        }
    }
    for (idx, pid) in ids.iter().enumerate() {
        db.execute(
            "UPDATE content_placements SET sort_order = ? WHERE id = ?",
            &[(idx as i64).into_arg(), pid.into_arg()],
        )
        .await?;
    }
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "content.placements.reorder",
        "content_placement",
        None,
        None,
        Some(json!({ "ordered_count": ids.len() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(json!({ "updated": ids.len() })))
}

// --- handlers: distribution + calendar --------------------------------------

fn req_query(params: &HashMap<String, String>, name: &str) -> Result<String, ApiError> {
    match params.get(name) {
        Some(s) => Ok(s.clone()),
        None => Err(validation_err(name, "field is required")),
    }
}

/// `GET /api/v1/admin/content/distribution?domain=&content_type=&id=`.
pub async fn get_distribution(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let domain = req_query(&params, "domain")?;
    let content_type = req_query(&params, "content_type")?;
    let content_id = req_query(&params, "id")?;
    let db = state.db_or_500()?;
    Ok(Json(
        distribution_state(db, &domain, &content_type, &content_id).await?,
    ))
}

/// `PUT /api/v1/admin/content/distribution?domain=&content_type=&id=`.
pub async fn set_distribution(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    uri: Uri,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let params = parse_query(&uri);
    let domain = req_query(&params, "domain")?;
    let content_type = req_query(&params, "content_type")?;
    let content_id = req_query(&params, "id")?;
    let channel = match parsed.get("channel").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return Err(validation_err("channel", "field is required")),
    };
    let enabled = match parsed.get("enabled") {
        Some(v) => as_bool(v).ok_or_else(|| validation_err("enabled", "must be a boolean"))?,
        None => return Err(validation_err("enabled", "field is required")),
    };
    let scheduled_at = opt_datetime(&parsed, "scheduled_at")?;
    let schedule_tz = parsed
        .get("schedule_tz")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let until_at = opt_datetime(&parsed, "until_at")?;
    let db = state.db_or_500()?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    Ok(Json(
        apply_distribution(
            db,
            &ctx,
            &domain,
            &content_type,
            &content_id,
            &channel,
            enabled,
            scheduled_at,
            schedule_tz,
            until_at,
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?,
    ))
}

/// `GET /api/v1/admin/content/calendar?date_from=&date_to=` — the
/// unified publish calendar plus placement-slot conflicts.
pub async fn calendar_feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    // Both params are required (FastAPI `Query(...)`); an empty value
    // falls back to now, like the router's `_parse_dt(...) or now()`.
    let date_from = match params.get("date_from") {
        None => return Err(validation_err("date_from", "field is required")),
        Some(s) => parse_dt_opt(Some(s.as_str()))?.unwrap_or_else(utc_now),
    };
    let date_to = match params.get("date_to") {
        None => return Err(validation_err("date_to", "field is required")),
        Some(s) => parse_dt_opt(Some(s.as_str()))?.unwrap_or_else(utc_now),
    };
    let db = state.db_or_500()?;
    let mut items: Vec<Value> = Vec::new();
    for adapter in adapters() {
        let rows = match list_adapter(
            db,
            adapter,
            &ListFilters {
                q: None,
                status: None,
                author_id: None,
                visibility: None,
                ai_scope: None,
                date_from: Some(date_from),
                date_to: Some(date_to),
                cursor_ts: None,
                limit: 200,
            },
        )
        .await
        {
            Ok(r) => r,
            Err(_) => continue,
        };
        for it in rows {
            if let Some(pub_at) = it.published_at {
                if pub_at >= date_from && pub_at <= date_to {
                    items.push(json!({
                        "kind": "published",
                        "domain": it.domain,
                        "content_type": it.content_type,
                        "id": it.id,
                        "title": it.title,
                        "status": it.status,
                        "at": pub_at.to_rfc3339(),
                    }));
                }
            }
        }
    }
    let sched_rows = db
        .fetch_all(
            "SELECT domain, content_type, content_id, channel, scheduled_at FROM \
             content_distributions WHERE enabled = TRUE AND scheduled_at IS NOT NULL \
             AND scheduled_at >= ? AND scheduled_at <= ? ORDER BY scheduled_at ASC LIMIT 500",
            &[date_from.into_arg(), date_to.into_arg()],
        )
        .await?;
    for row in &sched_rows {
        let domain: String = row.try_get("domain")?;
        let content_type: String = row.try_get("content_type")?;
        let content_id: String = row.try_get("content_id")?;
        let channel: String = row.try_get("channel")?;
        let at: DateTime<Utc> = row.try_get("scheduled_at")?;
        items.push(json!({
            "kind": "scheduled",
            "domain": domain,
            "content_type": content_type,
            "id": content_id,
            "title": Value::Null,
            "status": Value::Null,
            "channel": channel,
            "at": at.to_rfc3339(),
        }));
    }
    items.sort_by(|a, b| {
        let ta = a.get("at").and_then(|v| v.as_str()).unwrap_or("");
        let tb = b.get("at").and_then(|v| v.as_str()).unwrap_or("");
        ta.cmp(tb)
    });
    let conflicts = placement_conflicts(db).await?;
    Ok(Json(json!({
        "from": date_from.to_rfc3339(),
        "to": date_to.to_rfc3339(),
        "items": items,
        "conflicts": conflicts,
    })))
}

/// Two active contents in one slot with overlapping windows.
async fn placement_conflicts(db: &Db) -> ApiResult<Vec<Value>> {
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {PLACEMENT_COLS} FROM content_placements WHERE active = TRUE \
                 AND starts_at IS NOT NULL AND ends_at IS NOT NULL \
                 ORDER BY surface ASC, slot ASC"
            ),
            &[],
        )
        .await?;
    let mut by_slot: HashMap<(String, Option<String>), Vec<DynRow>> = HashMap::new();
    for row in rows {
        let surface: String = row.try_get("surface")?;
        let slot: Option<String> = row.try_get("slot")?;
        by_slot.entry((surface, slot)).or_default().push(row);
    }
    let mut conflicts = Vec::new();
    for ((surface, slot), plist) in &by_slot {
        for i in 0..plist.len() {
            for j in (i + 1)..plist.len() {
                let a = &plist[i];
                let b = &plist[j];
                let a_id: Uuid = a.try_get("id")?;
                let b_id: Uuid = b.try_get("id")?;
                if a_id == b_id {
                    continue;
                }
                let a_s: DateTime<Utc> = a.try_get("starts_at")?;
                let a_e: DateTime<Utc> = a.try_get("ends_at")?;
                let b_s: DateTime<Utc> = b.try_get("starts_at")?;
                let b_e: DateTime<Utc> = b.try_get("ends_at")?;
                if a_s < b_e && b_s < a_e {
                    conflicts.push(json!({
                        "surface": surface,
                        "slot": slot,
                        "a": placement_out(a)?,
                        "b": placement_out(b)?,
                    }));
                }
            }
        }
    }
    Ok(conflicts)
}

// --- handlers: transition + policy + audit + AI sources ---------------------

/// `POST /api/v1/admin/content/items/{domain}/{content_type}/{item_id}/transition`.
pub async fn transition_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path((domain, content_type, item_id)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let to = match parsed.get("to").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() && s.chars().count() <= 32 => s.to_string(),
        _ => return Err(validation_err("to", "field is required")),
    };
    let note = match parsed.get("note") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => {
            if s.chars().count() > 2000 {
                return Err(validation_err("note", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("note", "must be a string")),
    };
    let db = state.db_or_500()?;
    let adapter = get_adapter(&domain, &content_type)?;
    if !adapter.supports_transition() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "transition_unsupported",
            format!("نوع {} گردش‌کار §۹ ندارد", adapter.content_type),
        ));
    }
    let detail = get_adapter_item(db, adapter, &item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("not_found", "محتوا یافت نشد"))?;
    let current = detail.status.clone();
    if current == to {
        return Ok(Json(json!({
            "ok": true,
            "skipped": true,
            "reason": "already_in_target_status",
            "from": current,
            "to": to,
        })));
    }
    validate_transition(&current, &to)?;
    let new_status = adapter_transition(db, adapter, &item_id, &to, &ctx).await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let mut new_values = json!({ "workflow_status": new_status });
    if let Some(n) = &note {
        new_values["note"] = json!(n);
    }
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "content.transition",
        adapter.audit_entity_type,
        Some(&item_id),
        Some(json!({ "workflow_status": current })),
        Some(new_values),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(
        json!({ "ok": true, "from": current, "to": new_status }),
    ))
}

/// `PATCH /api/v1/admin/content/items/{domain}/{content_type}/{item_id}/distribution`.
pub async fn set_distribution_policy(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path((domain, content_type, item_id)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let policy = match parsed.get("policy") {
        Some(Value::Object(map)) if !map.is_empty() => map.clone(),
        _ => return Err(validation_err("policy", "must be a non-empty object")),
    };
    let mut enabled_map: HashMap<String, bool> = HashMap::new();
    for (ch, v) in &policy {
        match as_bool(v) {
            Some(b) => {
                enabled_map.insert(ch.clone(), b);
            }
            None => return Err(validation_err("policy", "values must be booleans")),
        }
    }
    let bad: Vec<String> = policy
        .keys()
        .filter(|c| !CHANNELS.contains(&c.as_str()))
        .cloned()
        .collect();
    if !bad.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_channel",
            format!("کانال نامعتبر: {bad:?}"),
        ));
    }
    let scheduled_at = opt_datetime(&parsed, "scheduled_at")?;
    let until_at = opt_datetime(&parsed, "until_at")?;
    let db = state.db_or_500()?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let mut results = Map::new();
    for (channel, enabled) in &enabled_map {
        results.insert(
            channel.clone(),
            apply_distribution(
                db,
                &ctx,
                &domain,
                &content_type,
                &item_id,
                channel,
                *enabled,
                scheduled_at,
                None,
                until_at,
                ip.as_deref(),
                ua.as_deref(),
            )
            .await?,
        );
    }
    Ok(Json(json!({ "policy": results })))
}

/// `GET /api/v1/admin/content/audit` — the audit feed.
pub async fn audit_feed(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 200)?;
    let domain = params.get("domain").filter(|s| !s.is_empty()).cloned();
    let content_type = params
        .get("content_type")
        .filter(|s| !s.is_empty())
        .cloned();
    let item_id = params.get("id").filter(|s| !s.is_empty()).cloned();
    let actor_id = params.get("actor_id").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut conds: Vec<String> = Vec::new();
    let mut args: Vec<Arg> = Vec::new();
    if let Some(d) = &domain {
        let adapter = get_adapter(d, content_type.as_deref().unwrap_or(""))?;
        conds.push("entity_type = ?".to_string());
        args.push(adapter.audit_entity_type.to_string().into_arg());
    }
    if let Some(id) = &item_id {
        conds.push("entity_id = ?".to_string());
        args.push(id.clone().into_arg());
    }
    if let Some(a) = &actor_id {
        let uid = a.parse::<Uuid>().map_err(|_| {
            ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_actor_id",
                "actor_id نامعتبر است",
            )
        })?;
        conds.push("actor_id = ?".to_string());
        args.push(uid.into_arg());
    }
    let mut sql = "SELECT id, actor_id, actor_type, action, entity_type, entity_id, \
         old_values, new_values, ip, created_at FROM audit_logs"
        .to_string();
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push(limit.into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut out = Vec::new();
    for row in &rows {
        let id: Uuid = row.try_get("id")?;
        let actor_id: Option<Uuid> = row.try_get("actor_id")?;
        let actor_type: String = row.try_get("actor_type")?;
        let action: String = row.try_get("action")?;
        let entity_type: String = row.try_get("entity_type")?;
        let entity_id: Option<String> = row.try_get("entity_id")?;
        let old_values: Option<Value> = row.try_get("old_values")?;
        let new_values: Option<Value> = row.try_get("new_values")?;
        let ip: Option<String> = row.try_get("ip")?;
        let created_at: Option<DateTime<Utc>> = row.try_get("created_at")?;
        out.push(json!({
            "id": id.to_string(),
            "actor_id": actor_id.map(|u| u.to_string()),
            "actor_type": actor_type,
            "action": action,
            "entity_type": entity_type,
            "entity_id": entity_id,
            "old_values": old_values,
            "new_values": new_values,
            "ip": ip,
            "created_at": dt_json(created_at),
        }));
    }
    Ok(Json(json!(out)))
}

/// `GET /api/v1/admin/content/ai-sources` — items with an `ai_scope`.
pub async fn list_ai_sources(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 100, 1, 200)?;
    let scope = params.get("scope").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut out: Vec<Value> = Vec::new();
    for adapter in adapters() {
        let items = match list_adapter(
            db,
            adapter,
            &ListFilters {
                q: None,
                status: None,
                author_id: None,
                visibility: None,
                ai_scope: None,
                date_from: None,
                date_to: None,
                cursor_ts: None,
                limit: 50,
            },
        )
        .await
        {
            Ok(items) => items,
            Err(_) => continue,
        };
        for it in items {
            if it.ai_scopes.is_empty() {
                continue;
            }
            if let Some(s) = &scope {
                if !it.ai_scopes.iter().any(|x| x == s) {
                    continue;
                }
            }
            out.push(json!({
                "domain": it.domain,
                "content_type": it.content_type,
                "id": it.id,
                "title": it.title,
                "status": it.status,
                "author_id": it.author_id,
                "ai_scopes": it.ai_scopes,
                "updated_at": dt_json(it.updated_at),
                "reindex_supported": (it.domain.as_str(), it.content_type.as_str())
                    == ("library", "library_item"),
            }));
            if out.len() >= limit as usize {
                return Ok(Json(json!(out)));
            }
        }
    }
    Ok(Json(json!(out)))
}

/// `POST /api/v1/admin/content/ai-sources/{domain}/{content_type}/{item_id}/reindex`
/// — 202, run `queued`, `enqueued: false` (ADR-022 precedent: no ARQ
/// worker in the Rust binary, so nothing is really queued).
pub async fn reindex_ai_source(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path((domain, content_type, item_id)): Path<(String, String, String)>,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers).await?;
    if (domain.as_str(), content_type.as_str()) != ("library", "library_item") {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "reindex_unsupported",
            format!("re-index برای {domain}/{content_type} پشتیبانی نمی‌شود"),
        ));
    }
    let db = state.db_or_500()?;
    let adapter = get_adapter(&domain, &content_type)?;
    let detail = get_adapter_item(db, adapter, &item_id)
        .await?
        .ok_or_else(|| ApiError::not_found("not_found", "محتوا یافت نشد"))?;
    let mut scopes: Vec<String> = if detail.ai_scopes.is_empty() {
        vec!["fatwa".to_string()]
    } else {
        detail
            .ai_scopes
            .iter()
            .filter(|s| AI_SCOPES.contains(&s.as_str()))
            .cloned()
            .collect()
    };
    if scopes.is_empty() {
        scopes = vec!["fatwa".to_string()];
    }
    let run_id = new_id();
    let stats = json!({
        "run_config": {
            "payload": { "library_item_id": item_id },
            "topic_category": Value::Null,
            "madhab_scope": Value::Null,
            "ai_scopes": scopes,
            "source_citation": { "type": "library_item", "id": item_id },
            "triggered_by": ctx.user.id.to_string(),
        },
        "enqueued": false,
    });
    db.execute(
        "INSERT INTO ingestion_runs (id, source_type, status, stats, started_at, finished_at, \
         created_at) VALUES (?, 'file', 'queued', ?, NULL, NULL, ?)",
        &[run_id.into_arg(), stats.into_arg(), utc_now().into_arg()],
    )
    .await?;
    emit_event(
        db,
        "ingestion_run",
        &run_id.to_string(),
        "ingestion.run_triggered",
        json!({ "source_type": "file", "enqueued": false }),
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "content.ai.reindex",
        adapter.audit_entity_type,
        Some(&item_id),
        None,
        Some(json!({ "run_id": run_id.to_string() })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "run_id": run_id.to_string(), "status": "queued" })),
    ))
}
