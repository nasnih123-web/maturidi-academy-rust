//! YouTube / Aparat external-video links — mirrors
//! `app/modules/videos/youtube.py` and `aparat.py` exactly.
//!
//! Only parsing and URL building live here (pure, no network): the
//! best-effort oEmbed / Aparat-API title and cover fetches belong to
//! the library create path (Wave 6) and are never needed by the
//! videos read/admin routes.

/// Percent-decode a query value (`parse_qs` semantics for the `v`
/// parameter — the 11-char video id never contains `%`, but a
/// percent-encoded id must still validate after decoding).
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let h = |c: u8| (c as char).to_digit(16);
                match (h(bytes[i + 1]), h(bytes[i + 2])) {
                    (Some(a), Some(b)) => {
                        out.push(((a << 4) | b) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Minimal `urlparse` split: (scheme, host, path, query). Returns
/// `None` when the URL has no `://` separator.
fn split_url(url: &str) -> Option<(String, String, String, String)> {
    let url = url.trim();
    let (scheme, rest) = url.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    let (authority, path_query) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    // Strip userinfo / port from the authority; host is case-insensitive.
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let (path, query) = match path_query.split_once('?') {
        Some((p, q)) => (p.to_string(), q.split('#').next().unwrap_or("").to_string()),
        None => (
            path_query.split('#').next().unwrap_or("").to_string(),
            String::new(),
        ),
    };
    Some((scheme, host, path, query))
}

fn path_segments(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

fn is_youtube_id(s: &str) -> bool {
    s.len() == 11
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `youtube.parse_youtube_video_id` — watch / shorts / live / embed
/// on `youtube.com` (+ `www`/`m`) and `youtu.be`; the id must match
/// `^[A-Za-z0-9_-]{11}$`.
pub fn parse_youtube_video_id(url: Option<&str>) -> Option<String> {
    let url = url?;
    let (scheme, host, path, query) = split_url(url)?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let parts = path_segments(&path);
    let candidate: Option<String> = if host == "youtu.be" {
        if parts.len() == 1 {
            Some(parts[0].to_string())
        } else {
            None
        }
    } else if host == "youtube.com" || host == "www.youtube.com" || host == "m.youtube.com" {
        if path == "/watch" {
            query.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                if k == "v" {
                    Some(percent_decode(v))
                } else {
                    None
                }
            })
        } else if parts.len() == 2 && matches!(parts[0], "shorts" | "live" | "embed") {
            Some(parts[1].to_string())
        } else {
            None
        }
    } else {
        None
    };
    candidate.filter(|c| is_youtube_id(c))
}

/// Canonical watch URL (used by the Wave 6 library create path).
#[allow(dead_code)]
pub fn youtube_canonical_url(video_id: &str) -> String {
    format!("https://www.youtube.com/watch?v={video_id}")
}

pub fn youtube_embed_url(video_id: &str) -> String {
    format!("https://www.youtube-nocookie.com/embed/{video_id}")
}

pub fn youtube_thumbnail_url(video_id: &str) -> String {
    format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg")
}

fn is_aparat_hash(s: &str) -> bool {
    (4..=12).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `aparat.parse_aparat_video_hash` — `/v/{hash}` or the embed form
/// `/video/video/embed/videohash/{hash}/vt/frame` on `aparat.com`
/// (+ `www`); the hash must match `^[A-Za-z0-9]{4,12}$`.
pub fn parse_aparat_video_hash(url: Option<&str>) -> Option<String> {
    let url = url?;
    let (scheme, host, path, _) = split_url(url)?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if host != "aparat.com" && host != "www.aparat.com" {
        return None;
    }
    let parts = path_segments(&path);
    let candidate: Option<&str> = if parts.len() == 2 && parts[0] == "v" {
        Some(parts[1])
    } else if parts.len() == 7
        && parts[0] == "video"
        && parts[1] == "video"
        && parts[2] == "embed"
        && parts[3] == "videohash"
        && parts[5] == "vt"
        && parts[6] == "frame"
    {
        Some(parts[4])
    } else {
        None
    };
    candidate.filter(|c| is_aparat_hash(c)).map(str::to_string)
}

/// Canonical watch URL (used by the Wave 6 library create path).
#[allow(dead_code)]
pub fn aparat_canonical_url(video_hash: &str) -> String {
    format!("https://www.aparat.com/v/{video_hash}")
}

pub fn aparat_embed_url(video_hash: &str) -> String {
    format!("https://www.aparat.com/video/video/embed/videohash/{video_hash}/vt/frame")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn youtube_id_forms() {
        assert_eq!(
            parse_youtube_video_id(Some("https://www.youtube.com/watch?v=dQw4w9WgXcQ")),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_youtube_video_id(Some("https://youtu.be/dQw4w9WgXcQ?t=10")),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_youtube_video_id(Some("https://www.youtube.com/shorts/dQw4w9WgXcQ")),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_youtube_video_id(Some("https://m.youtube.com/embed/dQw4w9WgXcQ")),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            parse_youtube_video_id(Some("https://example.com/watch?v=dQw4w9WgXcQ")),
            None
        );
        assert_eq!(
            parse_youtube_video_id(Some("https://www.youtube.com/watch?v=short")),
            None
        );
        assert_eq!(parse_youtube_video_id(None), None);
        assert_eq!(
            youtube_embed_url("dQw4w9WgXcQ"),
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ"
        );
        assert_eq!(
            youtube_thumbnail_url("dQw4w9WgXcQ"),
            "https://i.ytimg.com/vi/dQw4w9WgXcQ/hqdefault.jpg"
        );
    }

    #[test]
    fn aparat_hash_forms() {
        assert_eq!(
            parse_aparat_video_hash(Some("https://www.aparat.com/v/cpuac8h")),
            Some("cpuac8h".to_string())
        );
        assert_eq!(
            parse_aparat_video_hash(Some(
                "https://www.aparat.com/video/video/embed/videohash/cpuac8h/vt/frame"
            )),
            Some("cpuac8h".to_string())
        );
        assert_eq!(
            parse_aparat_video_hash(Some("https://aparat.com/v/ab")),
            None
        );
        assert_eq!(
            parse_aparat_video_hash(Some("https://example.com/v/cpuac8h")),
            None
        );
        assert_eq!(parse_aparat_video_hash(None), None);
        assert_eq!(
            aparat_embed_url("cpuac8h"),
            "https://www.aparat.com/video/video/embed/videohash/cpuac8h/vt/frame"
        );
    }
}
