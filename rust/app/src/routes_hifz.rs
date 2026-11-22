//! Hifz routes — mirrors `app/modules/quran/hifz_router.py`,
//! `hifz_service.py`, `hifz_testing.py`, `hifz_plan_service.py`,
//! `hifz_plan.py`, `hifz_mutashabihat.py`, `hifz_purge.py` and
//! `hifz_listen_sync.py`.
//!
//! ASR-dependent endpoints stay PROXIED (ADR-008/ADR-031): the Rust
//! binary embeds no speech recognizer and never fakes scores —
//! `POST /attempts/{id}/grade`, `POST /attempts/{id}/grade-word`,
//! `POST /voice-search`, `POST /tests/{id}/questions/{qid}/answer`
//! are not registered here: since the Wave 8 lift they are explicit
//! sidecar-contract routes in `routes_lift` (ADR-134) that forward
//! to `SIDECAR_URL` when one is configured and answer an honest 503
//! `hifz_asr_unavailable` when it is not. The live WebSocket
//! `WS /attempts/{id}/stream` is registered explicitly as an honest
//! 503 — there is no ASR in the Rust binary; see ADR-031.
//! Everything else in this module is native.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::{
    body::Bytes,
    extract::{Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::authn::{authenticate, require_permission, AuthCtx};
use crate::db::{new_id, utc_now, Arg, Db, DynRow, IntoArg};
use crate::errors::{ApiError, ApiResult};
use crate::hifz_logic::{
    ayah_words, build_passages, method_3x3_initial_state, method_3x3_tap,
    method_6446_initial_state, method_6446_tap, plan_ayah_order, quality_from_grade, sm2_next,
    tasmi8_advance, tasmi8_initial_state, tasmi_current, tasmi_review_due_at, test_question_score,
    test_total_score,
};
use crate::hifz_mutashabihat::{group_for_phrase, groups_for_ayah, phrase_count};
use crate::quran_data::get_surah;
use crate::routes_quran::qset;
use crate::validate::{
    opt_bool_loose, opt_int_loose, parse_body, parse_path_uuid, parse_query, query_int,
    query_opt_int, req_int_loose, req_str_raw,
};
use crate::{AppState, Config};

// --- small helpers -----------------------------------------------------------

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn not_found(code: &'static str, message: &str) -> ApiError {
    ApiError::not_found(code, message)
}

fn domain(status: StatusCode, code: &'static str, message: &str) -> ApiError {
    ApiError::new(status, code, message)
}

fn iso(dt: DateTime<Utc>) -> String {
    dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn iso_opt(dt: Option<DateTime<Utc>>) -> Value {
    match dt {
        Some(d) => Value::String(iso(d)),
        None => Value::Null,
    }
}

fn opt_dt(row: &DynRow, col: &str) -> Result<Option<DateTime<Utc>>, ApiError> {
    row.try_get(col)
}

fn counts_json(c: [i64; 4]) -> Value {
    json!({
        "correct": c[0],
        "close": c[1],
        "wrong": c[2],
        "skipped": c[3],
    })
}

async fn attempt_counts(db: &Db, attempt_id: Uuid) -> Result<[i64; 4], ApiError> {
    let rows = db
        .fetch_all(
            "SELECT status, COUNT(*) AS n FROM hifz_word_scores WHERE attempt_id = ? GROUP BY status",
            &[(attempt_id).into_arg()],
        )
        .await?;
    let mut out = [0i64; 4];
    for r in &rows {
        let status: String = r.try_get("status")?;
        let n: i64 = r.try_get("n")?;
        match status.as_str() {
            "correct" => out[0] = n,
            "close" => out[1] = n,
            "wrong" => out[2] = n,
            "skipped" => out[3] = n,
            _ => {}
        }
    }
    Ok(out)
}

async fn attempt_counts_many(
    db: &Db,
    attempt_ids: &[Uuid],
) -> Result<HashMap<Uuid, [i64; 4]>, ApiError> {
    let mut map: HashMap<Uuid, [i64; 4]> = HashMap::new();
    for id in attempt_ids {
        map.insert(*id, attempt_counts(db, *id).await?);
    }
    Ok(map)
}

fn attempt_out(row: &DynRow, counts: [i64; 4]) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let surah: i32 = row.try_get("surah")?;
    let page: Option<i32> = row.try_get("page")?;
    let status: String = row.try_get("status")?;
    let engine: String = row.try_get("engine")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: Option<DateTime<Utc>> = opt_dt(row, "updated_at")?;
    let completed_at: Option<DateTime<Utc>> = opt_dt(row, "completed_at")?;
    let start_ayah: Option<i32> = row.try_get("start_ayah")?;
    let end_ayah: Option<i32> = row.try_get("end_ayah")?;
    let current_surah: Option<i32> = row.try_get("current_surah")?;
    let current_ayah: Option<i32> = row.try_get("current_ayah")?;
    let current_word_index: Option<i32> = row.try_get("current_word_index")?;
    let repeat_ayah: i32 = row.try_get("repeat_ayah")?;
    let repeat_range: i32 = row.try_get("repeat_range")?;
    Ok(json!({
        "id": id.to_string(),
        "surah": surah,
        "page": page,
        "status": status,
        "engine": engine,
        "counts": counts_json(counts),
        "created_at": iso(created_at),
        "updated_at": iso_opt(updated_at),
        "completed_at": iso_opt(completed_at),
        "start_ayah": start_ayah,
        "end_ayah": end_ayah,
        "current_surah": current_surah,
        "current_ayah": current_ayah,
        "current_word_index": current_word_index,
        "repeat_ayah": repeat_ayah,
        "repeat_range": repeat_range,
    }))
}

async fn attempt_or_404(db: &Db, user_id: Uuid, attempt_id: Uuid) -> Result<DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_attempts WHERE id = ?",
            &[(attempt_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != user_id {
                return Err(not_found(
                    "hifz_attempt_not_found",
                    "نشست حفظی با این شناسه یافت نشد",
                ));
            }
            Ok(r)
        }
        None => Err(not_found(
            "hifz_attempt_not_found",
            "نشست حفظی با این شناسه یافت نشد",
        )),
    }
}

async fn engine_snapshot(db: &Db, config: &Config) -> Result<String, ApiError> {
    let raw = qset(db, config, "quran.asr_engine", json!("zipformer")).await?;
    let raw = raw.as_str().unwrap_or("zipformer").to_string();
    if raw == "zipformer" || raw == "whisper_quran" {
        Ok(raw)
    } else {
        Ok("zipformer".to_string())
    }
}

/// Profile gate for `hifz_advanced` (tests + tasmi creation):
/// only enforced when the CRM setting is `true`; incomplete profile
/// → 403 `profile_incomplete` listing the missing fields.
async fn ensure_profile_for_hifz_advanced(
    db: &Db,
    config: &Config,
    ctx: &AuthCtx,
) -> Result<(), ApiError> {
    let required = qset(
        db,
        config,
        "profile.require_for_hifz_advanced",
        json!(false),
    )
    .await?;
    if required.as_bool() != Some(true) {
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
        return Err(ApiError::with_details(
            StatusCode::FORBIDDEN,
            "profile_incomplete",
            format!(
                "برای استفاده از این بخش، اول پروفایل‌تان را کامل کنید: {}",
                missing.join("، ")
            ),
            json!({ "missing_fields": missing }),
        ));
    }
    Ok(())
}

/// Purge rate limit (Python: 3/hour per login session). In-process
/// sliding window keyed by session id, falling back to the user id.
static PURGE_LIMITER: OnceLock<Mutex<HashMap<String, Vec<Instant>>>> = OnceLock::new();

fn purge_rate_limited(ctx: &AuthCtx) -> bool {
    let key = match ctx.session_id {
        Some(sid) => format!("session:{sid}"),
        None => format!("user:{}", ctx.user.id),
    };
    let map = PURGE_LIMITER.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut guard) = map.lock() else {
        return false;
    };
    let now = Instant::now();
    let window = Duration::from_secs(3600);
    let entries = guard.entry(key).or_default();
    entries.retain(|t| now.duration_since(*t) <= window);
    if entries.len() >= 3 {
        return true;
    }
    entries.push(now);
    false
}

fn rate_limited_err() -> ApiError {
    domain(
        StatusCode::TOO_MANY_REQUESTS,
        "rate_limited",
        "تعداد درخواست بیش از حد مجاز است",
    )
}

// --- status --------------------------------------------------------------------

/// `GET /api/v1/quran/hifz/status` — public (no auth).
///
/// Honest Rust answer: the binary embeds no ASR engine, so the
/// selected engine is always reported unavailable (ADR-031); the
/// phoneme map is likewise not loaded. The kill switch
/// `RUST_ANYTHING_UNIT_HIFZ=original` restores the Python probe.
pub async fn hifz_status(State(state): State<AppState>) -> ApiResult<Json<Value>> {
    let db = state.db_or_500()?;
    let raw = qset(db, &state.config, "quran.asr_engine", json!("zipformer")).await?;
    let engine = raw.as_str().unwrap_or("zipformer").to_string();
    let valid = engine == "zipformer" || engine == "whisper_quran";
    if !valid {
        return Ok(Json(json!({
            "engine": engine,
            "available": false,
            "reason": "موتور انتخاب‌شده معتبر نیست",
            "map_available": false,
            "map_reason": "map_unavailable",
        })));
    }
    Ok(Json(json!({
        "engine": engine,
        "available": false,
        "reason": "موتور تشخیص گفتار در نسخهٔ Rust پیکربندی نشده است",
        "map_available": false,
        "map_reason": "map_unavailable",
    })))
}

// --- attempts -------------------------------------------------------------------

/// `POST /api/v1/quran/hifz/attempts`.
pub async fn create_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let parsed = parse_body(&body)?;
    let surah = req_int_loose(&parsed, "surah", 1, 114)?;
    let page = opt_int_loose(&parsed, "page", 1, 604)?;
    let start_ayah = opt_int_loose(&parsed, "start_ayah", 1, 286)?;
    let end_ayah = opt_int_loose(&parsed, "end_ayah", 1, 286)?;
    let repeat_ayah = opt_int_loose(&parsed, "repeat_ayah", 1, 20)?.unwrap_or(1);
    let repeat_range = opt_int_loose(&parsed, "repeat_range", 1, 20)?.unwrap_or(1);
    if let (Some(s), Some(e)) = (start_ayah, end_ayah) {
        if s > e {
            return Err(domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hifz_range_invalid",
                "آیهٔ شروع بازه نمی‌تواند پس از آیهٔ پایان باشد",
            ));
        }
    }
    let db = state.db_or_500()?;
    let engine = engine_snapshot(db, &state.config).await?;
    let id = new_id();
    let now = utc_now();
    let current_ayah = start_ayah.unwrap_or(1) as i32;
    db.execute(
        "INSERT INTO hifz_attempts (id, user_id, surah, page, status, engine, extra_words, \
         start_ayah, end_ayah, current_surah, current_ayah, current_word_index, \
         repeat_ayah, repeat_range, created_at, updated_at, completed_at) \
         VALUES (?, ?, ?, ?, 'in_progress', ?, '[]', ?, ?, ?, ?, 0, ?, ?, ?, NULL, NULL)",
        &[
            (id).into_arg(),
            (ctx.user.id).into_arg(),
            (surah).into_arg(),
            match page {
                Some(p) => (p).into_arg(),
                None => Arg::NullInt,
            },
            (engine).into_arg(),
            match start_ayah {
                Some(v) => (v).into_arg(),
                None => Arg::NullInt,
            },
            match end_ayah {
                Some(v) => (v).into_arg(),
                None => Arg::NullInt,
            },
            (surah).into_arg(),
            (current_ayah).into_arg(),
            (repeat_ayah).into_arg(),
            (repeat_range).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    let row = db
        .fetch_one(
            "SELECT * FROM hifz_attempts WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    Ok((StatusCode::CREATED, Json(attempt_out(&row, [0, 0, 0, 0])?)))
}

/// `GET /api/v1/quran/hifz/attempts/resume` — newest open attempt,
/// or JSON `null` (200) when none.
pub async fn resume_attempt_lookup(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts/resume").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_attempts WHERE user_id = ? AND status IN ('in_progress', 'paused') \
             ORDER BY updated_at DESC NULLS LAST, created_at DESC LIMIT 1",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    match row {
        None => Ok(Json(Value::Null)),
        Some(r) => {
            let id: Uuid = r.try_get("id")?;
            let counts = attempt_counts(db, id).await?;
            Ok(Json(attempt_out(&r, counts)?))
        }
    }
}

/// `GET /api/v1/quran/hifz/attempts`.
pub async fn list_attempts(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let offset = query_int(&params, "offset", 0, 0, i64::MAX)?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT * FROM hifz_attempts WHERE user_id = ? \
             ORDER BY updated_at DESC NULLS LAST, created_at DESC LIMIT ? OFFSET ?",
            &[
                (ctx.user.id).into_arg(),
                (limit).into_arg(),
                (offset).into_arg(),
            ],
        )
        .await?;
    let ids: Vec<Uuid> = rows
        .iter()
        .map(|r| r.try_get::<Uuid, _>("id"))
        .collect::<Result<Vec<_>, _>>()?;
    let counts = attempt_counts_many(db, &ids).await?;
    let mut out: Vec<Value> = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("id")?;
        out.push(attempt_out(
            r,
            counts.get(&id).copied().unwrap_or([0, 0, 0, 0]),
        )?);
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/quran/hifz/attempts/{attempt_id}`.
pub async fn get_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let counts = attempt_counts(db, attempt_id).await?;
    let words = db
        .fetch_all(
            "SELECT * FROM hifz_word_scores WHERE attempt_id = ? ORDER BY surah, ayah, word_index",
            &[(attempt_id).into_arg()],
        )
        .await?;
    let mistakes = db
        .fetch_all(
            "SELECT * FROM hifz_mistakes WHERE attempt_id = ? ORDER BY created_at DESC",
            &[(attempt_id).into_arg()],
        )
        .await?;
    Ok(Json(json!({
        "attempt": attempt_out(&row, counts)?,
        "words": words.iter().map(word_score_out).collect::<Result<Vec<_>, _>>()?,
        "mistakes": mistakes.iter().map(mistake_out).collect::<Result<Vec<_>, _>>()?,
    })))
}

fn word_score_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let attempt_id: Uuid = row.try_get("attempt_id")?;
    let surah: i32 = row.try_get("surah")?;
    let ayah: i32 = row.try_get("ayah")?;
    let word_index: i32 = row.try_get("word_index")?;
    let expected_text: String = row.try_get("expected_text")?;
    let status: String = row.try_get("status")?;
    let prob: f64 = row.try_get("prob")?;
    let heard: Option<String> = row.try_get("heard")?;
    let mistake_type: Option<String> = row.try_get("mistake_type")?;
    let disputed: bool = row.try_get("disputed")?;
    Ok(json!({
        "id": id.to_string(),
        "attempt_id": attempt_id.to_string(),
        "surah": surah,
        "ayah": ayah,
        "word_index": word_index,
        "expected_text": expected_text,
        "status": status,
        "prob": prob,
        "heard": heard,
        "mistake_type": mistake_type,
        "disputed": disputed,
    }))
}

fn mistake_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let attempt_id: Uuid = row.try_get("attempt_id")?;
    let surah: i32 = row.try_get("surah")?;
    let ayah: i32 = row.try_get("ayah")?;
    let word_index: Option<i32> = row.try_get("word_index")?;
    let mistake_type: String = row.try_get("mistake_type")?;
    let expected_text: String = row.try_get("expected_text")?;
    let heard: Option<String> = row.try_get("heard")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "attempt_id": attempt_id.to_string(),
        "surah": surah,
        "ayah": ayah,
        "word_index": word_index,
        "mistake_type": mistake_type,
        "expected_text": expected_text,
        "heard": heard,
        "created_at": iso(created_at),
    }))
}

/// `POST /api/v1/quran/hifz/attempts/{attempt_id}/complete`.
pub async fn complete_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let now = utc_now();
    db.execute(
        "UPDATE hifz_attempts SET status = 'completed', completed_at = ?, updated_at = ? WHERE id = ?",
        &[(now).into_arg(), (now).into_arg(), (attempt_id).into_arg()],
    )
    .await?;
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let counts = attempt_counts(db, attempt_id).await?;
    Ok(Json(attempt_out(&row, counts)?))
}

/// `PATCH /api/v1/quran/hifz/attempts/{attempt_id}/position`.
pub async fn update_position(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let parsed = parse_body(&body)?;
    let current_surah = req_int_loose(&parsed, "current_surah", 1, 114)?;
    let current_ayah = req_int_loose(&parsed, "current_ayah", 1, 286)?;
    let current_word_index = opt_int_loose(&parsed, "current_word_index", 0, i64::MAX)?;
    let db = state.db_or_500()?;
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let status: String = row.try_get("status")?;
    if status == "completed" {
        return Err(domain(
            StatusCode::CONFLICT,
            "hifz_attempt_completed",
            "نشست تکمیل شده است؛ موقعیت آن ثبت نمی‌شود",
        ));
    }
    let now = utc_now();
    db.execute(
        "UPDATE hifz_attempts SET current_surah = ?, current_ayah = ?, current_word_index = ?, \
         updated_at = ? WHERE id = ?",
        &[
            (current_surah).into_arg(),
            (current_ayah).into_arg(),
            match current_word_index {
                Some(v) => (v).into_arg(),
                None => Arg::NullInt,
            },
            (now).into_arg(),
            (attempt_id).into_arg(),
        ],
    )
    .await?;
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let counts = attempt_counts(db, attempt_id).await?;
    Ok(Json(attempt_out(&row, counts)?))
}

/// `POST /api/v1/quran/hifz/attempts/{attempt_id}/pause`.
pub async fn pause_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let status: String = row.try_get("status")?;
    if status == "completed" {
        return Err(domain(
            StatusCode::CONFLICT,
            "hifz_attempt_completed",
            "نشست تکمیل شده است",
        ));
    }
    if status != "paused" {
        let now = utc_now();
        db.execute(
            "UPDATE hifz_attempts SET status = 'paused', updated_at = ? WHERE id = ?",
            &[(now).into_arg(), (attempt_id).into_arg()],
        )
        .await?;
    }
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let counts = attempt_counts(db, attempt_id).await?;
    Ok(Json(attempt_out(&row, counts)?))
}

/// `POST /api/v1/quran/hifz/attempts/{attempt_id}/resume`.
pub async fn resume_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let status: String = row.try_get("status")?;
    if status == "completed" {
        return Err(domain(
            StatusCode::CONFLICT,
            "hifz_attempt_completed",
            "نشست تکمیل شده است",
        ));
    }
    if status == "paused" {
        let now = utc_now();
        db.execute(
            "UPDATE hifz_attempts SET status = 'in_progress', updated_at = ? WHERE id = ?",
            &[(now).into_arg(), (attempt_id).into_arg()],
        )
        .await?;
    }
    let row = attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let counts = attempt_counts(db, attempt_id).await?;
    Ok(Json(attempt_out(&row, counts)?))
}

/// `GET /api/v1/quran/hifz/attempts/{attempt_id}/mistakes` — grouped
/// by (surah, ayah, category); `peek` rows are excluded.
pub async fn attempt_mistakes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let rows = db
        .fetch_all(
            "SELECT * FROM hifz_mistakes WHERE attempt_id = ? ORDER BY created_at",
            &[(attempt_id).into_arg()],
        )
        .await?;
    // Group by (surah, ayah, category), sorted by (surah, ayah, category).
    let mut groups: Vec<(i32, i32, String, Vec<Value>)> = Vec::new();
    for r in &rows {
        let mistake_type: String = r.try_get("mistake_type")?;
        let category = match mistake_type.as_str() {
            "extra" => "extra_words".to_string(),
            "wrong_word" | "wrong_tashkeel" | "skipped" | "transposed" => "wrong_words".to_string(),
            _ => continue, // peek and anything else are excluded
        };
        let surah: i32 = r.try_get("surah")?;
        let ayah: i32 = r.try_get("ayah")?;
        let item = json!({
            "id": r.try_get::<Uuid, _>("id")?.to_string(),
            "mistake_type": mistake_type,
            "word_index": r.try_get::<Option<i32>, _>("word_index")?,
            "expected_text": r.try_get::<String, _>("expected_text")?,
            "heard": r.try_get::<Option<String>, _>("heard")?,
            "created_at": iso(r.try_get::<DateTime<Utc>, _>("created_at")?),
        });
        if let Some(g) = groups
            .iter_mut()
            .find(|(s, a, c, _)| *s == surah && *a == ayah && *c == category)
        {
            g.3.push(item);
        } else {
            groups.push((surah, ayah, category, vec![item]));
        }
    }
    groups.sort_by_key(|a| (a.0, a.1, a.2.clone()));
    let out: Vec<Value> = groups
        .into_iter()
        .map(|(surah, ayah, category, items)| {
            json!({ "surah": surah, "ayah": ayah, "category": category, "items": items })
        })
        .collect();
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/quran/hifz/attempts/{attempt_id}` (204).
pub async fn delete_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    attempt_or_404(db, ctx.user.id, attempt_id).await?;
    db.execute(
        "DELETE FROM hifz_word_scores WHERE attempt_id = ?",
        &[(attempt_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_mistakes WHERE attempt_id = ?",
        &[(attempt_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_attempts WHERE id = ?",
        &[(attempt_id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- peek / dispute / mistakes -----------------------------------------------

/// `POST /api/v1/quran/hifz/attempts/{attempt_id}/peek`.
/// Revealing a word = recording a `peek` mistake (one row per word).
pub async fn peek_word(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let parsed = parse_body(&body)?;
    let surah = req_int_loose(&parsed, "surah", 1, 114)?;
    let ayah = req_int_loose(&parsed, "ayah", 1, 286)?;
    let kind = parsed
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("word")
        .to_string();
    if kind != "word" && kind != "ayah" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_peek_kind",
            "نوع نمایش کلمه نامعتبر است",
        ));
    }
    let word_index = opt_int_loose(&parsed, "word_index", 0, i64::MAX)?;
    let word_indices: Option<Vec<i64>> = match parsed.get("word_indices").filter(|v| !v.is_null()) {
        Some(Value::Array(arr)) => {
            let mut out = Vec::new();
            for v in arr {
                let n = v
                    .as_i64()
                    .ok_or_else(|| validation_err("word_indices", "must be integers"))?;
                if n < 0 {
                    return Err(validation_err("word_indices", "out of range"));
                }
                out.push(n);
            }
            Some(out)
        }
        Some(_) => return Err(validation_err("word_indices", "must be a list")),
        None => None,
    };
    let targets: Vec<i64> = match (word_index, &word_indices) {
        (Some(i), None) => vec![i],
        (None, Some(list)) => {
            if list.is_empty() {
                return Err(domain(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "hifz_peek_target",
                    "هدف نمایش مشخص نشده است",
                ));
            }
            list.clone()
        }
        _ => {
            return Err(domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hifz_peek_target",
                "دقیقاً یکی از word_index یا word_indices لازم است",
            ));
        }
    };
    if kind == "ayah" && word_indices.is_none() {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_peek_target",
            "برای نمایش آیه، word_indices لازم است",
        ));
    }
    let Some(words) = ayah_words(surah as i32, ayah as i32) else {
        return Err(domain(
            StatusCode::SERVICE_UNAVAILABLE,
            "quran_text_unavailable",
            "متن این آیه در دسترس نیست",
        ));
    };
    for idx in &targets {
        if *idx as usize >= words.len() {
            return Err(domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hifz_word_index",
                "شمارهٔ کلمه خارج از بازهٔ آیه است",
            ));
        }
    }
    let db = state.db_or_500()?;
    attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let now = utc_now();
    let mut revealed: Vec<Value> = Vec::new();
    let mut first_mistake_id: Option<Uuid> = None;
    for idx in &targets {
        let mid = new_id();
        if first_mistake_id.is_none() {
            first_mistake_id = Some(mid);
        }
        db.execute(
            "INSERT INTO hifz_mistakes (id, attempt_id, user_id, surah, ayah, word_index, \
             mistake_type, expected_text, heard, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'peek', ?, NULL, ?)",
            &[
                (mid).into_arg(),
                (attempt_id).into_arg(),
                (ctx.user.id).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
                (*idx).into_arg(),
                (words[*idx as usize].clone()).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
        revealed.push(json!({
            "mistake_id": mid.to_string(),
            "word_index": idx,
            "word": words[*idx as usize],
        }));
    }
    db.execute(
        "UPDATE hifz_attempts SET updated_at = ? WHERE id = ?",
        &[(now).into_arg(), (attempt_id).into_arg()],
    )
    .await?;
    let first = revealed.first().cloned().unwrap_or(Value::Null);
    Ok(Json(json!({
        "mistake_id": first_mistake_id.map(|u| u.to_string()),
        "surah": surah,
        "ayah": ayah,
        "word_index": first.get("word_index").cloned().unwrap_or(Value::Null),
        "word": first.get("word").cloned().unwrap_or(Value::Null),
        "revealed": revealed,
    })))
}

/// `DELETE /api/v1/quran/hifz/mistakes/{mistake_id}` (204).
pub async fn delete_mistake(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(mistake_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/mistakes").await?;
    let mistake_id = parse_path_uuid(&mistake_id_s, "mistake_id")?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT user_id FROM hifz_mistakes WHERE id = ?",
            &[(mistake_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != ctx.user.id {
                return Err(not_found(
                    "hifz_mistake_not_found",
                    "خطایی با این شناسه یافت نشد",
                ));
            }
        }
        None => {
            return Err(not_found(
                "hifz_mistake_not_found",
                "خطایی با این شناسه یافت نشد",
            ));
        }
    }
    db.execute(
        "DELETE FROM hifz_mistakes WHERE id = ?",
        &[(mistake_id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

/// `POST /api/v1/quran/hifz/attempts/{attempt_id}/dispute`.
pub async fn dispute_word(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/attempts").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let parsed = parse_body(&body)?;
    let word_score_id = parsed
        .get("word_score_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| validation_err("word_score_id", "field is required"))?;
    let word_score_id = Uuid::parse_str(word_score_id)
        .map_err(|_| validation_err("word_score_id", "must be a UUID"))?;
    let db = state.db_or_500()?;
    attempt_or_404(db, ctx.user.id, attempt_id).await?;
    let row = db
        .fetch_optional(
            "SELECT attempt_id FROM hifz_word_scores WHERE id = ?",
            &[(word_score_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner_attempt: Uuid = r.try_get("attempt_id")?;
            if owner_attempt != attempt_id {
                return Err(not_found(
                    "hifz_word_score_not_found",
                    "نمرهٔ کلمه‌ای با این شناسه یافت نشد",
                ));
            }
        }
        None => {
            return Err(not_found(
                "hifz_word_score_not_found",
                "نمرهٔ کلمه‌ای با این شناسه یافت نشد",
            ));
        }
    }
    db.execute(
        "UPDATE hifz_word_scores SET disputed = TRUE WHERE id = ?",
        &[(word_score_id).into_arg()],
    )
    .await?;
    Ok(Json(json!({
        "word_score_id": word_score_id.to_string(),
        "disputed": true,
    })))
}

// --- user settings -------------------------------------------------------------

fn settings_out(row: Option<&DynRow>) -> Result<Value, ApiError> {
    match row {
        None => Ok(json!({
            "freeze_on_error": false,
            "mistake_detection": true,
            "tashkeel_detection": true,
            "resumable_sessions": false,
            "hide_ayah_markers": false,
            "daily_goal_ayahs": 0,
            "updated_at": Value::Null,
        })),
        Some(r) => {
            let updated_at: Option<DateTime<Utc>> = opt_dt(r, "updated_at")?;
            Ok(json!({
                "freeze_on_error": r.try_get::<bool, _>("freeze_on_error")?,
                "mistake_detection": r.try_get::<bool, _>("mistake_detection")?,
                "tashkeel_detection": r.try_get::<bool, _>("tashkeel_detection")?,
                "resumable_sessions": r.try_get::<bool, _>("resumable_sessions")?,
                "hide_ayah_markers": r.try_get::<bool, _>("hide_ayah_markers")?,
                "daily_goal_ayahs": r.try_get::<i32, _>("daily_goal_ayahs")?,
                "updated_at": iso_opt(updated_at),
            }))
        }
    }
}

/// `GET /api/v1/quran/hifz/settings`.
pub async fn get_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/settings").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_user_settings WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    Ok(Json(settings_out(row.as_ref())?))
}

/// `PUT /api/v1/quran/hifz/settings` — only non-null known fields.
pub async fn put_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/settings").await?;
    let parsed = parse_body(&body)?;
    let freeze_on_error = opt_bool_loose(&parsed, "freeze_on_error")?;
    let mistake_detection = opt_bool_loose(&parsed, "mistake_detection")?;
    let tashkeel_detection = opt_bool_loose(&parsed, "tashkeel_detection")?;
    let resumable_sessions = opt_bool_loose(&parsed, "resumable_sessions")?;
    let hide_ayah_markers = opt_bool_loose(&parsed, "hide_ayah_markers")?;
    let daily_goal_ayahs = opt_int_loose(&parsed, "daily_goal_ayahs", 0, 6236)?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_user_settings WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let now = utc_now();
    if row.is_none() {
        db.execute(
            "INSERT INTO hifz_user_settings (user_id, freeze_on_error, mistake_detection, \
             tashkeel_detection, resumable_sessions, hide_ayah_markers, daily_goal_ayahs, \
             created_at, updated_at) VALUES (?, FALSE, TRUE, TRUE, FALSE, FALSE, 0, ?, ?)",
            &[(ctx.user.id).into_arg(), (now).into_arg(), (now).into_arg()],
        )
        .await?;
    }
    let mut sets: Vec<(&str, Arg)> = Vec::new();
    if let Some(v) = freeze_on_error {
        sets.push(("freeze_on_error", (v).into_arg()));
    }
    if let Some(v) = mistake_detection {
        sets.push(("mistake_detection", (v).into_arg()));
    }
    if let Some(v) = tashkeel_detection {
        sets.push(("tashkeel_detection", (v).into_arg()));
    }
    if let Some(v) = resumable_sessions {
        sets.push(("resumable_sessions", (v).into_arg()));
    }
    if let Some(v) = hide_ayah_markers {
        sets.push(("hide_ayah_markers", (v).into_arg()));
    }
    if let Some(v) = daily_goal_ayahs {
        sets.push(("daily_goal_ayahs", (v).into_arg()));
    }
    if !sets.is_empty() {
        let mut sql = "UPDATE hifz_user_settings SET ".to_string();
        let mut args: Vec<Arg> = Vec::new();
        for (i, (col, arg)) in sets.into_iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push_str(&format!("{col} = ?"));
            args.push(arg);
        }
        sql.push_str(", updated_at = ? WHERE user_id = ?");
        args.push((now).into_arg());
        args.push((ctx.user.id).into_arg());
        db.execute(&sql, &args).await?;
    } else {
        db.execute(
            "UPDATE hifz_user_settings SET updated_at = ? WHERE user_id = ?",
            &[(now).into_arg(), (ctx.user.id).into_arg()],
        )
        .await?;
    }
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_user_settings WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    Ok(Json(settings_out(row.as_ref())?))
}

// --- listen position -----------------------------------------------------------

const LISTEN_SPEEDS: [f64; 6] = [0.5, 0.75, 1.0, 1.25, 1.5, 1.75];

fn listen_position_out(row: &DynRow) -> Result<Value, ApiError> {
    let reciter: String = row.try_get("reciter")?;
    let surah: i32 = row.try_get("surah")?;
    let ayah: i32 = row.try_get("ayah")?;
    let progress_ms: i32 = row.try_get("progress_ms")?;
    let speed: f64 = row.try_get("speed")?;
    let repeat_mode: String = row.try_get("repeat_mode")?;
    let repeat_ayah: i32 = row.try_get("repeat_ayah")?;
    let repeat_range_start: Option<i32> = row.try_get("repeat_range_start")?;
    let repeat_range_end: Option<i32> = row.try_get("repeat_range_end")?;
    let repeat_range_count: i32 = row.try_get("repeat_range_count")?;
    let updated_at: Option<DateTime<Utc>> = opt_dt(row, "updated_at")?;
    let created_at: Option<DateTime<Utc>> = opt_dt(row, "created_at")?;
    Ok(json!({
        "reciter": reciter,
        "surah": surah,
        "ayah": ayah,
        "progress_ms": progress_ms,
        "speed": speed,
        "repeat_mode": repeat_mode,
        "repeat_ayah": repeat_ayah,
        "repeat_range_start": repeat_range_start,
        "repeat_range_end": repeat_range_end,
        "repeat_range_count": repeat_range_count,
        "updated_at": iso_opt(updated_at),
        "created_at": iso_opt(created_at),
    }))
}

/// `GET /api/v1/quran/hifz/listen-position` — position or JSON `null`.
pub async fn get_listen_position(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/listen-position").await?;
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_listen_positions WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    match row {
        None => Ok(Json(Value::Null)),
        Some(r) => Ok(Json(listen_position_out(&r)?)),
    }
}

/// `PUT /api/v1/quran/hifz/listen-position` — last-write-wins on
/// `updated_at`; a stale write returns `{applied:false,
/// conflict:true, position:<server state>}` with HTTP 200.
pub async fn put_listen_position(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/listen-position").await?;
    let parsed = parse_body(&body)?;
    let reciter = req_str_raw(&parsed, "reciter", 1, 40)?;
    let surah = req_int_loose(&parsed, "surah", 1, 114)?;
    let ayah = req_int_loose(&parsed, "ayah", 1, 286)?;
    let progress_ms = req_int_loose(&parsed, "progress_ms", 0, 10_800_000)?;
    let speed = match parsed.get("speed").filter(|v| !v.is_null()) {
        Some(v) => v
            .as_f64()
            .ok_or_else(|| validation_err("speed", "must be a number"))?,
        None => 1.0,
    };
    let repeat_mode = match parsed.get("repeat_mode").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => {
            if s.chars().count() > 10 {
                return Err(validation_err("repeat_mode", "length out of range"));
            }
            s.clone()
        }
        Some(_) => return Err(validation_err("repeat_mode", "must be a string")),
        None => "off".to_string(),
    };
    let repeat_ayah = opt_int_loose(&parsed, "repeat_ayah", 1, 20)?.unwrap_or(1);
    let repeat_range_start = opt_int_loose(&parsed, "repeat_range_start", 1, 286)?;
    let repeat_range_end = opt_int_loose(&parsed, "repeat_range_end", 1, 286)?;
    let repeat_range_count = opt_int_loose(&parsed, "repeat_range_count", 1, 20)?.unwrap_or(1);
    let updated_at_raw = parsed
        .get("updated_at")
        .filter(|v| !v.is_null())
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    // Domain validation (all 422 with stable codes).
    let valid_reciters: HashSet<&str> = crate::quran_data::TIMING_RECITERS
        .iter()
        .map(|(id, _)| *id)
        .collect();
    if !valid_reciters.contains(reciter.as_str()) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_listen_reciter_unknown",
            "شناسهٔ قاری ناشناخته است",
            json!({ "reciter": reciter }),
        ));
    }
    if !LISTEN_SPEEDS.contains(&speed) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_listen_speed_invalid",
            "سرعت پخش نامعتبر است",
        ));
    }
    if !["off", "ayah", "range", "surah"].contains(&repeat_mode.as_str()) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_listen_repeat_invalid",
            "حالت تکرار نامعتبر است",
        ));
    }
    if repeat_mode == "range" {
        match (repeat_range_start, repeat_range_end) {
            (Some(_), Some(_)) => {}
            _ => {
                return Err(domain(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "hifz_listen_range_missing",
                    "برای حالت تکرار بازه، هر دو کران لازم است",
                ));
            }
        }
        if repeat_range_start.unwrap() > repeat_range_end.unwrap() {
            return Err(domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hifz_listen_range_invalid",
                "شروع بازه نمی‌تواند پس از پایان آن باشد",
            ));
        }
    } else if repeat_range_start.is_some() || repeat_range_end.is_some() {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_listen_range_unexpected",
            "کران بازه فقط با حالت تکرار بازه فرستاده می‌شود",
        ));
    }
    let now = utc_now();
    let incoming = match updated_at_raw {
        None => now,
        Some(s) => {
            let dt = crate::validate::parse_datetime_str(&s)
                .ok_or_else(|| validation_err("updated_at", "must be a datetime"))?;
            dt.min(now)
        }
    };
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_listen_positions WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    if let Some(r) = &row {
        let stored: Option<DateTime<Utc>> = opt_dt(r, "updated_at")?;
        if let Some(stored) = stored {
            if incoming < stored {
                return Ok(Json(json!({
                    "applied": false,
                    "conflict": true,
                    "position": listen_position_out(r)?,
                })));
            }
        }
    }
    if row.is_none() {
        db.execute(
            "INSERT INTO hifz_listen_positions (user_id, reciter, surah, ayah, progress_ms, \
             speed, repeat_mode, repeat_ayah, repeat_range_start, repeat_range_end, \
             repeat_range_count, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                (ctx.user.id).into_arg(),
                (reciter.clone()).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
                (progress_ms).into_arg(),
                (speed).into_arg(),
                (repeat_mode.clone()).into_arg(),
                (repeat_ayah).into_arg(),
                match repeat_range_start {
                    Some(v) => (v).into_arg(),
                    None => Arg::NullInt,
                },
                match repeat_range_end {
                    Some(v) => (v).into_arg(),
                    None => Arg::NullInt,
                },
                (repeat_range_count).into_arg(),
                (now).into_arg(),
                (incoming).into_arg(),
            ],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE hifz_listen_positions SET reciter = ?, surah = ?, ayah = ?, progress_ms = ?, \
             speed = ?, repeat_mode = ?, repeat_ayah = ?, repeat_range_start = ?, \
             repeat_range_end = ?, repeat_range_count = ?, updated_at = ? WHERE user_id = ?",
            &[
                (reciter.clone()).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
                (progress_ms).into_arg(),
                (speed).into_arg(),
                (repeat_mode.clone()).into_arg(),
                (repeat_ayah).into_arg(),
                match repeat_range_start {
                    Some(v) => (v).into_arg(),
                    None => Arg::NullInt,
                },
                match repeat_range_end {
                    Some(v) => (v).into_arg(),
                    None => Arg::NullInt,
                },
                (repeat_range_count).into_arg(),
                (incoming).into_arg(),
                (ctx.user.id).into_arg(),
            ],
        )
        .await?;
    }
    let row = db
        .fetch_one(
            "SELECT * FROM hifz_listen_positions WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    Ok(Json(json!({
        "applied": true,
        "conflict": false,
        "position": listen_position_out(&row)?,
    })))
}

/// `DELETE /api/v1/quran/hifz/listen-position` (204, idempotent).
pub async fn delete_listen_position(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/listen-position").await?;
    let db = state.db_or_500()?;
    db.execute(
        "DELETE FROM hifz_listen_positions WHERE user_id = ?",
        &[(ctx.user.id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- stats / review ------------------------------------------------------------

/// The mistake-based review queue (`hifz_service.review_queue`):
/// ayahs whose newest attempt's scores still contain wrong/skipped,
/// or peek-only ayahs never graded clean. Sorted by mistake count
/// desc, then last activity desc; max 100.
pub async fn review_queue(db: &Db, user_id: Uuid) -> Result<Vec<Value>, ApiError> {
    let mistake_rows = db
        .fetch_all(
            "SELECT surah, ayah, created_at FROM hifz_mistakes WHERE user_id = ? \
             ORDER BY created_at DESC LIMIT 2000",
            &[(user_id).into_arg()],
        )
        .await?;
    if mistake_rows.is_empty() {
        return Ok(Vec::new());
    }
    // Group preserving first-seen order; count + max(created_at).
    let mut keys: Vec<(i32, i32, i64, DateTime<Utc>)> = Vec::new();
    for m in &mistake_rows {
        let surah: i32 = m.try_get("surah")?;
        let ayah: i32 = m.try_get("ayah")?;
        let created_at: DateTime<Utc> = m.try_get("created_at")?;
        if let Some(e) = keys
            .iter_mut()
            .find(|(s, a, _, _)| *s == surah && *a == ayah)
        {
            e.2 += 1;
            if created_at > e.3 {
                e.3 = created_at;
            }
        } else {
            keys.push((surah, ayah, 1, created_at));
        }
    }
    let mut due: Vec<(i32, i32, i64, DateTime<Utc>)> = Vec::new();
    for (surah, ayah, count, last_activity) in keys {
        let score_rows = db
            .fetch_all(
                "SELECT ws.attempt_id, ws.status, ws.updated_at, ws.created_at \
                 FROM hifz_word_scores ws JOIN hifz_attempts a ON ws.attempt_id = a.id \
                 WHERE a.user_id = ? AND ws.surah = ? AND ws.ayah = ? \
                 ORDER BY ws.updated_at DESC NULLS LAST, ws.created_at DESC LIMIT 50",
                &[(user_id).into_arg(), (surah).into_arg(), (ayah).into_arg()],
            )
            .await?;
        if score_rows.is_empty() {
            due.push((surah, ayah, count, last_activity));
            continue;
        }
        // Newest attempt = max by coalesce(updated_at, created_at);
        // first max wins (rows arrive newest-first).
        let mut newest_attempt: Option<Uuid> = None;
        let mut newest_key: Option<DateTime<Utc>> = None;
        for r in &score_rows {
            let updated_at: Option<DateTime<Utc>> = r.try_get("updated_at")?;
            let created_at: DateTime<Utc> = r.try_get("created_at")?;
            let key = updated_at.unwrap_or(created_at);
            if newest_key.map(|k| key > k).unwrap_or(true) {
                newest_key = Some(key);
                newest_attempt = Some(r.try_get("attempt_id")?);
            }
        }
        let newest_attempt = newest_attempt.unwrap();
        let still_wrong = score_rows.iter().any(|r| {
            let att: Uuid = r.try_get("attempt_id").unwrap_or(Uuid::nil());
            let status: String = r.try_get("status").unwrap_or_default();
            att == newest_attempt && (status == "wrong" || status == "skipped")
        });
        if still_wrong {
            due.push((surah, ayah, count, last_activity));
        }
    }
    // Stable sorts: last activity desc, then mistake count desc.
    due.sort_by_key(|a| std::cmp::Reverse(a.3));
    due.sort_by_key(|a| std::cmp::Reverse(a.2));
    due.truncate(100);
    Ok(due
        .into_iter()
        .map(|(surah, ayah, count, last_activity)| {
            json!({
                "surah": surah,
                "ayah": ayah,
                "mistake_count": count,
                "last_activity_at": iso(last_activity),
            })
        })
        .collect())
}

/// `GET /api/v1/quran/hifz/stats`.
pub async fn hifz_stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/stats").await?;
    let db = state.db_or_500()?;
    let attempts_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM hifz_attempts WHERE user_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let attempts: i64 = attempts_row.try_get("n")?;
    let score_rows = db
        .fetch_all(
            "SELECT ws.status, COUNT(*) AS n FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id \
             WHERE a.user_id = ? GROUP BY ws.status",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let mut counts = [0i64; 4];
    for r in &score_rows {
        let status: String = r.try_get("status")?;
        let n: i64 = r.try_get("n")?;
        match status.as_str() {
            "correct" => counts[0] = n,
            "close" => counts[1] = n,
            "wrong" => counts[2] = n,
            "skipped" => counts[3] = n,
            _ => {}
        }
    }
    let graded_words: i64 = counts.iter().sum();
    let dominant = db
        .fetch_optional(
            "SELECT mistake_type, COUNT(*) AS n FROM hifz_mistakes WHERE user_id = ? \
             GROUP BY mistake_type ORDER BY n DESC LIMIT 1",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let dominant_mistake: Option<String> = match dominant {
        Some(r) => Some(r.try_get("mistake_type")?),
        None => None,
    };
    let due = review_queue(db, ctx.user.id).await?;
    Ok(Json(json!({
        "attempts": attempts,
        "graded_words": graded_words,
        "counts": counts_json(counts),
        "dominant_mistake": dominant_mistake,
        "due_ayahs": due.len(),
    })))
}

/// `GET /api/v1/quran/hifz/review`.
pub async fn hifz_review(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/review").await?;
    let db = state.db_or_500()?;
    Ok(Json(Value::Array(review_queue(db, ctx.user.id).await?)))
}

// --- admin ---------------------------------------------------------------------

fn admin_attempt_out(row: &DynRow, counts: [i64; 4]) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let user_id: Uuid = row.try_get("user_id")?;
    let surah: i32 = row.try_get("surah")?;
    let page: Option<i32> = row.try_get("page")?;
    let status: String = row.try_get("status")?;
    let engine: String = row.try_get("engine")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    Ok(json!({
        "id": id.to_string(),
        "user_id": user_id.to_string(),
        "surah": surah,
        "page": page,
        "status": status,
        "engine": engine,
        "counts": counts_json(counts),
        "created_at": iso(created_at),
    }))
}

/// `GET /api/v1/quran/hifz/admin/attempts` (`platform.settings`).
pub async fn admin_list_attempts(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/admin/attempts").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT * FROM hifz_attempts ORDER BY updated_at DESC NULLS LAST, created_at DESC LIMIT ?",
            &[(limit).into_arg()],
        )
        .await?;
    let ids: Vec<Uuid> = rows
        .iter()
        .map(|r| r.try_get::<Uuid, _>("id"))
        .collect::<Result<Vec<_>, _>>()?;
    let counts = attempt_counts_many(db, &ids).await?;
    let mut out: Vec<Value> = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("id")?;
        out.push(admin_attempt_out(
            r,
            counts.get(&id).copied().unwrap_or([0, 0, 0, 0]),
        )?);
    }
    Ok(Json(Value::Array(out)))
}

/// `DELETE /api/v1/quran/hifz/admin/attempts/{attempt_id}` (204,
/// `platform.settings`, no ownership check).
pub async fn admin_delete_attempt(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(attempt_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/admin/attempts").await?;
    require_permission(&state, &ctx, "platform.settings").await?;
    let attempt_id = parse_path_uuid(&attempt_id_s, "attempt_id")?;
    let db = state.db_or_500()?;
    let exists = db
        .fetch_optional(
            "SELECT id FROM hifz_attempts WHERE id = ?",
            &[(attempt_id).into_arg()],
        )
        .await?;
    if exists.is_none() {
        return Err(not_found(
            "hifz_attempt_not_found",
            "نشست حفظی با این شناسه یافت نشد",
        ));
    }
    db.execute(
        "DELETE FROM hifz_word_scores WHERE attempt_id = ?",
        &[(attempt_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_mistakes WHERE attempt_id = ?",
        &[(attempt_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_attempts WHERE id = ?",
        &[(attempt_id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- SRS reviews (SM-2) ------------------------------------------------------

fn review_out(row: &DynRow, quality: i64) -> Result<Value, ApiError> {
    let surah: i32 = row.try_get("surah")?;
    let ayah: i32 = row.try_get("ayah")?;
    let repetitions: i32 = row.try_get("repetitions")?;
    let ease: f64 = row.try_get("ease")?;
    let interval_days: i32 = row.try_get("interval_days")?;
    let due_at: Option<DateTime<Utc>> = opt_dt(row, "due_at")?;
    let params_version: i32 = row.try_get("params_version")?;
    Ok(json!({
        "surah": surah,
        "ayah": ayah,
        "quality": quality,
        "repetitions": repetitions,
        "ease": ease,
        "interval_days": interval_days,
        "due_at": iso_opt(due_at),
        "params_version": params_version,
    }))
}

/// `POST /api/v1/quran/hifz/reviews` — SM-2.
pub async fn record_review(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/reviews").await?;
    let parsed = parse_body(&body)?;
    let surah = req_int_loose(&parsed, "surah", 1, 114)?;
    let ayah = req_int_loose(&parsed, "ayah", 1, 286)?;
    let quality_in = opt_int_loose(&parsed, "quality", 0, 5)?;
    let peek_count = opt_int_loose(&parsed, "peek_count", 0, i64::MAX)?.unwrap_or(0);
    let counts_in: Option<[i64; 4]> = match parsed.get("counts").filter(|v| !v.is_null()) {
        Some(Value::Object(map)) => {
            let get = |k: &str| map.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            Some([get("correct"), get("close"), get("wrong"), get("skipped")])
        }
        Some(_) => return Err(validation_err("counts", "must be an object")),
        None => None,
    };
    let quality = match quality_in {
        Some(q) => q,
        None => match counts_in {
            Some(c) => quality_from_grade(c[0], c[1], c[2], c[3], peek_count, None),
            None => {
                return Err(domain(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "hifz_review_quality",
                    "کیفیت یا نمره (counts) برای ثبت مرور لازم است",
                ));
            }
        },
    };
    if !(0..=5).contains(&quality) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_review_quality",
            "کیفیت باید ۰ تا ۵ باشد",
        ));
    }
    let db = state.db_or_500()?;
    let now = utc_now();
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_reviews WHERE user_id = ? AND surah = ? AND ayah = ?",
            &[
                (ctx.user.id).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
            ],
        )
        .await?;
    let (repetitions, ease, interval_days) = match &row {
        Some(r) => (
            r.try_get::<i32, _>("repetitions")? as i64,
            r.try_get::<f64, _>("ease")?,
            r.try_get::<i32, _>("interval_days")? as i64,
        ),
        None => (0, 2.5, 0),
    };
    let (new_reps, new_ease, new_interval, due_at) =
        sm2_next(quality, repetitions, ease, interval_days, now);
    if row.is_none() {
        db.execute(
            "INSERT INTO hifz_reviews (id, user_id, surah, ayah, ease, interval_days, \
             repetitions, due_at, last_quality, last_reviewed_at, params_version, \
             created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
            &[
                (new_id()).into_arg(),
                (ctx.user.id).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
                (new_ease).into_arg(),
                (new_interval).into_arg(),
                (new_reps).into_arg(),
                (due_at).into_arg(),
                (quality).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE hifz_reviews SET ease = ?, interval_days = ?, repetitions = ?, \
             due_at = ?, last_quality = ?, last_reviewed_at = ?, params_version = 1, \
             updated_at = ? WHERE user_id = ? AND surah = ? AND ayah = ?",
            &[
                (new_ease).into_arg(),
                (new_interval).into_arg(),
                (new_reps).into_arg(),
                (due_at).into_arg(),
                (quality).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
                (ctx.user.id).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
            ],
        )
        .await?;
    }
    let row = db
        .fetch_one(
            "SELECT * FROM hifz_reviews WHERE user_id = ? AND surah = ? AND ayah = ?",
            &[
                (ctx.user.id).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
            ],
        )
        .await?;
    Ok(Json(review_out(&row, quality)?))
}

/// `GET /api/v1/quran/hifz/reviews/due` — SRS rows due, then legacy
/// mistake-queue ayahs not already present. All `HifzDueReviewOut`
/// fields are present (null where the source has none).
pub async fn due_reviews(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/reviews/due").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 100, 1, 200)?;
    let db = state.db_or_500()?;
    let now = utc_now();
    let srs_rows = db
        .fetch_all(
            "SELECT * FROM hifz_reviews WHERE user_id = ? AND due_at <= ? ORDER BY due_at LIMIT ?",
            &[
                (ctx.user.id).into_arg(),
                (now).into_arg(),
                (limit).into_arg(),
            ],
        )
        .await?;
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<(i32, i32)> = HashSet::new();
    for r in &srs_rows {
        let surah: i32 = r.try_get("surah")?;
        let ayah: i32 = r.try_get("ayah")?;
        seen.insert((surah, ayah));
        let due_at: Option<DateTime<Utc>> = opt_dt(r, "due_at")?;
        let last_quality: Option<i32> = r.try_get("last_quality")?;
        out.push(json!({
            "surah": surah,
            "ayah": ayah,
            "due_at": iso_opt(due_at),
            "repetitions": r.try_get::<i32, _>("repetitions")?,
            "ease": r.try_get::<f64, _>("ease")?,
            "interval_days": r.try_get::<i32, _>("interval_days")?,
            "last_quality": last_quality,
            "mistake_count": Value::Null,
            "last_activity_at": Value::Null,
            "source": "srs",
        }));
    }
    for entry in review_queue(db, ctx.user.id).await? {
        let surah = entry.get("surah").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let ayah = entry.get("ayah").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        if seen.contains(&(surah, ayah)) {
            continue;
        }
        seen.insert((surah, ayah));
        let mut item = entry.clone();
        if let Some(obj) = item.as_object_mut() {
            obj.insert("due_at".to_string(), Value::Null);
            obj.insert("repetitions".to_string(), Value::Null);
            obj.insert("ease".to_string(), Value::Null);
            obj.insert("interval_days".to_string(), Value::Null);
            obj.insert("last_quality".to_string(), Value::Null);
            obj.insert("source".to_string(), json!("mistakes"));
        }
        out.push(item);
    }
    out.truncate(limit as usize);
    Ok(Json(Value::Array(out)))
}

// --- plans ---------------------------------------------------------------------

async fn plan_progress(db: &Db, plan_id: Uuid) -> Result<(i64, i64), ApiError> {
    let total_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM hifz_plan_items WHERE plan_id = ?",
            &[(plan_id).into_arg()],
        )
        .await?;
    let done_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM hifz_plan_items WHERE plan_id = ? AND status = 'done'",
            &[(plan_id).into_arg()],
        )
        .await?;
    Ok((total_row.try_get("n")?, done_row.try_get("n")?))
}

fn plan_out(row: &DynRow, total: i64, done: i64, items: Value) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let title: String = row.try_get("title")?;
    let kind: String = row.try_get("kind")?;
    let direction: String = row.try_get("direction")?;
    let start_surah: i32 = row.try_get("start_surah")?;
    let start_ayah: i32 = row.try_get("start_ayah")?;
    let end_surah: i32 = row.try_get("end_surah")?;
    let end_ayah: i32 = row.try_get("end_ayah")?;
    let daily_target_new: i32 = row.try_get("daily_target_new")?;
    let daily_target_review: i32 = row.try_get("daily_target_review")?;
    let status: String = row.try_get("status")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: Option<DateTime<Utc>> = opt_dt(row, "updated_at")?;
    let completed_at: Option<DateTime<Utc>> = opt_dt(row, "completed_at")?;
    Ok(json!({
        "id": id.to_string(),
        "title": title,
        "kind": kind,
        "direction": direction,
        "start_surah": start_surah,
        "start_ayah": start_ayah,
        "end_surah": end_surah,
        "end_ayah": end_ayah,
        "daily_target_new": daily_target_new,
        "daily_target_review": daily_target_review,
        "status": status,
        "created_at": iso(created_at),
        "updated_at": iso_opt(updated_at),
        "completed_at": iso_opt(completed_at),
        "total_ayahs": total,
        "done_ayahs": done,
        "items": items,
    }))
}

async fn plan_or_404(db: &Db, user_id: Uuid, plan_id: Uuid) -> Result<DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_plans WHERE id = ?",
            &[(plan_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != user_id {
                return Err(not_found("hifz_plan_not_found", "برنامه پیدا نشد"));
            }
            Ok(r)
        }
        None => Err(not_found("hifz_plan_not_found", "برنامه پیدا نشد")),
    }
}

async fn list_plans(db: &Db, user_id: Uuid) -> Result<Vec<Value>, ApiError> {
    let rows = db
        .fetch_all(
            "SELECT * FROM hifz_plans WHERE user_id = ? ORDER BY created_at DESC",
            &[(user_id).into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("id")?;
        let (total, done) = plan_progress(db, id).await?;
        out.push(plan_out(r, total, done, json!([]))?);
    }
    Ok(out)
}

/// `POST /api/v1/quran/hifz/plans`.
pub async fn create_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/plans").await?;
    let parsed = parse_body(&body)?;
    let title = req_str_raw(&parsed, "title", 1, 200)?;
    let kind = match parsed.get("kind").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(validation_err("kind", "must be a string")),
        None => "memorize".to_string(),
    };
    let direction = match parsed.get("direction").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(validation_err("direction", "must be a string")),
        None => "normal".to_string(),
    };
    let start_surah = req_int_loose(&parsed, "start_surah", 1, 114)?;
    let start_ayah = req_int_loose(&parsed, "start_ayah", 1, 286)?;
    let end_surah = req_int_loose(&parsed, "end_surah", 1, 114)?;
    let end_ayah = req_int_loose(&parsed, "end_ayah", 1, 286)?;
    let daily_target_new = opt_int_loose(&parsed, "daily_target_new", 0, 100)?.unwrap_or(1);
    let daily_target_review = opt_int_loose(&parsed, "daily_target_review", 0, 500)?.unwrap_or(5);
    if !["memorize", "review", "recite"].contains(&kind.as_str()) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_kind",
            "نوع برنامه باید memorize/review/recite باشد",
        ));
    }
    if direction != "normal" && direction != "reverse" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_direction",
            "جهت برنامه باید normal یا reverse باشد",
        ));
    }
    if (start_surah, start_ayah) > (end_surah, end_ayah) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_range",
            "شروع بازه باید پیش از پایان آن باشد",
        ));
    }
    let start_max = get_surah(start_surah as i32)
        .map(|m| m.ayahs as i64)
        .unwrap_or(0);
    let end_max = get_surah(end_surah as i32)
        .map(|m| m.ayahs as i64)
        .unwrap_or(0);
    if start_ayah > start_max {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_range",
            "آیهٔ شروع خارج از سوره است",
        ));
    }
    if end_ayah > end_max {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_range",
            "آیهٔ پایان خارج از سوره است",
        ));
    }
    if daily_target_new < 0 || daily_target_review < 0 {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_target",
            "هدف روزانه نمی‌تواند منفی باشد",
        ));
    }
    let db = state.db_or_500()?;
    let order = plan_ayah_order(
        start_surah as i32,
        start_ayah as i32,
        end_surah as i32,
        end_ayah as i32,
        &direction,
    );
    let plan_id = new_id();
    let now = utc_now();
    let is_new = kind == "memorize";
    db.execute(
        "INSERT INTO hifz_plans (id, user_id, title, kind, direction, start_surah, start_ayah, \
         end_surah, end_ayah, daily_target_new, daily_target_review, status, created_at, \
         updated_at, completed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'active', ?, NULL, NULL)",
        &[
            (plan_id).into_arg(),
            (ctx.user.id).into_arg(),
            (title.trim().to_string()).into_arg(),
            (kind).into_arg(),
            (direction).into_arg(),
            (start_surah).into_arg(),
            (start_ayah).into_arg(),
            (end_surah).into_arg(),
            (end_ayah).into_arg(),
            (daily_target_new).into_arg(),
            (daily_target_review).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    for (pos, (s, a)) in order.iter().enumerate() {
        db.execute(
            "INSERT INTO hifz_plan_items (id, plan_id, user_id, surah, ayah, position, is_new, \
             status, completed_at, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'pending', NULL, ?)",
            &[
                (new_id()).into_arg(),
                (plan_id).into_arg(),
                (ctx.user.id).into_arg(),
                (*s).into_arg(),
                (*a).into_arg(),
                (pos as i64).into_arg(),
                (is_new).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
    }
    let row = plan_or_404(db, ctx.user.id, plan_id).await?;
    let (total, done) = plan_progress(db, plan_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(plan_out(&row, total, done, json!([]))?),
    ))
}

/// `GET /api/v1/quran/hifz/plans`.
pub async fn list_plans_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/plans").await?;
    let db = state.db_or_500()?;
    Ok(Json(Value::Array(list_plans(db, ctx.user.id).await?)))
}

/// `GET /api/v1/quran/hifz/plans/{plan_id}` — with items.
pub async fn get_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/plans").await?;
    let plan_id = parse_path_uuid(&plan_id_s, "plan_id")?;
    let db = state.db_or_500()?;
    let row = plan_or_404(db, ctx.user.id, plan_id).await?;
    let (total, done) = plan_progress(db, plan_id).await?;
    let items = db
        .fetch_all(
            "SELECT * FROM hifz_plan_items WHERE plan_id = ? ORDER BY position",
            &[(plan_id).into_arg()],
        )
        .await?;
    let items_json: Vec<Value> = items
        .iter()
        .map(|r| {
            let id: Uuid = r.try_get("id")?;
            let surah: i32 = r.try_get("surah")?;
            let ayah: i32 = r.try_get("ayah")?;
            let position: i32 = r.try_get("position")?;
            let is_new: bool = r.try_get("is_new")?;
            let status: String = r.try_get("status")?;
            let completed_at: Option<DateTime<Utc>> = opt_dt(r, "completed_at")?;
            Ok(json!({
                "id": id.to_string(),
                "surah": surah,
                "ayah": ayah,
                "position": position,
                "is_new": is_new,
                "status": status,
                "completed_at": iso_opt(completed_at),
            }))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(plan_out(&row, total, done, Value::Array(items_json))?))
}

/// `PATCH /api/v1/quran/hifz/plans/{plan_id}` — status only, no items.
pub async fn set_plan_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/plans").await?;
    let plan_id = parse_path_uuid(&plan_id_s, "plan_id")?;
    let parsed = parse_body(&body)?;
    let status = req_str_raw(&parsed, "status", 1, 20)?;
    if !["active", "paused", "completed"].contains(&status.as_str()) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_status",
            "وضعیت برنامه نامعتبر است",
        ));
    }
    let db = state.db_or_500()?;
    plan_or_404(db, ctx.user.id, plan_id).await?;
    let now = utc_now();
    if status == "completed" {
        db.execute(
            "UPDATE hifz_plans SET status = 'completed', completed_at = ?, updated_at = ? WHERE id = ?",
            &[(now).into_arg(), (now).into_arg(), (plan_id).into_arg()],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE hifz_plans SET status = ?, completed_at = NULL, updated_at = ? WHERE id = ?",
            &[(status).into_arg(), (now).into_arg(), (plan_id).into_arg()],
        )
        .await?;
    }
    let row = plan_or_404(db, ctx.user.id, plan_id).await?;
    let (total, done) = plan_progress(db, plan_id).await?;
    Ok(Json(plan_out(&row, total, done, json!([]))?))
}

/// `PATCH /api/v1/quran/hifz/plans/{plan_id}/items/{item_id}` —
/// returns the plan (no items) + the updated item.
pub async fn set_plan_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((plan_id_s, item_id_s)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/plans").await?;
    let plan_id = parse_path_uuid(&plan_id_s, "plan_id")?;
    let item_id = parse_path_uuid(&item_id_s, "item_id")?;
    let parsed = parse_body(&body)?;
    let status = req_str_raw(&parsed, "status", 1, 20)?;
    if !["pending", "in_progress", "done", "skipped"].contains(&status.as_str()) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_plan_item_status",
            "وضعیت آیه نامعتبر است",
        ));
    }
    let db = state.db_or_500()?;
    plan_or_404(db, ctx.user.id, plan_id).await?;
    let item = db
        .fetch_optional(
            "SELECT * FROM hifz_plan_items WHERE id = ? AND plan_id = ?",
            &[(item_id).into_arg(), (plan_id).into_arg()],
        )
        .await?;
    if item.is_none() {
        return Err(not_found("hifz_plan_item_not_found", "آیهٔ برنامه پیدا نشد"));
    }
    let now = utc_now();
    if status == "done" {
        db.execute(
            "UPDATE hifz_plan_items SET status = 'done', completed_at = ? WHERE id = ?",
            &[(now).into_arg(), (item_id).into_arg()],
        )
        .await?;
    } else {
        db.execute(
            "UPDATE hifz_plan_items SET status = ?, completed_at = NULL WHERE id = ?",
            &[(status).into_arg(), (item_id).into_arg()],
        )
        .await?;
    }
    let row = plan_or_404(db, ctx.user.id, plan_id).await?;
    let (total, done) = plan_progress(db, plan_id).await?;
    let mut out = plan_out(&row, total, done, json!([]))?;
    let item = db
        .fetch_one(
            "SELECT * FROM hifz_plan_items WHERE id = ?",
            &[(item_id).into_arg()],
        )
        .await?;
    let item_id_v: Uuid = item.try_get("id")?;
    let item_surah: i32 = item.try_get("surah")?;
    let item_ayah: i32 = item.try_get("ayah")?;
    let item_status: String = item.try_get("status")?;
    let item_completed: Option<DateTime<Utc>> = opt_dt(&item, "completed_at")?;
    if let Some(obj) = out.as_object_mut() {
        obj.insert(
            "item".to_string(),
            json!({
                "id": item_id_v.to_string(),
                "surah": item_surah,
                "ayah": item_ayah,
                "status": item_status,
                "completed_at": iso_opt(item_completed),
            }),
        );
    }
    Ok(Json(out))
}

/// `DELETE /api/v1/quran/hifz/plans/{plan_id}` (204). SRS rows are
/// NOT deleted (ayah review history).
pub async fn delete_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(plan_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/plans").await?;
    let plan_id = parse_path_uuid(&plan_id_s, "plan_id")?;
    let db = state.db_or_500()?;
    plan_or_404(db, ctx.user.id, plan_id).await?;
    db.execute(
        "DELETE FROM hifz_plan_items WHERE plan_id = ?",
        &[(plan_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_plans WHERE id = ?",
        &[(plan_id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- dashboard / activity / suggested ----------------------------------------

fn day_key(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%d").to_string()
}

fn streaks(active: &HashSet<NaiveDate>, today: NaiveDate) -> (i64, i64) {
    let mut best = 0i64;
    let mut run = 0i64;
    let mut prev: Option<NaiveDate> = None;
    let mut days: Vec<NaiveDate> = active.iter().copied().collect();
    days.sort();
    for day in days {
        run = if prev.map(|p| (day - p).num_days() == 1).unwrap_or(false) {
            run + 1
        } else {
            1
        };
        best = best.max(run);
        prev = Some(day);
    }
    let mut current = 0i64;
    let mut cursor = if active.contains(&today) {
        today
    } else {
        today - chrono::Duration::days(1)
    };
    while active.contains(&cursor) {
        current += 1;
        cursor -= chrono::Duration::days(1);
    }
    (current, best)
}

async fn goal_ayahs(db: &Db, user_id: Uuid) -> Result<i64, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT daily_goal_ayahs FROM hifz_user_settings WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(r.try_get::<i32, _>("daily_goal_ayahs")? as i64),
        None => Ok(0),
    }
}

/// Active dates over all history: ≥1 attempt, ≥1 graded word, or ≥1
/// SRS review that day (UTC days).
async fn active_dates(db: &Db, user_id: Uuid) -> Result<HashSet<NaiveDate>, ApiError> {
    let mut out: HashSet<NaiveDate> = HashSet::new();
    let attempts = db
        .fetch_all(
            "SELECT created_at FROM hifz_attempts WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    for r in &attempts {
        let d: DateTime<Utc> = r.try_get("created_at")?;
        out.insert(d.date_naive());
    }
    let words = db
        .fetch_all(
            "SELECT ws.updated_at, ws.created_at FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id WHERE a.user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    for r in &words {
        let updated_at: Option<DateTime<Utc>> = r.try_get("updated_at")?;
        let created_at: DateTime<Utc> = r.try_get("created_at")?;
        out.insert(updated_at.unwrap_or(created_at).date_naive());
    }
    let reviews = db
        .fetch_all(
            "SELECT last_reviewed_at FROM hifz_reviews WHERE user_id = ? AND last_reviewed_at IS NOT NULL",
            &[(user_id).into_arg()],
        )
        .await?;
    for r in &reviews {
        let d: DateTime<Utc> = r.try_get("last_reviewed_at")?;
        out.insert(d.date_naive());
    }
    Ok(out)
}

/// Distinct active ayahs of one UTC day (words by coalesce
/// (updated_at, created_at) ∪ reviews by last_reviewed_at).
async fn day_ayahs(db: &Db, user_id: Uuid, day: NaiveDate) -> Result<i64, ApiError> {
    let mut keys: HashSet<(i32, i32)> = HashSet::new();
    let words = db
        .fetch_all(
            "SELECT ws.surah, ws.ayah, ws.updated_at, ws.created_at FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id WHERE a.user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    for r in &words {
        let updated_at: Option<DateTime<Utc>> = r.try_get("updated_at")?;
        let created_at: DateTime<Utc> = r.try_get("created_at")?;
        if updated_at.unwrap_or(created_at).date_naive() == day {
            keys.insert((r.try_get("surah")?, r.try_get("ayah")?));
        }
    }
    let reviews = db
        .fetch_all(
            "SELECT surah, ayah, last_reviewed_at FROM hifz_reviews \
             WHERE user_id = ? AND last_reviewed_at IS NOT NULL",
            &[(user_id).into_arg()],
        )
        .await?;
    for r in &reviews {
        let d: DateTime<Utc> = r.try_get("last_reviewed_at")?;
        if d.date_naive() == day {
            keys.insert((r.try_get("surah")?, r.try_get("ayah")?));
        }
    }
    Ok(keys.len() as i64)
}

/// `GET /api/v1/quran/hifz/dashboard`.
pub async fn hifz_dashboard(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/dashboard").await?;
    let db = state.db_or_500()?;
    let user_id = ctx.user.id;
    let attempts_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM hifz_attempts WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let attempt_count: i64 = attempts_row.try_get("n")?;
    let score_rows = db
        .fetch_all(
            "SELECT ws.status, COUNT(*) AS n FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id \
             WHERE a.user_id = ? GROUP BY ws.status",
            &[(user_id).into_arg()],
        )
        .await?;
    let mut counts = [0i64; 4];
    for r in &score_rows {
        let status: String = r.try_get("status")?;
        let n: i64 = r.try_get("n")?;
        match status.as_str() {
            "correct" => counts[0] = n,
            "close" => counts[1] = n,
            "wrong" => counts[2] = n,
            "skipped" => counts[3] = n,
            _ => {}
        }
    }
    let graded: i64 = counts.iter().sum();
    let error_rows = db
        .fetch_all(
            "SELECT mistake_type, COUNT(*) AS n FROM hifz_mistakes WHERE user_id = ? GROUP BY mistake_type",
            &[(user_id).into_arg()],
        )
        .await?;
    let mut errors_by_type = Map::new();
    for r in &error_rows {
        let t: String = r.try_get("mistake_type")?;
        let n: i64 = r.try_get("n")?;
        errors_by_type.insert(t, json!(n));
    }
    let now = utc_now();
    let today = now.date_naive();
    let since = now - chrono::Duration::days(30);
    // Daily series, last 30 days incl. today. Attempts by created_at
    // date; words by created_at date only (unlike activity).
    let attempt_days = db
        .fetch_all(
            "SELECT created_at FROM hifz_attempts WHERE user_id = ? AND created_at >= ?",
            &[(user_id).into_arg(), (since).into_arg()],
        )
        .await?;
    let mut attempts_by_day: HashMap<String, i64> = HashMap::new();
    for r in &attempt_days {
        let d: DateTime<Utc> = r.try_get("created_at")?;
        *attempts_by_day.entry(day_key(d)).or_insert(0) += 1;
    }
    let word_days = db
        .fetch_all(
            "SELECT ws.created_at FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id \
             WHERE a.user_id = ? AND ws.created_at >= ?",
            &[(user_id).into_arg(), (since).into_arg()],
        )
        .await?;
    let mut words_by_day: HashMap<String, i64> = HashMap::new();
    for r in &word_days {
        let d: DateTime<Utc> = r.try_get("created_at")?;
        *words_by_day.entry(day_key(d)).or_insert(0) += 1;
    }
    let mut daily_series: Vec<Value> = Vec::new();
    for i in 0..30 {
        let day = today - chrono::Duration::days(29 - i);
        let key = day.format("%Y-%m-%d").to_string();
        daily_series.push(json!({
            "date": key,
            "attempts": attempts_by_day.get(&key).copied().unwrap_or(0),
            "graded_words": words_by_day.get(&key).copied().unwrap_or(0),
        }));
    }
    let due = due_reviews_list(db, user_id, 100).await?;
    let plans = list_plans(db, user_id).await?;
    let (streak_current, streak_best) = streaks(&active_dates(db, user_id).await?, today);
    let goal = goal_ayahs(db, user_id).await?;
    let goal_today = day_ayahs(db, user_id, today).await?;
    let worst = db
        .fetch_all(
            "SELECT surah, ayah, COUNT(*) AS n FROM hifz_mistakes WHERE user_id = ? \
             GROUP BY surah, ayah ORDER BY n DESC LIMIT 5",
            &[(user_id).into_arg()],
        )
        .await?;
    let weakest: Vec<Value> = worst
        .iter()
        .map(|r| {
            let s: i32 = r.try_get("surah").unwrap_or(0);
            let a: i32 = r.try_get("ayah").unwrap_or(0);
            let n: i64 = r.try_get("n").unwrap_or(0);
            json!({ "surah": s, "ayah": a, "mistake_count": n })
        })
        .collect();
    Ok(Json(json!({
        "attempts": attempt_count,
        "graded_words": graded,
        "counts": counts_json(counts),
        "errors_by_type": errors_by_type,
        "daily_series": daily_series,
        "streak_current": streak_current,
        "streak_best": streak_best,
        "goal_ayahs": goal,
        "goal_today_ayahs": goal_today,
        "goal_met": goal > 0 && goal_today >= goal,
        "due_count": due.len(),
        "due_ayahs": due.into_iter().take(10).collect::<Vec<_>>(),
        "plans": plans,
        "weakest_ayahs": weakest,
    })))
}

/// Shared due-list computation for the dashboard (no HTTP wrapper).
async fn due_reviews_list(db: &Db, user_id: Uuid, limit: i64) -> Result<Vec<Value>, ApiError> {
    let now = utc_now();
    let srs_rows = db
        .fetch_all(
            "SELECT * FROM hifz_reviews WHERE user_id = ? AND due_at <= ? ORDER BY due_at LIMIT ?",
            &[(user_id).into_arg(), (now).into_arg(), (limit).into_arg()],
        )
        .await?;
    let mut out: Vec<Value> = Vec::new();
    let mut seen: HashSet<(i32, i32)> = HashSet::new();
    for r in &srs_rows {
        let surah: i32 = r.try_get("surah")?;
        let ayah: i32 = r.try_get("ayah")?;
        seen.insert((surah, ayah));
        let due_at: Option<DateTime<Utc>> = opt_dt(r, "due_at")?;
        let last_quality: Option<i32> = r.try_get("last_quality")?;
        out.push(json!({
            "surah": surah,
            "ayah": ayah,
            "due_at": iso_opt(due_at),
            "repetitions": r.try_get::<i32, _>("repetitions")?,
            "ease": r.try_get::<f64, _>("ease")?,
            "interval_days": r.try_get::<i32, _>("interval_days")?,
            "last_quality": last_quality,
            "mistake_count": Value::Null,
            "last_activity_at": Value::Null,
            "source": "srs",
        }));
    }
    for entry in review_queue(db, user_id).await? {
        let surah = entry.get("surah").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        let ayah = entry.get("ayah").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
        if seen.contains(&(surah, ayah)) {
            continue;
        }
        seen.insert((surah, ayah));
        let mut item = entry.clone();
        if let Some(obj) = item.as_object_mut() {
            obj.insert("due_at".to_string(), Value::Null);
            obj.insert("repetitions".to_string(), Value::Null);
            obj.insert("ease".to_string(), Value::Null);
            obj.insert("interval_days".to_string(), Value::Null);
            obj.insert("last_quality".to_string(), Value::Null);
            obj.insert("source".to_string(), json!("mistakes"));
        }
        out.push(item);
    }
    out.truncate(limit as usize);
    Ok(out)
}

/// `GET /api/v1/quran/hifz/activity?days=371`.
pub async fn hifz_activity(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/activity").await?;
    let params = parse_query(&uri);
    let days = query_int(&params, "days", 371, 7, 371)?;
    let db = state.db_or_500()?;
    let user_id = ctx.user.id;
    let now = utc_now();
    let today = now.date_naive();
    let since = now - chrono::Duration::days(days);
    // Attempts by created_at date.
    let attempt_rows = db
        .fetch_all(
            "SELECT created_at FROM hifz_attempts WHERE user_id = ? AND created_at >= ?",
            &[(user_id).into_arg(), (since).into_arg()],
        )
        .await?;
    let mut attempts_by_day: HashMap<String, i64> = HashMap::new();
    for r in &attempt_rows {
        let d: DateTime<Utc> = r.try_get("created_at")?;
        *attempts_by_day.entry(day_key(d)).or_insert(0) += 1;
    }
    // Words by coalesce(updated_at, created_at) date + ayahs.
    let word_rows = db
        .fetch_all(
            "SELECT ws.surah, ws.ayah, ws.updated_at, ws.created_at FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id WHERE a.user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let mut words_by_day: HashMap<String, i64> = HashMap::new();
    let mut ayah_keys: HashMap<String, HashSet<(i32, i32)>> = HashMap::new();
    for r in &word_rows {
        let updated_at: Option<DateTime<Utc>> = r.try_get("updated_at")?;
        let created_at: DateTime<Utc> = r.try_get("created_at")?;
        let ts = updated_at.unwrap_or(created_at);
        if ts < since {
            continue;
        }
        let key = day_key(ts);
        *words_by_day.entry(key.clone()).or_insert(0) += 1;
        ayah_keys
            .entry(key)
            .or_default()
            .insert((r.try_get("surah")?, r.try_get("ayah")?));
    }
    // Reviews by last_reviewed_at date + ayahs.
    let review_rows = db
        .fetch_all(
            "SELECT surah, ayah, last_reviewed_at FROM hifz_reviews \
             WHERE user_id = ? AND last_reviewed_at IS NOT NULL AND last_reviewed_at >= ?",
            &[(user_id).into_arg(), (since).into_arg()],
        )
        .await?;
    let mut reviews_by_day: HashMap<String, i64> = HashMap::new();
    for r in &review_rows {
        let d: DateTime<Utc> = r.try_get("last_reviewed_at")?;
        let key = day_key(d);
        *reviews_by_day.entry(key.clone()).or_insert(0) += 1;
        ayah_keys
            .entry(key)
            .or_default()
            .insert((r.try_get("surah")?, r.try_get("ayah")?));
    }
    let mut series: Vec<Value> = Vec::new();
    for i in 0..days {
        let day = today - chrono::Duration::days(days - 1 - i);
        let key = day.format("%Y-%m-%d").to_string();
        series.push(json!({
            "date": key,
            "attempts": attempts_by_day.get(&key).copied().unwrap_or(0),
            "graded_words": words_by_day.get(&key).copied().unwrap_or(0),
            "reviews": reviews_by_day.get(&key).copied().unwrap_or(0),
            "ayahs": ayah_keys.get(&key).map(|s| s.len() as i64).unwrap_or(0),
        }));
    }
    let (streak_current, streak_best) = streaks(&active_dates(db, user_id).await?, today);
    let goal = goal_ayahs(db, user_id).await?;
    let today_ayahs = series
        .last()
        .and_then(|v| v.get("ayahs"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    Ok(Json(json!({
        "days": series,
        "streak_current": streak_current,
        "streak_best": streak_best,
        "goal_ayahs": goal,
        "today_ayahs": today_ayahs,
        "goal_met": goal > 0 && today_ayahs >= goal,
    })))
}

/// `GET /api/v1/quran/hifz/suggested`.
pub async fn hifz_suggested(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/suggested").await?;
    let db = state.db_or_500()?;
    let user_id = ctx.user.id;
    let plans = list_plans(db, user_id).await?;
    let mut memorized: HashSet<(i32, i32)> = HashSet::new();
    for plan in &plans {
        if plan.get("status").and_then(|v| v.as_str()) != Some("active") {
            continue;
        }
        if plan.get("done_ayahs").and_then(|v| v.as_i64()).unwrap_or(0) == 0 {
            continue;
        }
        let plan_id = plan
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        let Some(plan_id) = plan_id else { continue };
        let items = db
            .fetch_all(
                "SELECT surah, ayah FROM hifz_plan_items WHERE plan_id = ? AND status = 'done'",
                &[(plan_id).into_arg()],
            )
            .await?;
        for it in &items {
            memorized.insert((it.try_get("surah")?, it.try_get("ayah")?));
        }
    }
    let graded_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM hifz_word_scores ws \
             JOIN hifz_attempts a ON ws.attempt_id = a.id WHERE a.user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let graded_words: i64 = graded_row.try_get("n")?;
    if memorized.is_empty() {
        return Ok(Json(json!({
            "available": false,
            "reason": "setup_required",
            "detail": "هنوز بازهٔ محفوظات اعلام نشده است؛ اول یک برنامه بسازید و آیه‌های حفظ‌شده را کامل کنید.",
            "cards": [],
        })));
    }
    if graded_words < 10 {
        return Ok(Json(json!({
            "available": false,
            "reason": "insufficient_history",
            "detail": format!("تاریخچهٔ تمرین هنوز کم است ({graded_words} کلمهٔ ارزیابی‌شده؛ دست‌کم 10 لازم است)."),
            "cards": [],
        })));
    }
    let worst = db
        .fetch_all(
            "SELECT surah, ayah, COUNT(*) AS n FROM hifz_mistakes WHERE user_id = ? \
             GROUP BY surah, ayah ORDER BY n DESC LIMIT 20",
            &[(user_id).into_arg()],
        )
        .await?;
    let mut cards: Vec<Value> = Vec::new();
    for r in &worst {
        let s: i32 = r.try_get("surah")?;
        let a: i32 = r.try_get("ayah")?;
        if !memorized.contains(&(s, a)) {
            continue;
        }
        let n: i64 = r.try_get("n")?;
        let kind = if cards.is_empty() {
            "weakest_link"
        } else {
            "warm_up"
        };
        cards.push(json!({
            "surah": s,
            "ayah": a,
            "kind": kind,
            "mistake_count": n,
            "reason": "پرخطاترین آیهٔ محفوظات شما",
        }));
        if cards.len() >= 5 {
            break;
        }
    }
    if cards.is_empty() {
        return Ok(Json(json!({
            "available": false,
            "reason": "no_weak_ayahs",
            "detail": "در بازهٔ محفوظات شما آیهٔ پرخطایی نیست؛ کارت پیشنهادی‌ای نیست.",
            "cards": [],
        })));
    }
    Ok(Json(json!({
        "available": true,
        "reason": "ok",
        "detail": "",
        "cards": cards,
    })))
}

// --- tests ---------------------------------------------------------------------

fn test_out(
    row: &DynRow,
    questions: Value,
    total_score: Option<Value>,
    answered: Option<i64>,
) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let surah_start: i32 = row.try_get("surah_start")?;
    let surah_end: i32 = row.try_get("surah_end")?;
    let question_count: i32 = row.try_get("question_count")?;
    let time_limit_minutes: Option<i32> = row.try_get("time_limit_minutes")?;
    let question_length: String = row.try_get("question_length")?;
    let stop_rule: String = row.try_get("stop_rule")?;
    let status: String = row.try_get("status")?;
    let started_at: DateTime<Utc> = row.try_get("started_at")?;
    let ends_at: Option<DateTime<Utc>> = opt_dt(row, "ends_at")?;
    let completed_at: Option<DateTime<Utc>> = opt_dt(row, "completed_at")?;
    let mut out = json!({
        "id": id.to_string(),
        "surah_start": surah_start,
        "surah_end": surah_end,
        "question_count": question_count,
        "time_limit_minutes": time_limit_minutes,
        "question_length": question_length,
        "stop_rule": stop_rule,
        "status": status,
        "started_at": iso(started_at),
        "ends_at": iso_opt(ends_at),
        "completed_at": iso_opt(completed_at),
        "temporary_session": true,
        "questions": questions,
    });
    if let Some(total) = total_score {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("total_score".to_string(), total);
        }
    }
    if let Some(a) = answered {
        if let Some(obj) = out.as_object_mut() {
            obj.insert("answered".to_string(), json!(a));
        }
    }
    Ok(out)
}

fn question_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let position: i32 = row.try_get("position")?;
    let surah: i32 = row.try_get("surah")?;
    let ayah_start: i32 = row.try_get("ayah_start")?;
    let ayah_end: i32 = row.try_get("ayah_end")?;
    let prompt: Option<String> = row.try_get("prompt")?;
    let status: String = row.try_get("status")?;
    let score: Option<f64> = row.try_get("score")?;
    let counts: Value = row.try_get("counts")?;
    let peek_count: i32 = row.try_get("peek_count")?;
    let raw_events: Value = row.try_get("raw_events")?;
    let answered_at: Option<DateTime<Utc>> = opt_dt(row, "answered_at")?;
    Ok(json!({
        "id": id.to_string(),
        "position": position,
        "surah": surah,
        "ayah_start": ayah_start,
        "ayah_end": ayah_end,
        "prompt": prompt,
        "status": status,
        "score": score,
        "counts": counts,
        "peek_count": peek_count,
        "raw_events": raw_events,
        "answered_at": iso_opt(answered_at),
    }))
}

async fn test_or_404(db: &Db, user_id: Uuid, test_id: Uuid) -> Result<DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_tests WHERE id = ?",
            &[(test_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != user_id {
                return Err(not_found("hifz_test_not_found", "آزمون پیدا نشد"));
            }
            Ok(r)
        }
        None => Err(not_found("hifz_test_not_found", "آزمون پیدا نشد")),
    }
}

async fn question_or_404(db: &Db, test_id: Uuid, question_id: Uuid) -> Result<DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_test_questions WHERE id = ?",
            &[(question_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner_test: Uuid = r.try_get("test_id")?;
            if owner_test != test_id {
                return Err(not_found(
                    "hifz_test_question_not_found",
                    "سؤال آزمون پیدا نشد",
                ));
            }
            Ok(r)
        }
        None => Err(not_found(
            "hifz_test_question_not_found",
            "سؤال آزمون پیدا نشد",
        )),
    }
}

async fn test_total(db: &Db, test_id: Uuid) -> Result<Option<Value>, ApiError> {
    let rows = db
        .fetch_all(
            "SELECT score, peek_count FROM hifz_test_questions WHERE test_id = ? ORDER BY position",
            &[(test_id).into_arg()],
        )
        .await?;
    let qs: Vec<(Option<f64>, i64)> = rows
        .iter()
        .map(|r| {
            let s: Option<f64> = r.try_get("score")?;
            let p: i32 = r.try_get("peek_count")?;
            Ok((s, p as i64))
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(test_total_score(&qs))
}

/// `POST /api/v1/quran/hifz/tests` (profile gate `hifz_advanced`).
pub async fn create_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let parsed = parse_body(&body)?;
    let surah_start = req_int_loose(&parsed, "surah_start", 1, 114)?;
    let surah_end = req_int_loose(&parsed, "surah_end", 1, 114)?;
    let question_count = opt_int_loose(&parsed, "question_count", 1, 99)?.unwrap_or(10);
    let time_limit_minutes = opt_int_loose(&parsed, "time_limit_minutes", 1, 60)?;
    let question_length = match parsed.get("question_length").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(validation_err("question_length", "must be a string")),
        None => "medium".to_string(),
    };
    let stop_rule = match parsed.get("stop_rule").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(validation_err("stop_rule", "must be a string")),
        None => "ayah_end".to_string(),
    };
    // Service validation order: length → stop_rule → range.
    if !["short", "medium", "long"].contains(&question_length.as_str()) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_length",
            "طول سؤال باید short/medium/long باشد",
        ));
    }
    if stop_rule != "ayah_end" && stop_rule != "line_end" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_stop_rule",
            "stop_rule باید ayah_end یا line_end باشد",
        ));
    }
    if surah_start > surah_end {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_range",
            "شروع بازه باید پیش از پایان آن باشد",
        ));
    }
    let db = state.db_or_500()?;
    ensure_profile_for_hifz_advanced(db, &state.config, &ctx).await?;
    let test_id = new_id();
    let now = utc_now();
    let ends_at = time_limit_minutes.map(|m| now + chrono::Duration::minutes(m));
    // Seed: first 8 bytes of the test UUID, little-endian (Python).
    let seed = u64::from_le_bytes(test_id.as_bytes()[..8].try_into().unwrap());
    let passages = build_passages(
        surah_start,
        surah_end,
        question_count,
        &question_length,
        &stop_rule,
        seed,
    )?;
    db.execute(
        "INSERT INTO hifz_tests (id, user_id, surah_start, surah_end, question_count, \
         time_limit_minutes, question_length, stop_rule, status, started_at, ends_at, \
         completed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'in_progress', ?, ?, NULL)",
        &[
            (test_id).into_arg(),
            (ctx.user.id).into_arg(),
            (surah_start).into_arg(),
            (surah_end).into_arg(),
            (question_count).into_arg(),
            match time_limit_minutes {
                Some(v) => (v).into_arg(),
                None => Arg::NullInt,
            },
            (question_length).into_arg(),
            (stop_rule).into_arg(),
            (now).into_arg(),
            match ends_at {
                Some(v) => (v).into_arg(),
                None => Arg::NullTime,
            },
        ],
    )
    .await?;
    for (pos, p) in passages.iter().enumerate() {
        let surah = p.get("surah").and_then(|v| v.as_i64()).unwrap_or(0);
        let ayah_start = p.get("ayah_start").and_then(|v| v.as_i64()).unwrap_or(0);
        let ayah_end = p.get("ayah_end").and_then(|v| v.as_i64()).unwrap_or(0);
        let prompt = p
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        db.execute(
            "INSERT INTO hifz_test_questions (id, test_id, position, surah, ayah_start, \
             ayah_end, prompt_words, prompt, status, score, counts, peek_count, raw_events, \
             created_at, answered_at) VALUES (?, ?, ?, ?, ?, ?, 3, ?, 'pending', NULL, '{}', \
             0, '[]', ?, NULL)",
            &[
                (new_id()).into_arg(),
                (test_id).into_arg(),
                (pos as i64).into_arg(),
                (surah).into_arg(),
                (ayah_start).into_arg(),
                (ayah_end).into_arg(),
                (prompt).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
    }
    let row = test_or_404(db, ctx.user.id, test_id).await?;
    Ok((
        StatusCode::CREATED,
        Json(test_out(&row, Value::Array(passages), None, None)?),
    ))
}

/// `GET /api/v1/quran/hifz/tests`.
pub async fn list_tests(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT * FROM hifz_tests WHERE user_id = ? ORDER BY started_at DESC LIMIT ?",
            &[(ctx.user.id).into_arg(), (limit).into_arg()],
        )
        .await?;
    let mut out: Vec<Value> = Vec::new();
    for r in &rows {
        let id: Uuid = r.try_get("id")?;
        let total = test_total(db, id).await?;
        let answered_row = db
            .fetch_one(
                "SELECT COUNT(*) AS n FROM hifz_test_questions WHERE test_id = ? AND status = 'answered'",
                &[(id).into_arg()],
            )
            .await?;
        let answered: i64 = answered_row.try_get("n")?;
        out.push(test_out(r, json!([]), total, Some(answered))?);
    }
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/quran/hifz/tests/{test_id}`.
pub async fn get_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(test_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let test_id = parse_path_uuid(&test_id_s, "test_id")?;
    let db = state.db_or_500()?;
    let row = test_or_404(db, ctx.user.id, test_id).await?;
    let total = test_total(db, test_id).await?;
    let questions = db
        .fetch_all(
            "SELECT * FROM hifz_test_questions WHERE test_id = ? ORDER BY position",
            &[(test_id).into_arg()],
        )
        .await?;
    let qs: Vec<Value> = questions
        .iter()
        .map(question_out)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(test_out(&row, Value::Array(qs), total, None)?))
}

/// `POST /api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/skip`.
pub async fn skip_test_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((test_id_s, question_id_s)): Path<(String, String)>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let test_id = parse_path_uuid(&test_id_s, "test_id")?;
    let question_id = parse_path_uuid(&question_id_s, "question_id")?;
    let db = state.db_or_500()?;
    test_or_404(db, ctx.user.id, test_id).await?;
    let q = question_or_404(db, test_id, question_id).await?;
    let status: String = q.try_get("status")?;
    if status != "pending" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_question_closed",
            "این سؤال بسته شده است",
        ));
    }
    db.execute(
        "UPDATE hifz_test_questions SET status = 'skipped' WHERE id = ?",
        &[(question_id).into_arg()],
    )
    .await?;
    Ok(Json(
        json!({ "question_id": question_id.to_string(), "status": "skipped" }),
    ))
}

/// `POST /api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/peek`.
pub async fn peek_test_question(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((test_id_s, question_id_s)): Path<(String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let test_id = parse_path_uuid(&test_id_s, "test_id")?;
    let question_id = parse_path_uuid(&question_id_s, "question_id")?;
    let parsed = parse_body(&body)?;
    let surah = req_int_loose(&parsed, "surah", 1, 114)?;
    let ayah = req_int_loose(&parsed, "ayah", 1, 286)?;
    let word_index = req_int_loose(&parsed, "word_index", 0, i64::MAX)?;
    let db = state.db_or_500()?;
    test_or_404(db, ctx.user.id, test_id).await?;
    let q = question_or_404(db, test_id, question_id).await?;
    let status: String = q.try_get("status")?;
    if status != "pending" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_question_closed",
            "این سؤال بسته شده است",
        ));
    }
    let Some(words) = ayah_words(surah as i32, ayah as i32) else {
        return Err(domain(
            StatusCode::SERVICE_UNAVAILABLE,
            "quran_text_unavailable",
            "متن این آیه در دسترس نیست",
        ));
    };
    let ayah_start: i32 = q.try_get("ayah_start")?;
    let ayah_end: i32 = q.try_get("ayah_end")?;
    if !(ayah_start as i64 <= ayah && ayah <= ayah_end as i64) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_peek_range",
            "Peek باید درون گذر سؤال باشد",
        ));
    }
    if word_index as usize >= words.len() {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_word_index",
            "شمارهٔ کلمه خارج از آیه است",
        ));
    }
    let raw_events: Value = q.try_get("raw_events")?;
    let mut events: Vec<Value> = raw_events.as_array().cloned().unwrap_or_default();
    events.push(json!({
        "surah": surah,
        "ayah": ayah,
        "word_index": word_index,
        "expected": words[word_index as usize],
        "status": "peeked",
        "prob": Value::Null,
        "heard": Value::Null,
        "mistake_type": "peek",
        "removed": false,
    }));
    let (_, _, peek_count) = test_question_score(&events);
    db.execute(
        "UPDATE hifz_test_questions SET raw_events = ?, peek_count = ? WHERE id = ?",
        &[
            (Value::Array(events)).into_arg(),
            (peek_count).into_arg(),
            (question_id).into_arg(),
        ],
    )
    .await?;
    Ok(Json(json!({
        "question_id": question_id.to_string(),
        "word": words[word_index as usize],
        "surah": surah,
        "ayah": ayah,
        "word_index": word_index,
        "peek_count": peek_count,
    })))
}

/// `POST /api/v1/quran/hifz/tests/{test_id}/questions/{question_id}/events/{event_index}/remove`.
pub async fn remove_test_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((test_id_s, question_id_s, event_index_s)): Path<(String, String, String)>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let test_id = parse_path_uuid(&test_id_s, "test_id")?;
    let question_id = parse_path_uuid(&question_id_s, "question_id")?;
    let event_index: i64 = event_index_s
        .parse()
        .map_err(|_| validation_err("event_index", "must be an integer"))?;
    // Body is optional: `{removed: bool}` defaults to true. An empty
    // body is treated as `{}` (Python's `removed: bool = True`).
    let removed = if body.is_empty() {
        true
    } else {
        let parsed = parse_body(&body)?;
        opt_bool_loose(&parsed, "removed")?.unwrap_or(true)
    };
    let db = state.db_or_500()?;
    test_or_404(db, ctx.user.id, test_id).await?;
    let q = question_or_404(db, test_id, question_id).await?;
    let raw_events: Value = q.try_get("raw_events")?;
    let mut events: Vec<Value> = raw_events.as_array().cloned().unwrap_or_default();
    if event_index < 0 || event_index as usize >= events.len() {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_event_index",
            "شمارهٔ رویداد خارج از فهرست است",
        ));
    }
    if let Some(obj) = events[event_index as usize].as_object_mut() {
        obj.insert("removed".to_string(), json!(removed));
    }
    let (score, counts, peek_count) = test_question_score(&events);
    db.execute(
        "UPDATE hifz_test_questions SET raw_events = ?, counts = ?, peek_count = ?, score = ? WHERE id = ?",
        &[
            (Value::Array(events.clone())).into_arg(),
            (json!({
                "correct": counts[0],
                "close": counts[1],
                "wrong": counts[2],
                "skipped": counts[3],
            }))
            .into_arg(),
            (peek_count).into_arg(),
            match score {
                Some(s) => (s).into_arg(),
                None => Arg::NullFloat,
            },
            (question_id).into_arg(),
        ],
    )
    .await?;
    Ok(Json(json!({
        "question_id": question_id.to_string(),
        "event_index": event_index,
        "removed": removed,
        "score": score,
        "counts": {
            "correct": counts[0],
            "close": counts[1],
            "wrong": counts[2],
            "skipped": counts[3],
        },
        "peek_count": peek_count,
        "raw_events": events,
    })))
}

/// `POST /api/v1/quran/hifz/tests/{test_id}/complete`.
pub async fn complete_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(test_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let test_id = parse_path_uuid(&test_id_s, "test_id")?;
    let db = state.db_or_500()?;
    let row = test_or_404(db, ctx.user.id, test_id).await?;
    let status: String = row.try_get("status")?;
    if status != "in_progress" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_test_closed",
            "این آزمون بسته شده است",
        ));
    }
    let now = utc_now();
    db.execute(
        "UPDATE hifz_tests SET status = 'completed', completed_at = ? WHERE id = ?",
        &[(now).into_arg(), (test_id).into_arg()],
    )
    .await?;
    let row = test_or_404(db, ctx.user.id, test_id).await?;
    let total = test_total(db, test_id).await?;
    let questions = db
        .fetch_all(
            "SELECT * FROM hifz_test_questions WHERE test_id = ? ORDER BY position",
            &[(test_id).into_arg()],
        )
        .await?;
    let qs: Vec<Value> = questions
        .iter()
        .map(question_out)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Json(test_out(&row, Value::Array(qs), total, None)?))
}

/// `DELETE /api/v1/quran/hifz/tests/{test_id}` (204).
pub async fn delete_test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(test_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tests").await?;
    let test_id = parse_path_uuid(&test_id_s, "test_id")?;
    let db = state.db_or_500()?;
    test_or_404(db, ctx.user.id, test_id).await?;
    db.execute(
        "DELETE FROM hifz_test_questions WHERE test_id = ?",
        &[(test_id).into_arg()],
    )
    .await?;
    db.execute(
        "DELETE FROM hifz_tests WHERE id = ?",
        &[(test_id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- mutashabihat --------------------------------------------------------------

/// `GET /api/v1/quran/hifz/mutashabihat?surah&ayah`.
pub async fn mutashabihat(
    State(_state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = authenticate(&_state, &headers, "/api/v1/quran/hifz/mutashabihat").await?;
    let params = parse_query(&uri);
    let surah = query_opt_int(&params, "surah", 1, 114)?
        .ok_or_else(|| validation_err("surah", "field is required"))?;
    let ayah = query_opt_int(&params, "ayah", 1, 286)?
        .ok_or_else(|| validation_err("ayah", "field is required"))?;
    Ok(Json(Value::Array(groups_for_ayah(
        surah as i32,
        ayah as i32,
    ))))
}

/// `GET /api/v1/quran/hifz/mutashabihat/phrase?phrase=` — one group
/// or JSON `null`.
pub async fn mutashabihat_phrase(
    State(_state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = authenticate(&_state, &headers, "/api/v1/quran/hifz/mutashabihat/phrase").await?;
    let params = parse_query(&uri);
    let phrase = params
        .get("phrase")
        .cloned()
        .ok_or_else(|| validation_err("phrase", "field is required"))?;
    if phrase.chars().count() < 1 || phrase.chars().count() > 500 {
        return Err(validation_err("phrase", "length out of range"));
    }
    Ok(Json(group_for_phrase(&phrase).unwrap_or(Value::Null)))
}

/// `GET /api/v1/quran/hifz/mutashabihat/count`.
pub async fn mutashabihat_count(
    State(_state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let _ctx = authenticate(&_state, &headers, "/api/v1/quran/hifz/mutashabihat/count").await?;
    Ok(Json(json!({ "phrase_count": phrase_count() })))
}

// --- tasmi ---------------------------------------------------------------------

fn tasmi_out(row: &DynRow) -> Result<Value, ApiError> {
    let id: Uuid = row.try_get("id")?;
    let method: String = row.try_get("method")?;
    let variant: String = row.try_get("variant")?;
    let start_surah: i32 = row.try_get("start_surah")?;
    let start_ayah: i32 = row.try_get("start_ayah")?;
    let end_surah: i32 = row.try_get("end_surah")?;
    let end_ayah: i32 = row.try_get("end_ayah")?;
    let state: Value = row.try_get("state")?;
    let self_assessed: bool = row.try_get("self_assessed")?;
    let reflection: Option<String> = row.try_get("reflection")?;
    let status: String = row.try_get("status")?;
    let created_at: DateTime<Utc> = row.try_get("created_at")?;
    let updated_at: Option<DateTime<Utc>> = opt_dt(row, "updated_at")?;
    let completed_at: Option<DateTime<Utc>> = opt_dt(row, "completed_at")?;
    Ok(json!({
        "id": id.to_string(),
        "method": method,
        "variant": variant,
        "start_surah": start_surah,
        "start_ayah": start_ayah,
        "end_surah": end_surah,
        "end_ayah": end_ayah,
        "state": state,
        "current": tasmi_current(&method, &state),
        "self_assessed": self_assessed,
        "reflection": reflection,
        "status": status,
        "created_at": iso(created_at),
        "updated_at": iso_opt(updated_at),
        "completed_at": iso_opt(completed_at),
    }))
}

async fn tasmi_or_404(db: &Db, user_id: Uuid, session_id: Uuid) -> Result<DynRow, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT * FROM hifz_tasmi_sessions WHERE id = ?",
            &[(session_id).into_arg()],
        )
        .await?;
    match row {
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != user_id {
                return Err(not_found("hifz_tasmi_not_found", "نشست تسمی پیدا نشد"));
            }
            Ok(r)
        }
        None => Err(not_found("hifz_tasmi_not_found", "نشست تسمی پیدا نشد")),
    }
}

/// `POST /api/v1/quran/hifz/tasmi/sessions` (profile gate).
pub async fn create_tasmi(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tasmi/sessions").await?;
    let parsed = parse_body(&body)?;
    let method = req_str_raw(&parsed, "method", 1, 20)?;
    let variant = match parsed.get("variant").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(validation_err("variant", "must be a string")),
        None => "standard".to_string(),
    };
    let start_surah = req_int_loose(&parsed, "start_surah", 1, 114)?;
    let start_ayah = req_int_loose(&parsed, "start_ayah", 1, 286)?;
    let end_surah = req_int_loose(&parsed, "end_surah", 1, 114)?;
    let end_ayah = req_int_loose(&parsed, "end_ayah", 1, 286)?;
    if !["tasmi8", "method_6446", "method_3x3"].contains(&method.as_str()) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_tasmi_method",
            "روش باید tasmi8 یا method_6446 یا method_3x3 باشد",
        ));
    }
    if (start_surah, start_ayah) > (end_surah, end_ayah) {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_tasmi_range",
            "شروع بازه باید پیش از پایان آن باشد",
        ));
    }
    let db = state.db_or_500()?;
    ensure_profile_for_hifz_advanced(db, &state.config, &ctx).await?;
    let ayahs = plan_ayah_order(
        start_surah as i32,
        start_ayah as i32,
        end_surah as i32,
        end_ayah as i32,
        "normal",
    );
    let initial_state = match method.as_str() {
        "tasmi8" => tasmi8_initial_state(&variant)?,
        "method_6446" => method_6446_initial_state(&ayahs),
        _ => method_3x3_initial_state(&ayahs),
    };
    let session_id = new_id();
    let now = utc_now();
    db.execute(
        "INSERT INTO hifz_tasmi_sessions (id, user_id, method, variant, start_surah, \
         start_ayah, end_surah, end_ayah, state, self_assessed, reflection, status, \
         created_at, updated_at, completed_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, TRUE, \
         NULL, 'active', ?, NULL, NULL)",
        &[
            (session_id).into_arg(),
            (ctx.user.id).into_arg(),
            (method).into_arg(),
            (variant).into_arg(),
            (start_surah).into_arg(),
            (start_ayah).into_arg(),
            (end_surah).into_arg(),
            (end_ayah).into_arg(),
            (initial_state).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    let row = tasmi_or_404(db, ctx.user.id, session_id).await?;
    Ok((StatusCode::CREATED, Json(tasmi_out(&row)?)))
}

/// `GET /api/v1/quran/hifz/tasmi/sessions` — limit 20, updated_at desc.
pub async fn list_tasmi(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tasmi/sessions").await?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT * FROM hifz_tasmi_sessions WHERE user_id = ? ORDER BY updated_at DESC LIMIT 20",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let out: Vec<Value> = rows.iter().map(tasmi_out).collect::<Result<Vec<_>, _>>()?;
    Ok(Json(Value::Array(out)))
}

/// `GET /api/v1/quran/hifz/tasmi/sessions/{session_id}`.
pub async fn get_tasmi(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id_s): Path<String>,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tasmi/sessions").await?;
    let session_id = parse_path_uuid(&session_id_s, "session_id")?;
    let db = state.db_or_500()?;
    let row = tasmi_or_404(db, ctx.user.id, session_id).await?;
    Ok(Json(tasmi_out(&row)?))
}

/// `PATCH /api/v1/quran/hifz/tasmi/sessions/{session_id}`.
pub async fn patch_tasmi(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id_s): Path<String>,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tasmi/sessions").await?;
    let session_id = parse_path_uuid(&session_id_s, "session_id")?;
    let parsed = parse_body(&body)?;
    let action = req_str_raw(&parsed, "action", 1, 20)?;
    let rating = match parsed.get("rating").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(validation_err("rating", "must be a string")),
        None => None,
    };
    let reflection = match parsed.get("reflection").filter(|v| !v.is_null()) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(validation_err("reflection", "must be a string")),
        None => None,
    };
    let db = state.db_or_500()?;
    let row = tasmi_or_404(db, ctx.user.id, session_id).await?;
    let method: String = row.try_get("method")?;
    let status: String = row.try_get("status")?;
    let mut state_value: Value = row.try_get("state")?;
    if status == "completed" && action != "complete" {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_tasmi_closed",
            "این نشست تسمی تکمیل شده است",
        ));
    }
    let mut new_status = status.clone();
    let now = utc_now();
    match action.as_str() {
        "tap" => {
            if method == "method_6446" {
                state_value = method_6446_tap(&state_value);
            } else if method == "method_3x3" {
                state_value = method_3x3_tap(&state_value);
            } else {
                return Err(domain(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "hifz_tasmi_action",
                    "روش هشت‌مرحله‌ای tap ندارد؛ از next_stage استفاده کنید",
                ));
            }
        }
        "next_stage" => {
            if method != "tasmi8" {
                return Err(domain(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "hifz_tasmi_action",
                    "next_stage فقط برای روش هشت‌مرحله‌ای است",
                ));
            }
            state_value = tasmi8_advance(&state_value);
        }
        "rate" => {
            let Some(rating) = rating else {
                return Err(domain(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "hifz_tasmi_rating",
                    "رتبه‌بندی مرور لازم است",
                ));
            };
            let due = tasmi_review_due_at(&rating, now)?;
            if let Some(obj) = state_value.as_object_mut() {
                obj.insert("last_rating".to_string(), json!(rating));
                obj.insert("review_due_at".to_string(), json!(iso(due)));
            }
        }
        "reflect" => {}
        "pause" => new_status = "paused".to_string(),
        "resume" => new_status = "active".to_string(),
        "complete" => new_status = "completed".to_string(),
        _ => {
            return Err(domain(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hifz_tasmi_action",
                "کنش نامعتبر است",
            ));
        }
    }
    let done = state_value
        .get("done")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut completed_at: Option<DateTime<Utc>> = opt_dt(&row, "completed_at")?;
    if new_status == "completed" {
        completed_at = Some(now);
    } else if done && new_status == "active" {
        new_status = "completed".to_string();
        completed_at = Some(now);
    }
    let reflection_value: Option<String> = reflection.map(|r| r.chars().take(2000).collect());
    db.execute(
        "UPDATE hifz_tasmi_sessions SET state = ?, status = ?, reflection = ?, \
         completed_at = ?, updated_at = ? WHERE id = ?",
        &[
            (state_value).into_arg(),
            (new_status).into_arg(),
            match reflection_value {
                Some(v) => (v).into_arg(),
                None => {
                    // Keep the stored reflection when none is sent.
                    let cur: Option<String> = row.try_get("reflection")?;
                    match cur {
                        Some(v) => (v).into_arg(),
                        None => Arg::NullText,
                    }
                }
            },
            match completed_at {
                Some(v) => (v).into_arg(),
                None => Arg::NullTime,
            },
            (now).into_arg(),
            (session_id).into_arg(),
        ],
    )
    .await?;
    let row = tasmi_or_404(db, ctx.user.id, session_id).await?;
    Ok(Json(tasmi_out(&row)?))
}

/// `DELETE /api/v1/quran/hifz/tasmi/sessions/{session_id}` (204).
pub async fn delete_tasmi(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id_s): Path<String>,
) -> ApiResult<(StatusCode, ())> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/tasmi/sessions").await?;
    let session_id = parse_path_uuid(&session_id_s, "session_id")?;
    let db = state.db_or_500()?;
    tasmi_or_404(db, ctx.user.id, session_id).await?;
    db.execute(
        "DELETE FROM hifz_tasmi_sessions WHERE id = ?",
        &[(session_id).into_arg()],
    )
    .await?;
    Ok((StatusCode::NO_CONTENT, ()))
}

// --- data preview / purge ------------------------------------------------------

async fn count_by_ids(db: &Db, table: &str, col: &str, ids: &[Uuid]) -> Result<i64, ApiError> {
    let mut total = 0i64;
    for id in ids {
        let row = db
            .fetch_one(
                &format!("SELECT COUNT(*) AS n FROM {table} WHERE {col} = ?"),
                &[(*id).into_arg()],
            )
            .await?;
        total += row.try_get::<i64, _>("n")?;
    }
    Ok(total)
}

async fn count_by_user(db: &Db, table: &str, user_id: Uuid) -> Result<i64, ApiError> {
    let row = db
        .fetch_one(
            &format!("SELECT COUNT(*) AS n FROM {table} WHERE user_id = ?"),
            &[(user_id).into_arg()],
        )
        .await?;
    row.try_get("n")
}

async fn purge_counts(db: &Db, user_id: Uuid) -> Result<Map<String, Value>, ApiError> {
    let mut counts = Map::new();
    let attempt_ids = db
        .fetch_all(
            "SELECT id FROM hifz_attempts WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let test_ids = db
        .fetch_all(
            "SELECT id FROM hifz_tests WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let plan_ids = db
        .fetch_all(
            "SELECT id FROM hifz_plans WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let attempts: Vec<Uuid> = attempt_ids
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<Vec<_>, _>>()?;
    let tests: Vec<Uuid> = test_ids
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<Vec<_>, _>>()?;
    let plans: Vec<Uuid> = plan_ids
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<Vec<_>, _>>()?;
    let word_scores = count_by_ids(db, "hifz_word_scores", "attempt_id", &attempts).await?;
    let mistakes_row = db
        .fetch_one(
            "SELECT COUNT(*) AS n FROM hifz_mistakes WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?;
    let mistakes_total: i64 = mistakes_row.try_get("n")?;
    let test_questions = count_by_ids(db, "hifz_test_questions", "test_id", &tests).await?;
    let plan_items = count_by_ids(db, "hifz_plan_items", "plan_id", &plans).await?;
    counts.insert("hifz_word_scores".to_string(), json!(word_scores));
    counts.insert("hifz_mistakes".to_string(), json!(mistakes_total));
    counts.insert("hifz_test_questions".to_string(), json!(test_questions));
    counts.insert("hifz_plan_items".to_string(), json!(plan_items));
    counts.insert("hifz_attempts".to_string(), json!(attempts.len() as i64));
    counts.insert("hifz_tests".to_string(), json!(tests.len() as i64));
    counts.insert("hifz_plans".to_string(), json!(plans.len() as i64));
    counts.insert(
        "hifz_reviews".to_string(),
        json!(count_by_user(db, "hifz_reviews", user_id).await?),
    );
    counts.insert(
        "hifz_tasmi_sessions".to_string(),
        json!(count_by_user(db, "hifz_tasmi_sessions", user_id).await?),
    );
    counts.insert(
        "hifz_user_settings".to_string(),
        json!(count_by_user(db, "hifz_user_settings", user_id).await?),
    );
    Ok(counts)
}

/// `GET /api/v1/quran/hifz/data/preview` — counts only, no content.
pub async fn data_preview(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/data/preview").await?;
    let db = state.db_or_500()?;
    let counts = purge_counts(db, ctx.user.id).await?;
    let total: i64 = counts.values().filter_map(|v| v.as_i64()).sum();
    Ok(Json(json!({
        "scope": "hifz_only",
        "counts": counts,
        "total": total,
    })))
}

/// `POST /api/v1/quran/hifz/data/purge` — 3/hour per session.
/// Body: `{confirm: true, idempotency_key: <UUID>}`; any extra field
/// → 422. Idempotency is natural (repeat on empty data → all zeros).
pub async fn data_purge(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/quran/hifz/data/purge").await?;
    if purge_rate_limited(&ctx) {
        return Err(rate_limited_err());
    }
    let parsed = parse_body(&body)?;
    // `extra="forbid"`: any field besides confirm/idempotency_key → 422.
    if let Some(obj) = parsed.as_object() {
        for key in obj.keys() {
            if key != "confirm" && key != "idempotency_key" {
                return Err(validation_err(key, "extra fields are not permitted"));
            }
        }
    }
    let confirm = opt_bool_loose(&parsed, "confirm")?.unwrap_or(false);
    if !confirm {
        return Err(domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_purge_not_confirmed",
            "برای حذف داده‌ها، تأیید (confirm=true) لازم است",
        ));
    }
    let key_raw = parsed
        .get("idempotency_key")
        .and_then(|v| v.as_str())
        .ok_or_else(|| validation_err("idempotency_key", "field is required"))?;
    let key = Uuid::parse_str(key_raw).map_err(|_| {
        domain(
            StatusCode::UNPROCESSABLE_ENTITY,
            "hifz_purge_bad_idempotency_key",
            "idempotency_key باید UUID معتبر باشد",
        )
    })?;
    let db = state.db_or_500()?;
    let user_id = ctx.user.id;
    let counts = purge_counts(db, user_id).await?;
    // Child → parent order; mistakes by attempt first, then the
    // remainder by user (peek rows survive their attempt's delete
    // only when the attempt is gone — Python deletes the same way).
    let attempt_ids: Vec<Uuid> = db
        .fetch_all(
            "SELECT id FROM hifz_attempts WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<Vec<_>, _>>()?;
    for id in &attempt_ids {
        db.execute(
            "DELETE FROM hifz_word_scores WHERE attempt_id = ?",
            &[(*id).into_arg()],
        )
        .await?;
        db.execute(
            "DELETE FROM hifz_mistakes WHERE attempt_id = ?",
            &[(*id).into_arg()],
        )
        .await?;
    }
    db.execute(
        "DELETE FROM hifz_mistakes WHERE user_id = ?",
        &[(user_id).into_arg()],
    )
    .await?;
    let test_ids: Vec<Uuid> = db
        .fetch_all(
            "SELECT id FROM hifz_tests WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<Vec<_>, _>>()?;
    for id in &test_ids {
        db.execute(
            "DELETE FROM hifz_test_questions WHERE test_id = ?",
            &[(*id).into_arg()],
        )
        .await?;
    }
    let plan_ids: Vec<Uuid> = db
        .fetch_all(
            "SELECT id FROM hifz_plans WHERE user_id = ?",
            &[(user_id).into_arg()],
        )
        .await?
        .iter()
        .map(|r| r.try_get("id"))
        .collect::<Result<Vec<_>, _>>()?;
    for id in &plan_ids {
        db.execute(
            "DELETE FROM hifz_plan_items WHERE plan_id = ?",
            &[(*id).into_arg()],
        )
        .await?;
    }
    for table in [
        "hifz_attempts",
        "hifz_tests",
        "hifz_plans",
        "hifz_reviews",
        "hifz_tasmi_sessions",
        "hifz_user_settings",
    ] {
        db.execute(
            &format!("DELETE FROM {table} WHERE user_id = ?"),
            &[(user_id).into_arg()],
        )
        .await?;
    }
    let total: i64 = counts.values().filter_map(|v| v.as_i64()).sum();
    crate::audit::emit_event(
        db,
        "user",
        &user_id.to_string(),
        "user.hifz_data_purged",
        json!({ "scope": "hifz_only" }),
    )
    .await?;
    // Audit log: counts + key only, never content (like Python).
    crate::audit::log_audit(
        db,
        Some(user_id),
        "user",
        "hifz.data_purged",
        "user",
        Some(&user_id.to_string()),
        None,
        Some(json!({
            "scope": "hifz_only",
            "deleted": counts,
            "idempotency_key": key.to_string(),
        })),
        None,
        None,
    )
    .await?;
    Ok(Json(json!({
        "scope": "hifz_only",
        "deleted": counts,
        "total": total,
    })))
}

// --- live stream (not portable) -------------------------------------------------

/// `WS /api/v1/quran/hifz/attempts/{attempt_id}/stream` — the live
/// reading stream needs a stateful WebSocket holding a native
/// sherpa-onnx recognizer in memory; the Rust binary has no ASR and
/// the HTTP proxy cannot relay WebSocket upgrades (ADR-031). This
/// explicit 503 keeps the failure honest instead of letting the
/// proxy mangle the handshake. Use the Python origin directly
/// (or `RUST_ANYTHING_UNIT_HIFZ=original` + direct origin access)
/// for live streaming.
pub async fn hifz_stream_unavailable() -> ApiResult<(StatusCode, Json<Value>)> {
    Ok((
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": {
                "code": "hifz_live_unavailable",
                "message": "پخش زندهٔ حفظ در نسخهٔ Rust در دسترس نیست؛ از سرور اصلی استفاده کنید",
                "details": {},
            },
            "request_id": Uuid::new_v4().to_string(),
        })),
    ))
}
