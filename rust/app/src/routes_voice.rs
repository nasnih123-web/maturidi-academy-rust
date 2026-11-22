//! Voice routes — mirrors `app/modules/voice/router.py` (Wave 5).
//!
//! Natively implemented: admin settings (GET/PUT), admin usage log
//! (GET), and the signed TTS-audio download
//! (`GET /api/v1/voice/tts-audio-by-ref`).
//!
//! `POST /voice/transcribe` and `POST /voice/ask` are deliberately
//! NOT registered here: both routes exist only to run STT
//! (faster-whisper), which is a sidecar per ADR-008 and never lives
//! in the Rust binary. Registering them natively would answer an
//! honest 503 where the Python origin transcribes for real — a
//! regression. They fall through to the proxy, exactly like the
//! Wave 4 hifz ASR endpoints (ADR-031 precedent). See ADR-046.
//! The deterministic pieces (intents, TTS client) are ported as
//! `voice_intents` / `voice_tts` and unit-tested, ready for the day
//! an STT sidecar is wired (ADR-047).

use axum::{
    body::Body,
    extract::{ConnectInfo, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Json, Response},
};
use base64::{engine::general_purpose::URL_SAFE, Engine as _};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::LazyLock;
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission},
    db::{Arg, IntoArg},
    errors::{ApiError, ApiResult},
    settings::get_setting,
    storage::{read_bytes, verify_download_sig},
    validate::{parse_body, parse_datetime_str, parse_query, query_int, req_dict},
    voice_intents::INTENTS,
    voice_tts::{stage_voice_setting, voice_default, VOICE_SETTING_KEYS},
    AppState,
};

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Python `str(value)` — used for provider checks and for staging
/// plain-string settings.
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

/// Python `int(value)` — JSON number (truncated), numeric string,
/// or bool; anything else → `None` (the caller raises 400).
fn py_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(b) => Some(i64::from(*b)),
        _ => None,
    }
}

// --- admin: settings ---------------------------------------------------------

/// `GET /api/v1/admin/voice/settings` — all nine keys. A stored
/// value comes back as its raw decrypted string (the `voice.*` keys
/// are not in Python's `SETTING_DEFS`); an unset key returns its
/// typed default (ADR-049).
pub async fn get_voice_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/voice").await?;
    require_permission(&state, &ctx, "voice.manage").await?;
    let db = state.db_or_500()?;
    let mut settings = Map::new();
    for key in VOICE_SETTING_KEYS {
        let value = match get_setting(db, &state.config, key).await? {
            Some(raw) => Value::String(raw),
            None => voice_default(key),
        };
        settings.insert((*key).to_string(), value);
    }
    Ok(Json(json!({ "settings": Value::Object(settings) })))
}

/// `PUT /api/v1/admin/voice/settings` — `{"settings": {...}}`.
/// Unknown keys → 400 `unknown_setting`; bad provider values → 400
/// `invalid_setting_value` (with `allowed`); bad int values → 400.
/// Every key is validated before anything is staged (Python rolls
/// the whole patch back on error; Rust has no transaction here, so
/// validate-then-stage is the faithful equivalent).
pub async fn admin_update_voice_settings(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/voice").await?;
    require_permission(&state, &ctx, "voice.manage").await?;
    let parsed = parse_body(&body)?;
    let patch = req_dict(&parsed, "settings")?;
    let patch = patch
        .as_object()
        .ok_or_else(|| validation_err("settings", "must be an object"))?;
    // Pass 1 — validate everything.
    let mut staged: Vec<(String, String)> = Vec::new();
    for (key, value) in patch {
        if !VOICE_SETTING_KEYS.contains(&key.as_str()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "unknown_setting",
                format!("کلید ناشناخته: {key}"),
            ));
        }
        let plaintext = match key.as_str() {
            "voice.stt_provider" => {
                let allowed = ["auto", "faster_whisper", "none"];
                let s = py_str(value);
                if !allowed.contains(&s.as_str()) {
                    return Err(ApiError::with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_setting_value",
                        format!("مقدار نامعتبر برای {key}"),
                        json!({ "allowed": allowed }),
                    ));
                }
                s
            }
            "voice.tts_provider" => {
                let allowed = ["none", "coqui", "cloud", "auto"];
                let s = py_str(value);
                if !allowed.contains(&s.as_str()) {
                    return Err(ApiError::with_details(
                        StatusCode::BAD_REQUEST,
                        "invalid_setting_value",
                        format!("مقدار نامعتبر برای {key}"),
                        json!({ "allowed": allowed }),
                    ));
                }
                s
            }
            "voice.max_audio_seconds" | "voice.max_audio_mb" => {
                let Some(iv) = py_int(value) else {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_setting_value",
                        format!("{key} باید عدد باشد"),
                    ));
                };
                if !(1..=3600).contains(&iv) {
                    return Err(ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "invalid_setting_value",
                        format!("{key} خارج از بازه است"),
                    ));
                }
                iv.to_string()
            }
            _ => py_str(value),
        };
        staged.push((key.clone(), plaintext));
    }
    // Pass 2 — stage, then bump the version, event and audit.
    let db = state.db_or_500()?;
    let changed: Vec<String> = staged.iter().map(|(k, _)| k.clone()).collect();
    for (key, plaintext) in &staged {
        stage_voice_setting(&state, key, plaintext).await?;
    }
    if !changed.is_empty() {
        state.bump_settings_version();
        emit_event(
            db,
            "voice_settings",
            "voice",
            "voice.settings_updated",
            json!({ "keys": changed, "updated_by": ctx.user.id.to_string() }),
        )
        .await?;
        let ip = client_ip(&headers, Some(peer.0), &state.config.trusted_proxies);
        let ua = headers
            .get("user-agent")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        log_audit(
            db,
            Some(ctx.user.id),
            "admin",
            "voice.settings_update",
            "voice_settings",
            Some("voice"),
            None,
            Some(json!({ "keys": changed })),
            ip.as_deref(),
            ua.as_deref(),
        )
        .await?;
    }
    Ok(Json(json!({ "updated": changed })))
}

// --- admin: usage log ----------------------------------------------------------

/// `voice/service.py::_encode_cursor` — `{"t": iso, "id": str}`
/// base64url (a dict, unlike the library list cursor).
fn encode_usage_cursor(created_at: DateTime<Utc>, id: Uuid) -> String {
    let raw = json!({ "t": created_at.to_rfc3339(), "id": id.to_string() });
    URL_SAFE.encode(serde_json::to_string(&raw).unwrap_or_default().as_bytes())
}

fn decode_usage_cursor(cursor: &str) -> Result<(DateTime<Utc>, Uuid), ApiError> {
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
    let ts = parsed
        .get("t")
        .and_then(|v| v.as_str())
        .and_then(parse_datetime_str)
        .ok_or_else(invalid)?;
    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok())
        .ok_or_else(invalid)?;
    Ok((ts, id))
}

/// `GET /api/v1/admin/voice/usage` — cursor page, `limit` 1–50
/// default 20, optional `intent` filter.
pub async fn list_voice_usage(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let ctx = authenticate(&state, &headers, "/api/v1/admin/voice").await?;
    require_permission(&state, &ctx, "voice.manage").await?;
    let params = parse_query(&uri);
    let limit = query_int(&params, "limit", 20, 1, 50)?;
    let intent = params.get("intent").filter(|s| !s.is_empty()).cloned();
    if let Some(i) = &intent {
        if !INTENTS.contains(&i.as_str()) {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "invalid_intent",
                "قصد نامعتبر است",
            ));
        }
    }
    let cursor = params.get("cursor").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let mut sql = String::from(
        "SELECT id, session_id, intent, lang, stt_ms, tts_provider, created_at \
         FROM voice_usage_log",
    );
    let mut args: Vec<Arg> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    if let Some(i) = &intent {
        conds.push("intent = ?".to_string());
        args.push(i.into_arg());
    }
    if let Some(c) = &cursor {
        let (ts, lid) = decode_usage_cursor(c)?;
        conds.push("(created_at < ? OR (created_at = ? AND id < ?))".to_string());
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    if !conds.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conds.join(" AND "));
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items: Vec<Value> = Vec::new();
    let mut keys: Vec<(DateTime<Utc>, Uuid)> = Vec::new();
    for row in rows {
        let id: Uuid = row.try_get("id")?;
        let session_id: Uuid = row.try_get("session_id")?;
        let intent: String = row.try_get("intent")?;
        let lang: String = row.try_get("lang")?;
        let stt_ms: Option<i32> = row.try_get("stt_ms")?;
        let tts_provider: Option<String> = row.try_get("tts_provider")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        keys.push((created_at, id));
        items.push(json!({
            "id": id.to_string(),
            "session_id": session_id.to_string(),
            "intent": intent,
            "lang": lang,
            "stt_ms": stt_ms,
            "tts_provider": tts_provider,
            "created_at": created_at.to_rfc3339(),
        }));
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let (ts, lid) = keys[(limit as usize) - 1];
        next_cursor = Some(encode_usage_cursor(ts, lid));
        items.truncate(limit as usize);
    }
    Ok(Json(json!({ "items": items, "next_cursor": next_cursor })))
}

// --- public: signed TTS download ----------------------------------------------

static TTS_KEY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^voice/tts/[0-9a-fA-F-]{36}\.(mp3|bin)$").expect("valid tts key pattern")
});

/// `GET /api/v1/voice/tts-audio-by-ref` — signed download. The key
/// is strictly `voice/tts/<uuid>.<mp3|bin>` and the purpose must be
/// `voice-tts`; anything else is 403 `invalid_signature`. Missing /
/// non-integer query params are 422 (FastAPI validation).
pub async fn tts_audio_by_ref(
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
    if purpose != "voice-tts"
        || !verify_download_sig(&state.config, &key, exp, &sig, "voice-tts")
        || !TTS_KEY_RE.is_match(&key)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_signature",
            "امضای دانلود نامعتبر یا منقضی است",
        ));
    }
    let data = read_bytes(&state.config, &key).await?;
    let mime = if key.ends_with(".mp3") {
        "audio/mpeg"
    } else {
        "application/octet-stream"
    };
    let res = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", mime)
        .body(Body::from(data))
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}
