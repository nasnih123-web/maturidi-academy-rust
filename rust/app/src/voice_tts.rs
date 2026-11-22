//! Text-to-speech — a faithful port of `app/modules/voice/tts.py`.
//!
//! Provider abstraction with honest degradation: when no provider is
//! configured the service returns text with `audio_kind="none"` and
//! the `tts_unavailable` notice — never silence, never fake bytes.
//! Coqui (local) is never available in the Rust binary, so it
//! always degrades; the cloud provider is a real OpenAI-shape HTTP
//! client. This module is wired for the future native `/voice/ask`
//! (ADR-047); today the ask route itself is sidecar-contract (ADR-134)
//! because it needs the STT sidecar first (ADR-046).
//!
//! Dead code is allowed here on purpose for the synthesis helpers:
//! no native route calls them yet (same ADR-046/ADR-047 reason).
//! They are exercised by unit tests.
#![allow(dead_code)]

use axum::http::StatusCode;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    errors::ApiError,
    settings::{get_setting, stage_setting},
    storage::{presigned_url, put_object},
    AppState,
};

pub const TTS_DOWNLOAD_PATH: &str = "/api/v1/voice/tts-audio-by-ref";
pub const SIGNED_PURPOSE_VOICE_TTS: &str = "voice-tts";

/// `tts.get_tts_provider` resolution — `None` means no TTS is
/// available (honest degradation in the caller). Coqui is never
/// installed in the Rust binary.
pub fn tts_provider_available(configured: &str, cloud_endpoint: &str) -> bool {
    match configured {
        "none" => false,
        "coqui" => false,
        "cloud" => !cloud_endpoint.trim().is_empty(),
        // auto: coqui if installed (never), else cloud if configured.
        _ => !cloud_endpoint.trim().is_empty(),
    }
}

/// `tts.CloudTTS.synthesize` — POST `{model, input, voice,
/// response_format: "mp3"}` to the configured endpoint. Any upstream
/// error becomes 503 `tts_unavailable` (the caller degrades to text).
pub async fn cloud_synthesize(
    client: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    model: &str,
    text: &str,
    voice: &str,
) -> Result<(Vec<u8>, String), ApiError> {
    let model = if model.trim().is_empty() {
        "tts-1"
    } else {
        model
    };
    let mut request = client
        .post(endpoint)
        .header("Content-Type", "application/json")
        .json(&json!({
            "model": model,
            "input": text,
            "voice": voice,
            "response_format": "mp3",
        }));
    if !api_key.is_empty() {
        request = request.header("Authorization", format!("Bearer {api_key}"));
    }
    let resp = match request.send().await {
        Ok(r) => r,
        Err(_) => {
            return Err(ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "tts_unavailable",
                "سرویس TTS ابری در دسترس نیست",
            ));
        }
    };
    if !resp.status().is_success() {
        return Err(ApiError::with_details(
            StatusCode::SERVICE_UNAVAILABLE,
            "tts_unavailable",
            "سرویس TTS ابری خطا برگرداند",
            json!({ "provider": "cloud", "upstream_status": resp.status().as_u16() }),
        ));
    }
    let mime = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or("").trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "audio/mpeg".to_string());
    let bytes = resp.bytes().await.map_err(|_| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "tts_unavailable",
            "سرویس TTS ابری در دسترس نیست",
        )
    })?;
    Ok((bytes.to_vec(), mime))
}

/// `tts.synthesize_or_degrade` — `(audio_bytes, mime, notice_code)`.
/// `notice_code` is always a message code (`"tts_unavailable"`);
/// "not configured" never raises, so the text answer is not lost.
#[allow(clippy::too_many_arguments)]
pub async fn synthesize_or_degrade(
    client: &reqwest::Client,
    text: &str,
    lang: &str,
    voice: &str,
    configured_provider: &str,
    cloud_endpoint: &str,
    cloud_api_key: &str,
    cloud_model: &str,
) -> Result<(Option<Vec<u8>>, Option<String>, Option<&'static str>), ApiError> {
    if lang != "fa" && lang != "ar" {
        return Err(ApiError::with_details(
            StatusCode::BAD_REQUEST,
            "unsupported_language",
            "فقط فارسی و عربی پشتیبانی می‌شوند",
            json!({ "lang": lang }),
        ));
    }
    if !tts_provider_available(configured_provider, cloud_endpoint) {
        return Ok((None, None, Some("tts_unavailable")));
    }
    match cloud_synthesize(
        client,
        cloud_endpoint,
        cloud_api_key,
        cloud_model,
        text,
        voice,
    )
    .await
    {
        Ok((bytes, mime)) => Ok((Some(bytes), Some(mime), None)),
        Err(e) if e.code == "tts_unavailable" => Ok((None, None, Some("tts_unavailable"))),
        Err(e) => Err(e),
    }
}

/// Voice settings read (raw strings — the `voice.*` keys are NOT in
/// Python's `SETTING_DEFS`, so `get_typed_setting` returns the raw
/// decrypted string for them; only the three `voice.tts_cloud_*`
/// keys are registered, as `string`/`secret`).
pub async fn voice_setting_raw(
    state: &AppState,
    key: &str,
    default: &str,
) -> Result<String, ApiError> {
    let db = state.db_or_500()?;
    Ok(get_setting(db, &state.config, key)
        .await?
        .unwrap_or_else(|| default.to_string()))
}

/// `voice/service.py::_int_setting` — `int()` with a default fallback.
pub async fn voice_int_setting(state: &AppState, key: &str, default: i64) -> Result<i64, ApiError> {
    let raw = voice_setting_raw(state, key, "").await?;
    if raw.is_empty() {
        return Ok(default);
    }
    Ok(raw.trim().parse::<i64>().unwrap_or(default))
}

/// `voice/service.py::_synthesize_answer_audio` — synthesize the
/// answer, store it under `voice/tts/{session_id}.{mp3|bin}`, and
/// return the signed download URL. Storage failure falls back to
/// base64 honestly, so the spoken answer is not lost.
/// Returns `(audio_url, audio_base64, audio_mime, provider_name, notice)`.
pub async fn synthesize_answer_audio(
    state: &AppState,
    session_id: Uuid,
    answer: &str,
    lang: &str,
) -> Result<
    (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<&'static str>,
    ),
    ApiError,
> {
    let voice_name =
        voice_setting_raw(state, &format!("voice.tts_voice_{lang}"), "default").await?;
    let configured = voice_setting_raw(state, "voice.tts_provider", "none").await?;
    let endpoint = voice_setting_raw(state, "voice.tts_cloud_endpoint", "").await?;
    let api_key = voice_setting_raw(state, "voice.tts_cloud_api_key", "").await?;
    let model = voice_setting_raw(state, "voice.tts_cloud_model", "tts-1").await?;
    let (raw, mime, notice) = synthesize_or_degrade(
        &state.client,
        answer,
        lang,
        &voice_name,
        &configured,
        &endpoint,
        &api_key,
        &model,
    )
    .await?;
    let Some(raw) = raw else {
        return Ok((None, None, None, None, notice));
    };
    let mime = mime.unwrap_or_else(|| "audio/mpeg".to_string());
    let ext = if mime.contains("mpeg") || mime.contains("mp3") {
        "mp3"
    } else {
        "bin"
    };
    let key = format!("voice/tts/{session_id}.{ext}");
    match put_object(&state.config, &key, &raw).await {
        Ok(()) => {
            let url = presigned_url(
                &state.config,
                &key,
                300,
                TTS_DOWNLOAD_PATH,
                SIGNED_PURPOSE_VOICE_TTS,
            )?;
            Ok((Some(url), None, Some(mime), Some(configured), None))
        }
        Err(_) => {
            use base64::{engine::general_purpose::STANDARD, Engine as _};
            Ok((
                None,
                Some(STANDARD.encode(&raw)),
                Some(mime),
                Some(configured),
                None,
            ))
        }
    }
}

/// Re-export for the settings route: stage one voice setting value.
pub async fn stage_voice_setting(
    state: &AppState,
    key: &str,
    plaintext: &str,
) -> Result<(), ApiError> {
    let db = state.db_or_500()?;
    stage_setting(db, &state.config, key, plaintext).await
}

/// The nine known voice settings, in Python `DEFAULTS` order.
pub const VOICE_SETTING_KEYS: &[&str] = &[
    "voice.stt_provider",
    "voice.tts_provider",
    "voice.max_audio_seconds",
    "voice.max_audio_mb",
    "voice.tts_voice_fa",
    "voice.tts_voice_ar",
    "voice.tts_cloud_endpoint",
    "voice.tts_cloud_api_key",
    "voice.tts_cloud_model",
];

/// The typed default for a voice key (what `GET /admin/voice/settings`
/// returns when the key was never stored).
pub fn voice_default(key: &str) -> Value {
    match key {
        "voice.stt_provider" => json!("auto"),
        "voice.tts_provider" => json!("none"),
        "voice.max_audio_seconds" => json!(120),
        "voice.max_audio_mb" => json!(25),
        "voice.tts_voice_fa" => json!("default"),
        "voice.tts_voice_ar" => json!("default"),
        "voice.tts_cloud_endpoint" => json!(""),
        "voice.tts_cloud_api_key" => json!(""),
        "voice.tts_cloud_model" => json!("tts-1"),
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A one-shot mock cloud TTS endpoint: captures the request JSON
    /// body, answers with the canned status/body. Returns the URL.
    async fn mock_server(
        status: u16,
        resp_body: &'static [u8],
        content_type: &'static str,
        captured: Arc<Mutex<Option<String>>>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status_text = if status == 200 {
            "OK"
        } else {
            "Internal Server Error"
        };
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            // Read headers, then the Content-Length body.
            let mut content_len = 0usize;
            let mut header_end = None;
            while header_end.is_none() {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    header_end = Some(pos + 4);
                    let head = String::from_utf8_lossy(&buf[..pos + 4]).to_string();
                    for line in head.lines() {
                        if let Some(v) = line.strip_prefix("Content-Length:") {
                            content_len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
            let end = header_end.unwrap_or(0);
            while buf.len() < end + content_len {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[end..]).to_string();
            *captured.lock().unwrap() = Some(body);
            let response = format!(
                "HTTP/1.1 {status} {status_text}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                resp_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(resp_body).await.unwrap();
        });
        format!("http://{addr}/tts")
    }

    #[test]
    fn provider_availability_matrix() {
        assert!(!tts_provider_available("none", "https://example.com"));
        assert!(!tts_provider_available("coqui", "https://example.com"));
        assert!(!tts_provider_available("cloud", ""));
        assert!(tts_provider_available("cloud", "https://example.com"));
        assert!(!tts_provider_available("auto", ""));
        assert!(tts_provider_available("auto", "https://example.com"));
    }

    #[tokio::test]
    async fn degrade_when_no_provider() {
        let client = reqwest::Client::new();
        let (bytes, mime, notice) =
            synthesize_or_degrade(&client, "سلام", "fa", "default", "none", "", "", "tts-1")
                .await
                .unwrap();
        assert_eq!(bytes, None);
        assert_eq!(mime, None);
        assert_eq!(notice, Some("tts_unavailable"));
        // auto with no endpoint configured also degrades honestly.
        let (_, _, notice) =
            synthesize_or_degrade(&client, "سلام", "fa", "default", "auto", "", "", "tts-1")
                .await
                .unwrap();
        assert_eq!(notice, Some("tts_unavailable"));
    }

    #[tokio::test]
    async fn degrade_when_cloud_down() {
        let client = reqwest::Client::new();
        let (bytes, mime, notice) = synthesize_or_degrade(
            &client,
            "سلام",
            "fa",
            "default",
            "cloud",
            "http://127.0.0.1:9/tts",
            "",
            "tts-1",
        )
        .await
        .unwrap();
        assert_eq!(bytes, None);
        assert_eq!(mime, None);
        assert_eq!(notice, Some("tts_unavailable"));
    }

    #[tokio::test]
    async fn unsupported_language_errors() {
        let client = reqwest::Client::new();
        let err = synthesize_or_degrade(&client, "hello", "en", "default", "none", "", "", "tts-1")
            .await
            .unwrap_err();
        assert_eq!(err.code, "unsupported_language");
    }

    #[tokio::test]
    async fn cloud_success_sends_openai_shape_body() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let endpoint = mock_server(200, b"mp3bytes", "audio/mpeg", captured.clone()).await;
        let client = reqwest::Client::new();
        let (bytes, mime, notice) = synthesize_or_degrade(
            &client, "سلام", "fa", "default", "cloud", &endpoint, "k", "tts-1",
        )
        .await
        .unwrap();
        assert_eq!(bytes, Some(b"mp3bytes".to_vec()));
        assert_eq!(mime, Some("audio/mpeg".to_string()));
        assert_eq!(notice, None);
        let body = captured.lock().unwrap().clone().unwrap();
        let parsed: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["model"], json!("tts-1"));
        assert_eq!(parsed["input"], json!("سلام"));
        assert_eq!(parsed["response_format"], json!("mp3"));
    }

    #[tokio::test]
    async fn cloud_500_degrades() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let endpoint = mock_server(500, b"boom", "text/plain", captured).await;
        let client = reqwest::Client::new();
        let (bytes, _, notice) = synthesize_or_degrade(
            &client, "سلام", "fa", "default", "cloud", &endpoint, "", "tts-1",
        )
        .await
        .unwrap();
        assert_eq!(bytes, None);
        assert_eq!(notice, Some("tts_unavailable"));
    }
}
