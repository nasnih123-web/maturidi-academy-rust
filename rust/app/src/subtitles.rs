//! Timed subtitles — mirrors `app/modules/videos/subtitles.py`.
//!
//! Pure VTT building/parsing plus the settings reads and row reset
//! the admin routes need.
//!
//! The generation worker itself (faster-whisper + LLM translation,
//! `videos/tasks.py`) is a Python sidecar and is NOT ported
//! (ADR-045). The HTTP trigger only marks rows `pending` and
//! honestly reports `enqueued: false` when no ARQ enqueue exists
//! from Rust — the same precedent as Wave 3's ingest trigger.

use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    db::{utc_now, Db, IntoArg},
    errors::ApiError,
    library_visibility::LibraryItem,
    settings::get_typed_setting,
    video_links::{parse_aparat_video_hash, parse_youtube_video_id},
    AppState,
};

/// `videos/schemas.py: SUBTITLE_LANGS`.
pub const SUBTITLE_LANGS: &[&str] = &["fa", "ar", "en", "ur", "dari"];

pub fn is_subtitle_lang(lang: &str) -> bool {
    SUBTITLE_LANGS.contains(&lang)
}

/// `subtitles.is_youtube_external`.
pub fn is_youtube_external(item: &LibraryItem) -> bool {
    item.external_url
        .as_deref()
        .is_some_and(|u| parse_youtube_video_id(Some(u)).is_some())
}

/// `subtitles.is_aparat_external`.
pub fn is_aparat_external(item: &LibraryItem) -> bool {
    item.external_url
        .as_deref()
        .is_some_and(|u| parse_aparat_video_hash(Some(u)).is_some())
}

/// `subtitles.is_embed_external` — YouTube or Aparat: playback is
/// only from the source iframe, no downloadable file.
pub fn is_embed_external(item: &LibraryItem) -> bool {
    is_youtube_external(item) || is_aparat_external(item)
}

/// `subtitles.is_direct_file_external` — a direct file link (PR #182):
/// has an `external_url` that is not an embed.
pub fn is_direct_file_external(item: &LibraryItem) -> bool {
    item.external_url.is_some() && !is_embed_external(item)
}

// --- WebVTT ---------------------------------------------------------------

/// `format_vtt_timestamp` — seconds → `HH:MM:SS.mmm`, negatives
/// clamped to 0.
pub fn format_vtt_timestamp(seconds: f64) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as i64;
    let ms = total_ms % 1000;
    let total_s = total_ms / 1000;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        total_s / 3600,
        (total_s / 60) % 60,
        total_s % 60,
        ms
    )
}

/// One cue: `{start, end, text}` (seconds as floats).
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// `build_vtt` — segments → full WebVTT text. Empty-text segments
/// are skipped; `& < >` are escaped; `end <= start` clamps to
/// `start + 0.5`.
pub fn build_vtt(segments: &[Cue]) -> String {
    let mut cues: Vec<String> = Vec::new();
    for seg in segments {
        let text = seg.text.trim();
        if text.is_empty() {
            continue;
        }
        let start = seg.start;
        let mut end = seg.end;
        if end <= start {
            end = start + 0.5;
        }
        let escaped = text
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        cues.push(format!(
            "{} --> {}\n{}",
            format_vtt_timestamp(start),
            format_vtt_timestamp(end),
            escaped
        ));
    }
    let body = cues.join("\n\n");
    if body.is_empty() {
        "WEBVTT\n".to_string()
    } else {
        format!("WEBVTT\n\n{body}\n")
    }
}

fn parse_vtt_timestamp(raw: &str) -> Option<f64> {
    let cleaned = raw.trim().replace(',', ".");
    let parts: Vec<&str> = cleaned.split(':').collect();
    let parse = |s: &str| s.parse::<f64>().ok();
    match parts.len() {
        3 => {
            let h: f64 = parts[0].parse().ok()?;
            let m: f64 = parts[1].parse().ok()?;
            let s = parse(parts[2])?;
            Some(h * 3600.0 + m * 60.0 + s)
        }
        2 => {
            let m: f64 = parts[0].parse().ok()?;
            let s = parse(parts[1])?;
            Some(m * 60.0 + s)
        }
        1 => parse(parts[0]),
        _ => None,
    }
}

/// `parse_vtt` — WebVTT text → cues, the inverse of `build_vtt`.
/// Blocks without a `-->` line, with bad timing, or with empty text
/// are skipped; escapes are unescaped; output is sorted by start.
pub fn parse_vtt(vtt: Option<&str>) -> Vec<Cue> {
    let Some(vtt) = vtt.filter(|v| !v.is_empty()) else {
        return Vec::new();
    };
    let normalized = vtt.replace("\r\n", "\n").replace('\r', "\n");
    let mut segments: Vec<Cue> = Vec::new();
    // Split on blank lines (`\n[ \t]*\n` in Python).
    let mut block: Vec<&str> = Vec::new();
    let mut blocks: Vec<Vec<&str>> = Vec::new();
    for line in normalized.split('\n') {
        if line.trim().is_empty() {
            if !block.is_empty() {
                blocks.push(std::mem::take(&mut block));
            }
        } else {
            block.push(line);
        }
    }
    if !block.is_empty() {
        blocks.push(block);
    }
    for lines in blocks {
        let Some(timing_idx) = lines.iter().position(|l| l.contains("-->")) else {
            continue;
        };
        let timing = lines[timing_idx];
        let (start_raw, rest) = match timing.split_once("-->") {
            Some((s, r)) => (s, r),
            None => continue,
        };
        let end_token = rest.trim().split(' ').next().unwrap_or("");
        let (Some(start), Some(end)) = (
            parse_vtt_timestamp(start_raw),
            parse_vtt_timestamp(end_token),
        ) else {
            continue;
        };
        let text = lines[timing_idx + 1..]
            .join("\n")
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&");
        if text.is_empty() {
            continue;
        }
        segments.push(Cue { start, end, text });
    }
    segments.sort_by(|a, b| {
        a.start
            .partial_cmp(&b.start)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    segments
}

pub fn cue_to_json(cue: &Cue) -> Value {
    json!({ "start": cue.start, "end": cue.end, "text": cue.text })
}

// --- Settings -------------------------------------------------------------

/// `subtitles.subtitles_enabled` — CRM switch
/// `videos.subtitles_enabled`, default on.
pub async fn subtitles_enabled(state: &AppState) -> Result<bool, ApiError> {
    let db = state.db_or_500()?;
    let value =
        get_typed_setting(db, &state.config, "videos.subtitles_enabled", json!(true)).await?;
    Ok(value.as_bool().unwrap_or(true))
}

/// `subtitles.target_langs` — target langs from
/// `videos.subtitle_langs`; not a list → all five; a list → only
/// the allowed entries, in order.
pub async fn target_langs(state: &AppState) -> Result<Vec<String>, ApiError> {
    let db = state.db_or_500()?;
    let raw = get_typed_setting(db, &state.config, "videos.subtitle_langs", Value::Null).await?;
    match raw.as_array() {
        Some(items) => Ok(items
            .iter()
            .filter_map(|v| v.as_str())
            .filter(|s| SUBTITLE_LANGS.contains(s))
            .map(str::to_string)
            .collect()),
        None => Ok(SUBTITLE_LANGS.iter().map(|s| s.to_string()).collect()),
    }
}

// --- Rows -----------------------------------------------------------------

/// `subtitles.reset_rows` — delete every subtitle row for the item
/// and insert one `pending` row per target lang. Returns
/// `{lang: "pending"}` in lang order.
pub async fn reset_rows(db: &Db, item_id: Uuid, langs: &[String]) -> Result<Value, ApiError> {
    db.execute(
        "DELETE FROM item_subtitles WHERE item_id = ?",
        &[(item_id).into_arg()],
    )
    .await?;
    let now = utc_now();
    let mut statuses = serde_json::Map::new();
    for lang in langs {
        db.execute(
            "INSERT INTO item_subtitles (id, item_id, lang, status, vtt, error, created_at, updated_at) \
             VALUES (?, ?, ?, 'pending', NULL, NULL, ?, ?)",
            &[
                (Uuid::new_v4()).into_arg(),
                (item_id).into_arg(),
                (lang.clone()).into_arg(),
                (now).into_arg(),
                (now).into_arg(),
            ],
        )
        .await?;
        statuses.insert(lang.clone(), Value::String("pending".to_string()));
    }
    Ok(Value::Object(statuses))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtt_timestamp_format() {
        assert_eq!(format_vtt_timestamp(0.0), "00:00:00.000");
        assert_eq!(format_vtt_timestamp(61.5), "00:01:01.500");
        assert_eq!(format_vtt_timestamp(3600.25), "01:00:00.250");
        assert_eq!(format_vtt_timestamp(3661.999), "01:01:01.999");
    }

    #[test]
    fn vtt_build_parse_round_trip() {
        let cues = vec![
            Cue {
                start: 2.0,
                end: 3.0,
                text: "دوم <ب>".to_string(),
            },
            Cue {
                start: 0.0,
                end: 1.0,
                text: "اول & آخر".to_string(),
            },
        ];
        let vtt = build_vtt(&cues);
        assert!(vtt.starts_with("WEBVTT\n\n"));
        assert!(vtt.contains("اول &amp; آخر"));
        assert!(vtt.contains("دوم &lt;ب&gt;"));
        let parsed = parse_vtt(Some(&vtt));
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].text, "اول & آخر");
        assert_eq!(parsed[1].text, "دوم <ب>");
        assert_eq!(parsed[0].start, 0.0);
        assert_eq!(parsed[1].start, 2.0);
    }

    #[test]
    fn vtt_empty_and_clamp() {
        assert_eq!(build_vtt(&[]), "WEBVTT\n");
        // end <= start clamps to start + 0.5.
        let vtt = build_vtt(&[Cue {
            start: 1.0,
            end: 0.5,
            text: "x".to_string(),
        }]);
        assert!(vtt.contains("00:00:01.000 --> 00:00:01.500"));
        // Blocks without a timing line or with empty text are skipped.
        let parsed = parse_vtt(Some(
            "WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nسلام\n\nno timing here\n\n00:00:03.000 --> 00:00:04.000\n\n",
        ));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].text, "سلام");
        assert_eq!(parse_vtt(None).len(), 0);
    }

    #[test]
    fn subtitle_lang_allow_list() {
        assert!(is_subtitle_lang("fa"));
        assert!(is_subtitle_lang("dari"));
        assert!(!is_subtitle_lang("xx"));
        assert!(!is_subtitle_lang(""));
    }
}
