//! The ADR-051 upload scanner, shared by every upload path
//! (ADR-164): SHA-256 against the admin-managed
//! `library.blocked_hashes` setting plus the EICAR standard test
//! signature. A hit is 422 `file_infected` with the caller's audit
//! row; clean bytes pass. There is no ClamAV anywhere in this binary
//! (the user's 2026-11-19 decision), and no un-scanned upload path:
//! library, finance receipts, admin fonts and user avatars all call
//! this one function with their own audit action/entity — the exact
//! values Python writes, preserved from the three copies this
//! replaces (`library.upload_rejected_infected` / `library_item`,
//! `finance.upload_rejected_infected` / `manual_donation_proof`,
//! `admin.font_upload_rejected_infected` / `platform_setting`,
//! `user.avatar_upload_rejected_infected` / `user`).
//!
//! Avatars have no `draft` state, so the manual-review half of
//! ADR-051 does not apply to them — the scan is the gate, and the
//! image is public immediately on upload, as Python does.

use axum::http::StatusCode;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    audit::log_audit,
    db::Db,
    errors::{ApiError, ApiResult},
    settings::get_setting,
    Config,
};

/// The EICAR standard antivirus test file — the one signature every
/// scanner recognises, which makes the `file_infected` path testable
/// without a scanner daemon.
const EICAR: &[u8] = b"X5O!P%@AP[4\\PZX54(P^)7CC)7}$EICAR-STANDARD-ANTIVIRUS-TEST-FILE!$H+H*";

fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Audit context for a scan rejection (kept as one argument so the
/// scan helper stays under clippy's argument-count limit).
pub struct ScanAudit<'a> {
    pub actor_id: Option<Uuid>,
    pub actor_type: &'a str,
    pub ip: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

/// Scan `data` and reject it 422 `file_infected` on a hit. `action`
/// and `entity_type` are the caller's audit row (see the module docs
/// for the four values in use).
pub async fn scan_upload(
    db: &Db,
    config: &Config,
    action: &str,
    entity_type: &str,
    audit: ScanAudit<'_>,
    data: &[u8],
    filename: Option<&str>,
) -> ApiResult<()> {
    let mut signature: Option<&str> = None;
    if data.len() >= EICAR.len() && data.windows(EICAR.len()).any(|w| w == EICAR) {
        signature = Some("Eicar-Test-Signature");
    } else if let Some(raw) = get_setting(db, config, "library.blocked_hashes").await? {
        if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(&raw) {
            let hex = sha256_hex(data);
            if items.iter().any(|v| v.as_str() == Some(hex.as_str())) {
                signature = Some("blocked-hash");
            }
        }
    }
    if let Some(sig) = signature {
        log_audit(
            db,
            audit.actor_id,
            audit.actor_type,
            action,
            entity_type,
            None,
            None,
            Some(json!({ "filename": filename, "signature": sig })),
            audit.ip,
            audit.user_agent,
        )
        .await?;
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "file_infected",
            "فایل به ویروس آلوده است و رد شد",
            json!({ "signature": sig }),
        ));
    }
    Ok(())
}
