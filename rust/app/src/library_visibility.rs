//! Shared library-visibility primitives — mirrors the helpers in
//! `app/modules/library/service.py` that the videos module (Wave 5)
//! reuses verbatim: `is_privileged`, `_public_base_stmt`,
//! `_may_access`, `_signed_url`.
//!
//! The library module itself (CRUD, uploads, workflow) is Wave 6;
//! only the read-side rules videos needs live here, so the videos
//! list can never diverge from the library list (same contract as
//! the Python `videos/service.py` docstring).

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    authn::{user_role_names, AuthCtx},
    db::{Db, DynRow, IntoArg},
    errors::ApiError,
    storage::presigned_url,
    Config,
};

/// Signed media URLs live 300 s (`library/service.py:
/// SIGNED_URL_EXPIRY_S`) and go through the library download endpoint.
pub const SIGNED_URL_EXPIRY_S: i64 = 300;
pub const LIBRARY_DOWNLOAD_PATH: &str = "/api/v1/library/download-by-ref";
pub const SIGNED_PURPOSE_LIBRARY: &str = "library";

/// Roles with full library access even without the permission
/// (`library/service.py: _PRIVILEGED_ROLES`).
pub const PRIVILEGED_LIBRARY_ROLES: &[&str] = &["scholar", "admin", "super_admin"];

/// `library_service.is_privileged` — `library.manage` permission or
/// a privileged role. Guests are never privileged.
pub async fn is_privileged(db: &Db, user: Option<&AuthCtx>) -> Result<bool, ApiError> {
    let Some(ctx) = user else {
        return Ok(false);
    };
    let row = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
             JOIN role_permissions rp ON rp.role_id = ur.role_id \
             JOIN permissions p ON p.id = rp.permission_id \
             WHERE ur.user_id = ? AND p.key = ? LIMIT 1",
            &[(ctx.user.id).into_arg(), ("library.manage").into_arg()],
        )
        .await?;
    if row.is_some() {
        return Ok(true);
    }
    let roles = user_role_names(db, ctx.user.id).await?;
    Ok(PRIVILEGED_LIBRARY_ROLES.iter().any(|r| roles.contains(*r)))
}

/// `library_service._public_base_stmt` as a WHERE fragment (fixed
/// literals only — no user input is interpolated). Privileged callers
/// see every visibility; logged-in users see `public` + `registered`;
/// guests see only `public`. Unpublished items are never listed.
pub fn public_where(privileged: bool, authed: bool) -> &'static str {
    if privileged {
        "workflow_status = 'published'"
    } else if authed {
        "workflow_status = 'published' AND visibility IN ('public', 'registered')"
    } else {
        "workflow_status = 'published' AND visibility = 'public'"
    }
}

/// `library_service._may_access` — may this caller see this published
/// item? (`scholars_only` is visible only to privileged callers.)
pub fn may_access(item: &LibraryItem, authed: bool, privileged: bool) -> bool {
    if privileged {
        return true;
    }
    match item.visibility.as_str() {
        "public" => true,
        "registered" => authed,
        _ => false,
    }
}

/// `library_service._signed_url` — `None` for an empty key or a
/// `pending:` key; otherwise a 300 s presigned library download URL.
/// A non-local storage provider fails with 503
/// `storage_not_configured` (ADR-015), like the Python provider.
pub fn signed_url(config: &Config, key: Option<&str>) -> Result<Option<String>, ApiError> {
    let Some(key) = key.filter(|k| !k.is_empty() && !k.starts_with("pending:")) else {
        return Ok(None);
    };
    Ok(Some(presigned_url(
        config,
        key,
        SIGNED_URL_EXPIRY_S,
        LIBRARY_DOWNLOAD_PATH,
        SIGNED_PURPOSE_LIBRARY,
    )?))
}

/// One `library_items` row, with exactly the columns the videos
/// module reads (Wave 5 extends `TEST_SCHEMA` with these; the
/// production table has more, read by Wave 6).
#[derive(Debug, Clone)]
pub struct LibraryItem {
    pub id: Uuid,
    pub kind: String,
    pub title: Value,
    pub description: Option<Value>,
    pub cover_media_key: Option<String>,
    pub media_key: Option<String>,
    pub external_url: Option<String>,
    pub scholar_id: Option<Uuid>,
    pub topic_tags: Value,
    pub duration_seconds: Option<i64>,
    pub visibility: String,
    pub workflow_status: String,
    pub created_at: DateTime<Utc>,
    pub published_at: Option<DateTime<Utc>>,
}

/// Columns selected by every videos query over `library_items`.
pub const LIBRARY_ITEM_COLUMNS: &str = "id, kind, title, description, cover_media_key, \
    media_key, external_url, scholar_id, topic_tags, duration_seconds, visibility, \
    workflow_status, created_at, published_at";

pub fn library_item_from_row(row: &DynRow) -> Result<LibraryItem, ApiError> {
    // INTEGER columns are read as i32 on Postgres (ADR-028).
    let duration: Option<i32> = row.try_get("duration_seconds")?;
    let tags: Option<Value> = row.try_get("topic_tags")?;
    Ok(LibraryItem {
        id: row.try_get("id")?,
        kind: row.try_get("kind")?,
        title: row.try_get("title")?,
        description: row.try_get("description")?,
        cover_media_key: row.try_get("cover_media_key")?,
        media_key: row.try_get("media_key")?,
        external_url: row.try_get("external_url")?,
        scholar_id: row.try_get("scholar_id")?,
        topic_tags: tags
            .filter(|v| v.is_array())
            .unwrap_or(Value::Array(vec![])),
        duration_seconds: duration.map(i64::from),
        visibility: row.try_get("visibility")?,
        workflow_status: row.try_get("workflow_status")?,
        created_at: row.try_get("created_at")?,
        published_at: row.try_get("published_at")?,
    })
}

/// `videos/service.py::_scholar_names` — batched `full_name` lookup.
pub async fn scholar_names(
    db: &Db,
    scholar_ids: &[Option<Uuid>],
) -> Result<std::collections::HashMap<Uuid, String>, ApiError> {
    let mut ids: Vec<Uuid> = scholar_ids.iter().filter_map(|s| *s).collect();
    ids.sort();
    ids.dedup();
    let mut out = std::collections::HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql = format!("SELECT id, full_name FROM users WHERE id IN ({placeholders})");
    let args: Vec<crate::db::Arg> = ids.iter().map(|id| id.into_arg()).collect();
    for row in db.fetch_all(&sql, &args).await? {
        let id: Uuid = row.try_get("id")?;
        let name: String = row.try_get("full_name")?;
        out.insert(id, name);
    }
    Ok(out)
}
