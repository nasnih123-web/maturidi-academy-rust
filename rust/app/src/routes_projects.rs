//! Projects routes — mirrors `app/modules/projects/` (Wave 8, the lift).
//!
//! Natively implemented: all 17 Python endpoints — project CRUD with
//! cursor pagination and live task counts, task CRUD + kanban `move`
//! with the `TASK_TRANSITIONS` guard, reverse lookup by related entity,
//! staff CRUD (one row per user), the staff-reader gate (an
//! authenticated user who either holds `projects.manage` or has an
//! active `staff_members` row), audit rows and domain events
//! (`project.created`, `task.created`, `task.assigned`,
//! `task.status_changed`, `task.due_soon`), and the due-soon cycle
//! (`run_task_due_soon_cycle`, ported from `due_cron.py` /
//! `service.run_task_due_soon_cycle`).
//!
//! Kill switch: wire under `if unit_native("PROJECTS")` in `lib.rs`
//! (like the other modules); `RUST_ANYTHING_UNIT_PROJECTS=original`
//! is retired at the Wave 8 lift (ADR-133) — it only logs a warning
//! and the routes stay native.
//!
//! Deliberate Wave 8 addition (ADR-142): the due-soon cycle is also
//! exposed as `POST /api/v1/admin/projects/due-soon-cycle` (requires
//! `projects.manage`, returns `{scanned, notified}`). Python has no
//! HTTP route for it — it is an ARQ cron on the notifications queue,
//! every 6 h at minute 0.
//!
//! The unused `project_title` argument of Python's `_notify_task_event`
//! is dropped (ADR-146): the notification titles and bodies never use
//! it, so there is nothing to carry over.

use std::collections::HashMap;
use std::net::SocketAddr;

use axum::{
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, load_user, require_permission, AuthCtx},
    db::{utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    mail::{send_email, send_sms},
    validate::{
        as_bool, opt_bool_loose, opt_date, opt_datetime, opt_localized, opt_str_raw_max, opt_uuid,
        parse_body, parse_datetime_str, parse_path_uuid, parse_query, query_int, req_localized,
    },
    AppState, Config,
};

/// Cursor page cap (Python `Query(le=50)`).
const CURSOR_LIMIT: i64 = 50;

const PROJECT_STATUSES: &[&str] = &["active", "on_hold", "completed", "archived"];
const TASK_STATUSES: &[&str] = &["backlog", "todo", "in_progress", "review", "done"];
const TASK_PRIORITIES: &[&str] = &["low", "medium", "high", "urgent"];
const NOTIF_CHANNELS: &[&str] = &["email", "sms", "push", "in_app"];

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn req_meta(
    state: &AppState,
    headers: &HeaderMap,
    peer: ConnectInfo<SocketAddr>,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer.0), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

// --- cursor (Python `_encode_cursor` / `_decode_cursor`) ------------------

/// Python's projects cursor is `base64url("{iso}|{uuid}")` — the same
/// shape as finance, not the shared JSON-array `validate` cursor.
/// Malformed → 400 `bad_cursor`.
fn encode_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    let raw = format!("{}|{}", created_at.to_rfc3339(), id);
    URL_SAFE.encode(raw.as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
    let invalid = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "bad_cursor",
            "نشانگر صفحه‌بندی نامعتبر است",
        )
    };
    let bytes = URL_SAFE.decode(cursor.as_bytes()).map_err(|_| invalid())?;
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    let (ts_s, id_s) = text.rsplit_once('|').ok_or_else(invalid)?;
    let ts = parse_datetime_str(ts_s).ok_or_else(invalid)?;
    let id = id_s.parse::<Uuid>().map_err(|_| invalid())?;
    Ok((ts, id))
}

fn page_limit(params: &HashMap<String, String>) -> ApiResult<i64> {
    query_int(params, "limit", 20, 1, CURSOR_LIMIT)
}

fn decode_page_cursor(
    params: &HashMap<String, String>,
) -> ApiResult<Option<(DateTime<Utc>, Uuid)>> {
    match params.get("cursor").filter(|s| !s.is_empty()) {
        Some(c) => Ok(Some(decode_cursor(c)?)),
        None => Ok(None),
    }
}

/// `(created_at < ts OR (created_at = ts AND id < lid))` — appended to
/// a `WHERE` clause that already has at least one condition, or to a
/// bare `SELECT ... ` with a `WHERE` added by the caller.
fn cursor_sql() -> &'static str {
    "(created_at < ? OR (created_at = ? AND id < ?))"
}

fn cursor_args(ts: DateTime<Utc>, lid: Uuid) -> [Arg; 3] {
    [ts.into_arg(), ts.into_arg(), lid.into_arg()]
}

// --- auth ------------------------------------------------------------------

/// `require_permission("projects.manage")` without the full-scope /
/// 2FA-enrolment gate: the staff-reader gate only needs to know
/// whether the user holds the permission (Python's `_staff_reader`
/// runs on plain `get_current_user`) — ADR-144.
async fn has_permission(db: &Db, user_id: Uuid, key: &str) -> ApiResult<bool> {
    let row = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
             JOIN role_permissions rp ON rp.role_id = ur.role_id \
             JOIN permissions p ON p.id = rp.permission_id \
             WHERE ur.user_id = ? AND p.key = ? LIMIT 1",
            &[(user_id).into_arg(), (key).into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

/// Read gate: authenticated user who either holds `projects.manage`
/// or has an active row in `staff_members`; else 403 `forbidden`.
async fn staff_reader_ctx(state: &AppState, headers: &HeaderMap, path: &str) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, path).await?;
    let db = state.db_or_500()?;
    if has_permission(db, ctx.user.id, "projects.manage").await? {
        return Ok(ctx);
    }
    let row = db
        .fetch_optional(
            "SELECT id FROM staff_members WHERE user_id = ? AND is_active = TRUE",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    if row.is_some() {
        return Ok(ctx);
    }
    Err(ApiError::new(
        StatusCode::FORBIDDEN,
        "forbidden",
        "دسترسی کافی نیست — فقط اعضای تیم",
    ))
}

/// Write gate: `projects.manage` (full `require_permission`, like the
/// other modules — full scope + privileged-2FA enrolment check).
async fn admin_ctx(state: &AppState, headers: &HeaderMap, path: &str) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, path).await?;
    require_permission(state, &ctx, "projects.manage").await?;
    Ok(ctx)
}

// --- validation helpers ----------------------------------------------------

/// Optional enum body field: absent/null → `None` (caller applies the
/// default); present but outside the set → 422.
fn opt_enum(body: &Value, field: &str, allowed: &[&str]) -> Result<Option<String>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(Some(s.clone())),
        Some(_) => Err(validation_err(field, "value not allowed")),
    }
}

/// Required enum body field → 422 when absent or outside the set.
fn req_enum(body: &Value, field: &str, allowed: &[&str]) -> Result<String, ApiError> {
    match opt_enum(body, field, allowed)? {
        Some(s) => Ok(s),
        None => Err(validation_err(field, "field is required")),
    }
}

/// Optional query enum: absent → `None`; present but outside the set
/// → 422 (FastAPI `Query(pattern=...)`).
fn query_enum(
    params: &HashMap<String, String>,
    field: &str,
    allowed: &[&str],
) -> Result<Option<String>, ApiError> {
    match params.get(field) {
        None => Ok(None),
        Some(s) if allowed.contains(&s.as_str()) => Ok(Some(s.clone())),
        Some(_) => Err(validation_err(field, "value not allowed")),
    }
}

/// Optional query UUID: absent → `None`; present but malformed → 422.
fn query_uuid(params: &HashMap<String, String>, field: &str) -> Result<Option<Uuid>, ApiError> {
    match params.get(field) {
        None => Ok(None),
        Some(s) => s
            .parse::<Uuid>()
            .map(Some)
            .map_err(|_| validation_err(field, "must be a UUID")),
    }
}

/// Required query UUID: absent or malformed → 422.
fn req_query_uuid(params: &HashMap<String, String>, field: &str) -> Result<Uuid, ApiError> {
    match query_uuid(params, field)? {
        Some(u) => Ok(u),
        None => Err(validation_err(field, "field is required")),
    }
}

// --- notifications (mirrors `_notify_task_event`) --------------------------

/// Effective channels = requested ∩ the user's active preferences
/// (`notif_type="product"`, like `notify_service.effective_channels`).
/// A missing row means disabled, except `in_app`, which defaults to
/// enabled.
async fn effective_channels(db: &Db, user_id: Uuid, requested: &[&str]) -> ApiResult<Vec<String>> {
    let rows = db
        .fetch_all(
            "SELECT channel, enabled FROM notification_preferences \
             WHERE user_id = ? AND notif_type = 'product'",
            &[user_id.into_arg()],
        )
        .await?;
    let mut pref: HashMap<String, bool> = HashMap::new();
    for row in rows {
        let channel: String = row.try_get("channel")?;
        let enabled: bool = row.try_get("enabled")?;
        pref.insert(channel, enabled);
    }
    let mut out = Vec::new();
    for ch in requested {
        if !NOTIF_CHANNELS.contains(ch) {
            continue;
        }
        let enabled = pref.get(*ch).copied().unwrap_or_else(|| *ch == "in_app");
        if enabled {
            out.push(ch.to_string());
        }
    }
    Ok(out)
}

fn env_trimmed(key: &str) -> String {
    std::env::var(key).unwrap_or_default().trim().to_string()
}

/// Mirrors `notify_service.CHANNEL_CONFIGURED` (same env-backed shape
/// as the admin broadcast's `channel_configured`).
fn channel_configured(key: &str) -> bool {
    match key {
        "email" => !env_trimmed("SMTP_HOST").is_empty(),
        "sms" => !env_trimmed("SMS_API_KEY").is_empty(),
        "push" => false,
        "in_app" => true,
        _ => false,
    }
}

/// Notify the assignee about `kind` (`"assigned"` | `"due_soon"`),
/// honouring `notification_preferences`. `in_app` writes a
/// `notifications` row (`notif_type="task"`, trilingual title, body =
/// the Persian string only, `data={source:"pms", kind, task_id,
/// project_id}`); the other channels go through the existing sender
/// dispatch. Returns whether at least one channel delivered. Missing
/// or inactive users deliver nothing.
///
/// Email/SMS honesty note (ADR-158): delivery is real now — a
/// channel counts as delivered only when the send returns `Ok`; a
/// failed send (unconfigured or undeliverable) is an honest
/// non-delivery, and a missing address/phone likewise.
async fn notify_task_event(
    db: &Db,
    config: &Config,
    assignee_id: Uuid,
    kind: &str,
    task: &Task,
) -> ApiResult<bool> {
    let Some(user) = load_user(db, assignee_id).await? else {
        return Ok(false);
    };
    if !user.is_active {
        return Ok(false);
    }
    let channels = effective_channels(db, user.id, NOTIF_CHANNELS).await?;
    if channels.is_empty() {
        return Ok(false);
    }
    let fa_title = task.title.get("fa").and_then(|v| v.as_str()).unwrap_or("");
    let (titles, bodies): (Value, Value) = if kind == "assigned" {
        (
            json!({
                "fa": "تسک جدید به شما محول شد",
                "ar": "تم إسناد مهمة جديدة إليك",
                "en": "A new task was assigned to you",
            }),
            json!({
                "fa": format!("«{fa_title}» به شما محول شد."),
                "ar": format!("تم إسناد «{fa_title}» إليك."),
                "en": format!("«{fa_title}» was assigned to you."),
            }),
        )
    } else {
        (
            json!({
                "fa": "سررسید تسک نزدیک است",
                "ar": "موعد المهمة قريب",
                "en": "Task deadline is approaching",
            }),
            json!({
                "fa": format!("سررسید «{fa_title}» کمتر از ۲۴ ساعت دیگر است."),
                "ar": format!("موعد «{fa_title}» خلال أقل من ٢٤ ساعة."),
                "en": format!("«{fa_title}» is due within 24 hours."),
            }),
        )
    };
    let body_fa = bodies
        .get("fa")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let title_fa = titles
        .get("fa")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let mut delivered: Vec<String> = Vec::new();
    for ch in &channels {
        if !channel_configured(ch) {
            continue;
        }
        match ch.as_str() {
            "in_app" => {
                db.execute(
                    "INSERT INTO notifications (id, user_id, notif_type, title, body, data, \
                     read_at, created_at) VALUES (?, ?, 'task', ?, ?, ?, NULL, ?)",
                    &[
                        (Uuid::new_v4()).into_arg(),
                        (user.id).into_arg(),
                        (titles.clone()).into_arg(),
                        (body_fa.clone()).into_arg(),
                        (json!({
                            "source": "pms",
                            "kind": kind,
                            "task_id": task.id.to_string(),
                            "project_id": task.project_id.to_string(),
                        }))
                        .into_arg(),
                        (utc_now()).into_arg(),
                    ],
                )
                .await?;
                delivered.push(ch.clone());
            }
            "email" => {
                if let Some(email) = &user.email {
                    if send_email(config, email, &title_fa, &body_fa, "")
                        .await
                        .is_ok()
                    {
                        delivered.push(ch.clone());
                    }
                }
            }
            "sms" => {
                if let Some(phone) = &user.phone {
                    let text: String = format!("{title_fa}\n{body_fa}").chars().take(500).collect();
                    if send_sms(config, phone, &text).await.is_ok() {
                        delivered.push(ch.clone());
                    }
                }
            }
            // "push" is never configured, so this arm is unreachable
            // in practice; kept for symmetry with CHANNEL_SENDERS.
            _ => {}
        }
    }
    Ok(!delivered.is_empty())
}

// --- rows ------------------------------------------------------------------

struct Project {
    id: Uuid,
    title: Value,
    status: String,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
}

impl Project {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            title: row.try_get::<Value, _>("title")?,
            status: row.try_get::<String, _>("status")?,
            created_by: row.try_get::<Option<Uuid>, _>("created_by")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
            updated_at: row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?,
        })
    }

    fn out(&self, task_count: i64) -> Value {
        json!({
            "id": self.id.to_string(),
            "title": self.title,
            "status": self.status,
            "created_by": self.created_by.map(|u| u.to_string()),
            "created_at": self.created_at.to_rfc3339(),
            "updated_at": self.updated_at.map(|t| t.to_rfc3339()),
            "task_count": task_count,
        })
    }
}

async fn get_project_row(db: &Db, project_id: Uuid) -> ApiResult<Project> {
    let row = db
        .fetch_optional(
            "SELECT id, title, status, created_by, created_at, updated_at \
             FROM projects WHERE id = ?",
            &[(project_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Project::from_row(&r)?),
        None => Err(ApiError::not_found("project_not_found", "پروژه یافت نشد")),
    }
}

async fn project_task_count(db: &Db, project_id: Uuid) -> ApiResult<i64> {
    let row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM tasks WHERE project_id = ?",
            &[(project_id).into_arg()],
        )
        .await?;
    row.try_get::<i64, _>("n")
}

async fn project_out(db: &Db, p: &Project) -> ApiResult<Value> {
    let cnt = project_task_count(db, p.id).await?;
    Ok(p.out(cnt))
}

struct Task {
    id: Uuid,
    project_id: Uuid,
    title: Value,
    description: Option<Value>,
    assignee_id: Option<Uuid>,
    status: String,
    due_date: Option<DateTime<Utc>>,
    priority: String,
    related_entity_type: Option<String>,
    related_entity_id: Option<Uuid>,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
}

const TASK_COLUMNS: &str = "id, project_id, title, description, assignee_id, status, \
    due_date, priority, related_entity_type, related_entity_id, \
    created_by, created_at, updated_at";

impl Task {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            project_id: row.try_get::<Uuid, _>("project_id")?,
            title: row.try_get::<Value, _>("title")?,
            description: row.try_get::<Option<Value>, _>("description")?,
            assignee_id: row.try_get::<Option<Uuid>, _>("assignee_id")?,
            status: row.try_get::<String, _>("status")?,
            due_date: row.try_get::<Option<DateTime<Utc>>, _>("due_date")?,
            priority: row.try_get::<String, _>("priority")?,
            related_entity_type: row.try_get::<Option<String>, _>("related_entity_type")?,
            related_entity_id: row.try_get::<Option<Uuid>, _>("related_entity_id")?,
            created_by: row.try_get::<Option<Uuid>, _>("created_by")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
            updated_at: row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?,
        })
    }

    fn out(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "project_id": self.project_id.to_string(),
            "title": self.title,
            "description": self.description,
            "assignee_id": self.assignee_id.map(|u| u.to_string()),
            "status": self.status,
            "due_date": self.due_date.map(|t| t.to_rfc3339()),
            "priority": self.priority,
            "related_entity_type": self.related_entity_type,
            "related_entity_id": self.related_entity_id.map(|u| u.to_string()),
            "created_by": self.created_by.map(|u| u.to_string()),
            "created_at": self.created_at.to_rfc3339(),
            "updated_at": self.updated_at.map(|t| t.to_rfc3339()),
        })
    }
}

async fn get_task_row(db: &Db, task_id: Uuid) -> ApiResult<Task> {
    let row = db
        .fetch_optional(
            &format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id = ?"),
            &[(task_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Task::from_row(&r)?),
        None => Err(ApiError::not_found("task_not_found", "تسک یافت نشد")),
    }
}

async fn require_assignee(db: &Db, assignee_id: Option<Uuid>) -> ApiResult<()> {
    let Some(assignee_id) = assignee_id else {
        return Ok(());
    };
    let user = load_user(db, assignee_id).await?;
    match user {
        Some(u) if u.is_active => Ok(()),
        _ => Err(ApiError::not_found(
            "assignee_not_found",
            "کاربر محول‌شونده یافت نشد",
        )),
    }
}

struct StaffMember {
    id: Uuid,
    user_id: Uuid,
    title: Value,
    department: Value,
    joined_date: Option<NaiveDate>,
    is_active: bool,
    created_at: DateTime<Utc>,
    updated_at: Option<DateTime<Utc>>,
}

impl StaffMember {
    fn from_row(row: &DynRow) -> Result<Self, ApiError> {
        Ok(Self {
            id: row.try_get::<Uuid, _>("id")?,
            user_id: row.try_get::<Uuid, _>("user_id")?,
            title: row.try_get::<Value, _>("title")?,
            department: row.try_get::<Value, _>("department")?,
            joined_date: row.try_get::<Option<NaiveDate>, _>("joined_date")?,
            is_active: row.try_get::<bool, _>("is_active")?,
            created_at: row.try_get::<DateTime<Utc>, _>("created_at")?,
            updated_at: row.try_get::<Option<DateTime<Utc>>, _>("updated_at")?,
        })
    }

    fn out(&self) -> Value {
        json!({
            "id": self.id.to_string(),
            "user_id": self.user_id.to_string(),
            "title": self.title,
            "department": self.department,
            "joined_date": self.joined_date.map(|d| d.format("%Y-%m-%d").to_string()),
            "is_active": self.is_active,
            "created_at": self.created_at.to_rfc3339(),
            "updated_at": self.updated_at.map(|t| t.to_rfc3339()),
        })
    }
}

async fn get_staff_row(db: &Db, staff_id: Uuid) -> ApiResult<StaffMember> {
    let row = db
        .fetch_optional(
            "SELECT id, user_id, title, department, joined_date, is_active, created_at, \
             updated_at FROM staff_members WHERE id = ?",
            &[(staff_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(StaffMember::from_row(&r)?),
        None => Err(ApiError::not_found("staff_not_found", "عضو تیم یافت نشد")),
    }
}

// --- projects ---------------------------------------------------------------

/// `GET /api/v1/projects` — cursor page, `created_at DESC, id DESC`.
pub async fn list_projects(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/projects").await?;
    let params = parse_query(&uri);
    let limit = page_limit(&params)?;
    let cursor = decode_page_cursor(&params)?;
    let db = state.db_or_500()?;
    let mut sql =
        String::from("SELECT id, title, status, created_by, created_at, updated_at FROM projects");
    let mut args: Vec<Arg> = Vec::new();
    if let Some((ts, lid)) = cursor {
        sql.push_str(&format!(" WHERE {}", cursor_sql()));
        args.extend(cursor_args(ts, lid));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut keys: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
    for row in rows {
        let p = Project::from_row(&row)?;
        keys.push((p.created_at, p.id));
        items.push(project_out(db, &p).await?);
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `POST /api/v1/projects` → 201, `task_count` 0 without querying.
pub async fn create_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let parsed = parse_body(&body)?;
    let title = req_localized(&parsed, "title")?;
    let status =
        opt_enum(&parsed, "status", PROJECT_STATUSES)?.unwrap_or_else(|| "active".to_string());
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO projects (id, title, status, created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, NULL)",
        &[
            (id).into_arg(),
            (title.clone()).into_arg(),
            (&status).into_arg(),
            (ctx.user.id).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.project.create",
        "project",
        Some(&id.to_string()),
        None,
        Some(json!({ "title": title, "status": status })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    emit_event(
        db,
        "project",
        &id.to_string(),
        "project.created",
        json!({
            "project_id": id.to_string(),
            "title": title,
            "status": status,
        }),
    )
    .await?;
    let p = get_project_row(db, id).await?;
    Ok((StatusCode::CREATED, Json(p.out(0))))
}

/// `GET /api/v1/projects/{project_id}` — live `task_count`.
pub async fn get_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/projects").await?;
    let id = parse_path_uuid(&project_id, "project_id")?;
    let db = state.db_or_500()?;
    let p = get_project_row(db, id).await?;
    Ok(Json(project_out(db, &p).await?))
}

/// `PATCH /api/v1/projects/{project_id}` — sets `updated_at=now`.
pub async fn update_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(project_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let parsed = parse_body(&body)?;
    let title = opt_localized(&parsed, "title")?;
    let status = opt_enum(&parsed, "status", PROJECT_STATUSES)?;
    let id = parse_path_uuid(&project_id, "project_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let p = get_project_row(db, id).await?;
    let old = json!({ "title": p.title, "status": p.status });
    let new_title = title.clone().unwrap_or_else(|| p.title.clone());
    let new_status = status.clone().unwrap_or_else(|| p.status.clone());
    let now = utc_now();
    db.execute(
        "UPDATE projects SET title = ?, status = ?, updated_at = ? WHERE id = ?",
        &[
            (new_title.clone()).into_arg(),
            (new_status.clone()).into_arg(),
            (now).into_arg(),
            (id).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.project.update",
        "project",
        Some(&id.to_string()),
        Some(old),
        Some(json!({ "title": new_title, "status": new_status })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let p = get_project_row(db, id).await?;
    Ok(Json(project_out(db, &p).await?))
}

/// `DELETE /api/v1/projects/{project_id}` → 204; deletes all its
/// tasks first, explicit loop (no reliance on DB cascade).
pub async fn delete_project(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(project_id): Path<String>,
) -> ApiResult<StatusCode> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let id = parse_path_uuid(&project_id, "project_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let p = get_project_row(db, id).await?;
    let task_rows = db
        .fetch_all(
            "SELECT id FROM tasks WHERE project_id = ?",
            &[(id).into_arg()],
        )
        .await?;
    let mut deleted_tasks = 0i64;
    for row in &task_rows {
        let tid: Uuid = row.try_get("id")?;
        db.execute("DELETE FROM tasks WHERE id = ?", &[(tid).into_arg()])
            .await?;
        deleted_tasks += 1;
    }
    db.execute("DELETE FROM projects WHERE id = ?", &[(id).into_arg()])
        .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.project.delete",
        "project",
        Some(&id.to_string()),
        Some(json!({
            "title": p.title,
            "status": p.status,
            "deleted_tasks": deleted_tasks,
        })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- tasks -----------------------------------------------------------------

/// `GET /api/v1/projects/tasks/by-entity` — reverse lookup, no
/// pagination. Register before `/tasks/{task_id}` so `by-entity` is
/// not captured as a task id.
pub async fn tasks_by_entity(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/projects").await?;
    let params = parse_query(&uri);
    let related_entity_type = match params.get("related_entity_type") {
        Some(s) if !s.is_empty() => s.clone(),
        _ => return Err(validation_err("related_entity_type", "field is required")),
    };
    let related_entity_id = req_query_uuid(&params, "related_entity_id")?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE related_entity_type = ? AND related_entity_id = ? \
                 ORDER BY created_at DESC, id DESC"
            ),
            &[
                (related_entity_type).into_arg(),
                (related_entity_id).into_arg(),
            ],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(Task::from_row(&row)?.out());
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/projects/{project_id}/tasks` — 404 if the project is
/// missing; optional `status` / `assignee_id` filters + cursor page.
pub async fn list_tasks(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/projects").await?;
    let params = parse_query(&uri);
    let limit = page_limit(&params)?;
    let cursor = decode_page_cursor(&params)?;
    let status = query_enum(&params, "status", TASK_STATUSES)?;
    let assignee_id = query_uuid(&params, "assignee_id")?;
    let pid = parse_path_uuid(&project_id, "project_id")?;
    let db = state.db_or_500()?;
    get_project_row(db, pid).await?;
    let mut sql = format!("SELECT {TASK_COLUMNS} FROM tasks WHERE project_id = ?");
    let mut args: Vec<Arg> = vec![(pid).into_arg()];
    if let Some(status) = &status {
        sql.push_str(" AND status = ?");
        args.push(status.into_arg());
    }
    if let Some(assignee_id) = assignee_id {
        sql.push_str(" AND assignee_id = ?");
        args.push(assignee_id.into_arg());
    }
    if let Some((ts, lid)) = cursor {
        sql.push_str(&format!(" AND {}", cursor_sql()));
        args.extend(cursor_args(ts, lid));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut keys: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
    for row in rows {
        let t = Task::from_row(&row)?;
        keys.push((t.created_at, t.id));
        items.push(t.out());
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `POST /api/v1/projects/{project_id}/tasks` → 201; 404
/// `assignee_not_found` when the assignee is missing or inactive.
pub async fn create_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(project_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let parsed = parse_body(&body)?;
    let title = req_localized(&parsed, "title")?;
    let description = opt_localized(&parsed, "description")?;
    let assignee_id = opt_uuid(&parsed, "assignee_id")?;
    let status =
        opt_enum(&parsed, "status", TASK_STATUSES)?.unwrap_or_else(|| "backlog".to_string());
    let due_date = opt_datetime(&parsed, "due_date")?;
    let priority =
        opt_enum(&parsed, "priority", TASK_PRIORITIES)?.unwrap_or_else(|| "medium".to_string());
    let related_entity_type = opt_str_raw_max(&parsed, "related_entity_type", 64)?;
    let related_entity_id = opt_uuid(&parsed, "related_entity_id")?;
    let pid = parse_path_uuid(&project_id, "project_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    get_project_row(db, pid).await?;
    require_assignee(db, assignee_id).await?;
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO tasks (id, project_id, title, description, assignee_id, status, \
         due_date, priority, related_entity_type, related_entity_id, due_notified_at, \
         created_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, NULL)",
        &[
            (id).into_arg(),
            (pid).into_arg(),
            (title.clone()).into_arg(),
            (description.clone()).into_arg(),
            (assignee_id).into_arg(),
            (&status).into_arg(),
            (due_date).into_arg(),
            (&priority).into_arg(),
            (related_entity_type.clone()).into_arg(),
            (related_entity_id).into_arg(),
            (ctx.user.id).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.task.create",
        "task",
        Some(&id.to_string()),
        None,
        Some(json!({
            "project_id": pid.to_string(),
            "title": title,
            "status": status,
            "assignee_id": assignee_id.map(|u| u.to_string()),
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    emit_event(
        db,
        "task",
        &id.to_string(),
        "task.created",
        json!({
            "task_id": id.to_string(),
            "project_id": pid.to_string(),
            "title": title,
            "status": status,
            "assignee_id": assignee_id.map(|u| u.to_string()),
        }),
    )
    .await?;
    if let Some(assignee) = assignee_id {
        assign_task_side_effects(
            db,
            &state.config,
            &ctx,
            id,
            assignee,
            None,
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
    }
    let t = get_task_row(db, id).await?;
    Ok((StatusCode::CREATED, Json(t.out())))
}

/// Shared assign side effects (create with assignee / assignee change
/// in PATCH): audit `projects.task.assign` + `task.assigned` event +
/// the notification helper. `old_values` is `None` on create (Python
/// passes no `old_values` there) and `{"assignee_id": ...}` on PATCH.
/// Eight arguments, allowed like `finalize_success`: db, config,
/// actor, task, assignee, old values and the audit ip/ua are all
/// genuinely needed here.
#[allow(clippy::too_many_arguments)]
async fn assign_task_side_effects(
    db: &Db,
    config: &Config,
    ctx: &AuthCtx,
    task_id: Uuid,
    new_assignee: Uuid,
    old_values: Option<Value>,
    ip: Option<&str>,
    ua: Option<&str>,
) -> ApiResult<()> {
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.task.assign",
        "task",
        Some(&task_id.to_string()),
        old_values,
        Some(json!({ "assignee_id": new_assignee.to_string() })),
        ip,
        ua,
    )
    .await?;
    emit_event(
        db,
        "task",
        &task_id.to_string(),
        "task.assigned",
        json!({
            "task_id": task_id.to_string(),
            "assignee_id": new_assignee.to_string(),
            "by": ctx.user.id.to_string(),
        }),
    )
    .await?;
    let task = get_task_row(db, task_id).await?;
    notify_task_event(db, config, new_assignee, "assigned", &task).await?;
    Ok(())
}

/// `GET /api/v1/projects/tasks/{task_id}`.
pub async fn get_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(task_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/projects").await?;
    let id = parse_path_uuid(&task_id, "task_id")?;
    let db = state.db_or_500()?;
    let t = get_task_row(db, id).await?;
    Ok(Json(t.out()))
}

/// `PATCH /api/v1/projects/tasks/{task_id}` — `assignee_id` changes
/// ONLY when the key is present (present-null = unassign, absent =
/// keep); `due_date` / `description` / `related_*` change only when
/// non-null, so they CANNOT be cleared via PATCH.
pub async fn update_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(task_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let parsed = parse_body(&body)?;
    let title = opt_localized(&parsed, "title")?;
    let description = opt_localized(&parsed, "description")?;
    let due_date = opt_datetime(&parsed, "due_date")?;
    let priority = opt_enum(&parsed, "priority", TASK_PRIORITIES)?;
    let related_entity_type = opt_str_raw_max(&parsed, "related_entity_type", 64)?;
    let related_entity_id = opt_uuid(&parsed, "related_entity_id")?;
    // Presence (not value) decides the assignee: null present =
    // unassign, absent = keep.
    let assignee_key_present = parsed
        .as_object()
        .map(|o| o.contains_key("assignee_id"))
        .unwrap_or(false);
    let assignee_id = opt_uuid(&parsed, "assignee_id")?;
    let id = parse_path_uuid(&task_id, "task_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let t = get_task_row(db, id).await?;
    let old_assignee = t.assignee_id;
    let new_assignee = if assignee_key_present {
        assignee_id
    } else {
        old_assignee
    };
    if new_assignee != old_assignee && new_assignee.is_some() {
        require_assignee(db, new_assignee).await?;
    }
    let old = json!({
        "title": t.title,
        "description": t.description,
        "assignee_id": old_assignee.map(|u| u.to_string()),
        "due_date": t.due_date.map(|d| d.to_rfc3339()),
        "priority": t.priority,
        "related_entity_type": t.related_entity_type,
        "related_entity_id": t.related_entity_id.map(|u| u.to_string()),
    });
    let new_title = title.clone().unwrap_or_else(|| t.title.clone());
    let new_description = description.clone().or_else(|| t.description.clone());
    let new_due_date = due_date.or(t.due_date);
    let new_priority = priority.clone().unwrap_or_else(|| t.priority.clone());
    let new_related_type = related_entity_type
        .clone()
        .or_else(|| t.related_entity_type.clone());
    let new_related_id = related_entity_id.or(t.related_entity_id);
    let now = utc_now();
    db.execute(
        "UPDATE tasks SET title = ?, description = ?, assignee_id = ?, due_date = ?, \
         priority = ?, related_entity_type = ?, related_entity_id = ?, updated_at = ? \
         WHERE id = ?",
        &[
            (new_title.clone()).into_arg(),
            (new_description.clone()).into_arg(),
            (new_assignee).into_arg(),
            (new_due_date).into_arg(),
            (new_priority.clone()).into_arg(),
            (new_related_type.clone()).into_arg(),
            (new_related_id).into_arg(),
            (now).into_arg(),
            (id).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.task.update",
        "task",
        Some(&id.to_string()),
        Some(old),
        Some(json!({
            "title": new_title,
            "assignee_id": new_assignee.map(|u| u.to_string()),
            "priority": new_priority,
            "status": t.status,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    if let Some(new) = new_assignee.filter(|n| Some(*n) != old_assignee) {
        assign_task_side_effects(
            db,
            &state.config,
            &ctx,
            id,
            new,
            Some(json!({ "assignee_id": old_assignee.map(|u| u.to_string()) })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
    }
    let t = get_task_row(db, id).await?;
    Ok(Json(t.out()))
}

/// `DELETE /api/v1/projects/tasks/{task_id}` → 204.
pub async fn delete_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(task_id): Path<String>,
) -> ApiResult<StatusCode> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let id = parse_path_uuid(&task_id, "task_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let t = get_task_row(db, id).await?;
    db.execute("DELETE FROM tasks WHERE id = ?", &[(id).into_arg()])
        .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.task.delete",
        "task",
        Some(&id.to_string()),
        Some(json!({
            "title": t.title,
            "status": t.status,
            "project_id": t.project_id.to_string(),
        })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Allowed kanban edges (Python `schemas.TASK_TRANSITIONS`).
fn allowed_transitions(from: &str) -> &'static [&'static str] {
    match from {
        "backlog" => &["todo"],
        "todo" => &["in_progress", "backlog"],
        "in_progress" => &["review", "todo"],
        "review" => &["done", "in_progress"],
        "done" => &["todo"],
        _ => &[],
    }
}

/// `POST /api/v1/projects/tasks/{task_id}/move` — illegal edge → 422
/// `invalid_status_transition` with `{from, to, allowed}`.
pub async fn move_task(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(task_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/projects").await?;
    let parsed = parse_body(&body)?;
    let to = req_enum(&parsed, "status", TASK_STATUSES)?;
    let id = parse_path_uuid(&task_id, "task_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let t = get_task_row(db, id).await?;
    let allowed = allowed_transitions(&t.status);
    if !allowed.contains(&to.as_str()) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_status_transition",
            format!("گذار از {} به {} مجاز نیست", t.status, to),
            json!({
                "from": t.status,
                "to": to,
                "allowed": allowed,
            }),
        ));
    }
    let from = t.status.clone();
    let now = utc_now();
    db.execute(
        "UPDATE tasks SET status = ?, updated_at = ? WHERE id = ?",
        &[(to.clone()).into_arg(), (now).into_arg(), (id).into_arg()],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.task.move",
        "task",
        Some(&id.to_string()),
        Some(json!({ "status": from })),
        Some(json!({ "status": to })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    emit_event(
        db,
        "task",
        &id.to_string(),
        "task.status_changed",
        json!({
            "task_id": id.to_string(),
            "from": from,
            "to": to,
            "by": ctx.user.id.to_string(),
        }),
    )
    .await?;
    let t = get_task_row(db, id).await?;
    Ok(Json(t.out()))
}

// --- staff -----------------------------------------------------------------

/// `GET /api/v1/staff` — `active_only=true` by default, `created_at
/// DESC` (Python orders by `created_at` only, no id tiebreak).
pub async fn list_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/staff").await?;
    let params = parse_query(&uri);
    let active_only = match params.get("active_only") {
        None => true,
        Some(v) => as_bool(&Value::String(v.clone()))
            .ok_or_else(|| validation_err("active_only", "must be a boolean"))?,
    };
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, user_id, title, department, joined_date, is_active, created_at, \
         updated_at FROM staff_members",
    );
    if active_only {
        sql.push_str(" WHERE is_active = TRUE");
    }
    sql.push_str(" ORDER BY created_at DESC");
    let rows = db.fetch_all(&sql, &[]).await?;
    let mut out = Vec::new();
    for row in rows {
        out.push(StaffMember::from_row(&row)?.out());
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/staff` → 201; 404 `user_not_found` for a missing or
/// inactive user; 409 `staff_exists` — one row per user.
pub async fn create_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/staff").await?;
    let parsed = parse_body(&body)?;
    let user_id = match opt_uuid(&parsed, "user_id")? {
        Some(u) => u,
        None => return Err(validation_err("user_id", "field is required")),
    };
    let title = req_localized(&parsed, "title")?;
    let department = req_localized(&parsed, "department")?;
    let joined_date = opt_date(&parsed, "joined_date")?;
    let is_active = opt_bool_loose(&parsed, "is_active")?.unwrap_or(true);
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let user = load_user(db, user_id).await?;
    match user {
        Some(u) if u.is_active => {}
        _ => {
            return Err(ApiError::not_found("user_not_found", "کاربر یافت نشد"));
        }
    }
    let existing = db
        .fetch_optional(
            "SELECT id FROM staff_members WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    if existing.is_some() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "staff_exists",
            "این کاربر قبلاً عضو تیم است",
        ));
    }
    let id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO staff_members (id, user_id, title, department, joined_date, \
         is_active, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, NULL)",
        &[
            (id).into_arg(),
            (user_id).into_arg(),
            (title.clone()).into_arg(),
            (department.clone()).into_arg(),
            (joined_date).into_arg(),
            (is_active).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.staff.create",
        "staff_member",
        Some(&id.to_string()),
        None,
        Some(json!({
            "user_id": user_id.to_string(),
            "title": title,
            "department": department,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let s = get_staff_row(db, id).await?;
    Ok((StatusCode::CREATED, Json(s.out())))
}

/// `GET /api/v1/staff/{staff_id}`.
pub async fn get_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(staff_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let _ctx = staff_reader_ctx(&state, &headers, "/api/v1/staff").await?;
    let id = parse_path_uuid(&staff_id, "staff_id")?;
    let db = state.db_or_500()?;
    let s = get_staff_row(db, id).await?;
    Ok(Json(s.out()))
}

/// `PATCH /api/v1/staff/{staff_id}` — fields change only when
/// non-null; sets `updated_at=now`.
pub async fn update_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(staff_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/staff").await?;
    let parsed = parse_body(&body)?;
    let title = opt_localized(&parsed, "title")?;
    let department = opt_localized(&parsed, "department")?;
    let joined_date = opt_date(&parsed, "joined_date")?;
    let is_active = opt_bool_loose(&parsed, "is_active")?;
    let id = parse_path_uuid(&staff_id, "staff_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let s = get_staff_row(db, id).await?;
    let old = json!({
        "title": s.title,
        "department": s.department,
        "is_active": s.is_active,
    });
    let new_title = title.clone().unwrap_or_else(|| s.title.clone());
    let new_department = department.clone().unwrap_or_else(|| s.department.clone());
    let new_joined = joined_date.or(s.joined_date);
    let new_active = is_active.unwrap_or(s.is_active);
    let now = utc_now();
    db.execute(
        "UPDATE staff_members SET title = ?, department = ?, joined_date = ?, \
         is_active = ?, updated_at = ? WHERE id = ?",
        &[
            (new_title.clone()).into_arg(),
            (new_department.clone()).into_arg(),
            (new_joined).into_arg(),
            (new_active).into_arg(),
            (now).into_arg(),
            (id).into_arg(),
        ],
    )
    .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.staff.update",
        "staff_member",
        Some(&id.to_string()),
        Some(old),
        Some(json!({
            "title": new_title,
            "department": new_department,
            "is_active": new_active,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let s = get_staff_row(db, id).await?;
    Ok(Json(s.out()))
}

/// `DELETE /api/v1/staff/{staff_id}` → 204.
pub async fn delete_staff(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<SocketAddr>,
    Path(staff_id): Path<String>,
) -> ApiResult<StatusCode> {
    let ctx = admin_ctx(&state, &headers, "/api/v1/staff").await?;
    let id = parse_path_uuid(&staff_id, "staff_id")?;
    let db = state.db_or_500()?;
    let (ip, ua) = req_meta(&state, &headers, peer);
    let s = get_staff_row(db, id).await?;
    db.execute("DELETE FROM staff_members WHERE id = ?", &[(id).into_arg()])
        .await?;
    log_audit(
        db,
        Some(ctx.user.id),
        "staff",
        "projects.staff.delete",
        "staff_member",
        Some(&id.to_string()),
        Some(json!({ "user_id": s.user_id.to_string(), "title": s.title })),
        None,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

// --- due-soon cycle (port of `due_cron.py` / `service.py`) -----------------

/// `run_task_due_soon_cycle(now)` — the ARQ cron's logic as a native
/// function, testable with an injected `now`.
///
/// Candidates: `due_date` NOT NULL AND `due_date <= now + 24h` AND
/// `status != 'done'` AND (`due_notified_at` IS NULL OR
/// `due_notified_at <= now - 20h`). Every candidate gets
/// `due_notified_at = now` — even unassigned rows, so a run never
/// re-scans them. Assigned candidates get a `due_soon` notification
/// plus a `task.due_soon` event `{task_id, due_date, assignee_id,
/// delivered}`; `notified` counts only rows where at least one
/// channel delivered. Returns `(scanned, notified)`.
///
/// Single-commit note (ADR-145): Python commits once at the end; the
/// Rust `Db` has no transaction API, so statements apply one by one
/// like every other module — the observable end state is identical,
/// but a mid-cycle crash would leave a partial stamp instead of
/// rolling back.
pub async fn run_task_due_soon_cycle(
    db: &Db,
    config: &Config,
    now: DateTime<Utc>,
) -> ApiResult<(i64, i64)> {
    let window_end = now + chrono::Duration::hours(24);
    let renotify_before = now - chrono::Duration::hours(20);
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {TASK_COLUMNS} FROM tasks \
                 WHERE due_date IS NOT NULL AND due_date <= ? AND status != 'done' \
                 AND (due_notified_at IS NULL OR due_notified_at <= ?)"
            ),
            &[(window_end).into_arg(), (renotify_before).into_arg()],
        )
        .await?;
    let scanned = rows.len() as i64;
    let mut notified: i64 = 0;
    for row in rows {
        let t = Task::from_row(&row)?;
        if t.due_date.is_none() {
            continue;
        }
        db.execute(
            "UPDATE tasks SET due_notified_at = ? WHERE id = ?",
            &[(now).into_arg(), (t.id).into_arg()],
        )
        .await?;
        if let Some(assignee_id) = t.assignee_id {
            let delivered = notify_task_event(db, config, assignee_id, "due_soon", &t).await?;
            if delivered {
                notified += 1;
            }
            emit_event(
                db,
                "task",
                &t.id.to_string(),
                "task.due_soon",
                json!({
                    "task_id": t.id.to_string(),
                    "due_date": t.due_date.map(|d| d.to_rfc3339()),
                    "assignee_id": assignee_id.to_string(),
                    "delivered": delivered,
                }),
            )
            .await?;
        }
    }
    Ok((scanned, notified))
}

/// `POST /api/v1/admin/projects/due-soon-cycle` — deliberate Wave 8
/// addition (ADR-142): runs the cycle on demand, requires
/// `projects.manage`, returns `{scanned, notified}`.
pub async fn due_soon_cycle(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers, "/api/v1/admin/projects/due-soon-cycle").await?;
    let db = state.db_or_500()?;
    let (scanned, notified) = run_task_due_soon_cycle(db, &state.config, utc_now()).await?;
    Ok(Json(json!({ "scanned": scanned, "notified": notified })))
}
