//! Audit log + domain events (outbox) — mirrors `app/core/audit.py`
//! and `app/core/events.py`.
//!
//! Hard rule (§14): passwords, tokens, TOTP secrets, API keys and any
//! auth value are never stored — a dict key containing one of the
//! secret hints is replaced by `{"field": <key>, "changed": true}`.

use crate::db::IntoArg;
use serde_json::Value;
use uuid::Uuid;

use crate::{
    db::{utc_now, Db},
    errors::ApiError,
};

const SECRET_HINTS: &[&str] = &[
    "password", "passwd", "pwd", "token", "totp", "secret", "api_key", "apikey", "session", "code",
];

/// Scrub a JSON value the way `_scrub` does in `audit.py`.
pub fn scrub(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, v) in map {
                let kl = k.to_lowercase();
                if SECRET_HINTS.iter().any(|h| kl.contains(h)) {
                    out.insert(
                        k.clone(),
                        serde_json::json!({ "field": k, "changed": true }),
                    );
                } else {
                    out.insert(k.clone(), scrub(v));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(scrub).collect()),
        other => other.clone(),
    }
}

/// `log_audit(...)` — inserts one `audit_logs` row (no commit; the
/// caller owns the transaction, but our handlers auto-commit per
/// statement, so this is effectively immediate).
#[allow(clippy::too_many_arguments)]
pub async fn log_audit(
    db: &Db,
    actor_id: Option<Uuid>,
    actor_type: &str,
    action: &str,
    entity_type: &str,
    entity_id: Option<&str>,
    old_values: Option<Value>,
    new_values: Option<Value>,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO audit_logs (id, actor_id, actor_type, action, entity_type, entity_id, \
         old_values, new_values, ip, user_agent, request_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            (Uuid::new_v4()).into_arg(),
            (actor_id).into_arg(),
            (actor_type).into_arg(),
            (action).into_arg(),
            (entity_type).into_arg(),
            (entity_id).into_arg(),
            (old_values.map(|v| scrub(&v))).into_arg(),
            (new_values.map(|v| scrub(&v))).into_arg(),
            (ip).into_arg(),
            (user_agent).into_arg(),
            (Uuid::new_v4().to_string()).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    Ok(())
}

/// `emit_event(...)` — inserts one `domain_events` (outbox) row.
pub async fn emit_event(
    db: &Db,
    aggregate_type: &str,
    aggregate_id: &str,
    event_type: &str,
    payload: Value,
) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO domain_events (event_id, idempotency_key, aggregate_type, aggregate_id, \
         event_type, payload, schema_version, source_client, created_at, retry_count) \
         VALUES (?, NULL, ?, ?, ?, ?, 1, 'api', ?, 0)",
        &[
            (Uuid::new_v4()).into_arg(),
            (aggregate_type).into_arg(),
            (aggregate_id).into_arg(),
            (event_type).into_arg(),
            (payload).into_arg(),
            (utc_now()).into_arg(),
        ],
    )
    .await?;
    Ok(())
}
