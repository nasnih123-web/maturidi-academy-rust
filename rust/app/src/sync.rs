//! Minimal sync-journal helpers — mirrors the two functions the
//! Quran module calls from `app/modules/sync/service.py`:
//! `record_entity_change` (append to `sync_changelog`, the only source
//! for device pull) and the `SyncTombstone` upsert on note delete.
//!
//! The full sync module (push/pull, conflict resolution) is Wave 7;
//! these two writes are ported now so Quran mutations stay
//! sync-compatible with the Python server.

use serde_json::Value;
use uuid::Uuid;

use crate::db::{utc_now, Db, IntoArg};
use crate::errors::ApiError;

/// Append one row to `sync_changelog` (`device_id` is `"api"` for
/// server-side writes, like Python). Returns nothing — Python returns
/// the `seq`, but no caller uses it.
///
/// Note: `seq` is allocated as `MAX(seq)+1` in a single statement.
/// Production Postgres uses a real sequence via the migrations; the
/// test schema has no sequence, so this keeps both engines working.
/// A concurrent-insert race could collide on `seq` (PK violation) —
/// revisit in Wave 7 when the sync module is fully ported.
#[allow(clippy::too_many_arguments)]
pub async fn record_entity_change(
    db: &Db,
    user_id: Uuid,
    device_id: &str,
    entity_type: &str,
    entity_id: &str,
    op_type: &str,
    payload: Option<Value>,
    version: i64,
) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO sync_changelog (seq, user_id, device_id, entity_type, entity_id, \
         op_type, payload, version, created_at) \
         VALUES ((SELECT COALESCE(MAX(seq), 0) + 1 FROM sync_changelog), ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            (user_id).into_arg(),
            (device_id).into_arg(),
            (entity_type).into_arg(),
            (entity_id).into_arg(),
            (op_type).into_arg(),
            (payload).into_arg(),
            (version).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    Ok(())
}

/// Upsert a `SyncTombstone` for `(user_id, entity_type, entity_id)`:
/// insert when absent, else bump `version` to the max and refresh
/// `deleted_at` / `deleted_by` — exactly like Python's note delete.
pub async fn upsert_tombstone(
    db: &Db,
    user_id: Uuid,
    entity_type: &str,
    entity_id: &str,
    version: i64,
) -> Result<(), ApiError> {
    let existing = db
        .fetch_optional(
            "SELECT id, version FROM sync_tombstones \
             WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
            &[
                (user_id).into_arg(),
                (entity_type).into_arg(),
                (entity_id).into_arg(),
            ],
        )
        .await?;
    match existing {
        None => {
            db.execute(
                "INSERT INTO sync_tombstones (id, user_id, entity_type, entity_id, \
                 deleted_by, deleted_at, version) VALUES (?, ?, ?, ?, ?, ?, ?)",
                &[
                    (Uuid::new_v4()).into_arg(),
                    (user_id).into_arg(),
                    (entity_type).into_arg(),
                    (entity_id).into_arg(),
                    (user_id).into_arg(),
                    (utc_now()).into_arg(),
                    (version).into_arg(),
                ],
            )
            .await?;
        }
        Some(row) => {
            let old_version: i64 = row.try_get("version")?;
            let new_version = old_version.max(version);
            db.execute(
                "UPDATE sync_tombstones SET version = ?, deleted_at = ?, deleted_by = ? \
                 WHERE user_id = ? AND entity_type = ? AND entity_id = ?",
                &[
                    (new_version).into_arg(),
                    (utc_now()).into_arg(),
                    (user_id).into_arg(),
                    (user_id).into_arg(),
                    (entity_type).into_arg(),
                    (entity_id).into_arg(),
                ],
            )
            .await?;
        }
    }
    Ok(())
}
