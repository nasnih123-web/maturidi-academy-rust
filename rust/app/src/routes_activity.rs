//! Activity routes — mirrors `app/modules/activity/router.py`.
//!
//! `POST /api/v1/activity/events` (201): authenticated, rate-limited to
//! 240/min per IP (`activity:<ip>`). Closed kind list; `meta` keeps
//! only short scalars (≤ 12 keys, string values truncated to 120 chars,
//! keys to 48). Anything else → 422 `bad_event_kind`.

use crate::db::IntoArg;
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::Json,
};
use serde_json::{json, Value};

use crate::{
    authn::{authenticate, client_ip},
    db::{new_id, utc_now},
    errors::{ApiError, ApiResult},
    validate::{parse_body, req_str},
    AppState,
};

pub const EVENT_KINDS: &[&str] = &[
    "media_play",
    "media_progress",
    "library_download",
    "library_open",
];

const MAX_META_KEYS: usize = 12;
const MAX_META_STR: usize = 120;

/// `_clean_meta` — only short scalars survive; nested objects, lists and
/// long strings are dropped or truncated.
fn clean_meta(meta: Option<&Value>) -> Option<Value> {
    let Some(Value::Object(map)) = meta else {
        return None;
    };
    let mut out = serde_json::Map::new();
    for (key, value) in map.iter().take(MAX_META_KEYS) {
        let short_key: String = key.chars().take(48).collect();
        match value {
            Value::Bool(_) | Value::Number(_) => {
                out.insert(short_key, value.clone());
            }
            Value::String(s) => {
                let short: String = s.chars().take(MAX_META_STR).collect();
                out.insert(short_key, Value::String(short));
            }
            _ => {}
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(Value::Object(out))
    }
}

pub async fn post_activity_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: axum::extract::ConnectInfo<std::net::SocketAddr>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = authenticate(&state, &headers, "/api/v1/activity/events").await?;
    let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
    let key = format!(
        "activity:{}",
        ip.clone().unwrap_or_else(|| "unknown".to_string())
    );
    if !state
        .limiter
        .is_allowed(&key, 240, 60, false, &state.config.app_env)
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    let db = state.db_or_500()?;
    let parsed = parse_body(&body)?;
    let kind = req_str(&parsed, "kind", 1, 48)?;
    let title = parsed
        .get("title")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(300).collect::<String>());
    let ref_type = parsed
        .get("ref_type")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(48).collect::<String>());
    let ref_id = parsed
        .get("ref_id")
        .and_then(|v| v.as_str())
        .map(|s| s.chars().take(64).collect::<String>());
    if !EVENT_KINDS.contains(&kind.as_str()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_event_kind",
            "نوع رویداد ناشناخته است",
        ));
    }
    let meta = clean_meta(parsed.get("meta"));
    let id = new_id();
    db.execute("INSERT INTO user_activity_events (id, user_id, kind, title, ref_type, ref_id, meta, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)", &[(id).into_arg(), (ctx.user.id).into_arg(), (&kind).into_arg(), (title).into_arg(), (ref_type).into_arg(), (ref_id).into_arg(), (meta).into_arg(), (utc_now()).into_arg()])
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "id": id.to_string(), "kind": kind })),
    ))
}
