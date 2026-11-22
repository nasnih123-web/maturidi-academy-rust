//! Shared content workflow machine — mirrors `app/core/workflow.py` (§9).
//!
//! One implementation for every content type (fatwa, library, CMS pages,
//! articles, social posts): `draft → in_review → approved → published`,
//! with `rejected` / `archived` as exits. No skipping; `published` can
//! only become `archived`; re-entry is `archived → draft` only.

use axum::http::StatusCode;
use serde_json::json;

use crate::errors::ApiError;

pub const DRAFT: &str = "draft";
pub const IN_REVIEW: &str = "in_review";
pub const APPROVED: &str = "approved";
pub const PUBLISHED: &str = "published";
pub const REJECTED: &str = "rejected";
pub const ARCHIVED: &str = "archived";

pub const STATUSES: &[&str] = &[DRAFT, IN_REVIEW, APPROVED, PUBLISHED, REJECTED, ARCHIVED];

/// `TRANSITIONS` in `workflow.py`: current → allowed next statuses.
pub fn allowed_next(current: &str) -> &'static [&'static str] {
    match current {
        DRAFT => &[IN_REVIEW, ARCHIVED],
        IN_REVIEW => &[APPROVED, REJECTED, DRAFT, ARCHIVED],
        APPROVED => &[PUBLISHED, REJECTED, DRAFT, ARCHIVED],
        PUBLISHED => &[ARCHIVED],
        REJECTED => &[DRAFT, ARCHIVED],
        ARCHIVED => &[DRAFT],
        _ => &[],
    }
}

pub fn can_transition(current: &str, target: &str) -> bool {
    allowed_next(current).contains(&target)
}

/// `validate_transition` — 422 `unknown_status` for an unknown target,
/// 422 `invalid_transition` (details `{"from","to"}`) for a disallowed
/// edge. Messages are copied verbatim from `workflow.py`.
pub fn validate_transition(current: &str, target: &str) -> Result<(), ApiError> {
    if !STATUSES.contains(&target) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "unknown_status",
            format!("وضعیت ناشناخته: {target}"),
        ));
    }
    if !can_transition(current, target) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_transition",
            format!("گذار از «{current}» به «{target}» مجاز نیست"),
            json!({ "from": current, "to": target }),
        ));
    }
    Ok(())
}
