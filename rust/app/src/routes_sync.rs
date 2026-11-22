//! Sync routes — mirrors `app/modules/sync/router.py` + `service.py`
//! (Wave 7).
//!
//! Offline-sync engine: idempotent push batches (replay on
//! `client_op_id` returns the stored result), pull by `seq` cursor,
//! per-device cursors, optimistic-version note conflicts, tombstones
//! for deletes, and the scored offline-pack.
//!
//! Merge policy (ported verbatim from the Python service):
//! - `user_progress` / `dhikr_daily_count`: deterministic max-wins,
//!   never a conflict.
//! - `user_ayah_note`: optimistic versioning; `base_version` below
//!   the current version with different content → a conflict row
//!   keeps both versions, nothing is silently discarded.
//! - Deletes are permanent tombstones; pull publishes them too.
//!
//! `sync_changelog.seq` is still allocated as `MAX(seq)+1` by the
//! shared `sync::record_entity_change` helper (ADR-035) — the race
//! is not closed here; see SYN-01 in the wave report.

use std::collections::HashMap;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Json,
};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::emit_event,
    authn::authenticate,
    db::{utc_now, Arg, Db, IntoArg},
    errors::{ApiError, ApiResult},
    sync::record_entity_change,
    validate::{
        as_int, opt_dict, opt_str_raw_max, parse_body, parse_path_uuid, parse_query, query_int,
        req_str,
    },
    AppState,
};

const SYNCABLE_TYPES: [&str; 3] = ["user_ayah_note", "user_progress", "dhikr_daily_count"];

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Python `str(value)` — strings as-is, `True`/`False`, `None`,
/// numbers formatted (see `routes_voice::py_str`).
fn py_str(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Bool(b) => {
            if *b {
                "True".to_string()
            } else {
                "False".to_string()
            }
        }
        Value::Null => "None".to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Python `bool(value)` for a payload field: absent → `default`,
/// present (even explicit null) → truthiness.
fn py_bool_field(payload: &Value, field: &str, default: bool) -> bool {
    match payload.get(field) {
        None => default,
        Some(Value::Bool(b)) => *b,
        Some(Value::Null) => false,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// Python `int(value)` with truncation: JSON int, integral or
/// fractional float (truncated toward zero, like `int(10.5)`),
/// digit string, bool. Anything else → `None` (the caller decides
/// whether that is a rejection or — for progress/dhikr counters —
/// an unhandled `ValueError`, i.e. 500, exactly like Python).
fn py_int_trunc(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

/// FastAPI lax bool query param: true/1/yes/on/y, false/0/no/off/n
/// (case-insensitive); anything else → 422.
fn query_bool(
    params: &HashMap<String, String>,
    name: &str,
    default: bool,
) -> Result<bool, ApiError> {
    match params.get(name) {
        None => Ok(default),
        Some(v) => match v.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" | "y" => Ok(true),
            "false" | "0" | "no" | "off" | "n" => Ok(false),
            _ => Err(validation_err(name, "must be a boolean")),
        },
    }
}

// --- request shapes ----------------------------------------------------------

struct SyncOpIn {
    client_op_id: String,
    entity_type: String,
    entity_id: String,
    op_type: String,
    payload: Value,
    base_version: i64,
}

fn parse_sync_op(v: &Value) -> Result<SyncOpIn, ApiError> {
    if !v.is_object() {
        return Err(validation_err("ops", "each op must be an object"));
    }
    let client_op_id = req_str(v, "client_op_id", 1, 128)?;
    let entity_type = req_str(v, "entity_type", 1, 32)?;
    if !SYNCABLE_TYPES.contains(&entity_type.as_str()) {
        return Err(validation_err("entity_type", "not a syncable entity type"));
    }
    let entity_id = req_str(v, "entity_id", 1, 128)?;
    let op_type = req_str(v, "op_type", 1, 16)?;
    if op_type != "upsert" && op_type != "delete" {
        return Err(validation_err("op_type", "must be upsert or delete"));
    }
    let payload = opt_dict(v, "payload")?.unwrap_or_else(|| json!({}));
    let base_version = match v.get("base_version") {
        None | Some(Value::Null) => 0,
        Some(bv) => match as_int(bv).filter(|n| *n >= 0) {
            Some(n) => n,
            None => return Err(validation_err("base_version", "must be an integer >= 0")),
        },
    };
    Ok(SyncOpIn {
        client_op_id,
        entity_type,
        entity_id,
        op_type,
        payload,
        base_version,
    })
}

fn op_result(
    client_op_id: &str,
    status: &str,
    entity_type: &str,
    entity_id: &str,
    version: Option<i64>,
    conflict_id: Option<Uuid>,
    detail: Option<&str>,
) -> Value {
    json!({
        "client_op_id": client_op_id,
        "status": status,
        "entity_type": entity_type,
        "entity_id": entity_id,
        "version": version,
        "conflict_id": conflict_id.map(|c| c.to_string()),
        "detail": detail,
    })
}

struct Outcome {
    status: &'static str,
    version: Option<i64>,
    conflict_id: Option<Uuid>,
    detail: Option<String>,
}

fn applied(version: i64) -> Outcome {
    Outcome {
        status: "applied",
        version: Some(version),
        conflict_id: None,
        detail: None,
    }
}

fn rejected(detail: &str) -> Outcome {
    Outcome {
        status: "rejected",
        version: None,
        conflict_id: None,
        detail: Some(detail.to_string()),
    }
}

// --- push --------------------------------------------------------------------

/// `POST /api/v1/sync/push` — idempotent batch. A `client_op_id`
/// owned by another user aborts the whole batch with 409
/// `sync_op_id_conflict` (pre-checked up front: Python flushes the
/// batch then rolls it back on the same error, so nothing persists
/// there either).
pub async fn push_batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let device_id = req_str(&parsed, "device_id", 1, 64)?;
    let ops_raw = parsed
        .get("ops")
        .and_then(|v| v.as_array())
        .ok_or_else(|| validation_err("ops", "must be a list of operations"))?;
    if ops_raw.is_empty() || ops_raw.len() > 100 {
        return Err(validation_err("ops", "must contain 1 to 100 operations"));
    }
    let mut ops = Vec::with_capacity(ops_raw.len());
    for raw in ops_raw {
        ops.push(parse_sync_op(raw)?);
    }
    // Atomicity pre-check for the 409 case (see doc comment above).
    for op in &ops {
        if let Some(row) = db
            .fetch_optional(
                "SELECT user_id FROM sync_operations WHERE client_op_id = ?",
                &[(op.client_op_id.clone()).into_arg()],
            )
            .await?
        {
            let owner: Uuid = row.try_get("user_id")?;
            if owner != ctx.user.id {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "sync_op_id_conflict",
                    "شناسهٔ عملیات متعلق به کاربر دیگری است",
                ));
            }
        }
    }
    let mut results = Vec::with_capacity(ops.len());
    for op in &ops {
        results.push(apply_op(db, &ctx.user.id, &device_id, op).await?);
    }
    Ok(Json(json!({ "device_id": device_id, "results": results })))
}

async fn apply_op(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
) -> Result<Value, ApiError> {
    // Idempotency: replay returns the stored result verbatim.
    if let Some(row) = db
        .fetch_optional(
            "SELECT result FROM sync_operations WHERE client_op_id = ?",
            &[(op.client_op_id.clone()).into_arg()],
        )
        .await?
    {
        let stored: Option<Value> = row.try_get("result")?;
        return Ok(stored.unwrap_or_else(|| json!({})));
    }
    let record_id = Uuid::new_v4();
    let now = utc_now();
    db.execute(
        "INSERT INTO sync_operations (id, client_op_id, user_id, device_id, entity_type, \
         entity_id, op_type, payload, base_version, status, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)",
        &[
            (record_id).into_arg(),
            (op.client_op_id.clone()).into_arg(),
            (*user_id).into_arg(),
            (device_id).into_arg(),
            (op.entity_type.clone()).into_arg(),
            (op.entity_id.clone()).into_arg(),
            (op.op_type.clone()).into_arg(),
            (op.payload.clone()).into_arg(),
            (op.base_version).into_arg(),
            (now).into_arg(),
        ],
    )
    .await?;
    let outcome = match op.op_type.as_str() {
        "delete" => apply_delete(db, user_id, device_id, op).await?,
        _ => apply_upsert(db, user_id, device_id, op).await?,
    };
    let result = op_result(
        &op.client_op_id,
        outcome.status,
        &op.entity_type,
        &op.entity_id,
        outcome.version,
        outcome.conflict_id,
        outcome.detail.as_deref(),
    );
    db.execute(
        "UPDATE sync_operations SET status = ?, result = ?, applied_at = ? WHERE id = ?",
        &[
            (outcome.status).into_arg(),
            (result.clone()).into_arg(),
            (utc_now()).into_arg(),
            (record_id).into_arg(),
        ],
    )
    .await?;
    Ok(result)
}

async fn get_tombstone(
    db: &Db,
    user_id: &Uuid,
    entity_type: &str,
    entity_id: &str,
) -> Result<Option<i64>, ApiError> {
    let row = db
        .fetch_optional(
            "SELECT version FROM sync_tombstones \
             WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
            &[
                (*user_id).into_arg(),
                (entity_type).into_arg(),
                (entity_id).into_arg(),
            ],
        )
        .await?;
    match row {
        None => Ok(None),
        Some(r) => {
            let v: i32 = r.try_get("version")?;
            Ok(Some(i64::from(v)))
        }
    }
}

async fn apply_upsert(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
) -> Result<Outcome, ApiError> {
    let base_version = op.base_version;
    if let Some(tomb_version) = get_tombstone(db, user_id, &op.entity_type, &op.entity_id).await? {
        if base_version <= tomb_version {
            if op.entity_type == "user_ayah_note" {
                return note_conflict(
                    db,
                    user_id,
                    device_id,
                    op,
                    None,
                    None,
                    "upsert_on_tombstone",
                )
                .await;
            }
            return Ok(rejected("tombstoned"));
        }
        // Client is newer than the delete → resurrect (tombstone goes).
        db.execute(
            "DELETE FROM sync_tombstones WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
            &[
                (*user_id).into_arg(),
                (op.entity_type.clone()).into_arg(),
                (op.entity_id.clone()).into_arg(),
            ],
        )
        .await?;
    }
    match op.entity_type.as_str() {
        "user_ayah_note" => upsert_note(db, user_id, device_id, op).await,
        "user_progress" => upsert_progress(db, user_id, device_id, op).await,
        "dhikr_daily_count" => upsert_dhikr(db, user_id, device_id, op).await,
        _ => Ok(rejected("entity_type_not_syncable")),
    }
}

fn parse_note_id(entity_id: &str) -> Option<Uuid> {
    entity_id.parse::<Uuid>().ok()
}

fn note_snapshot(
    id: Uuid,
    surah: i64,
    ayah: i64,
    note_text: &str,
    is_private: bool,
    version: i64,
) -> Value {
    json!({
        "id": id.to_string(),
        "surah": surah,
        "ayah": ayah,
        "note_text": note_text,
        "is_private": is_private,
        "version": version,
    })
}

fn note_content_equal(incoming_text: &str, incoming_private: bool, snapshot: &Value) -> bool {
    let snap_text = snapshot
        .get("note_text")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let snap_private = snapshot
        .get("is_private")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    incoming_text.trim() == snap_text.trim() && incoming_private == snap_private
}

async fn upsert_note(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
) -> Result<Outcome, ApiError> {
    let Some(note_id) = parse_note_id(&op.entity_id) else {
        return Ok(rejected("invalid_note_id"));
    };
    let row = db
        .fetch_optional(
            "SELECT user_id, surah, ayah, note_text, is_private, version FROM user_ayah_notes WHERE id = ?",
            &[(note_id).into_arg()],
        )
        .await?;
    if let Some(r) = &row {
        let owner: Uuid = r.try_get("user_id")?;
        if owner != *user_id {
            return Ok(rejected("not_owner"));
        }
    }
    let payload = &op.payload;
    let base_version = op.base_version;
    let Some(note_row) = row else {
        // Offline create — the client supplied its own UUID.
        let surah = payload.get("surah").and_then(py_int_trunc);
        let ayah = payload.get("ayah").and_then(py_int_trunc);
        let note_text = match payload.get("note_text") {
            Some(v) => py_str(v).trim().to_string(),
            None => return Ok(rejected("missing_note_fields")),
        };
        let (Some(surah), Some(ayah)) = (surah, ayah) else {
            return Ok(rejected("missing_note_fields"));
        };
        if note_text.is_empty() {
            return Ok(rejected("empty_note_text"));
        }
        let is_private = py_bool_field(payload, "is_private", true);
        let now = utc_now();
        db.execute(
            "INSERT INTO user_ayah_notes (id, user_id, surah, ayah, note_text, is_private, \
             version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?)",
            &[
                (note_id).into_arg(),
                (*user_id).into_arg(),
                (surah).into_arg(),
                (ayah).into_arg(),
                (note_text.clone()).into_arg(),
                (is_private).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
        record_entity_change(
            db,
            *user_id,
            device_id,
            "user_ayah_note",
            &note_id.to_string(),
            "upsert",
            Some(note_snapshot(
                note_id, surah, ayah, &note_text, is_private, 1,
            )),
            1,
        )
        .await?;
        return Ok(applied(1));
    };
    let surah: i32 = note_row.try_get("surah")?;
    let ayah: i32 = note_row.try_get("ayah")?;
    let cur_text: String = note_row.try_get("note_text")?;
    let cur_private: bool = note_row.try_get("is_private")?;
    let cur_version: i32 = note_row.try_get("version")?;
    let current_version = i64::from(cur_version);
    let incoming_text = match payload.get("note_text") {
        Some(v) => py_str(v),
        None => cur_text.clone(),
    };
    let incoming_private = py_bool_field(payload, "is_private", cur_private);
    if base_version >= current_version {
        // No concurrent edit (or the client is ahead — anomalous, but
        // the content is explicitly the user's, so it applies).
        let new_version = current_version.max(base_version) + 1;
        let now = utc_now();
        db.execute(
            "UPDATE user_ayah_notes SET note_text = ?, is_private = ?, version = ?, \
             updated_at = ? WHERE id = ?",
            &[
                (incoming_text.trim().to_string()).into_arg(),
                (incoming_private).into_arg(),
                (new_version).into_arg(),
                (now).into_arg(),
                (note_id).into_arg(),
            ],
        )
        .await?;
        record_entity_change(
            db,
            *user_id,
            device_id,
            "user_ayah_note",
            &note_id.to_string(),
            "upsert",
            Some(note_snapshot(
                note_id,
                i64::from(surah),
                i64::from(ayah),
                incoming_text.trim(),
                incoming_private,
                new_version,
            )),
            new_version,
        )
        .await?;
        return Ok(applied(new_version));
    }
    // base_version < current_version → concurrent edit?
    let snapshot = note_snapshot(
        note_id,
        i64::from(surah),
        i64::from(ayah),
        &cur_text,
        cur_private,
        current_version,
    );
    if note_content_equal(&incoming_text, incoming_private, &snapshot) {
        let mut out = applied(current_version);
        out.detail = Some("already_in_sync".to_string());
        return Ok(out);
    }
    note_conflict(
        db,
        user_id,
        device_id,
        op,
        Some(snapshot),
        Some(current_version),
        "concurrent_edit",
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn note_conflict(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
    remote_payload: Option<Value>,
    remote_version: Option<i64>,
    detail: &str,
) -> Result<Outcome, ApiError> {
    let conflict_id = Uuid::new_v4();
    let payload = &op.payload;
    let local_payload = json!({
        "note_text": payload.get("note_text").map(py_str).unwrap_or_default(),
        "is_private": py_bool_field(payload, "is_private", true),
        "surah": payload.get("surah").cloned().unwrap_or(Value::Null),
        "ayah": payload.get("ayah").cloned().unwrap_or(Value::Null),
        "device_id": device_id,
        "client_op_id": op.client_op_id,
    });
    db.execute(
        "INSERT INTO conflict_records (id, user_id, entity_type, entity_id, local_payload, \
         remote_payload, local_version, remote_version, resolution, created_at) \
         VALUES (?, ?, 'user_ayah_note', ?, ?, ?, ?, ?, 'pending', ?)",
        &[
            (conflict_id).into_arg(),
            (*user_id).into_arg(),
            (op.entity_id.clone()).into_arg(),
            (local_payload).into_arg(),
            (remote_payload).into_arg(),
            (op.base_version).into_arg(),
            (remote_version).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "sync_conflict",
        &conflict_id.to_string(),
        "sync.conflict_detected",
        json!({
            "user_id": user_id.to_string(),
            "entity_type": "user_ayah_note",
            "entity_id": op.entity_id,
            "local_version": op.base_version,
            "remote_version": remote_version,
            "device_id": device_id,
        }),
    )
    .await?;
    Ok(Outcome {
        status: "conflicted",
        version: remote_version,
        conflict_id: Some(conflict_id),
        detail: Some(detail.to_string()),
    })
}

const STATUS_RANK: [(&str, i64); 3] = [("not_started", 0), ("in_progress", 1), ("completed", 2)];

fn status_rank(status: &str) -> Option<i64> {
    STATUS_RANK
        .iter()
        .find(|(s, _)| *s == status)
        .map(|(_, r)| *r)
}

async fn upsert_progress(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
) -> Result<Outcome, ApiError> {
    let Some(lesson_id) = parse_note_id(&op.entity_id) else {
        return Ok(rejected("invalid_lesson_id"));
    };
    let lesson = db
        .fetch_optional(
            "SELECT id FROM lessons WHERE id = ?",
            &[(lesson_id).into_arg()],
        )
        .await?;
    if lesson.is_none() {
        return Ok(rejected("lesson_not_found"));
    }
    let payload = &op.payload;
    let incoming_status = payload
        .get("status")
        .map(py_str)
        .unwrap_or_else(|| "in_progress".to_string());
    let Some(incoming_rank) = status_rank(&incoming_status) else {
        return Ok(rejected("invalid_status"));
    };
    // Python: `int(payload.get("position_seconds", 0) or 0)` — an
    // unparseable value raises ValueError → 500, mirrored here.
    let incoming_pos = match payload.get("position_seconds") {
        None | Some(Value::Null) => 0,
        Some(v) => py_int_trunc(v).ok_or_else(ApiError::internal)?,
    }
    .max(0);
    let now = utc_now();
    let prog = db
        .fetch_optional(
            "SELECT id, status, position_seconds, completed_at, version FROM user_progress \
             WHERE user_id = ? AND lesson_id = ?",
            &[(*user_id).into_arg(), (lesson_id).into_arg()],
        )
        .await?;
    let version: i64;
    match prog {
        None => {
            let completed_at = if incoming_status == "completed" {
                Some(now)
            } else {
                None
            };
            db.execute(
                "INSERT INTO user_progress (id, user_id, lesson_id, status, position_seconds, \
                 completed_at, updated_at, version) VALUES (?, ?, ?, ?, ?, ?, ?, 1)",
                &[
                    (Uuid::new_v4()).into_arg(),
                    (*user_id).into_arg(),
                    (lesson_id).into_arg(),
                    (incoming_status.clone()).into_arg(),
                    (incoming_pos).into_arg(),
                    (completed_at).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
            record_entity_change(
                db,
                *user_id,
                device_id,
                "user_progress",
                &lesson_id.to_string(),
                "upsert",
                Some(json!({
                    "lesson_id": lesson_id.to_string(),
                    "status": incoming_status,
                    "position_seconds": incoming_pos,
                    "version": 1,
                })),
                1,
            )
            .await?;
            return Ok(applied(1));
        }
        Some(row) => {
            let prog_id: Uuid = row.try_get("id")?;
            let cur_status: String = row.try_get("status")?;
            let cur_pos: i32 = row.try_get("position_seconds")?;
            let cur_completed: Option<DateTime<Utc>> = row.try_get("completed_at")?;
            let cur_version: i32 = row.try_get("version")?;
            let new_pos = i64::from(cur_pos).max(incoming_pos);
            let cur_rank = status_rank(&cur_status).unwrap_or(0);
            let mut new_status = cur_status.clone();
            let mut new_completed = cur_completed;
            if incoming_rank > cur_rank {
                new_status = incoming_status.clone();
                if incoming_status == "completed" && new_completed.is_none() {
                    new_completed = Some(now);
                }
            }
            if new_status != "completed" {
                new_completed = None;
            }
            let new_version = i64::from(cur_version) + 1;
            db.execute(
                "UPDATE user_progress SET position_seconds = ?, status = ?, completed_at = ?, \
                 version = ?, updated_at = ? WHERE id = ?",
                &[
                    (new_pos).into_arg(),
                    (new_status.clone()).into_arg(),
                    (new_completed).into_arg(),
                    (new_version).into_arg(),
                    (now).into_arg(),
                    (prog_id).into_arg(),
                ],
            )
            .await?;
            version = new_version;
            record_entity_change(
                db,
                *user_id,
                device_id,
                "user_progress",
                &lesson_id.to_string(),
                "upsert",
                Some(json!({
                    "lesson_id": lesson_id.to_string(),
                    "status": new_status,
                    "position_seconds": new_pos,
                    "version": new_version,
                })),
                new_version,
            )
            .await?;
        }
    }
    Ok(applied(version))
}

/// Parse a dhikr entity id `"{date_iso}:{dhikr_text}"`.
fn parse_dhikr_id(entity_id: &str) -> Option<(NaiveDate, String)> {
    let (day_s, text) = entity_id.split_once(':')?;
    let day = NaiveDate::parse_from_str(day_s, "%Y-%m-%d").ok()?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some((day, text))
}

async fn upsert_dhikr(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
) -> Result<Outcome, ApiError> {
    let Some((day, text)) = parse_dhikr_id(&op.entity_id) else {
        return Ok(rejected("invalid_dhikr_id"));
    };
    let payload = &op.payload;
    if let Some(p_day) = payload.get("date").and_then(|v| v.as_str()) {
        if p_day != day.format("%Y-%m-%d").to_string() {
            return Ok(rejected("dhikr_id_payload_mismatch"));
        }
    }
    let p_text = payload.get("dhikr_text").map(py_str).unwrap_or_default();
    let p_text = p_text.trim();
    if !p_text.is_empty() && p_text != text {
        return Ok(rejected("dhikr_id_payload_mismatch"));
    }
    // Python: `int(payload.get("total_count", 0) or 0)` — unparseable
    // raises ValueError → 500, mirrored here.
    let incoming = match payload.get("total_count") {
        None | Some(Value::Null) => 0,
        Some(v) => py_int_trunc(v).ok_or_else(ApiError::internal)?,
    }
    .max(0);
    let now = utc_now();
    let row = db
        .fetch_optional(
            "SELECT id, total_count, version FROM dhikr_daily_counts \
             WHERE user_id = ? AND date = ? AND dhikr_text = ?",
            &[
                (*user_id).into_arg(),
                (day).into_arg(),
                (text.clone()).into_arg(),
            ],
        )
        .await?;
    let version: i64;
    let total: i64;
    match row {
        None => {
            db.execute(
                "INSERT INTO dhikr_daily_counts (id, user_id, date, dhikr_text, total_count, \
                 version, updated_at) VALUES (?, ?, ?, ?, ?, 1, ?)",
                &[
                    (Uuid::new_v4()).into_arg(),
                    (*user_id).into_arg(),
                    (day).into_arg(),
                    (text.clone()).into_arg(),
                    (incoming).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
            version = 1;
            total = incoming;
        }
        Some(r) => {
            let row_id: Uuid = r.try_get("id")?;
            let cur_total: i32 = r.try_get("total_count")?;
            let cur_version: i32 = r.try_get("version")?;
            total = i64::from(cur_total).max(incoming);
            version = i64::from(cur_version) + 1;
            db.execute(
                "UPDATE dhikr_daily_counts SET total_count = ?, version = ?, updated_at = ? \
                 WHERE id = ?",
                &[
                    (total).into_arg(),
                    (version).into_arg(),
                    (now).into_arg(),
                    (row_id).into_arg(),
                ],
            )
            .await?;
        }
    }
    record_entity_change(
        db,
        *user_id,
        device_id,
        "dhikr_daily_count",
        &op.entity_id,
        "upsert",
        Some(json!({
            "date": day.format("%Y-%m-%d").to_string(),
            "dhikr_text": text,
            "total_count": total,
            "version": version,
        })),
        version,
    )
    .await?;
    Ok(applied(version))
}

async fn apply_delete(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    op: &SyncOpIn,
) -> Result<Outcome, ApiError> {
    let now = utc_now();
    let mut entity_version = op.base_version;
    if let Some(deleted_version) = delete_entity_row(db, user_id, op).await? {
        entity_version = entity_version.max(deleted_version);
    }
    // Tombstone upsert (version = max), exactly like Python.
    match get_tombstone(db, user_id, &op.entity_type, &op.entity_id).await? {
        None => {
            db.execute(
                "INSERT INTO sync_tombstones (id, user_id, entity_type, entity_id, deleted_by, \
                 deleted_at, version) VALUES (?, ?, ?, ?, ?, ?, ?)",
                &[
                    (Uuid::new_v4()).into_arg(),
                    (*user_id).into_arg(),
                    (op.entity_type.clone()).into_arg(),
                    (op.entity_id.clone()).into_arg(),
                    (*user_id).into_arg(),
                    (now).into_arg(),
                    (entity_version).into_arg(),
                ],
            )
            .await?;
        }
        Some(old) => {
            let new_version = old.max(entity_version);
            db.execute(
                "UPDATE sync_tombstones SET version = ?, deleted_at = ?, deleted_by = ? \
                 WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
                &[
                    (new_version).into_arg(),
                    (now).into_arg(),
                    (*user_id).into_arg(),
                    (*user_id).into_arg(),
                    (op.entity_type.clone()).into_arg(),
                    (op.entity_id.clone()).into_arg(),
                ],
            )
            .await?;
            entity_version = new_version;
        }
    }
    record_entity_change(
        db,
        *user_id,
        device_id,
        &op.entity_type,
        &op.entity_id,
        "delete",
        None,
        entity_version,
    )
    .await?;
    Ok(applied(entity_version))
}

/// Physically delete the user's own entity row; returns the deleted
/// row's version, or `None` when there is nothing (or nothing owned)
/// to delete.
async fn delete_entity_row(
    db: &Db,
    user_id: &Uuid,
    op: &SyncOpIn,
) -> Result<Option<i64>, ApiError> {
    match op.entity_type.as_str() {
        "user_ayah_note" => {
            let Some(note_id) = parse_note_id(&op.entity_id) else {
                return Ok(None);
            };
            let row = db
                .fetch_optional(
                    "SELECT user_id, version FROM user_ayah_notes WHERE id = ?",
                    &[(note_id).into_arg()],
                )
                .await?;
            let Some(r) = row else { return Ok(None) };
            let owner: Uuid = r.try_get("user_id")?;
            if owner != *user_id {
                return Ok(None);
            }
            let version: i32 = r.try_get("version")?;
            db.execute(
                "DELETE FROM user_ayah_notes WHERE id = ?",
                &[(note_id).into_arg()],
            )
            .await?;
            Ok(Some(i64::from(version)))
        }
        "user_progress" => {
            let Some(lesson_id) = parse_note_id(&op.entity_id) else {
                return Ok(None);
            };
            let row = db
                .fetch_optional(
                    "SELECT id, version FROM user_progress WHERE user_id = ? AND lesson_id = ?",
                    &[(*user_id).into_arg(), (lesson_id).into_arg()],
                )
                .await?;
            let Some(r) = row else { return Ok(None) };
            let row_id: Uuid = r.try_get("id")?;
            let version: i32 = r.try_get("version")?;
            db.execute(
                "DELETE FROM user_progress WHERE id = ?",
                &[(row_id).into_arg()],
            )
            .await?;
            Ok(Some(i64::from(version)))
        }
        "dhikr_daily_count" => {
            let Some((day, text)) = parse_dhikr_id(&op.entity_id) else {
                return Ok(None);
            };
            let row = db
                .fetch_optional(
                    "SELECT id, version FROM dhikr_daily_counts \
                     WHERE user_id = ? AND date = ? AND dhikr_text = ?",
                    &[
                        (*user_id).into_arg(),
                        (day).into_arg(),
                        (text.clone()).into_arg(),
                    ],
                )
                .await?;
            let Some(r) = row else { return Ok(None) };
            let row_id: Uuid = r.try_get("id")?;
            let version: i32 = r.try_get("version")?;
            db.execute(
                "DELETE FROM dhikr_daily_counts WHERE id = ?",
                &[(row_id).into_arg()],
            )
            .await?;
            Ok(Some(i64::from(version)))
        }
        _ => Ok(None),
    }
}

// --- pull / cursor -----------------------------------------------------------

/// `GET /api/v1/sync/pull` — changes with `seq > cursor` for this
/// user, in `seq` order; the device cursor advances to the highest
/// returned `seq` (only when at least one change is returned).
pub async fn pull_changes(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let params = parse_query(&uri);
    let cursor = query_int(&params, "cursor", 0, 0, i64::MAX)?;
    let device_id = match params.get("device_id") {
        Some(d) if !d.is_empty() && d.chars().count() <= 64 => d.clone(),
        _ => return Err(validation_err("device_id", "field is required")),
    };
    let limit = query_int(&params, "limit", 200, 1, 500)?;
    let db = state.db_or_500()?;
    let rows = db
        .fetch_all(
            "SELECT seq, entity_type, entity_id, op_type, payload, version, device_id, \
             created_at FROM sync_changelog WHERE user_id = ? AND seq > ? \
             ORDER BY seq ASC LIMIT ?",
            &[
                (ctx.user.id).into_arg(),
                (cursor).into_arg(),
                (limit + 1).into_arg(),
            ],
        )
        .await?;
    let mut changes: Vec<Value> = Vec::new();
    let mut new_cursor = cursor;
    for row in rows.iter().take(limit as usize) {
        let seq: i32 = row.try_get("seq")?;
        let entity_type: String = row.try_get("entity_type")?;
        let entity_id: String = row.try_get("entity_id")?;
        let op_type: String = row.try_get("op_type")?;
        let payload: Option<Value> = row.try_get("payload")?;
        let version: i32 = row.try_get("version")?;
        let change_device_id: Option<String> = row.try_get("device_id")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        new_cursor = new_cursor.max(i64::from(seq));
        changes.push(json!({
            "seq": seq,
            "entity_type": entity_type,
            "entity_id": entity_id,
            "op_type": op_type,
            "payload": payload,
            "version": version,
            "device_id": change_device_id,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    if !changes.is_empty() {
        advance_cursor(db, &ctx.user.id, &device_id, new_cursor).await?;
    }
    Ok(Json(json!({
        "device_id": device_id,
        "changes": changes,
        "new_cursor": new_cursor,
    })))
}

async fn advance_cursor(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    seq: i64,
) -> Result<(), ApiError> {
    let row = db
        .fetch_optional(
            "SELECT id, cursor_seq FROM device_cursors WHERE user_id = ? AND device_id = ?",
            &[(*user_id).into_arg(), (device_id).into_arg()],
        )
        .await?;
    let now = utc_now();
    match row {
        None => {
            db.execute(
                "INSERT INTO device_cursors (id, user_id, device_id, cursor_seq, updated_at) \
                 VALUES (?, ?, ?, ?, ?)",
                &[
                    (Uuid::new_v4()).into_arg(),
                    (*user_id).into_arg(),
                    (device_id).into_arg(),
                    (seq).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
        }
        Some(r) => {
            let cur: i64 = r.try_get("cursor_seq")?;
            if seq > cur {
                db.execute(
                    "UPDATE device_cursors SET cursor_seq = ?, updated_at = ? \
                     WHERE user_id = ? AND device_id = ?",
                    &[
                        (seq).into_arg(),
                        (now).into_arg(),
                        (*user_id).into_arg(),
                        (device_id).into_arg(),
                    ],
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// `GET /api/v1/sync/cursor` — the current pull cursor for a device
/// (0 when the device never synced).
pub async fn get_cursor(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let params = parse_query(&uri);
    let device_id = match params.get("device_id") {
        Some(d) if !d.is_empty() && d.chars().count() <= 64 => d.clone(),
        _ => return Err(validation_err("device_id", "field is required")),
    };
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT cursor_seq FROM device_cursors WHERE user_id = ? AND device_id = ?",
            &[(ctx.user.id).into_arg(), (device_id.clone()).into_arg()],
        )
        .await?;
    let seq: i64 = match row {
        Some(r) => r.try_get("cursor_seq")?,
        None => 0,
    };
    Ok(Json(json!({ "device_id": device_id, "cursor_seq": seq })))
}

// --- devices -----------------------------------------------------------------

/// `GET /api/v1/sync/devices` — this user's devices: the union of
/// `device_cursors` (synced) and `user_devices` (registered with a
/// token), newest `updated_at` first.
pub async fn list_devices(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let db = state.db_or_500()?;
    let cursor_rows = db
        .fetch_all(
            "SELECT user_id, device_id, cursor_seq, updated_at FROM device_cursors \
             WHERE user_id = ? ORDER BY updated_at DESC LIMIT ?",
            &[(ctx.user.id).into_arg(), (limit).into_arg()],
        )
        .await?;
    let dev_rows = db
        .fetch_all(
            "SELECT user_id, device_id, platform, last_seen FROM user_devices \
             WHERE user_id = ? LIMIT ?",
            &[(ctx.user.id).into_arg(), (limit).into_arg()],
        )
        .await?;
    // Merge keyed by (user_id, device_id), cursor rows first.
    // `(user_id, device_id, cursor_seq, updated_at, platform)`.
    type MergedDevice = (String, String, i64, Option<DateTime<Utc>>, Option<String>);
    let mut merged: Vec<MergedDevice> = Vec::new();
    for row in &cursor_rows {
        let uid: Uuid = row.try_get("user_id")?;
        let did: String = row.try_get("device_id")?;
        let seq: i64 = row.try_get("cursor_seq")?;
        let updated: DateTime<Utc> = row.try_get("updated_at")?;
        merged.push((uid.to_string(), did, seq, Some(updated), None));
    }
    for row in &dev_rows {
        let uid: Uuid = row.try_get("user_id")?;
        let did: String = row.try_get("device_id")?;
        let platform: Option<String> = row.try_get("platform")?;
        let last_seen: DateTime<Utc> = row.try_get("last_seen")?;
        let key = (uid.to_string(), did.clone());
        match merged
            .iter_mut()
            .find(|(u, d, _, _, _)| (u.as_str(), d.as_str()) == (key.0.as_str(), key.1.as_str()))
        {
            Some(entry) => {
                entry.4 = platform;
                if entry.3.is_none_or(|t| last_seen > t) {
                    entry.3 = Some(last_seen);
                }
            }
            None => merged.push((key.0, key.1, 0, Some(last_seen), platform)),
        }
    }
    // Stable sort by updated_at desc (None sorts last).
    merged.sort_by(|a, b| {
        let ta = a.3.unwrap_or(DateTime::<Utc>::MIN_UTC);
        let tb = b.3.unwrap_or(DateTime::<Utc>::MIN_UTC);
        tb.cmp(&ta)
    });
    merged.truncate(limit as usize);
    let items: Vec<Value> = merged
        .iter()
        .map(|(uid, did, seq, updated, platform)| {
            json!({
                "user_id": uid,
                "device_id": did,
                "cursor_seq": seq,
                "updated_at": updated.map(|t| t.to_rfc3339()),
                "platform": platform,
            })
        })
        .collect();
    let total = items.len();
    Ok(Json(json!({ "items": items, "total": total })))
}

// --- conflicts ---------------------------------------------------------------

/// `GET /api/v1/sync/conflicts` — the user's version conflicts,
/// newest first; `total` counts all matching rows (pending or all).
pub async fn list_conflicts(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let params = parse_query(&uri);
    let only_pending = query_bool(&params, "only_pending", true)?;
    let limit = query_int(&params, "limit", 50, 1, 100)?;
    let db = state.db_or_500()?;
    let mut args: Vec<Arg> = vec![(ctx.user.id).into_arg()];
    let mut count_sql =
        "SELECT COUNT(*) AS total FROM conflict_records WHERE user_id = ?".to_string();
    let mut list_sql = "SELECT id, entity_type, entity_id, local_payload, remote_payload, \
         local_version, remote_version, resolution, created_at, resolved_at, resolution_note \
         FROM conflict_records WHERE user_id = ?"
        .to_string();
    if only_pending {
        count_sql.push_str(" AND resolution = 'pending'");
        list_sql.push_str(" AND resolution = 'pending'");
    }
    let total: i64 = db.fetch_one(&count_sql, &args).await?.try_get("total")?;
    list_sql.push_str(" ORDER BY created_at DESC LIMIT ?");
    args.push((limit).into_arg());
    let rows = db.fetch_all(&list_sql, &args).await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|row| {
            let id: Uuid = row.try_get("id").unwrap_or_default();
            let entity_type: String = row.try_get("entity_type").unwrap_or_default();
            let entity_id: String = row.try_get("entity_id").unwrap_or_default();
            let local_payload: Option<Value> = row.try_get("local_payload").ok().flatten();
            let remote_payload: Option<Value> = row.try_get("remote_payload").ok().flatten();
            let local_version: Option<i32> = row.try_get("local_version").ok().flatten();
            let remote_version: Option<i32> = row.try_get("remote_version").ok().flatten();
            let resolution: String = row.try_get("resolution").unwrap_or_default();
            let created_at: DateTime<Utc> = row
                .try_get("created_at")
                .unwrap_or(DateTime::<Utc>::MIN_UTC);
            let resolved_at: Option<DateTime<Utc>> = row.try_get("resolved_at").ok().flatten();
            let resolution_note: Option<String> = row.try_get("resolution_note").ok().flatten();
            json!({
                "id": id.to_string(),
                "entity_type": entity_type,
                "entity_id": entity_id,
                "local_payload": local_payload,
                "remote_payload": remote_payload,
                "local_version": local_version,
                "remote_version": remote_version,
                "resolution": resolution,
                "created_at": created_at.to_rfc3339(),
                "resolved_at": resolved_at.map(|t| t.to_rfc3339()),
                "resolution_note": resolution_note,
            })
        })
        .collect();
    Ok(Json(json!({ "items": items, "total": total })))
}

/// `POST /api/v1/sync/conflicts/{conflict_id}/resolve` — keep_local
/// (the device version wins) | keep_remote (the server version wins)
/// | merged (the caller's merged payload).
pub async fn resolve_conflict(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    axum::extract::Path(conflict_id): axum::extract::Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let id = parse_path_uuid(&conflict_id, "conflict_id")?;
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let parsed = parse_body(&body)?;
    let resolution = req_str(&parsed, "resolution", 1, 32)?;
    if resolution != "keep_local" && resolution != "keep_remote" && resolution != "merged" {
        return Err(validation_err(
            "resolution",
            "must be keep_local, keep_remote or merged",
        ));
    }
    let merged_payload = opt_dict(&parsed, "merged_payload")?;
    let note = opt_str_raw_max(&parsed, "note", 500)?;
    let params = parse_query(&uri);
    let device_id = params
        .get("device_id")
        .filter(|d| !d.is_empty() && d.chars().count() <= 64)
        .cloned()
        .unwrap_or_else(|| "api".to_string());
    if params.contains_key("device_id")
        && params
            .get("device_id")
            .is_some_and(|d| d.is_empty() || d.chars().count() > 64)
    {
        return Err(validation_err("device_id", "length out of range"));
    }
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            "SELECT id, user_id, entity_type, entity_id, local_payload, remote_payload, \
             local_version, remote_version, resolution, resolved_by, resolved_at, \
             resolution_note, created_at FROM conflict_records WHERE id = ?",
            &[(id).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "conflict_not_found",
            "تعارضی با این شناسه یافت نشد",
        ));
    };
    let owner: Uuid = row.try_get("user_id")?;
    if owner != ctx.user.id {
        return Err(ApiError::not_found(
            "conflict_not_found",
            "تعارضی با این شناسه یافت نشد",
        ));
    }
    let current_resolution: String = row.try_get("resolution")?;
    if current_resolution != "pending" {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "conflict_already_resolved",
            "این تعارض قبلاً حل شده است",
        ));
    }
    let entity_type: String = row.try_get("entity_type")?;
    if entity_type != "user_ayah_note" {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unsupported_entity_resolution",
            "حل تعارض فقط برای یادداشت آیه پشتیبانی می‌شود",
        ));
    }
    let entity_id: String = row.try_get("entity_id")?;
    let local_payload: Option<Value> = row.try_get("local_payload")?;
    let remote_payload: Option<Value> = row.try_get("remote_payload")?;
    let remote_version: Option<i32> = row.try_get("remote_version")?;
    let now = utc_now();
    let applied_version: Option<i64> = if resolution == "keep_remote" {
        remote_version.map(i64::from)
    } else {
        let payload = if resolution == "keep_local" {
            local_payload.clone().unwrap_or_else(|| json!({}))
        } else {
            match &merged_payload {
                Some(p) if !p.as_object().is_some_and(|o| o.is_empty()) => p.clone(),
                _ => {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "merged_payload_required",
                        "برای ادغام دستی، merged_payload الزامی است",
                    ));
                }
            }
        };
        Some(
            apply_resolution_payload(
                db,
                &ctx.user.id,
                &device_id,
                &entity_id,
                remote_payload.as_ref(),
                &payload,
            )
            .await?,
        )
    };
    db.execute(
        "UPDATE conflict_records SET resolution = ?, resolved_by = ?, resolved_at = ?, \
         resolution_note = ? WHERE id = ?",
        &[
            (resolution.clone()).into_arg(),
            (ctx.user.id).into_arg(),
            (now).into_arg(),
            (note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty())).into_arg(),
            (id).into_arg(),
        ],
    )
    .await?;
    emit_event(
        db,
        "sync_conflict",
        &id.to_string(),
        "sync.conflict_resolved",
        json!({
            "user_id": ctx.user.id.to_string(),
            "entity_type": entity_type,
            "entity_id": entity_id,
            "resolution": resolution,
            "applied_version": applied_version,
        }),
    )
    .await?;
    Ok(Json(json!({
        "id": id.to_string(),
        "resolution": resolution,
        "applied_version": applied_version,
    })))
}

/// Apply the winning payload of a note conflict onto the note itself
/// (or resurrect it after a tombstone). Returns the new version.
async fn apply_resolution_payload(
    db: &Db,
    user_id: &Uuid,
    device_id: &str,
    entity_id: &str,
    remote_payload: Option<&Value>,
    payload: &Value,
) -> Result<i64, ApiError> {
    let Some(note_id) = parse_note_id(entity_id) else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_note_id",
            "شناسهٔ یادداشت نامعتبر است",
        ));
    };
    let row = db
        .fetch_optional(
            "SELECT user_id, surah, ayah, note_text, is_private, version FROM user_ayah_notes WHERE id = ?",
            &[(note_id).into_arg()],
        )
        .await?;
    let now = utc_now();
    let version: i64;
    match row {
        None => {
            // Resurrect after a delete (upsert_on_tombstone conflict).
            let surah = payload
                .get("surah")
                .and_then(py_int_trunc)
                .filter(|v| *v != 0)
                .or_else(|| {
                    remote_payload
                        .and_then(|r| r.get("surah"))
                        .and_then(py_int_trunc)
                        .filter(|v| *v != 0)
                });
            let ayah = payload
                .get("ayah")
                .and_then(py_int_trunc)
                .filter(|v| *v != 0)
                .or_else(|| {
                    remote_payload
                        .and_then(|r| r.get("ayah"))
                        .and_then(py_int_trunc)
                        .filter(|v| *v != 0)
                });
            let note_text = payload.get("note_text").map(py_str).unwrap_or_default();
            let note_text = note_text.trim().to_string();
            if surah.is_none() || ayah.is_none() || note_text.is_empty() {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "insufficient_payload",
                    "برای احیای یادداشت، surah/ayah/note_text لازم است",
                ));
            }
            let (surah, ayah) = (surah.unwrap(), ayah.unwrap());
            let is_private = py_bool_field(payload, "is_private", true);
            db.execute(
                "INSERT INTO user_ayah_notes (id, user_id, surah, ayah, note_text, is_private, \
                 version, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?)",
                &[
                    (note_id).into_arg(),
                    (*user_id).into_arg(),
                    (surah).into_arg(),
                    (ayah).into_arg(),
                    (note_text.clone()).into_arg(),
                    (is_private).into_arg(),
                    (now).into_arg(),
                    (now).into_arg(),
                ],
            )
            .await?;
            if get_tombstone(db, user_id, "user_ayah_note", entity_id)
                .await?
                .is_some()
            {
                db.execute(
                    "DELETE FROM sync_tombstones WHERE user_id = ? AND entity_type = 'user_ayah_note' AND entity_id = ?",
                    &[(*user_id).into_arg(), (entity_id).into_arg()],
                )
                .await?;
            }
            version = 1;
            record_entity_change(
                db,
                *user_id,
                device_id,
                "user_ayah_note",
                &note_id.to_string(),
                "upsert",
                Some(note_snapshot(
                    note_id, surah, ayah, &note_text, is_private, 1,
                )),
                1,
            )
            .await?;
        }
        Some(r) => {
            let owner: Uuid = r.try_get("user_id")?;
            if owner != *user_id {
                return Err(ApiError::not_found(
                    "conflict_not_found",
                    "تعارضی با این شناسه یافت نشد",
                ));
            }
            let surah: i32 = r.try_get("surah")?;
            let ayah: i32 = r.try_get("ayah")?;
            let cur_text: String = r.try_get("note_text")?;
            let cur_private: bool = r.try_get("is_private")?;
            let cur_version: i32 = r.try_get("version")?;
            let note_text = match payload.get("note_text") {
                Some(v) => py_str(v),
                None => cur_text,
            };
            let note_text = note_text.trim().to_string();
            if note_text.is_empty() {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "empty_note_text",
                    "متن یادداشت نمی‌تواند خالی باشد",
                ));
            }
            let is_private = py_bool_field(payload, "is_private", cur_private);
            version = i64::from(cur_version) + 1;
            db.execute(
                "UPDATE user_ayah_notes SET note_text = ?, is_private = ?, version = ?, \
                 updated_at = ? WHERE id = ?",
                &[
                    (note_text.clone()).into_arg(),
                    (is_private).into_arg(),
                    (version).into_arg(),
                    (now).into_arg(),
                    (note_id).into_arg(),
                ],
            )
            .await?;
            record_entity_change(
                db,
                *user_id,
                device_id,
                "user_ayah_note",
                &note_id.to_string(),
                "upsert",
                Some(note_snapshot(
                    note_id,
                    i64::from(surah),
                    i64::from(ayah),
                    &note_text,
                    is_private,
                    version,
                )),
                version,
            )
            .await?;
        }
    }
    Ok(version)
}

// --- offline-pack ------------------------------------------------------------

const PACK_STARTED_IN_PROGRESS: i64 = 100;
const PACK_STARTED_COMPLETED: i64 = 90;
const PACK_FOLLOWED: i64 = 70;
const PACK_POPULAR: i64 = 50;
const PACK_LIMIT: i64 = 50;

/// `GET /api/v1/sync/offline-pack` — prioritised offline-download
/// list: started > followed > popular. Plain honest scoring, no ML.
pub async fn offline_pack(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/sync").await?;
    let db = state.db_or_500()?;
    // (kind, ref_id, title, score, reason)
    let mut items: Vec<(String, String, Value, i64, String)> = Vec::new();
    let mut seen: Vec<(String, String)> = Vec::new();
    // 1) Started lessons.
    let prog_rows = db
        .fetch_all(
            "SELECT up.status, l.id, l.title FROM user_progress up \
             JOIN lessons l ON l.id = up.lesson_id \
             WHERE up.user_id = ? ORDER BY up.updated_at DESC LIMIT ?",
            &[(ctx.user.id).into_arg(), (PACK_LIMIT).into_arg()],
        )
        .await?;
    for row in &prog_rows {
        let status: String = row.try_get("status")?;
        let lesson_id: Uuid = row.try_get("id")?;
        let title: Value = row.try_get("title")?;
        let key = ("lesson".to_string(), lesson_id.to_string());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        let score = if status == "in_progress" {
            PACK_STARTED_IN_PROGRESS
        } else {
            PACK_STARTED_COMPLETED
        };
        items.push((key.0, key.1, title, score, "started".to_string()));
    }
    // 2) Followed scholars' published library items.
    let followed_rows = db
        .fetch_all(
            "SELECT followed_id FROM social_follows WHERE follower_id = ?",
            &[(ctx.user.id).into_arg()],
        )
        .await?;
    let mut followed_ids: Vec<Uuid> = Vec::new();
    for row in &followed_rows {
        followed_ids.push(row.try_get("followed_id")?);
    }
    if !followed_ids.is_empty() {
        let placeholders = followed_ids
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, title FROM library_items WHERE scholar_id IN ({placeholders}) \
             AND workflow_status IN ('approved', 'published') \
             AND visibility IN ('public', 'registered') \
             ORDER BY published_at DESC NULLS LAST LIMIT ?"
        );
        let mut args: Vec<Arg> = followed_ids.iter().map(|id| id.into_arg()).collect();
        args.push((PACK_LIMIT).into_arg());
        let lib_rows = db.fetch_all(&sql, &args).await?;
        for row in &lib_rows {
            let item_id: Uuid = row.try_get("id")?;
            let title: Value = row.try_get("title")?;
            let key = ("library_item".to_string(), item_id.to_string());
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            items.push((key.0, key.1, title, PACK_FOLLOWED, "followed".to_string()));
        }
    }
    // 3) Popular — newest published items (recency as the honest
    // proxy; there is no download/view counter in this phase).
    let popular_rows = db
        .fetch_all(
            "SELECT id, title FROM library_items \
             WHERE workflow_status IN ('approved', 'published') \
             AND visibility IN ('public', 'registered') \
             ORDER BY published_at DESC NULLS LAST LIMIT ?",
            &[(PACK_LIMIT).into_arg()],
        )
        .await?;
    for row in &popular_rows {
        let item_id: Uuid = row.try_get("id")?;
        let title: Value = row.try_get("title")?;
        let key = ("library_item".to_string(), item_id.to_string());
        if seen.contains(&key) {
            continue;
        }
        seen.push(key.clone());
        items.push((key.0, key.1, title, PACK_POPULAR, "popular".to_string()));
    }
    items.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| a.0.cmp(&b.0))
            .then_with(|| a.1.cmp(&b.1))
    });
    items.truncate(PACK_LIMIT as usize);
    let out: Vec<Value> = items
        .iter()
        .map(|(kind, ref_id, title, score, reason)| {
            json!({
                "kind": kind,
                "ref_id": ref_id,
                "title": title,
                "score": score,
                "reason": reason,
            })
        })
        .collect();
    Ok(Json(json!({
        "items": out,
        "generated_at": utc_now().to_rfc3339(),
    })))
}
