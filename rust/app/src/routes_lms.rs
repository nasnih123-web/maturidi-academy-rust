//! LMS routes (Wave 6) — courses, curricula, lessons, progress,
//! lesson quizzes, study reports, recommendations, cohorts,
//! enrollments, prerequisites and certificates. Ported from
//! `apps/api/app/modules/lms/` (router.py, service.py, schemas.py,
//! certificates.py) and the audio-lessons spec (teacher lessons,
//! students, groups, verifiable PDF certificate per course level).
//!
//! Certificate PDF: reportlab cannot run inside the Rust binary
//! (ADR-008). The certificate *record* (code, holder, course,
//! issued_at, domain events) is issued natively on course completion;
//! the PDF bytes are rendered by a Python sidecar that fills in
//! `pdf_storage_ref` (see the ADR-051 draft in the wave report).
//! Download routes serve only bytes actually stored under the
//! certificate's `pdf_storage_ref` — a certificate whose PDF has not
//! been rendered yet answers 404 `certificate_not_found`, exactly
//! like Python when `pdf_storage_ref` is empty. No PDF is ever faked.
//!
//! Check order (FastAPI signature order): path UUID → query/body
//! (422) → auth (401) → permission (403) → service checks
//! (404/409/422). The private-course / private-lesson 404s and the
//! plain `_is_lms_manager` DB lookup mirror `router.py` exactly.

use std::collections::{HashMap, HashSet};

use axum::{
    body::Body,
    extract::{ConnectInfo, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Json, Response},
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, Datelike, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::emit_event,
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    crypto::token_hex,
    db::{utc_now, Arg, Db, DynRow, IntoArg},
    errors::{ApiError, ApiResult},
    routes_fatwa::{check_quota, log_usage},
    storage::{read_bytes, verify_download_sig},
    sync::record_entity_change,
    validate::{
        as_int, opt_bool_loose, opt_datetime, opt_int_loose, opt_list, opt_uuid, parse_body,
        parse_path_uuid, parse_query, query_int,
    },
    AppState,
};

/// `app/core/storage.py::SIGNED_PURPOSE_CERTIFICATE` — the purpose
/// a signed certificate download URL must carry (FIX2-08: a valid
/// signature for any other purpose, or for a key that is not a
/// certificate's `pdf_storage_ref`, is rejected).
pub const SIGNED_PURPOSE_CERTIFICATE: &str = "lms-certificate";

const LEVEL_ORDER: [&str; 3] = ["beginner", "intermediate", "advanced"];

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Optional auth (`_optional_user`): any auth failure → guest, never
/// an error — including a `2fa_setup` token.
async fn optional_user(state: &AppState, headers: &HeaderMap) -> Option<AuthCtx> {
    authenticate(state, headers, "/api/v1/lms").await.ok()
}

/// Plain permission lookup (`_is_lms_manager`): no full-scope or 2FA
/// re-check, just the RBAC row.
async fn is_lms_manager(db: &Db, user_id: Uuid) -> Result<bool, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
             JOIN role_permissions rp ON rp.role_id = ur.role_id \
             JOIN permissions p ON p.id = rp.permission_id \
             WHERE ur.user_id = ? AND p.key = ? LIMIT 1",
            &[(user_id).into_arg(), ("lms.manage").into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

// --- LMS cursor (`service._encode_cursor` / `_decode_cursor`) -----------

/// `base64url(json([sort_order, str(id)]))` — Python's
/// `json.dumps` puts a space after the comma, so the raw string is
/// built literally to stay byte-identical.
fn encode_cursor(sort_order: i64, id: Uuid) -> String {
    URL_SAFE.encode(format!("[{sort_order}, \"{id}\"]").as_bytes())
}

fn decode_cursor(cursor: &str) -> Result<(i64, Uuid), ApiError> {
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
    let arr = parsed.as_array().ok_or_else(invalid)?;
    if arr.len() != 2 {
        return Err(invalid());
    }
    let sort = arr[0].as_i64().ok_or_else(invalid)?;
    let id = arr[1]
        .as_str()
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or_else(invalid)?;
    Ok((sort, id))
}

// --- body validators -------------------------------------------------------

/// Required `dict[str, str]` (`LocalizedText` in `lms/schemas.py`:
/// a plain dict with string values — keys are NOT required to be
/// fa/ar/en, unlike the strict `LocalizedText` in `validate.rs`).
fn req_text_map(body: &Value, field: &str) -> Result<Value, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) if v.is_object() => {
            for val in v.as_object().unwrap().values() {
                if !val.is_string() {
                    return Err(validation_err(field, "values must be strings"));
                }
            }
            Ok(v.clone())
        }
        Some(_) => Err(validation_err(field, "must be an object")),
        None => Err(validation_err(field, "field is required")),
    }
}

/// Optional `dict[str, str]`: absent/null → None.
fn opt_text_map(body: &Value, field: &str) -> Result<Option<Value>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(v) if v.is_object() => {
            for val in v.as_object().unwrap().values() {
                if !val.is_string() {
                    return Err(validation_err(field, "values must be strings"));
                }
            }
            Ok(Some(v.clone()))
        }
        Some(_) => Err(validation_err(field, "must be an object")),
        None => Ok(None),
    }
}

/// Optional enum string with a fixed allowed set; absent/null → None.
fn opt_enum(body: &Value, field: &str, allowed: &[&str]) -> Result<Option<String>, ApiError> {
    match body.get(field).filter(|v| !v.is_null()) {
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => Ok(Some(s.clone())),
        Some(_) => Err(validation_err(field, "value not allowed")),
        None => Ok(None),
    }
}

/// Required UUID body field.
fn req_uuid(body: &Value, field: &str) -> Result<Uuid, ApiError> {
    match opt_uuid(body, field)? {
        Some(id) => Ok(id),
        None => Err(validation_err(field, "field is required")),
    }
}

/// Required datetime body field.
fn req_datetime(body: &Value, field: &str) -> Result<DateTime<Utc>, ApiError> {
    match opt_datetime(body, field)? {
        Some(dt) => Ok(dt),
        None => Err(validation_err(field, "field is required")),
    }
}

/// Required UUID query param (`course_id`, …): missing/invalid → 422.
fn query_uuid(params: &HashMap<String, String>, name: &str) -> Result<Uuid, ApiError> {
    match params.get(name) {
        Some(s) => s
            .parse::<Uuid>()
            .map_err(|_| validation_err(name, "must be a UUID")),
        None => Err(validation_err(name, "field is required")),
    }
}

/// Optional UUID query param: absent → None; present but invalid → 422.
fn query_opt_uuid(params: &HashMap<String, String>, name: &str) -> Result<Option<Uuid>, ApiError> {
    match params.get(name).filter(|s| !s.is_empty()) {
        Some(s) => s
            .parse::<Uuid>()
            .map(Some)
            .map_err(|_| validation_err(name, "must be a UUID")),
        None => Ok(None),
    }
}

/// Optional string query param: absent/empty → None.
fn query_opt_str(params: &HashMap<String, String>, name: &str) -> Option<String> {
    params.get(name).filter(|s| !s.is_empty()).cloned()
}

// --- row shapes ------------------------------------------------------------

struct CourseRow {
    id: Uuid,
    curriculum_id: Option<Uuid>,
    title: Value,
    description: Option<Value>,
    level: String,
    visibility: String,
    sort_order: i32,
    is_active: bool,
}

const COURSE_COLS: &str =
    "id, curriculum_id, title, description, level, visibility, sort_order, is_active";

fn course_from_row(row: &DynRow) -> Result<CourseRow, ApiError> {
    Ok(CourseRow {
        id: row.try_get("id")?,
        curriculum_id: row.try_get("curriculum_id")?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        level: row.try_get("level")?,
        visibility: row.try_get("visibility")?,
        sort_order: row.try_get("sort_order")?,
        is_active: row.try_get("is_active")?,
    })
}

fn course_out(c: &CourseRow, lesson_count: i64, progress_pct: Option<f64>) -> Value {
    json!({
        "id": c.id.to_string(),
        "curriculum_id": c.curriculum_id.map(|u| u.to_string()),
        "title": c.title,
        "description": c.description,
        "level": c.level,
        "visibility": c.visibility,
        "sort_order": c.sort_order,
        "is_active": c.is_active,
        "lesson_count": lesson_count,
        "progress_pct": progress_pct,
    })
}

struct LessonRow {
    id: Uuid,
    course_id: Uuid,
    title: Value,
    body: Value,
    duration_minutes: i32,
    sort_order: i32,
    is_free_preview: bool,
}

const LESSON_COLS: &str =
    "id, course_id, title, body, duration_minutes, sort_order, is_free_preview";

fn lesson_from_row(row: &DynRow) -> Result<LessonRow, ApiError> {
    Ok(LessonRow {
        id: row.try_get("id")?,
        course_id: row.try_get("course_id")?,
        title: row.try_get("title")?,
        body: row.try_get("body")?,
        duration_minutes: row.try_get("duration_minutes")?,
        sort_order: row.try_get("sort_order")?,
        is_free_preview: row.try_get("is_free_preview")?,
    })
}

#[allow(clippy::too_many_arguments)]
fn lesson_out(
    l: &LessonRow,
    status: Option<String>,
    position_seconds: i64,
    prev_lesson_id: Option<Uuid>,
    next_lesson_id: Option<Uuid>,
) -> Value {
    json!({
        "id": l.id.to_string(),
        "course_id": l.course_id.to_string(),
        "title": l.title,
        "body": l.body,
        "duration_minutes": l.duration_minutes,
        "sort_order": l.sort_order,
        "is_free_preview": l.is_free_preview,
        "status": status,
        "position_seconds": position_seconds,
        "prev_lesson_id": prev_lesson_id.map(|u| u.to_string()),
        "next_lesson_id": next_lesson_id.map(|u| u.to_string()),
    })
}

struct CohortRow {
    id: Uuid,
    course_id: Uuid,
    title: Value,
    starts_at: DateTime<Utc>,
    ends_at: DateTime<Utc>,
    max_seats: i32,
    is_open: bool,
}

const COHORT_COLS: &str = "id, course_id, title, starts_at, ends_at, max_seats, is_open";

fn cohort_from_row(row: &DynRow) -> Result<CohortRow, ApiError> {
    Ok(CohortRow {
        id: row.try_get("id")?,
        course_id: row.try_get("course_id")?,
        title: row.try_get("title")?,
        starts_at: row.try_get("starts_at")?,
        ends_at: row.try_get("ends_at")?,
        max_seats: row.try_get("max_seats")?,
        is_open: row.try_get("is_open")?,
    })
}

fn cohort_out(c: &CohortRow, enrolled_count: i64, seats_left: i64, user_enrolled: bool) -> Value {
    json!({
        "id": c.id.to_string(),
        "course_id": c.course_id.to_string(),
        "title": c.title,
        "starts_at": c.starts_at.to_rfc3339(),
        "ends_at": c.ends_at.to_rfc3339(),
        "max_seats": c.max_seats,
        "is_open": c.is_open,
        "enrolled_count": enrolled_count,
        "seats_left": seats_left,
        "user_enrolled": user_enrolled,
    })
}

/// `service._user_display` — full name, else email, else phone, else id.
fn user_display(full_name: &str, email: Option<&str>, phone: Option<&str>, id: Uuid) -> String {
    if !full_name.is_empty() {
        return full_name.to_string();
    }
    if let Some(e) = email.filter(|s| !s.is_empty()) {
        return e.to_string();
    }
    if let Some(p) = phone.filter(|s| !s.is_empty()) {
        return p.to_string();
    }
    id.to_string()
}

async fn lesson_count(db: &Db, course_id: Uuid) -> Result<i64, ApiError> {
    let row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM lessons WHERE course_id = ?",
            &[course_id.into_arg()],
        )
        .await?;
    row.try_get("n")
}

async fn enrolled_count(db: &Db, cohort_id: Uuid) -> Result<i64, ApiError> {
    let row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM cohort_enrollments \
             WHERE cohort_id = ? AND status = 'active'",
            &[cohort_id.into_arg()],
        )
        .await?;
    row.try_get("n")
}

// --- prerequisites ---------------------------------------------------------

/// `service.prerequisites_satisfied` — a prerequisite counts as met
/// only when the user holds a certificate for the required course.
async fn prerequisites_satisfied(
    db: &Db,
    user_id: Uuid,
    course_id: Uuid,
) -> Result<(bool, Vec<Uuid>), ApiError> {
    let rows = db
        .fetch_all(
            "SELECT requires_course_id FROM course_prerequisites WHERE course_id = ?",
            &[course_id.into_arg()],
        )
        .await?;
    let mut missing = Vec::new();
    for row in &rows {
        let req: Uuid = row.try_get("requires_course_id")?;
        let has = db
            .fetch_optional(
                "SELECT id FROM certificates WHERE user_id = ? AND course_id = ?",
                &[user_id.into_arg(), req.into_arg()],
            )
            .await?;
        if has.is_none() {
            missing.push(req);
        }
    }
    Ok((missing.is_empty(), missing))
}

// --- certificates ----------------------------------------------------------

/// `service._unique_cert_code` — `MA-{year}-{8 uppercase hex}`,
/// 5 attempts, then 500 `cert_code_failed`.
async fn unique_cert_code(db: &Db) -> Result<String, ApiError> {
    for _ in 0..5 {
        let code = format!("MA-{}-{}", Utc::now().year(), token_hex(4).to_uppercase());
        let exists = db
            .fetch_optional(
                "SELECT id FROM certificates WHERE code = ?",
                &[code.clone().into_arg()],
            )
            .await?;
        if exists.is_none() {
            return Ok(code);
        }
    }
    Err(ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "cert_code_failed",
        "تولید کد یکتای گواهی ناموفق بود",
    ))
}

/// `service._maybe_issue_certificate` — all lessons completed +
/// prerequisite chain satisfied → issue the certificate record
/// (idempotent: an existing certificate returns `None`).
/// The PDF itself is NOT rendered here (reportlab sidecar, ADR-051):
/// `pdf_storage_ref` stays NULL until the sidecar renders it, so the
/// download routes answer 404 until then — never a faked PDF.
async fn maybe_issue_certificate(
    db: &Db,
    user: &AuthCtx,
    course_id: Uuid,
) -> Result<Option<String>, ApiError> {
    let lesson_rows = db
        .fetch_all(
            "SELECT id FROM lessons WHERE course_id = ?",
            &[course_id.into_arg()],
        )
        .await?;
    if lesson_rows.is_empty() {
        return Ok(None);
    }
    let lesson_ids: Vec<Uuid> = lesson_rows
        .iter()
        .map(|r| r.try_get::<Uuid, _>("id"))
        .collect::<Result<_, _>>()?;
    let placeholders: Vec<&str> = lesson_ids.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT COUNT(*) AS n FROM user_progress \
         WHERE user_id = ? AND lesson_id IN ({}) AND status = 'completed'",
        placeholders.join(", ")
    );
    let mut args: Vec<Arg> = vec![user.user.id.into_arg()];
    args.extend(lesson_ids.iter().map(|id| id.into_arg()));
    let row = db.fetch_one(&sql, &args).await?;
    let done: i64 = row.try_get("n")?;
    if done < lesson_ids.len() as i64 {
        return Ok(None);
    }
    let (ok, _) = prerequisites_satisfied(db, user.user.id, course_id).await?;
    if !ok {
        return Ok(None);
    }
    let existing = db
        .fetch_optional(
            "SELECT id FROM certificates WHERE user_id = ? AND course_id = ?",
            &[user.user.id.into_arg(), course_id.into_arg()],
        )
        .await?;
    if existing.is_some() {
        return Ok(None);
    }
    let course = db
        .fetch_optional(
            &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
            &[course_id.into_arg()],
        )
        .await?;
    if course.is_none() {
        return Ok(None);
    }
    let code = unique_cert_code(db).await?;
    let cert_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO certificates (id, user_id, course_id, code, issued_at, pdf_storage_ref, locale) \
         VALUES (?, ?, ?, ?, ?, NULL, ?)",
        &[
            cert_id.into_arg(),
            user.user.id.into_arg(),
            course_id.into_arg(),
            code.clone().into_arg(),
            now.into_arg(),
            user.user.locale.clone().into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "course",
        &course_id.to_string(),
        "lms.course_completed",
        json!({ "user_id": user.user.id.to_string(), "course_id": course_id.to_string() }),
    )
    .await?;
    emit_event(
        db,
        "certificate",
        &cert_id.to_string(),
        "lms.certificate_issued",
        json!({
            "user_id": user.user.id.to_string(),
            "course_id": course_id.to_string(),
            "code": code,
        }),
    )
    .await?;
    Ok(Some(code))
}

async fn verify_impl(
    state: &AppState,
    headers: &HeaderMap,
    peer: std::net::SocketAddr,
    code: &str,
) -> ApiResult<Json<Value>> {
    // Public verification is rate-limited (30/min per IP, fail-closed
    // in production without Redis — `router._verify_impl`).
    let ip = client_ip(headers, Some(peer), &state.config.trusted_proxies)
        .unwrap_or_else(|| "unknown".to_string());
    if !state
        .limiter
        .is_allowed(
            &format!("cert_verify:{ip}"),
            30,
            60,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    let db = state.db_or_500()?;
    let norm = code.trim().to_uppercase();
    let row = db
        .fetch_optional(
            "SELECT id, user_id, course_id, code, issued_at, locale FROM certificates WHERE code = ?",
            &[norm.clone().into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Ok(Json(json!({ "valid": false, "code": norm })));
    };
    let user_id: Uuid = row.try_get("user_id")?;
    let course_id: Uuid = row.try_get("course_id")?;
    let code_out: String = row.try_get("code")?;
    let issued_at: DateTime<Utc> = row.try_get("issued_at")?;
    let locale: String = row.try_get("locale")?;
    let holder = db
        .fetch_optional(
            "SELECT full_name FROM users WHERE id = ?",
            &[user_id.into_arg()],
        )
        .await?;
    let holder_name: Option<String> = match holder {
        Some(r) => Some(r.try_get("full_name")?),
        None => None,
    };
    let course = db
        .fetch_optional(
            "SELECT title FROM courses WHERE id = ?",
            &[course_id.into_arg()],
        )
        .await?;
    let course_title: Option<Value> = match course {
        Some(r) => Some(r.try_get("title")?),
        None => None,
    };
    Ok(Json(json!({
        "valid": true,
        "code": code_out,
        "holder_name": holder_name,
        "course_title": course_title,
        "issued_at": issued_at.to_rfc3339(),
        "locale": locale,
    })))
}

// --- public: courses ---------------------------------------------------------

/// `GET /api/v1/lms/courses` — cursor page; private courses only
/// for LMS managers; `progress_pct` only for the authed user.
pub async fn list_courses(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let params = parse_query(&uri);
    let level = query_opt_str(&params, "level");
    if let Some(l) = &level {
        if !LEVEL_ORDER.contains(&l.as_str()) {
            return Err(validation_err("level", "value not allowed"));
        }
    }
    let curriculum_id = query_opt_uuid(&params, "curriculum_id")?;
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let cursor = query_opt_str(&params, "after");
    let db = state.db_or_500()?;
    let include_private = match &user {
        Some(ctx) => is_lms_manager(db, ctx.user.id).await?,
        None => false,
    };
    let mut conds: Vec<String> = vec!["is_active = TRUE".to_string()];
    let mut args: Vec<Arg> = Vec::new();
    if !include_private {
        conds.push("visibility = 'public'".to_string());
    }
    if let Some(l) = &level {
        conds.push("level = ?".to_string());
        args.push(l.into_arg());
    }
    if let Some(cid) = curriculum_id {
        conds.push("curriculum_id = ?".to_string());
        args.push(cid.into_arg());
    }
    if let Some(c) = &cursor {
        let (s, lid) = decode_cursor(c)?;
        conds.push("(sort_order > ? OR (sort_order = ? AND id > ?))".to_string());
        args.push(s.into_arg());
        args.push(s.into_arg());
        args.push(lid.into_arg());
    }
    let sql = format!(
        "SELECT {COURSE_COLS} FROM courses WHERE {} ORDER BY sort_order, id LIMIT ?",
        conds.join(" AND ")
    );
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut next_cursor: Option<String> = None;
    let mut rows = rows;
    if rows.len() > limit as usize {
        let last = course_from_row(&rows[(limit as usize) - 1])?;
        next_cursor = Some(encode_cursor(last.sort_order as i64, last.id));
        rows.truncate(limit as usize);
    }
    let user_id = user.as_ref().map(|c| c.user.id);
    let mut items: Vec<Value> = Vec::new();
    for row in &rows {
        let c = course_from_row(row)?;
        let total = lesson_count(db, c.id).await?;
        let progress_pct = match user_id {
            Some(uid) if total > 0 => {
                let done_row = db
                    .fetch_one(
                        "SELECT COUNT(*) AS n FROM user_progress up JOIN lessons l ON l.id = up.lesson_id \
                         WHERE l.course_id = ? AND up.user_id = ? AND up.status = 'completed'",
                        &[c.id.into_arg(), uid.into_arg()],
                    )
                    .await?;
                let done: i64 = done_row.try_get("n")?;
                Some(round1(done as f64 / total as f64 * 100.0))
            }
            _ => None,
        };
        items.push(course_out(&c, total, progress_pct));
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `GET /api/v1/lms/courses/{course_id}` — private course → 404
/// unless the caller is an LMS manager.
pub async fn get_course(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(course_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let course_id = parse_path_uuid(&course_id, "course_id")?;
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
            &[course_id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("course_not_found", "دوره یافت نشد"))?;
    let course = course_from_row(&row)?;
    if !course.is_active {
        return Err(ApiError::not_found("course_not_found", "دوره یافت نشد"));
    }
    if course.visibility == "private" {
        let is_manager = match &user {
            Some(ctx) => is_lms_manager(db, ctx.user.id).await?,
            None => false,
        };
        if !is_manager {
            return Err(ApiError::not_found("course_not_found", "دوره یافت نشد"));
        }
    }
    let lesson_rows = db
        .fetch_all(
            &format!(
                "SELECT {LESSON_COLS} FROM lessons WHERE course_id = ? ORDER BY sort_order, id"
            ),
            &[course.id.into_arg()],
        )
        .await?;
    let mut lessons: Vec<LessonRow> = Vec::new();
    for r in &lesson_rows {
        lessons.push(lesson_from_row(r)?);
    }
    let mut prog: HashMap<Uuid, (String, i64)> = HashMap::new();
    if let Some(ctx) = &user {
        if !lessons.is_empty() {
            let ids: Vec<Uuid> = lessons.iter().map(|l| l.id).collect();
            let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT lesson_id, status, position_seconds FROM user_progress \
                 WHERE user_id = ? AND lesson_id IN ({})",
                placeholders.join(", ")
            );
            let mut args: Vec<Arg> = vec![ctx.user.id.into_arg()];
            args.extend(ids.iter().map(|id| id.into_arg()));
            for r in db.fetch_all(&sql, &args).await? {
                let lid: Uuid = r.try_get("lesson_id")?;
                let status: String = r.try_get("status")?;
                let pos: i32 = r.try_get("position_seconds")?;
                prog.insert(lid, (status, pos as i64));
            }
        }
    }
    let total = lessons.len() as i64;
    let done = lessons
        .iter()
        .filter(|l| matches!(prog.get(&l.id), Some((s, _)) if s == "completed"))
        .count() as i64;
    let progress_pct = if total > 0 {
        round1(done as f64 / total as f64 * 100.0)
    } else {
        0.0
    };
    let mut has_certificate = false;
    if let Some(ctx) = &user {
        if total > 0 && done == total {
            has_certificate = db
                .fetch_optional(
                    "SELECT id FROM certificates WHERE user_id = ? AND course_id = ?",
                    &[ctx.user.id.into_arg(), course.id.into_arg()],
                )
                .await?
                .is_some();
        }
    }
    let lesson_outs: Vec<Value> = lessons
        .iter()
        .map(|l| {
            let (status, pos) = prog
                .get(&l.id)
                .map(|(s, p)| (Some(s.clone()), *p))
                .unwrap_or((None, 0));
            lesson_out(l, status, pos, None, None)
        })
        .collect();
    Ok(Json(json!({
        "course": course_out(&course, total, Some(progress_pct)),
        "lessons": lesson_outs,
        "completed_lessons": done,
        "total_lessons": total,
        "progress_pct": progress_pct,
        "has_certificate": has_certificate,
    })))
}

/// `GET /api/v1/lms/lessons/{lesson_id}` — shareable lesson URL,
/// with prev/next navigation inside its course.
pub async fn get_lesson(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lesson_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let lesson_id = parse_path_uuid(&lesson_id, "lesson_id")?;
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            &format!("SELECT {LESSON_COLS} FROM lessons WHERE id = ?"),
            &[lesson_id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("lesson_not_found", "درس یافت نشد"))?;
    let lesson = lesson_from_row(&row)?;
    let course_row = db
        .fetch_optional(
            &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
            &[lesson.course_id.into_arg()],
        )
        .await?;
    let Some(course_row) = course_row else {
        return Err(ApiError::not_found("lesson_not_found", "درس یافت نشد"));
    };
    let course = course_from_row(&course_row)?;
    if !course.is_active {
        return Err(ApiError::not_found("lesson_not_found", "درس یافت نشد"));
    }
    if course.visibility == "private" {
        let is_manager = match &user {
            Some(ctx) => is_lms_manager(db, ctx.user.id).await?,
            None => false,
        };
        if !is_manager {
            return Err(ApiError::not_found("lesson_not_found", "درس یافت نشد"));
        }
    }
    let (status, position) = match &user {
        Some(ctx) => {
            let prow = db
                .fetch_optional(
                    "SELECT status, position_seconds FROM user_progress \
                     WHERE user_id = ? AND lesson_id = ?",
                    &[ctx.user.id.into_arg(), lesson.id.into_arg()],
                )
                .await?;
            match prow {
                Some(r) => {
                    let s: String = r.try_get("status")?;
                    let p: i32 = r.try_get("position_seconds")?;
                    (Some(s), p as i64)
                }
                None => (None, 0),
            }
        }
        None => (None, 0),
    };
    let ordered = db
        .fetch_all(
            "SELECT id FROM lessons WHERE course_id = ? ORDER BY sort_order, id",
            &[course.id.into_arg()],
        )
        .await?;
    let ids: Vec<Uuid> = ordered
        .iter()
        .map(|r| r.try_get::<Uuid, _>("id"))
        .collect::<Result<_, _>>()?;
    let idx = ids
        .iter()
        .position(|id| *id == lesson.id)
        .ok_or_else(ApiError::internal)?;
    let prev = if idx > 0 { Some(ids[idx - 1]) } else { None };
    let next = if idx + 1 < ids.len() {
        Some(ids[idx + 1])
    } else {
        None
    };
    Ok(Json(lesson_out(&lesson, status, position, prev, next)))
}

// --- public: progress ------------------------------------------------------

/// `POST /api/v1/lms/lessons/{lesson_id}/progress` — update
/// position/status; completing the last lesson issues the
/// certificate record automatically (PDF rendered by the sidecar).
pub async fn update_progress(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(lesson_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let lesson_id = parse_path_uuid(&lesson_id, "lesson_id")?;
    let parsed = parse_body(&body)?;
    let status = match parsed.get("status").filter(|v| !v.is_null()) {
        Some(Value::String(s))
            if ["not_started", "in_progress", "completed"].contains(&s.as_str()) =>
        {
            s.clone()
        }
        _ => return Err(validation_err("status", "value not allowed")),
    };
    let position_seconds = opt_int_loose(&parsed, "position_seconds", 0, i64::MAX)?.unwrap_or(0);
    let ctx = authenticate(&state, &headers, "/api/v1/lms/lessons").await?;
    let db = state.db_or_500()?;
    let lesson_row = db
        .fetch_optional(
            &format!("SELECT {LESSON_COLS} FROM lessons WHERE id = ?"),
            &[lesson_id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("lesson_not_found", "درس یافت نشد"))?;
    let lesson = lesson_from_row(&lesson_row)?;
    let now = utc_now();
    let existing = db
        .fetch_optional(
            "SELECT id, version, completed_at FROM user_progress \
             WHERE user_id = ? AND lesson_id = ?",
            &[ctx.user.id.into_arg(), lesson_id.into_arg()],
        )
        .await?;
    let (prog_id, version, completed_at) = match existing {
        Some(r) => {
            let id: Uuid = r.try_get("id")?;
            let old_version: i32 = r.try_get("version")?;
            let old_completed: Option<DateTime<Utc>> = r.try_get("completed_at")?;
            let version = old_version + 1;
            let completed_at = if status == "completed" {
                old_completed.or(Some(now))
            } else {
                None
            };
            db.execute(
                "UPDATE user_progress SET status = ?, position_seconds = ?, \
                 completed_at = ?, updated_at = ?, version = ? WHERE id = ?",
                &[
                    status.clone().into_arg(),
                    position_seconds.into_arg(),
                    completed_at.into_arg(),
                    now.into_arg(),
                    version.into_arg(),
                    id.into_arg(),
                ],
            )
            .await?;
            (id, version, completed_at)
        }
        None => {
            let id = Uuid::new_v4();
            let completed_at = if status == "completed" {
                Some(now)
            } else {
                None
            };
            db.execute(
                "INSERT INTO user_progress (id, user_id, lesson_id, status, position_seconds, \
                 completed_at, updated_at, version) VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
                &[
                    id.into_arg(),
                    ctx.user.id.into_arg(),
                    lesson_id.into_arg(),
                    status.clone().into_arg(),
                    position_seconds.into_arg(),
                    completed_at.into_arg(),
                    now.into_arg(),
                ],
            )
            .await?;
            (id, 1, completed_at)
        }
    };
    record_entity_change(
        db,
        ctx.user.id,
        "api",
        "user_progress",
        &lesson_id.to_string(),
        "upsert",
        Some(json!({
            "lesson_id": lesson_id.to_string(),
            "status": status,
            "position_seconds": position_seconds,
            "version": version,
        })),
        version as i64,
    )
    .await?;
    let mut course_completed = false;
    let mut certificate_code: Option<String> = None;
    if status == "completed" {
        emit_event(
            db,
            "user_progress",
            &prog_id.to_string(),
            "lms.lesson_completed",
            json!({
                "user_id": ctx.user.id.to_string(),
                "lesson_id": lesson_id.to_string(),
                "course_id": lesson.course_id.to_string(),
            }),
        )
        .await?;
        if let Some(code) = maybe_issue_certificate(db, &ctx, lesson.course_id).await? {
            course_completed = true;
            certificate_code = Some(code);
        }
    }
    Ok(Json(json!({
        "lesson_id": lesson_id.to_string(),
        "status": status,
        "position_seconds": position_seconds,
        "completed_at": completed_at.map(|d| d.to_rfc3339()),
        "course_completed": course_completed,
        "certificate_code": certificate_code,
    })))
}

// --- public: lesson quiz (Phase-6 engine reuse) ----------------------------

/// `POST /api/v1/lms/quiz/{lesson_id}` — the fatwa quiz engine
/// (`routes_fatwa::quiz_generate` logic: quota, random approved
/// questions, attempt + usage log), with the attempt tagged to the
/// lesson.
pub async fn generate_lesson_quiz(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    uri: Uri,
    Path(lesson_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let lesson_id = parse_path_uuid(&lesson_id, "lesson_id")?;
    let params = parse_query(&uri);
    let count = query_int(&params, "count", 5, 1, 20)?;
    let ctx = authenticate(&state, &headers, "/api/v1/lms/quiz").await?;
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    let db = state.db_or_500()?;
    if db
        .fetch_optional(
            "SELECT id FROM lessons WHERE id = ?",
            &[lesson_id.into_arg()],
        )
        .await?
        .is_none()
    {
        return Err(ApiError::not_found("lesson_not_found", "درس یافت نشد"));
    }
    check_quota(&state, "fatwa.quiz", Some(&ctx), ip).await?;
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
        "INSERT INTO quiz_attempts (id, user_id, lesson_id, question_ids, answers, score, created_at) \
         VALUES (?, ?, ?, ?, ?, 0.0, ?)",
        &[
            attempt_id.into_arg(),
            ctx.user.id.into_arg(),
            lesson_id.into_arg(),
            Value::Array(
                question_ids
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            )
            .into_arg(),
            json!({}).into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    log_usage(db, Some(ctx.user.id), "fatwa.quiz", "none").await?;
    Ok(Json(json!({
        "attempt_id": attempt_id.to_string(),
        "questions": questions,
    })))
}

/// `POST /api/v1/lms/quiz/submit` — 404 `lms_attempt_not_found`
/// when the attempt does not exist or has no lesson; otherwise the
/// fatwa quiz-submit engine (ownership + scoring) answers.
pub async fn submit_lesson_quiz(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
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
    for v in answers_raw.values() {
        if as_int(v).is_none() {
            return Err(validation_err("answers", "values must be integers"));
        }
    }
    authenticate(&state, &headers, "/api/v1/lms/quiz").await?;
    let db = state.db_or_500()?;
    let attempt = db
        .fetch_optional(
            "SELECT lesson_id FROM quiz_attempts WHERE id = ?",
            &[attempt_id.into_arg()],
        )
        .await?;
    let lesson_id: Option<Uuid> = match attempt {
        Some(r) => r.try_get("lesson_id")?,
        None => None,
    };
    if lesson_id.is_none() {
        return Err(ApiError::not_found(
            "lms_attempt_not_found",
            "تلاش آزمون درس یافت نشد",
        ));
    }
    crate::routes_fatwa::quiz_submit(State(state), headers, body).await
}

// --- public: reports & recommendations -------------------------------------

async fn quiz_stats_for_lessons(
    db: &Db,
    user_id: Uuid,
    lesson_ids: &[Uuid],
) -> Result<(Option<f64>, i64), ApiError> {
    if lesson_ids.is_empty() {
        return Ok((None, 0));
    }
    let placeholders: Vec<&str> = lesson_ids.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT score, answers FROM quiz_attempts \
         WHERE user_id = ? AND lesson_id IN ({})",
        placeholders.join(", ")
    );
    let mut args: Vec<Arg> = vec![user_id.into_arg()];
    args.extend(lesson_ids.iter().map(|id| id.into_arg()));
    let rows = db.fetch_all(&sql, &args).await?;
    let mut scores: Vec<f64> = Vec::new();
    for row in &rows {
        let answers: Value = row.try_get("answers")?;
        let non_empty = match &answers {
            Value::Object(m) => !m.is_empty(),
            _ => false,
        };
        if non_empty {
            let score: f64 = row.try_get("score")?;
            scores.push(score);
        }
    }
    if scores.is_empty() {
        return Ok((None, 0));
    }
    let avg = scores.iter().sum::<f64>() / scores.len() as f64;
    Ok((Some(round1(avg)), scores.len() as i64))
}

/// `GET /api/v1/lms/reports/me` — computed from `user_progress` +
/// `quiz_attempts`, never a stored table.
pub async fn study_report(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/lms/reports").await?;
    let db = state.db_or_500()?;
    let progresses = db
        .fetch_all(
            "SELECT lesson_id, status, updated_at FROM user_progress WHERE user_id = ?",
            &[ctx.user.id.into_arg()],
        )
        .await?;
    let completed: HashSet<Uuid> = progresses
        .iter()
        .filter_map(|r| {
            let status: String = r.try_get("status").ok()?;
            if status == "completed" {
                r.try_get::<Uuid, _>("lesson_id").ok()
            } else {
                None
            }
        })
        .collect();
    let lesson_ids: Vec<Uuid> = progresses
        .iter()
        .filter_map(|r| r.try_get::<Uuid, _>("lesson_id").ok())
        .collect();
    let mut course_ids: Vec<Uuid> = Vec::new();
    if !lesson_ids.is_empty() {
        let placeholders: Vec<&str> = lesson_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT course_id FROM lessons WHERE id IN ({})",
            placeholders.join(", ")
        );
        let args: Vec<Arg> = lesson_ids.iter().map(|id| id.into_arg()).collect();
        for r in db.fetch_all(&sql, &args).await? {
            course_ids.push(r.try_get("course_id")?);
        }
    }
    let mut courses_out: Vec<Value> = Vec::new();
    let mut total_done = 0i64;
    let mut total_lessons = 0i64;
    let mut quiz_avgs: Vec<f64> = Vec::new();
    for cid in &course_ids {
        let Some(course_row) = db
            .fetch_optional(
                &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
                &[(*cid).into_arg()],
            )
            .await?
        else {
            continue;
        };
        let course = course_from_row(&course_row)?;
        let lids: Vec<Uuid> = db
            .fetch_all(
                "SELECT id FROM lessons WHERE course_id = ?",
                &[(*cid).into_arg()],
            )
            .await?
            .iter()
            .map(|r| r.try_get::<Uuid, _>("id"))
            .collect::<Result<_, _>>()?;
        let done = lids.iter().filter(|id| completed.contains(id)).count() as i64;
        let (quiz_avg, quiz_n) = quiz_stats_for_lessons(db, ctx.user.id, &lids).await?;
        total_done += done;
        total_lessons += lids.len() as i64;
        if let Some(a) = quiz_avg {
            quiz_avgs.push(a);
        }
        let pct = if lids.is_empty() {
            0.0
        } else {
            round1(done as f64 / lids.len() as f64 * 100.0)
        };
        courses_out.push(json!({
            "course_id": cid.to_string(),
            "course_title": course.title,
            "level": course.level,
            "total_lessons": lids.len(),
            "completed_lessons": done,
            "progress_pct": pct,
            "quiz_avg": quiz_avg,
            "quiz_attempts": quiz_n,
        }));
    }
    courses_out.sort_by(|a, b| {
        b["progress_pct"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["progress_pct"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let quiz_overall_avg = if quiz_avgs.is_empty() {
        None
    } else {
        Some(round1(
            quiz_avgs.iter().sum::<f64>() / quiz_avgs.len() as f64,
        ))
    };
    let overall = if total_lessons > 0 {
        round1(total_done as f64 / total_lessons as f64 * 100.0)
    } else {
        0.0
    };
    // Streak: distinct activity dates, counting back consecutively
    // from the latest day.
    let mut days: Vec<chrono::NaiveDate> = progresses
        .iter()
        .filter_map(|r| {
            r.try_get::<DateTime<Utc>, _>("updated_at")
                .ok()
                .map(|d| d.date_naive())
        })
        .collect();
    days.sort();
    days.dedup();
    days.reverse();
    let mut streak = 0i64;
    if let Some(first) = days.first() {
        let mut expected = *first;
        for d in &days {
            if *d == expected {
                streak += 1;
                expected -= chrono::Duration::days(1);
            } else if *d < expected {
                break;
            }
        }
    }
    Ok(Json(json!({
        "courses": courses_out,
        "total_completed_lessons": total_done,
        "total_lessons": total_lessons,
        "overall_progress_pct": overall,
        "quiz_overall_avg": quiz_overall_avg,
        "streak_days": streak,
    })))
}

/// `GET /api/v1/lms/recommendations` — unfinished lessons at the
/// user's current level first, then the next level; courses whose
/// prerequisites are unmet are skipped.
pub async fn recommendations(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/lms/recommendations").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 10, 1, 50)?;
    let db = state.db_or_500()?;
    let progresses = db
        .fetch_all(
            "SELECT up.lesson_id, up.status, l.course_id FROM user_progress up \
             JOIN lessons l ON l.id = up.lesson_id \
             WHERE up.user_id = ? ORDER BY up.updated_at DESC",
            &[ctx.user.id.into_arg()],
        )
        .await?;
    let completed: HashSet<Uuid> = progresses
        .iter()
        .filter_map(|r| {
            let status: String = r.try_get("status").ok()?;
            if status == "completed" {
                r.try_get::<Uuid, _>("lesson_id").ok()
            } else {
                None
            }
        })
        .collect();
    let mut user_level = "beginner".to_string();
    if let Some(first) = progresses.first() {
        let last_course_id: Uuid = first.try_get("course_id")?;
        if let Some(course_row) = db
            .fetch_optional(
                &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
                &[last_course_id.into_arg()],
            )
            .await?
        {
            user_level = course_from_row(&course_row)?.level;
        }
    }
    let level_idx = LEVEL_ORDER
        .iter()
        .position(|l| *l == user_level)
        .unwrap_or(0);
    let mut ordered_levels: Vec<&str> = LEVEL_ORDER[level_idx..].to_vec();
    ordered_levels.extend_from_slice(&LEVEL_ORDER[..level_idx]);
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<Uuid> = HashSet::new();
    for (idx, level) in ordered_levels.iter().enumerate() {
        let courses = db
            .fetch_all(
                &format!(
                    "SELECT {COURSE_COLS} FROM courses \
                     WHERE is_active = TRUE AND visibility = 'public' AND level = ? \
                     ORDER BY sort_order, id"
                ),
                &[(*level).into_arg()],
            )
            .await?;
        for course_row in &courses {
            let course = course_from_row(course_row)?;
            let (ok, _) = prerequisites_satisfied(db, ctx.user.id, course.id).await?;
            if !ok {
                continue;
            }
            let lessons = db
                .fetch_all(
                    &format!(
                        "SELECT {LESSON_COLS} FROM lessons WHERE course_id = ? \
                         ORDER BY sort_order, id"
                    ),
                    &[course.id.into_arg()],
                )
                .await?;
            for lesson_row in &lessons {
                let lesson = lesson_from_row(lesson_row)?;
                if completed.contains(&lesson.id) || seen.contains(&lesson.id) {
                    continue;
                }
                seen.insert(lesson.id);
                let reason = if idx == 0 {
                    "level_next_incomplete"
                } else {
                    "next_level"
                };
                out.push(json!({
                    "lesson": lesson_out(&lesson, None, 0, None, None),
                    "course_title": course.title,
                    "level": level,
                    "reason": reason,
                }));
                if out.len() >= limit as usize {
                    return Ok(Json(Value::Array(out)));
                }
            }
        }
    }
    Ok(Json(Value::Array(out)))
}

// --- public: cohorts ---------------------------------------------------------

/// `GET /api/v1/lms/cohorts` — note Python never applies the `after`
/// cursor (it fetches `limit+1` and still mints a cursor); mirrored
/// here, cursor in, cursor out, no filtering.
pub async fn list_cohorts(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let params = parse_query(&uri);
    let course_id = query_opt_uuid(&params, "course_id")?;
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let db = state.db_or_500()?;
    let mut sql = format!("SELECT {COHORT_COLS} FROM course_cohorts");
    let mut args: Vec<Arg> = Vec::new();
    if let Some(cid) = course_id {
        sql.push_str(" WHERE course_id = ?");
        args.push(cid.into_arg());
    }
    sql.push_str(" ORDER BY starts_at, id LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut next_cursor: Option<String> = None;
    let mut rows = rows;
    if rows.len() > limit as usize {
        let last = cohort_from_row(&rows[(limit as usize) - 1])?;
        next_cursor = Some(encode_cursor(0, last.id));
        rows.truncate(limit as usize);
    }
    let mut items: Vec<Value> = Vec::new();
    for row in &rows {
        let c = cohort_from_row(row)?;
        let enrolled = enrolled_count(db, c.id).await?;
        let mine = match &user {
            Some(ctx) => db
                .fetch_optional(
                    "SELECT id FROM cohort_enrollments \
                     WHERE cohort_id = ? AND user_id = ? AND status = 'active'",
                    &[c.id.into_arg(), ctx.user.id.into_arg()],
                )
                .await?
                .is_some(),
            None => false,
        };
        items.push(cohort_out(
            &c,
            enrolled,
            (c.max_seats as i64 - enrolled).max(0),
            mine,
        ));
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

/// `POST /api/v1/lms/cohorts/{cohort_id}/enroll`.
pub async fn enroll_cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cohort_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let cohort_id = parse_path_uuid(&cohort_id, "cohort_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/lms/cohorts").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            &format!("SELECT {COHORT_COLS} FROM course_cohorts WHERE id = ?"),
            &[cohort_id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("cohort_not_found", "هم‌گروهی یافت نشد"))?;
    let cohort = cohort_from_row(&row)?;
    let now = utc_now();
    if !cohort.is_open {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "cohort_closed",
            "ثبت‌نام این هم‌گروهی بسته است",
        ));
    }
    if now > cohort.ends_at {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "cohort_ended",
            "این هم‌گروهی به پایان رسیده است",
        ));
    }
    let existing = db
        .fetch_optional(
            "SELECT id, status, enrolled_at FROM cohort_enrollments \
             WHERE cohort_id = ? AND user_id = ?",
            &[cohort_id.into_arg(), ctx.user.id.into_arg()],
        )
        .await?;
    if let Some(r) = &existing {
        let status: String = r.try_get("status")?;
        if status == "active" {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "already_enrolled",
                "پیش‌تر در این هم‌گروهی ثبت‌نام کرده‌اید",
            ));
        }
    }
    let enrolled = enrolled_count(db, cohort_id).await?;
    if enrolled >= cohort.max_seats as i64 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "cohort_full",
            "ظرفیت این هم‌گروهی تکمیل است",
        ));
    }
    let (ok, missing) = prerequisites_satisfied(db, ctx.user.id, cohort.course_id).await?;
    if !ok {
        return Err(ApiError::with_details(
            StatusCode::CONFLICT,
            "prerequisite_not_met",
            "پیش‌نیازهای این دوره هنوز گذرانده نشده‌اند",
            json!({ "missing_course_ids": missing.iter().map(|u| u.to_string()).collect::<Vec<_>>() }),
        ));
    }
    let (enrollment_id, enrolled_at) = match &existing {
        Some(r) => {
            let id: Uuid = r.try_get("id")?;
            db.execute(
                "UPDATE cohort_enrollments SET status = 'active', enrolled_at = ? WHERE id = ?",
                &[now.into_arg(), id.into_arg()],
            )
            .await?;
            (id, now)
        }
        None => {
            let id = Uuid::new_v4();
            // A concurrent enrollment could still collide on the
            // unique (cohort_id, user_id) pair; Python answers that
            // race with 409 `already_enrolled`. The pre-check above
            // covers every non-race case.
            db.execute(
                "INSERT INTO cohort_enrollments (id, cohort_id, user_id, enrolled_at, status) \
                 VALUES (?, ?, ?, ?, 'active')",
                &[
                    id.into_arg(),
                    cohort_id.into_arg(),
                    ctx.user.id.into_arg(),
                    now.into_arg(),
                ],
            )
            .await?;
            (id, now)
        }
    };
    let _ = enrollment_id;
    emit_event(
        db,
        "cohort",
        &cohort_id.to_string(),
        "lms.cohort_enrolled",
        json!({
            "user_id": ctx.user.id.to_string(),
            "cohort_id": cohort_id.to_string(),
            "course_id": cohort.course_id.to_string(),
        }),
    )
    .await?;
    Ok(Json(json!({
        "cohort_id": cohort_id.to_string(),
        "status": "active",
        "enrolled_at": enrolled_at.to_rfc3339(),
        "seats_left": (cohort.max_seats as i64 - enrolled - 1).max(0),
    })))
}

// --- public: certificates ----------------------------------------------------

/// `GET /api/v1/lms/certificates/mine?course_id=…` — the user's
/// certificate for the course, or JSON `null`.
pub async fn my_certificate(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/lms/certificates").await?;
    let params = parse_query(&uri);
    let course_id = query_uuid(&params, "course_id")?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, code, course_id, issued_at FROM certificates \
             WHERE user_id = ? AND course_id = ?",
            &[ctx.user.id.into_arg(), course_id.into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Ok(Json(Value::Null));
    };
    let id: Uuid = row.try_get("id")?;
    let code: String = row.try_get("code")?;
    let cid: Uuid = row.try_get("course_id")?;
    let issued_at: DateTime<Utc> = row.try_get("issued_at")?;
    let course = db
        .fetch_optional("SELECT title FROM courses WHERE id = ?", &[cid.into_arg()])
        .await?;
    let course_title: Value = match course {
        Some(r) => r.try_get("title")?,
        None => json!({}),
    };
    Ok(Json(json!({
        "id": id.to_string(),
        "code": code,
        "course_id": cid.to_string(),
        "course_title": course_title,
        "issued_at": issued_at.to_rfc3339(),
        "download_url": format!("/api/v1/lms/certificates/{code}/download"),
    })))
}

/// `GET /api/v1/lms/certificates/{code}/download` — owner or LMS
/// manager only; 404 while the sidecar has not rendered the PDF yet.
pub async fn download_certificate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(code): Path<String>,
) -> ApiResult<Response<Body>> {
    let ctx = authenticate(&state, &headers, "/api/v1/lms/certificates").await?;
    let db = state.db_or_500()?;
    let norm = code.trim().to_uppercase();
    let row = db
        .fetch_optional(
            "SELECT user_id, code, pdf_storage_ref FROM certificates WHERE code = ?",
            &[norm.into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "certificate_not_found",
            "گواهی یافت نشد",
        ));
    };
    let owner: Uuid = row.try_get("user_id")?;
    let code_out: String = row.try_get("code")?;
    let pdf_ref: Option<String> = row.try_get("pdf_storage_ref")?;
    let Some(pdf_ref) = pdf_ref.filter(|s| !s.is_empty()) else {
        return Err(ApiError::not_found(
            "certificate_not_found",
            "گواهی یافت نشد",
        ));
    };
    if owner != ctx.user.id && !is_lms_manager(db, ctx.user.id).await? {
        return Err(ApiError::forbidden("forbidden", "دسترسی کافی نیست"));
    }
    let data = read_bytes(&state.config, &pdf_ref).await?;
    let res = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/pdf")
        .header(
            "content-disposition",
            format!("inline; filename=\"certificate-{code_out}.pdf\""),
        )
        .body(Body::from(data))
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}

/// `GET /api/v1/lms/certificates/download-by-ref` — signed download.
/// The signature must carry purpose `lms-certificate` AND the key
/// must really be a certificate's `pdf_storage_ref` (FIX2-08).
pub async fn download_certificate_by_ref(
    State(state): State<AppState>,
    uri: Uri,
) -> ApiResult<Response<Body>> {
    let params = parse_query(&uri);
    let Some(key) = params.get("key") else {
        return Err(validation_err("key", "field is required"));
    };
    let key = key.clone();
    let exp: i64 = match params.get("exp") {
        Some(v) => v
            .trim()
            .parse::<i64>()
            .map_err(|_| validation_err("exp", "must be an integer"))?,
        None => return Err(validation_err("exp", "field is required")),
    };
    let Some(sig) = params.get("sig") else {
        return Err(validation_err("sig", "field is required"));
    };
    let sig = sig.clone();
    let purpose = params.get("purpose").cloned().unwrap_or_default();
    if purpose != SIGNED_PURPOSE_CERTIFICATE
        || !verify_download_sig(&state.config, &key, exp, &sig, SIGNED_PURPOSE_CERTIFICATE)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_signature",
            "امضای دانلود نامعتبر یا منقضی است",
        ));
    }
    let db = state.db_or_500()?;
    let cert = db
        .fetch_optional(
            "SELECT id FROM certificates WHERE pdf_storage_ref = ?",
            &[key.clone().into_arg()],
        )
        .await?;
    if cert.is_none() {
        return Err(ApiError::not_found(
            "certificate_not_found",
            "گواهی یافت نشد",
        ));
    }
    let data = read_bytes(&state.config, &key).await?;
    let res = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/pdf")
        .body(Body::from(data))
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}

/// `GET /api/v1/lms/certificates/{code}/verify` (alias) and
/// `GET /api/v1/certificates/{code}/verify` (public) — same
/// rate-limited implementation.
pub async fn verify_certificate(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(code): Path<String>,
) -> ApiResult<Json<Value>> {
    verify_impl(&state, &headers, peer.0, &code).await
}

// --- admin -------------------------------------------------------------------

async fn require_lms_admin(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, path).await?;
    require_permission(state, &ctx, "lms.manage").await?;
    Ok(ctx)
}

async fn get_curriculum_or_404(db: &Db, id: Uuid) -> ApiResult<DynRow> {
    db.fetch_optional(
        "SELECT id, title, description, sort_order, is_active FROM curricula WHERE id = ?",
        &[id.into_arg()],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("curriculum_not_found", "curriculum یافت نشد"))
}

fn curriculum_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let title: Value = row.try_get("title")?;
    let description: Option<String> = row.try_get("description")?;
    let sort_order: i32 = row.try_get("sort_order")?;
    let is_active: bool = row.try_get("is_active")?;
    Ok(json!({
        "id": id.to_string(),
        "title": title,
        "description": description,
        "sort_order": sort_order,
        "is_active": is_active,
    }))
}

/// `POST /api/v1/admin/lms/curricula` (201).
pub async fn admin_create_curriculum(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let title = req_text_map(&parsed, "title")?;
    let description = match parsed.get("description").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(validation_err("description", "must be a string")),
        None => None,
    };
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?.unwrap_or(0);
    let is_active = opt_bool_loose(&parsed, "is_active")?.unwrap_or(true);
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    let id = Uuid::new_v4();
    db.execute(
        "INSERT INTO curricula (id, title, description, sort_order, is_active, created_at) \
         VALUES (?, ?, ?, ?, ?, ?)",
        &[
            id.into_arg(),
            title.into_arg(),
            description.into_arg(),
            sort_order.into_arg(),
            is_active.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    let row = get_curriculum_or_404(db, id).await?;
    Ok((StatusCode::CREATED, Json(curriculum_out(&row)?)))
}

/// `PATCH /api/v1/admin/lms/curricula/{mid}` — only provided
/// non-null fields are applied (Python's `if v is not None`).
pub async fn admin_update_curriculum(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let mid = parse_path_uuid(&mid, "mid")?;
    let parsed = parse_body(&body)?;
    let title = opt_text_map(&parsed, "title")?;
    let description = match parsed.get("description").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => Some(Some(s.clone())),
        Some(_) => return Err(validation_err("description", "must be a string")),
        None => None,
    };
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?;
    let is_active = opt_bool_loose(&parsed, "is_active")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_curriculum_or_404(db, mid).await?;
    if let Some(t) = title {
        db.execute(
            "UPDATE curricula SET title = ? WHERE id = ?",
            &[t.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(d) = description {
        db.execute(
            "UPDATE curricula SET description = ? WHERE id = ?",
            &[d.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(s) = sort_order {
        db.execute(
            "UPDATE curricula SET sort_order = ? WHERE id = ?",
            &[s.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(a) = is_active {
        db.execute(
            "UPDATE curricula SET is_active = ? WHERE id = ?",
            &[a.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    let row = get_curriculum_or_404(db, mid).await?;
    Ok(Json(curriculum_out(&row)?))
}

/// `GET /api/v1/admin/lms/curricula`.
pub async fn admin_list_curricula(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT id, title, description, sort_order, is_active FROM curricula \
             ORDER BY sort_order, id",
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(curriculum_out(r)?);
    }
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/admin/lms/curricula/{mid}` (204) — courses are
/// orphaned with `curriculum_id` NULL, never deleted with it.
pub async fn admin_delete_curriculum(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let mid = parse_path_uuid(&mid, "mid")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_curriculum_or_404(db, mid).await?;
    db.execute(
        "UPDATE courses SET curriculum_id = NULL WHERE curriculum_id = ?",
        &[mid.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM curricula WHERE id = ?", &[mid.into_arg()])
        .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

async fn get_course_or_404(db: &Db, id: Uuid) -> ApiResult<CourseRow> {
    let row = db
        .fetch_optional(
            &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("course_not_found", "course یافت نشد"))?;
    course_from_row(&row)
}

/// `POST /api/v1/admin/lms/courses` (201).
pub async fn admin_create_course(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let curriculum_id = opt_uuid(&parsed, "curriculum_id")?;
    let title = req_text_map(&parsed, "title")?;
    let description = opt_text_map(&parsed, "description")?;
    let level = opt_enum(&parsed, "level", &LEVEL_ORDER)?.unwrap_or_else(|| "beginner".to_string());
    let visibility = opt_enum(&parsed, "visibility", &["public", "private"])?
        .unwrap_or_else(|| "public".to_string());
    let thumbnail = match parsed.get("thumbnail_media_ref").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() > 1024 {
                return Err(validation_err("thumbnail_media_ref", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("thumbnail_media_ref", "must be a string")),
        None => None,
    };
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?.unwrap_or(0);
    let is_active = opt_bool_loose(&parsed, "is_active")?.unwrap_or(true);
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    if let Some(cid) = curriculum_id {
        get_curriculum_or_404(db, cid).await?;
    }
    let id = Uuid::new_v4();
    db.execute(
        "INSERT INTO courses (id, curriculum_id, title, description, level, visibility, \
         thumbnail_media_ref, sort_order, is_active, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            id.into_arg(),
            curriculum_id.into_arg(),
            title.into_arg(),
            description.into_arg(),
            level.into_arg(),
            visibility.into_arg(),
            thumbnail.into_arg(),
            sort_order.into_arg(),
            is_active.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    let course = get_course_or_404(db, id).await?;
    Ok((StatusCode::CREATED, Json(course_out(&course, 0, None))))
}

/// `PATCH /api/v1/admin/lms/courses/{mid}`.
pub async fn admin_update_course(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let mid = parse_path_uuid(&mid, "mid")?;
    let parsed = parse_body(&body)?;
    let curriculum_id = opt_uuid(&parsed, "curriculum_id")?;
    let title = opt_text_map(&parsed, "title")?;
    let description = opt_text_map(&parsed, "description")?;
    let level = opt_enum(&parsed, "level", &LEVEL_ORDER)?;
    let visibility = opt_enum(&parsed, "visibility", &["public", "private"])?;
    let thumbnail = match parsed.get("thumbnail_media_ref").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() > 1024 {
                return Err(validation_err("thumbnail_media_ref", "length out of range"));
            }
            Some(s.clone())
        }
        Some(_) => return Err(validation_err("thumbnail_media_ref", "must be a string")),
        None => None,
    };
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?;
    let is_active = opt_bool_loose(&parsed, "is_active")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_course_or_404(db, mid).await?;
    if let Some(cid) = curriculum_id {
        get_curriculum_or_404(db, cid).await?;
        db.execute(
            "UPDATE courses SET curriculum_id = ? WHERE id = ?",
            &[cid.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(t) = title {
        db.execute(
            "UPDATE courses SET title = ? WHERE id = ?",
            &[t.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(d) = description {
        db.execute(
            "UPDATE courses SET description = ? WHERE id = ?",
            &[d.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(l) = level {
        db.execute(
            "UPDATE courses SET level = ? WHERE id = ?",
            &[l.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(v) = visibility {
        db.execute(
            "UPDATE courses SET visibility = ? WHERE id = ?",
            &[v.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(t) = thumbnail {
        db.execute(
            "UPDATE courses SET thumbnail_media_ref = ? WHERE id = ?",
            &[t.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(s) = sort_order {
        db.execute(
            "UPDATE courses SET sort_order = ? WHERE id = ?",
            &[s.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(a) = is_active {
        db.execute(
            "UPDATE courses SET is_active = ? WHERE id = ?",
            &[a.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    let course = get_course_or_404(db, mid).await?;
    Ok(Json(course_out(&course, 0, None)))
}

/// `GET /api/v1/admin/lms/courses` — all courses incl. inactive;
/// `lesson_count` is hardcoded 0, like Python's `_course_admin_out`.
pub async fn admin_list_courses(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!("SELECT {COURSE_COLS} FROM courses ORDER BY sort_order, id"),
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(course_out(&course_from_row(r)?, 0, None));
    }
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/admin/lms/courses/{mid}` (204) — refused (409)
/// while the course has issued certificates.
pub async fn admin_delete_course(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let mid = parse_path_uuid(&mid, "mid")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_course_or_404(db, mid).await?;
    let cert_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM certificates WHERE course_id = ?",
            &[mid.into_arg()],
        )
        .await?;
    let cert_count: i64 = cert_row.try_get("n")?;
    if cert_count > 0 {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "course_has_certificates",
            "دوره گواهی صادرشده دارد و حذف نمی‌شود",
        ));
    }
    db.execute(
        "DELETE FROM user_progress WHERE lesson_id IN (SELECT id FROM lessons WHERE course_id = ?)",
        &[mid.into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE quiz_attempts SET lesson_id = NULL WHERE lesson_id IN (SELECT id FROM lessons WHERE course_id = ?)",
        &[mid.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM lessons WHERE course_id = ?", &[mid.into_arg()])
        .await?;
    db.execute(
        "DELETE FROM course_prerequisites WHERE course_id = ? OR requires_course_id = ?",
        &[mid.into_arg(), mid.into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM cohort_enrollments WHERE cohort_id IN (SELECT id FROM course_cohorts WHERE course_id = ?)",
        &[mid.into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM course_cohorts WHERE course_id = ?",
        &[mid.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM courses WHERE id = ?", &[mid.into_arg()])
        .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

async fn get_lesson_or_404(db: &Db, id: Uuid) -> ApiResult<LessonRow> {
    let row = db
        .fetch_optional(
            &format!("SELECT {LESSON_COLS} FROM lessons WHERE id = ?"),
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("lesson_not_found", "lesson یافت نشد"))?;
    lesson_from_row(&row)
}

/// `POST /api/v1/admin/lms/lessons` (201).
pub async fn admin_create_lesson(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let course_id = req_uuid(&parsed, "course_id")?;
    let title = req_text_map(&parsed, "title")?;
    let body_list = opt_list(&parsed, "body")?.unwrap_or_default();
    let duration_minutes =
        opt_int_loose(&parsed, "duration_minutes", i64::MIN, i64::MAX)?.unwrap_or(0);
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?.unwrap_or(0);
    let is_free_preview = opt_bool_loose(&parsed, "is_free_preview")?.unwrap_or(false);
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_course_or_404(db, course_id).await?;
    let id = Uuid::new_v4();
    db.execute(
        "INSERT INTO lessons (id, course_id, title, body, duration_minutes, sort_order, \
         is_free_preview, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            id.into_arg(),
            course_id.into_arg(),
            title.into_arg(),
            Value::Array(body_list).into_arg(),
            duration_minutes.into_arg(),
            sort_order.into_arg(),
            is_free_preview.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    let lesson = get_lesson_or_404(db, id).await?;
    Ok((
        StatusCode::CREATED,
        Json(lesson_out(&lesson, None, 0, None, None)),
    ))
}

/// `PATCH /api/v1/admin/lms/lessons/{mid}`.
pub async fn admin_update_lesson(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let mid = parse_path_uuid(&mid, "mid")?;
    let parsed = parse_body(&body)?;
    let title = opt_text_map(&parsed, "title")?;
    let body_list = opt_list(&parsed, "body")?;
    let duration_minutes = opt_int_loose(&parsed, "duration_minutes", i64::MIN, i64::MAX)?;
    let sort_order = opt_int_loose(&parsed, "sort_order", i64::MIN, i64::MAX)?;
    let is_free_preview = opt_bool_loose(&parsed, "is_free_preview")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_lesson_or_404(db, mid).await?;
    if let Some(t) = title {
        db.execute(
            "UPDATE lessons SET title = ? WHERE id = ?",
            &[t.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(b) = body_list {
        db.execute(
            "UPDATE lessons SET body = ? WHERE id = ?",
            &[Value::Array(b).into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(d) = duration_minutes {
        db.execute(
            "UPDATE lessons SET duration_minutes = ? WHERE id = ?",
            &[d.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(s) = sort_order {
        db.execute(
            "UPDATE lessons SET sort_order = ? WHERE id = ?",
            &[s.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(f) = is_free_preview {
        db.execute(
            "UPDATE lessons SET is_free_preview = ? WHERE id = ?",
            &[f.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    let lesson = get_lesson_or_404(db, mid).await?;
    Ok(Json(lesson_out(&lesson, None, 0, None, None)))
}

/// `GET /api/v1/admin/lms/courses/{course_id}/lessons`.
pub async fn admin_list_lessons(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(course_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let course_id = parse_path_uuid(&course_id, "course_id")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_course_or_404(db, course_id).await?;
    let rows = db
        .fetch_all(
            &format!(
                "SELECT {LESSON_COLS} FROM lessons WHERE course_id = ? ORDER BY sort_order, id"
            ),
            &[course_id.into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        out.push(lesson_out(&lesson_from_row(r)?, None, 0, None, None));
    }
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/admin/lms/lessons/{mid}` (204) — progress rows go
/// with it; quiz attempts keep their history with `lesson_id` NULL.
pub async fn admin_delete_lesson(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let mid = parse_path_uuid(&mid, "mid")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_lesson_or_404(db, mid).await?;
    db.execute(
        "DELETE FROM user_progress WHERE lesson_id = ?",
        &[mid.into_arg()],
    )
    .await?;
    db.execute(
        "UPDATE quiz_attempts SET lesson_id = NULL WHERE lesson_id = ?",
        &[mid.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM lessons WHERE id = ?", &[mid.into_arg()])
        .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

async fn get_cohort_or_404(db: &Db, id: Uuid) -> ApiResult<CohortRow> {
    let row = db
        .fetch_optional(
            &format!("SELECT {COHORT_COLS} FROM course_cohorts WHERE id = ?"),
            &[id.into_arg()],
        )
        .await?
        .ok_or_else(|| ApiError::not_found("cohort_not_found", "cohort یافت نشد"))?;
    cohort_from_row(&row)
}

fn bad_cohort_window() -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "bad_cohort_window",
        "پایان هم‌گروهی باید بعد از شروع باشد",
    )
}

/// `POST /api/v1/admin/lms/cohorts` (201).
pub async fn admin_create_cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let course_id = req_uuid(&parsed, "course_id")?;
    let title = req_text_map(&parsed, "title")?;
    let starts_at = req_datetime(&parsed, "starts_at")?;
    let ends_at = req_datetime(&parsed, "ends_at")?;
    let max_seats = match opt_int_loose(&parsed, "max_seats", 1, i64::MAX)? {
        Some(n) => n,
        None => return Err(validation_err("max_seats", "field is required")),
    };
    let is_open = opt_bool_loose(&parsed, "is_open")?.unwrap_or(true);
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_course_or_404(db, course_id).await?;
    if ends_at <= starts_at {
        return Err(bad_cohort_window());
    }
    let id = Uuid::new_v4();
    db.execute(
        "INSERT INTO course_cohorts (id, course_id, title, starts_at, ends_at, max_seats, \
         is_open, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            id.into_arg(),
            course_id.into_arg(),
            title.into_arg(),
            starts_at.into_arg(),
            ends_at.into_arg(),
            max_seats.into_arg(),
            is_open.into_arg(),
            utc_now().into_arg(),
        ],
    )
    .await?;
    let cohort = get_cohort_or_404(db, id).await?;
    Ok((
        StatusCode::CREATED,
        Json(cohort_out(&cohort, 0, max_seats, false)),
    ))
}

/// `PATCH /api/v1/admin/lms/cohorts/{mid}` — the merged window is
/// checked before any write.
pub async fn admin_update_cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let mid = parse_path_uuid(&mid, "mid")?;
    let parsed = parse_body(&body)?;
    let title = opt_text_map(&parsed, "title")?;
    let starts_at = opt_datetime(&parsed, "starts_at")?;
    let ends_at = opt_datetime(&parsed, "ends_at")?;
    let max_seats = opt_int_loose(&parsed, "max_seats", 1, i64::MAX)?;
    let is_open = opt_bool_loose(&parsed, "is_open")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    let cohort = get_cohort_or_404(db, mid).await?;
    let new_starts = starts_at.unwrap_or(cohort.starts_at);
    let new_ends = ends_at.unwrap_or(cohort.ends_at);
    if new_ends <= new_starts {
        return Err(bad_cohort_window());
    }
    if let Some(t) = title {
        db.execute(
            "UPDATE course_cohorts SET title = ? WHERE id = ?",
            &[t.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(s) = starts_at {
        db.execute(
            "UPDATE course_cohorts SET starts_at = ? WHERE id = ?",
            &[s.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(e) = ends_at {
        db.execute(
            "UPDATE course_cohorts SET ends_at = ? WHERE id = ?",
            &[e.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(m) = max_seats {
        db.execute(
            "UPDATE course_cohorts SET max_seats = ? WHERE id = ?",
            &[m.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    if let Some(o) = is_open {
        db.execute(
            "UPDATE course_cohorts SET is_open = ? WHERE id = ?",
            &[o.into_arg(), mid.into_arg()],
        )
        .await?;
    }
    let cohort = get_cohort_or_404(db, mid).await?;
    Ok(Json(cohort_out(&cohort, 0, cohort.max_seats as i64, false)))
}

/// `GET /api/v1/admin/lms/cohorts` — all cohorts with enrollment counts.
pub async fn admin_list_cohorts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            &format!("SELECT {COHORT_COLS} FROM course_cohorts ORDER BY starts_at, id"),
            &[],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let c = cohort_from_row(r)?;
        let enrolled = enrolled_count(db, c.id).await?;
        out.push(cohort_out(
            &c,
            enrolled,
            (c.max_seats as i64 - enrolled).max(0),
            false,
        ));
    }
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/admin/lms/cohorts/{mid}` (204).
pub async fn admin_delete_cohort(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mid): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let mid = parse_path_uuid(&mid, "mid")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_cohort_or_404(db, mid).await?;
    db.execute(
        "DELETE FROM cohort_enrollments WHERE cohort_id = ?",
        &[mid.into_arg()],
    )
    .await?;
    db.execute("DELETE FROM course_cohorts WHERE id = ?", &[mid.into_arg()])
        .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

/// `GET /api/v1/admin/lms/cohorts/{cohort_id}/enrollments`.
pub async fn admin_list_cohort_enrollments(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cohort_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let cohort_id = parse_path_uuid(&cohort_id, "cohort_id")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_cohort_or_404(db, cohort_id).await?;
    let rows = db
        .fetch_all(
            "SELECT e.id AS eid, e.cohort_id, e.user_id, e.enrolled_at, e.status, \
             u.full_name, u.email, u.phone FROM cohort_enrollments e \
             JOIN users u ON u.id = e.user_id \
             WHERE e.cohort_id = ? ORDER BY e.enrolled_at, e.id",
            &[cohort_id.into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("eid")?;
        let cid: Uuid = r.try_get("cohort_id")?;
        let uid: Uuid = r.try_get("user_id")?;
        let enrolled_at: DateTime<Utc> = r.try_get("enrolled_at")?;
        let status: String = r.try_get("status")?;
        let full_name: String = r.try_get("full_name")?;
        let email: Option<String> = r.try_get("email")?;
        let phone: Option<String> = r.try_get("phone")?;
        out.push(json!({
            "id": id.to_string(),
            "cohort_id": cid.to_string(),
            "user_id": uid.to_string(),
            "user_display": user_display(&full_name, email.as_deref(), phone.as_deref(), uid),
            "enrolled_at": enrolled_at.to_rfc3339(),
            "status": status,
        }));
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/admin/lms/users/{user_id}/progress` — per-course
/// lesson-by-lesson progress for one user.
pub async fn admin_user_progress(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let user_id = parse_path_uuid(&user_id, "user_id")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    if db
        .fetch_optional("SELECT id FROM users WHERE id = ?", &[user_id.into_arg()])
        .await?
        .is_none()
    {
        return Err(ApiError::not_found("user_not_found", "کاربر یافت نشد"));
    }
    let progresses = db
        .fetch_all(
            "SELECT lesson_id, status, completed_at FROM user_progress WHERE user_id = ?",
            &[user_id.into_arg()],
        )
        .await?;
    let mut by_lesson: HashMap<Uuid, (bool, Option<DateTime<Utc>>)> = HashMap::new();
    let mut lesson_ids: Vec<Uuid> = Vec::new();
    for r in &progresses {
        let lid: Uuid = r.try_get("lesson_id")?;
        let status: String = r.try_get("status")?;
        let completed_at: Option<DateTime<Utc>> = r.try_get("completed_at")?;
        by_lesson.insert(lid, (status == "completed", completed_at));
        lesson_ids.push(lid);
    }
    let mut course_ids: Vec<Uuid> = Vec::new();
    if !lesson_ids.is_empty() {
        let placeholders: Vec<&str> = lesson_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT course_id FROM lessons WHERE id IN ({})",
            placeholders.join(", ")
        );
        let args: Vec<Arg> = lesson_ids.iter().map(|id| id.into_arg()).collect();
        for r in db.fetch_all(&sql, &args).await? {
            course_ids.push(r.try_get("course_id")?);
        }
    }
    let mut courses_out: Vec<Value> = Vec::new();
    for cid in &course_ids {
        let Some(course_row) = db
            .fetch_optional(
                &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
                &[(*cid).into_arg()],
            )
            .await?
        else {
            continue;
        };
        let course = course_from_row(&course_row)?;
        let lessons = db
            .fetch_all(
                &format!(
                    "SELECT {LESSON_COLS} FROM lessons WHERE course_id = ? ORDER BY sort_order, id"
                ),
                &[(*cid).into_arg()],
            )
            .await?;
        let mut items: Vec<Value> = Vec::new();
        let mut done = 0i64;
        for r in &lessons {
            let lesson = lesson_from_row(r)?;
            let (completed, completed_at) =
                by_lesson.get(&lesson.id).copied().unwrap_or((false, None));
            if completed {
                done += 1;
            }
            items.push(json!({
                "lesson_id": lesson.id.to_string(),
                "lesson_title": lesson.title,
                "sort_order": lesson.sort_order,
                "completed": completed,
                "completed_at": completed_at.map(|d| d.to_rfc3339()),
            }));
        }
        let total = lessons.len() as i64;
        let pct = if total > 0 {
            round1(done as f64 / total as f64 * 100.0)
        } else {
            0.0
        };
        courses_out.push(json!({
            "course_id": cid.to_string(),
            "course_title": course.title,
            "total_lessons": total,
            "completed_lessons": done,
            "progress_pct": pct,
            "lessons": items,
        }));
    }
    courses_out.sort_by(|a, b| {
        b["progress_pct"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["progress_pct"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(Json(json!({
        "user_id": user_id.to_string(),
        "courses": courses_out,
    })))
}

/// `GET /api/v1/admin/lms/certificates` — newest first.
pub async fn admin_list_certificates(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 100, 1, 500)?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT c.id AS cert_id, c.code, c.user_id, c.course_id, c.issued_at, \
             u.full_name, u.email, u.phone, co.title AS course_title \
             FROM certificates c JOIN users u ON u.id = c.user_id \
             JOIN courses co ON co.id = c.course_id \
             ORDER BY c.issued_at DESC, c.id LIMIT ?",
            &[limit.into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("cert_id")?;
        let code: String = r.try_get("code")?;
        let uid: Uuid = r.try_get("user_id")?;
        let cid: Uuid = r.try_get("course_id")?;
        let issued_at: DateTime<Utc> = r.try_get("issued_at")?;
        let full_name: String = r.try_get("full_name")?;
        let email: Option<String> = r.try_get("email")?;
        let phone: Option<String> = r.try_get("phone")?;
        let course_title: Value = r.try_get("course_title")?;
        out.push(json!({
            "id": id.to_string(),
            "code": code,
            "user_id": uid.to_string(),
            "user_display": user_display(&full_name, email.as_deref(), phone.as_deref(), uid),
            "course_id": cid.to_string(),
            "course_title": course_title,
            "issued_at": issued_at.to_rfc3339(),
        }));
    }
    Ok(Json(Value::Array(out)))
}

/// DFS from `requires_course_id` over the prerequisite graph —
///
/// adding (course_id ← requires_course_id) would close a cycle iff
/// `course_id` is reachable.
async fn would_create_cycle(
    db: &Db,
    course_id: Uuid,
    requires_course_id: Uuid,
) -> Result<bool, ApiError> {
    let mut seen: HashSet<Uuid> = HashSet::new();
    let mut stack = vec![requires_course_id];
    while let Some(cur) = stack.pop() {
        if cur == course_id {
            return Ok(true);
        }
        if !seen.insert(cur) {
            continue;
        }
        let rows = db
            .fetch_all(
                "SELECT requires_course_id FROM course_prerequisites WHERE course_id = ?",
                &[cur.into_arg()],
            )
            .await?;
        for r in &rows {
            stack.push(r.try_get("requires_course_id")?);
        }
    }
    Ok(false)
}

/// `POST /api/v1/admin/lms/prerequisites` (201).
pub async fn admin_add_prerequisite(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let parsed = parse_body(&body)?;
    let course_id = req_uuid(&parsed, "course_id")?;
    let requires_course_id = req_uuid(&parsed, "requires_course_id")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    get_course_or_404(db, course_id).await?;
    if db
        .fetch_optional(
            &format!("SELECT {COURSE_COLS} FROM courses WHERE id = ?"),
            &[requires_course_id.into_arg()],
        )
        .await?
        .is_none()
    {
        // Python names this lookup "required course", so the code is
        // literally `required course_not_found`.
        return Err(ApiError::not_found(
            "required course_not_found",
            "required course یافت نشد",
        ));
    }
    if course_id == requires_course_id {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "self_prerequisite",
            "یک دوره نمی‌تواند پیش‌نیاز خودش باشد",
        ));
    }
    if would_create_cycle(db, course_id, requires_course_id).await? {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "prerequisite_cycle",
            "این پیش‌نیاز چرخه می‌سازد",
        ));
    }
    let id = Uuid::new_v4();
    // Pre-check (the `Db` layer already converts a failed INSERT to a
    // plain 500 `ApiError`, so a duplicate must be detected here —
    // the same 409 Python raises from its IntegrityError handler).
    if db
        .fetch_optional(
            "SELECT id FROM course_prerequisites WHERE course_id = ? AND requires_course_id = ?",
            &[course_id.into_arg(), requires_course_id.into_arg()],
        )
        .await?
        .is_some()
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "prerequisite_exists",
            "این پیش‌نیاز قبلاً ثبت شده است",
        ));
    }
    db.execute(
        "INSERT INTO course_prerequisites (id, course_id, requires_course_id) VALUES (?, ?, ?)",
        &[
            id.into_arg(),
            course_id.into_arg(),
            requires_course_id.into_arg(),
        ],
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({
            "id": id.to_string(),
            "course_id": course_id.to_string(),
            "requires_course_id": requires_course_id.to_string(),
        })),
    ))
}

/// `DELETE /api/v1/admin/lms/prerequisites?course_id=…&requires_course_id=…` (204).
pub async fn admin_remove_prerequisite(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<(StatusCode, ())> {
    let params = parse_query(&uri);
    let course_id = query_uuid(&params, "course_id")?;
    let requires_course_id = query_uuid(&params, "requires_course_id")?;
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id FROM course_prerequisites WHERE course_id = ? AND requires_course_id = ?",
            &[course_id.into_arg(), requires_course_id.into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "prerequisite_not_found",
            "پیش‌نیاز یافت نشد",
        ));
    };
    let id: Uuid = row.try_get("id")?;
    db.execute(
        "DELETE FROM course_prerequisites WHERE id = ?",
        &[id.into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

/// `GET /api/v1/admin/lms/prerequisites` — optional `course_id` filter.
pub async fn admin_list_prerequisites(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    require_lms_admin(&state, &headers, "/api/v1/admin/lms").await?;
    let params = parse_query(&uri);
    let course_id = query_opt_uuid(&params, "course_id")?;
    let db = state.db_or_500()?;
    let mut sql =
        String::from("SELECT id, course_id, requires_course_id FROM course_prerequisites");
    let mut args: Vec<Arg> = Vec::new();
    if let Some(cid) = course_id {
        sql.push_str(" WHERE course_id = ?");
        args.push(cid.into_arg());
    }
    sql.push_str(" ORDER BY course_id, requires_course_id");
    let rows = db.fetch_all(&sql, &args).await?;
    let mut out = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("id")?;
        let cid: Uuid = r.try_get("course_id")?;
        let req: Uuid = r.try_get("requires_course_id")?;
        out.push(json!({
            "id": id.to_string(),
            "course_id": cid.to_string(),
            "requires_course_id": req.to_string(),
        }));
    }
    Ok(Json(Value::Array(out)))
}
