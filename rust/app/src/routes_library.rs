//! Library routes — mirrors `app/modules/library/{router,service,schemas}.py`
//! (Wave 6).
//!
//! Public reads (list / search / detail / stream / signed download) plus
//! the full admin surface: CRUD, multipart uploads, workflow transitions,
//! version history + revert, AI indexing, Telegram-ready import and
//! reprocessing. Visibility rules are exactly the library's own
//! (`library_visibility::public_where`); `scholars_only` items answer
//! 404 to non-privileged callers, never 403.
//!
//! Upload scanning (ADR-050, resolved here): ClamAV is NOT reintroduced.
//! Every uploaded byte passes the shared `upload_scan::scan_upload` —
//! SHA-256 hash-check against the `library.blocked_hashes` setting
//! plus the EICAR standard test signature — and every upload lands as
//! `draft`, so no file reaches `published` without human review
//! (hash-check + manual admin review). Infected bytes are rejected
//! 422 `file_infected` with the same audit row Python writes. There
//! is no un-scanned upload path.
//!
//! The media pipeline (transcribe / OCR) is a Python ARQ worker; the
//! Rust trigger only marks rows `pending` and reports `enqueued: false`
//! (the Wave 3 ingest precedent). Subtitle-style generation never runs
//! inside this binary.

use std::collections::HashMap;

use axum::{
    body::Body,
    extract::{ConnectInfo, Multipart, Path, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Json, Response},
};
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{
    audit::{emit_event, log_audit},
    authn::{authenticate, client_ip, require_permission, AuthCtx},
    db::{new_id, utc_now, Arg, Db, DynRow, IntoArg},
    embeddings::{
        hash_embed, resolve_provider, AI_SCOPES, HASH_MODEL, HASH_PROVIDER, PROJECTION_ID,
    },
    errors::{ApiError, ApiResult},
    library_visibility::{is_privileged, public_where, signed_url, SIGNED_PURPOSE_LIBRARY},
    settings::get_setting,
    storage::{put_object, read_bytes, verify_download_sig},
    upload_scan::{scan_upload, ScanAudit},
    validate::{decode_cursor, encode_cursor, parse_body, parse_path_uuid, parse_query, query_int},
    video_links::{
        aparat_canonical_url, parse_aparat_video_hash, parse_youtube_video_id,
        youtube_canonical_url,
    },
    workflow::{validate_transition, APPROVED, ARCHIVED, PUBLISHED},
    AppState,
};

/// Item kinds (`LibraryItemMetaIn.kind`).
const KINDS: &[&str] = &["video", "video_360", "pdf", "audio", "mindmap"];
const VISIBILITIES: &[&str] = &["public", "registered", "scholars_only"];
const WORKFLOW_STATUSES: &[&str] = &[
    "draft",
    "in_review",
    "approved",
    "published",
    "rejected",
    "archived",
];

/// `KIND_EXTENSIONS` — allowed upload extensions per kind (§7).
fn kind_extensions(kind: &str) -> &'static [&'static str] {
    match kind {
        "video" | "video_360" => &[".mp4", ".webm", ".mov", ".mkv"],
        "pdf" => &[".pdf"],
        "audio" => &[".mp3", ".wav", ".ogg", ".m4a", ".flac"],
        "mindmap" => &[".mm", ".json", ".xmind"],
        _ => &[],
    }
}

const COVER_EXTENSIONS: &[&str] = &[".jpg", ".jpeg", ".png", ".webp"];

/// `EXT_MIME` (§7).
fn ext_mime(ext: &str) -> Option<&'static str> {
    Some(match ext {
        ".mp4" => "video/mp4",
        ".webm" => "video/webm",
        ".mov" => "video/quicktime",
        ".mkv" => "video/x-matroska",
        ".pdf" => "application/pdf",
        ".mp3" => "audio/mpeg",
        ".wav" => "audio/wav",
        ".ogg" => "audio/ogg",
        ".m4a" => "audio/mp4",
        ".flac" => "audio/flac",
        ".mm" => "application/xml",
        ".json" => "application/json",
        ".xmind" => "application/zip",
        ".jpg" | ".jpeg" => "image/jpeg",
        ".png" => "image/png",
        ".webp" => "image/webp",
        _ => return None,
    })
}

/// `MAX_BYTES` — per-kind upload caps (§7).
fn upload_limit_for_kind(kind: &str) -> ApiResult<i64> {
    match kind {
        "video" | "video_360" => Ok(512 * 1024 * 1024),
        "pdf" => Ok(100 * 1024 * 1024),
        "audio" => Ok(200 * 1024 * 1024),
        "mindmap" => Ok(20 * 1024 * 1024),
        "cover" => Ok(5 * 1024 * 1024),
        _ => Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_kind",
            "نوع فایل نامعتبر است",
        )),
    }
}

/// Largest possible file cap — the provisional multipart streaming cap
/// before the kind is known (the metadata field usually comes first).
const MAX_FILE_BYTES: usize = 512 * 1024 * 1024;
const MAX_COVER_BYTES: usize = 5 * 1024 * 1024;
/// Aparat og-cover download cap (`_COVER_DOWNLOAD_CAP`).
const COVER_DOWNLOAD_CAP: usize = 6 * 1024 * 1024;

const APARAT_API_URL: &str = "https://www.aparat.com/etc/api/video/videohash/";
const YOUTUBE_OEMBED_URL: &str = "https://www.youtube.com/oembed";
const APARAT_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/126.0 Safari/537.36";

fn validation_err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

fn invalid_metadata(detail: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_metadata",
        format!("متادیتای JSON نامعتبر است: {detail}"),
    )
}

fn file_too_large(max_bytes: i64) -> ApiError {
    ApiError::with_details(
        StatusCode::PAYLOAD_TOO_LARGE,
        "file_too_large",
        "حجم فایل بیش از سقف مجاز است",
        json!({ "max_bytes": max_bytes }),
    )
}

fn not_found_item() -> ApiError {
    ApiError::not_found("item_not_found", "آیتم یافت نشد")
}

/// Optional auth (`_optional_user`): any auth failure → guest.
async fn optional_user(state: &AppState, headers: &HeaderMap) -> Option<AuthCtx> {
    authenticate(state, headers, "/api/v1/library").await.ok()
}

async fn admin_ctx(state: &AppState, headers: &HeaderMap) -> ApiResult<AuthCtx> {
    let ctx = authenticate(state, headers, "/api/v1/admin/library").await?;
    require_permission(state, &ctx, "library.manage").await?;
    Ok(ctx)
}

fn client_ip_ua(
    headers: &HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    state: &AppState,
) -> (Option<String>, Option<String>) {
    let ip = client_ip(headers, Some(peer.0), &state.config.trusted_proxies);
    let ua = headers
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (ip, ua)
}

// --- full item row -----------------------------------------------------------

/// Every `library_items` column (production model; the Wave 5
/// `TEST_SCHEMA` subset is extended by the Wave 6 test DDL).
const FULL_COLUMNS: &str = "id, kind, title, description, cover_media_key, media_key, \
    external_url, scholar_id, topic_tags, related_lesson_id, religious_place_ref, visibility, \
    workflow_status, version, ai_scopes, include_in_fatwa_rag, ai_indexed, download_policy, \
    transcript, transcript_status, ocr_text, ocr_status, processing_error, duration_seconds, \
    file_size_bytes, mime_type, source_type, source_ref, created_by, created_at, updated_at, \
    published_at";

#[derive(Debug, Clone)]
struct FullItem {
    id: Uuid,
    kind: String,
    title: Value,
    description: Option<Value>,
    cover_media_key: Option<String>,
    media_key: Option<String>,
    external_url: Option<String>,
    scholar_id: Option<Uuid>,
    topic_tags: Value,
    related_lesson_id: Option<Uuid>,
    religious_place_ref: Option<String>,
    visibility: String,
    workflow_status: String,
    version: i32,
    ai_scopes: Value,
    include_in_fatwa_rag: bool,
    ai_indexed: bool,
    download_policy: Option<Value>,
    transcript: Option<String>,
    transcript_status: String,
    ocr_text: Option<String>,
    ocr_status: String,
    processing_error: Option<String>,
    duration_seconds: Option<i32>,
    file_size_bytes: Option<i32>,
    mime_type: Option<String>,
    source_type: String,
    source_ref: Option<Value>,
    created_by: Option<Uuid>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    published_at: Option<DateTime<Utc>>,
}

impl FullItem {
    fn from_row(row: &DynRow) -> ApiResult<Self> {
        // INTEGER columns are read as i32 on Postgres (ADR-028).
        let duration: Option<i32> = row.try_get("duration_seconds")?;
        let file_size: Option<i32> = row.try_get("file_size_bytes")?;
        let tags: Option<Value> = row.try_get("topic_tags")?;
        let scopes: Option<Value> = row.try_get("ai_scopes")?;
        Ok(Self {
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
            related_lesson_id: row.try_get("related_lesson_id")?,
            religious_place_ref: row.try_get("religious_place_ref")?,
            visibility: row.try_get("visibility")?,
            workflow_status: row.try_get("workflow_status")?,
            version: row.try_get("version")?,
            ai_scopes: scopes
                .filter(|v| v.is_array())
                .unwrap_or(Value::Array(vec![])),
            include_in_fatwa_rag: row.try_get("include_in_fatwa_rag")?,
            ai_indexed: row.try_get("ai_indexed")?,
            download_policy: row.try_get("download_policy")?,
            transcript: row.try_get("transcript")?,
            transcript_status: row.try_get("transcript_status")?,
            ocr_text: row.try_get("ocr_text")?,
            ocr_status: row.try_get("ocr_status")?,
            processing_error: row.try_get("processing_error")?,
            duration_seconds: duration,
            file_size_bytes: file_size,
            mime_type: row.try_get("mime_type")?,
            source_type: row.try_get("source_type")?,
            source_ref: row.try_get("source_ref")?,
            created_by: row.try_get("created_by")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            published_at: row.try_get("published_at")?,
        })
    }

    /// INSERT all columns explicitly (ids/timestamps are generated in
    /// Rust, no `RETURNING` — ADR-014).
    async fn insert(&self, db: &Db) -> ApiResult<()> {
        db.execute(
            "INSERT INTO library_items (id, kind, title, description, cover_media_key, \
             media_key, external_url, scholar_id, topic_tags, related_lesson_id, \
             religious_place_ref, visibility, workflow_status, version, ai_scopes, \
             include_in_fatwa_rag, ai_indexed, download_policy, transcript, \
             transcript_status, ocr_text, ocr_status, processing_error, duration_seconds, \
             file_size_bytes, mime_type, source_type, source_ref, created_by, created_at, \
             updated_at, published_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            &[
                self.id.into_arg(),
                self.kind.clone().into_arg(),
                self.title.clone().into_arg(),
                self.description.clone().into_arg(),
                self.cover_media_key.clone().into_arg(),
                self.media_key.clone().into_arg(),
                self.external_url.clone().into_arg(),
                self.scholar_id.into_arg(),
                self.topic_tags.clone().into_arg(),
                self.related_lesson_id.into_arg(),
                self.religious_place_ref.clone().into_arg(),
                self.visibility.clone().into_arg(),
                self.workflow_status.clone().into_arg(),
                self.version.into_arg(),
                self.ai_scopes.clone().into_arg(),
                self.include_in_fatwa_rag.into_arg(),
                self.ai_indexed.into_arg(),
                self.download_policy.clone().into_arg(),
                self.transcript.clone().into_arg(),
                self.transcript_status.clone().into_arg(),
                self.ocr_text.clone().into_arg(),
                self.ocr_status.clone().into_arg(),
                self.processing_error.clone().into_arg(),
                self.duration_seconds.into_arg(),
                self.file_size_bytes.into_arg(),
                self.mime_type.clone().into_arg(),
                self.source_type.clone().into_arg(),
                self.source_ref.clone().into_arg(),
                self.created_by.into_arg(),
                self.created_at.into_arg(),
                self.updated_at.into_arg(),
                self.published_at.into_arg(),
            ],
        )
        .await?;
        Ok(())
    }

    /// UPDATE all mutable columns (read-modify-write; the app owns the
    /// row between load and save in these handlers).
    async fn save(&self, db: &Db) -> ApiResult<()> {
        db.execute(
            "UPDATE library_items SET kind = ?, title = ?, description = ?, \
             cover_media_key = ?, media_key = ?, external_url = ?, scholar_id = ?, \
             topic_tags = ?, related_lesson_id = ?, religious_place_ref = ?, visibility = ?, \
             workflow_status = ?, version = ?, ai_scopes = ?, include_in_fatwa_rag = ?, \
             ai_indexed = ?, download_policy = ?, transcript = ?, transcript_status = ?, \
             ocr_text = ?, ocr_status = ?, processing_error = ?, duration_seconds = ?, \
             file_size_bytes = ?, mime_type = ?, source_type = ?, source_ref = ?, \
             created_by = ?, updated_at = ?, published_at = ? WHERE id = ?",
            &[
                self.kind.clone().into_arg(),
                self.title.clone().into_arg(),
                self.description.clone().into_arg(),
                self.cover_media_key.clone().into_arg(),
                self.media_key.clone().into_arg(),
                self.external_url.clone().into_arg(),
                self.scholar_id.into_arg(),
                self.topic_tags.clone().into_arg(),
                self.related_lesson_id.into_arg(),
                self.religious_place_ref.clone().into_arg(),
                self.visibility.clone().into_arg(),
                self.workflow_status.clone().into_arg(),
                self.version.into_arg(),
                self.ai_scopes.clone().into_arg(),
                self.include_in_fatwa_rag.into_arg(),
                self.ai_indexed.into_arg(),
                self.download_policy.clone().into_arg(),
                self.transcript.clone().into_arg(),
                self.transcript_status.clone().into_arg(),
                self.ocr_text.clone().into_arg(),
                self.ocr_status.clone().into_arg(),
                self.processing_error.clone().into_arg(),
                self.duration_seconds.into_arg(),
                self.file_size_bytes.into_arg(),
                self.mime_type.clone().into_arg(),
                self.source_type.clone().into_arg(),
                self.source_ref.clone().into_arg(),
                self.created_by.into_arg(),
                self.updated_at.into_arg(),
                self.published_at.into_arg(),
                self.id.into_arg(),
            ],
        )
        .await?;
        Ok(())
    }
}

async fn load_item(db: &Db, item_id: Uuid) -> ApiResult<Option<FullItem>> {
    let row = db
        .fetch_optional(
            &format!("SELECT {FULL_COLUMNS} FROM library_items WHERE id = ?"),
            &[item_id.into_arg()],
        )
        .await?;
    match row {
        Some(r) => Ok(Some(FullItem::from_row(&r)?)),
        None => Ok(None),
    }
}

async fn get_item_or_404(db: &Db, item_id: Uuid) -> ApiResult<FullItem> {
    load_item(db, item_id).await?.ok_or_else(not_found_item)
}

/// `get_item_detail` — unpublished / invisible → 404, never 403.
async fn get_detail_item(db: &Db, item_id: Uuid, user: Option<&AuthCtx>) -> ApiResult<FullItem> {
    let item = load_item(db, item_id).await?.ok_or_else(not_found_item)?;
    if item.workflow_status != PUBLISHED {
        return Err(not_found_item());
    }
    let privileged = is_privileged(db, user).await?;
    let allowed = privileged
        || item.visibility == "public"
        || (item.visibility == "registered" && user.is_some());
    if !allowed {
        return Err(not_found_item());
    }
    Ok(item)
}

/// `library_service._may_access` on the visibility string.
fn may_access(visibility: &str, authed: bool, privileged: bool) -> bool {
    if privileged {
        return true;
    }
    match visibility {
        "public" => true,
        "registered" => authed,
        _ => false,
    }
}

fn urls_for(
    state: &AppState,
    item: &FullItem,
    authed: bool,
    privileged: bool,
) -> ApiResult<(Option<String>, Option<String>)> {
    if !may_access(&item.visibility, authed, privileged) {
        return Ok((None, None));
    }
    Ok((
        signed_url(&state.config, item.media_key.as_deref())?,
        signed_url(&state.config, item.cover_media_key.as_deref())?,
    ))
}

/// `LibraryItemOut` — field order follows `schemas.py`.
fn item_json(item: &FullItem, media_url: Option<String>, cover_url: Option<String>) -> Value {
    json!({
        "id": item.id.to_string(),
        "kind": item.kind,
        "title": item.title,
        "description": item.description.clone().unwrap_or(Value::Null),
        "scholar_id": item.scholar_id.map(|u| u.to_string()),
        "topic_tags": item.topic_tags,
        "related_lesson_id": item.related_lesson_id.map(|u| u.to_string()),
        "religious_place_ref": item.religious_place_ref,
        "visibility": item.visibility,
        "workflow_status": item.workflow_status,
        "version": item.version,
        "ai_scopes": item.ai_scopes,
        "include_in_fatwa_rag": item.include_in_fatwa_rag,
        "ai_indexed": item.ai_indexed,
        "download_policy": item.download_policy.clone().unwrap_or(Value::Null),
        "transcript_status": item.transcript_status,
        "ocr_status": item.ocr_status,
        "has_transcript": item.transcript.as_deref().map(|t| !t.is_empty()).unwrap_or(false),
        "duration_seconds": item.duration_seconds,
        "file_size_bytes": item.file_size_bytes,
        "mime_type": item.mime_type,
        "source_type": item.source_type,
        "source_ref": item.source_ref.clone().unwrap_or(Value::Null),
        "external_url": item.external_url,
        "created_at": item.created_at.to_rfc3339(),
        "updated_at": item.updated_at.to_rfc3339(),
        "published_at": item.published_at.map(|d| d.to_rfc3339()),
        "media_url": media_url,
        "cover_url": cover_url,
    })
}

/// `LibraryItemAdminOut` — the public shape plus transcript/OCR/error.
fn admin_json(item: &FullItem, media_url: Option<String>, cover_url: Option<String>) -> Value {
    let mut v = item_json(item, media_url, cover_url);
    if let Value::Object(map) = &mut v {
        map.insert(
            "transcript".to_string(),
            item.transcript
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        map.insert(
            "ocr_text".to_string(),
            item.ocr_text
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        map.insert(
            "processing_error".to_string(),
            item.processing_error
                .clone()
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
    }
    v
}

async fn item_out_with_urls(
    state: &AppState,
    item: &FullItem,
    user: Option<&AuthCtx>,
    privileged: bool,
) -> ApiResult<Value> {
    let (media_url, cover_url) = urls_for(state, item, user.is_some(), privileged)?;
    Ok(item_json(item, media_url, cover_url))
}

async fn admin_out_with_urls(
    state: &AppState,
    item: &FullItem,
    user: Option<&AuthCtx>,
    privileged: bool,
) -> ApiResult<Value> {
    let (media_url, cover_url) = urls_for(state, item, user.is_some(), privileged)?;
    Ok(admin_json(item, media_url, cover_url))
}

/// `json.dumps(..., ensure_ascii=False)` — Python's default separators
/// (`", "` / `": "`). Used for the lexical rank blobs and the admin
/// `q` filter, where Python matches against the dumped form.
fn py_json_dumps(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(py_json_dumps)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(k, val)| {
                    format!(
                        "{}: {}",
                        serde_json::to_string(k).unwrap_or_default(),
                        py_json_dumps(val)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

// --- upload validation (§7) -------------------------------------------------

/// `Path.suffix` — the last-dot suffix of the basename, lowercased
/// (a leading dot is not a suffix, and a dot in a directory is not one).
fn file_ext(filename: Option<&str>) -> String {
    let name = filename.unwrap_or("");
    let base = name.rsplit('/').next().unwrap_or(name);
    match base.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => base[i..].to_lowercase(),
    }
}

/// `service._check_magic` — magic bytes against the extension.
fn check_magic(content: &[u8], ext: &str) -> bool {
    let head = &content[..content.len().min(16)];
    match ext {
        ".mp4" | ".m4a" | ".mov" => content.len() >= 8 && content[4..8] == *b"ftyp",
        ".webm" | ".mkv" => head.starts_with(b"\x1a\x45\xdf\xa3"),
        ".pdf" => head.starts_with(b"%PDF-"),
        ".mp3" => {
            head.starts_with(b"ID3")
                || head.len() >= 2
                    && (head[..2] == *b"\xff\xfb"
                        || head[..2] == *b"\xff\xf3"
                        || head[..2] == *b"\xff\xf2")
        }
        ".wav" => head.starts_with(b"RIFF") && content.len() >= 12 && content[8..12] == *b"WAVE",
        ".ogg" => head.starts_with(b"OggS"),
        ".flac" => head.starts_with(b"fLaC"),
        ".mm" => {
            let stripped: &[u8] = {
                let mut s = content;
                while let Some((&b, rest)) = s.split_first() {
                    if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                        s = rest;
                    } else {
                        break;
                    }
                }
                s
            };
            stripped.len() >= 4 && stripped[..4] == *b"<map"
        }
        ".json" => {
            let stripped: &[u8] = {
                let mut s = content;
                while let Some((&b, rest)) = s.split_first() {
                    if matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                        s = rest;
                    } else {
                        break;
                    }
                }
                s
            };
            stripped.first() == Some(&b'{') || stripped.first() == Some(&b'[')
        }
        ".xmind" => head.starts_with(b"PK\x03\x04"),
        ".jpg" | ".jpeg" => head.starts_with(b"\xff\xd8\xff"),
        ".png" => head.starts_with(b"\x89PNG\r\n\x1a\n"),
        ".webp" => head.starts_with(b"RIFF") && content.len() >= 12 && content[8..12] == *b"WEBP",
        _ => false,
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The PDF JavaScript-action check (`_PDF_JS_ACTION_RE`):
/// `/JavaScript` or `/JS` not followed by `[A-Za-z0-9_]`.
fn pdf_has_js(content: &[u8]) -> bool {
    for needle in [b"/JavaScript".as_slice(), b"/JS".as_slice()] {
        let mut start = 0;
        while start + needle.len() <= content.len() {
            let Some(pos) = find_subslice(&content[start..], needle) else {
                break;
            };
            let abs = start + pos;
            let after = abs + needle.len();
            let followed = after < content.len()
                && matches!(
                    content[after],
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_'
                );
            if !followed {
                return true;
            }
            start = abs + 1;
        }
    }
    false
}

/// `service._validate_upload` — extension + size + magic + PDF JS.
/// Returns `(ext, mime)`.
fn validate_upload(
    kind: &str,
    filename: Option<&str>,
    content: &[u8],
) -> ApiResult<(String, String)> {
    let ext = file_ext(filename);
    let allowed = kind_extensions(kind);
    if !allowed.contains(&ext.as_str()) {
        let mut sorted: Vec<&str> = allowed.to_vec();
        sorted.sort_unstable();
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_file_extension",
            format!("پسوند «{ext}» برای نوع «{kind}» مجاز نیست"),
            json!({ "allowed": sorted }),
        ));
    }
    let limit = upload_limit_for_kind(kind)?;
    if content.len() as i64 > limit {
        return Err(ApiError::with_details(
            StatusCode::PAYLOAD_TOO_LARGE,
            "file_too_large",
            "حجم فایل بیش از سقف مجاز است",
            json!({ "max_bytes": limit, "kind": kind }),
        ));
    }
    if content.is_empty() || !check_magic(content, &ext) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "file_content_mismatch",
            "محتوای فایل با پسوند اعلام‌شده سازگار نیست (magic-byte نامعتبر)",
            json!({ "extension": ext }),
        ));
    }
    if ext == ".pdf" && pdf_has_js(content) {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "pdf_active_content",
            "فایل PDF حاوی کد جاوااسکریپت جاسازی‌شده است و پذیرفته نمی‌شود",
            json!({ "extension": ext }),
        ));
    }
    Ok((
        ext.clone(),
        ext_mime(&ext)
            .unwrap_or("application/octet-stream")
            .to_string(),
    ))
}

/// `service._validate_cover` — extension + 5 MB cap + magic.
fn validate_cover(filename: Option<&str>, content: &[u8]) -> ApiResult<(String, String)> {
    let ext = file_ext(filename);
    if !COVER_EXTENSIONS.contains(&ext.as_str()) {
        let mut sorted: Vec<&str> = COVER_EXTENSIONS.to_vec();
        sorted.sort_unstable();
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_cover_extension",
            "پسوند کاور باید jpg/png/webp باشد",
            json!({ "allowed": sorted }),
        ));
    }
    if content.len() as i64 > 5 * 1024 * 1024 {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "cover_too_large",
            "حجم کاور بیش از ۵ مگابایت است",
        ));
    }
    if content.is_empty() || !check_magic(content, &ext) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "cover_content_mismatch",
            "محتوای کاور با پسوند سازگار نیست",
        ));
    }
    Ok((
        ext.clone(),
        ext_mime(&ext)
            .unwrap_or("application/octet-stream")
            .to_string(),
    ))
}

/// `service._auto_pipeline_enabled` — `media.auto_pipeline`, default
/// true. The enqueue itself is a no-op (no ARQ in Rust); statuses go
/// `pending` exactly as Python with an offline worker.
async fn auto_pipeline_enabled(db: &Db, config: &crate::Config) -> ApiResult<bool> {
    let Some(raw) = get_setting(db, config, "media.auto_pipeline").await? else {
        return Ok(true);
    };
    if let Ok(Value::Bool(b)) = serde_json::from_str::<Value>(&raw) {
        return Ok(b);
    }
    Ok(matches!(
        raw.trim().to_lowercase().as_str(),
        "true" | "1" | "yes"
    ))
}

// --- external videos ---------------------------------------------------------

/// `service._direct_file_url` — https + a path ending in a video
/// extension (query/fragment ignored for the check; the raw URL is
/// returned as given, stripped).
fn direct_file_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (scheme, rest) = trimmed.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    let (authority, path_query) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_string()),
    };
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or(authority)
        .split(':')
        .next()
        .unwrap_or("");
    if host.is_empty() {
        return None;
    }
    let path = path_query.split(['?', '#']).next().unwrap_or("");
    if [".mp4", ".webm", ".m4v", ".mov"]
        .iter()
        .any(|e| path.to_ascii_lowercase().ends_with(e))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// `service._canonical_external_url` — YouTube / Aparat canonical
/// watch URL, or a direct video-file URL verbatim.
fn canonical_external_url(kind: &str, raw: &str) -> ApiResult<String> {
    if kind != "video" {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "external_url_not_allowed",
            "لینک خارجی فقط برای ویدیو مجاز است",
            json!({ "kind": kind }),
        ));
    }
    if let Some(id) = parse_youtube_video_id(Some(raw)) {
        return Ok(youtube_canonical_url(&id));
    }
    if let Some(hash) = parse_aparat_video_hash(Some(raw)) {
        return Ok(aparat_canonical_url(&hash));
    }
    if let Some(direct) = direct_file_url(raw) {
        return Ok(direct);
    }
    Err(ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_youtube_url",
        "لینک معتبر نیست؛ لینک یوتیوب، آپارات یا لینک مستقیم فایل ویدیو بدهید",
    ))
}

/// `service._usable_title` — a dict with at least one non-empty
/// string value.
fn usable_title(title: Option<&Value>) -> bool {
    match title {
        Some(Value::Object(map)) => map
            .values()
            .any(|v| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false)),
        _ => false,
    }
}

/// A localized dict (`dict[str, str]`, any keys) — the library
/// `LocalizedText`, looser than the strict fa/ar/en validator.
fn is_localized_dict(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.values().all(|x| x.is_string()),
        _ => false,
    }
}

/// `youtube.fetch_youtube_title` — oEmbed, best-effort, 4 s timeout.
async fn fetch_youtube_title(client: &reqwest::Client, watch_url: &str) -> Option<String> {
    let fut = client
        .get(YOUTUBE_OEMBED_URL)
        .query(&[("url", watch_url), ("format", "json")])
        .send();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(4), fut)
        .await
        .ok()?
        .ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    data.get("title")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// Parse `<meta ...>` attributes (`name="value"` / `name='value'`).
fn parse_meta_attrs(tag: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let chars: Vec<char> = tag.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len()
            && !(chars[i].is_ascii_alphanumeric()
                || chars[i] == '_'
                || chars[i] == '-'
                || chars[i] == ':')
        {
            i += 1;
        }
        let name_start = i;
        while i < chars.len()
            && (chars[i].is_ascii_alphanumeric()
                || chars[i] == '_'
                || chars[i] == '-'
                || chars[i] == ':')
        {
            i += 1;
        }
        if i == name_start {
            break;
        }
        let name: String = chars[name_start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || chars[i] != '=' {
            continue;
        }
        i += 1;
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() || (chars[i] != '"' && chars[i] != '\'') {
            continue;
        }
        let quote = chars[i];
        i += 1;
        let val_start = i;
        while i < chars.len() && chars[i] != quote {
            i += 1;
        }
        let value: String = chars[val_start..i].iter().collect();
        if i < chars.len() {
            i += 1;
        }
        out.insert(name.to_ascii_lowercase(), value);
    }
    out
}

/// Minimal `html.unescape` for og content.
fn html_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        let tail = &rest[pos..];
        match tail.find(';') {
            Some(end) if end <= 10 => {
                let entity = &tail[1..end];
                let decoded = match entity {
                    "amp" => Some("&".to_string()),
                    "lt" => Some("<".to_string()),
                    "gt" => Some(">".to_string()),
                    "quot" => Some("\"".to_string()),
                    "apos" | "#39" | "#x27" | "#X27" => Some("'".to_string()),
                    _ => {
                        if let Some(num) = entity
                            .strip_prefix("#x")
                            .or_else(|| entity.strip_prefix("#X"))
                        {
                            u32::from_str_radix(num, 16)
                                .ok()
                                .and_then(char::from_u32)
                                .map(|c| c.to_string())
                        } else if let Some(num) = entity.strip_prefix('#') {
                            num.parse::<u32>()
                                .ok()
                                .and_then(char::from_u32)
                                .map(|c| c.to_string())
                        } else {
                            None
                        }
                    }
                };
                match decoded {
                    Some(d) => out.push_str(&d),
                    None => out.push_str(&tail[..=end]),
                }
                rest = &tail[end + 1..];
            }
            _ => {
                out.push_str(tail);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// `aparat._og_content` — the `content` of the og meta tag, read
/// tag-by-tag so attribute order does not matter.
fn og_content(page: &str, prop: &str) -> Option<String> {
    let lower = page.to_ascii_lowercase();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find("<meta") {
        let tag_start = pos + rel;
        let Some(end) = lower[tag_start..].find('>') else {
            break;
        };
        let tag = &page[tag_start..tag_start + end];
        let attrs = parse_meta_attrs(tag);
        let is_prop = attrs.get("property").map(|s| s.as_str()) == Some(prop)
            || attrs.get("name").map(|s| s.as_str()) == Some(prop);
        if is_prop {
            if let Some(content) = attrs.get("content") {
                let content = content.trim();
                if !content.is_empty() {
                    return Some(html_unescape(content));
                }
            }
        }
        pos = tag_start + end + 1;
    }
    None
}

/// `aparat.fetch_aparat_meta` — JSON API first, og tags as fallback.
/// Best-effort: any failure is `(None, None)`.
async fn fetch_aparat_meta(
    client: &reqwest::Client,
    watch_url: &str,
) -> (Option<String>, Option<String>) {
    let hash = parse_aparat_video_hash(Some(watch_url));
    if let Some(hash) = hash {
        let url = format!("{APARAT_API_URL}{hash}");
        let fut = client
            .get(&url)
            .header("user-agent", APARAT_USER_AGENT)
            .send();
        if let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(4), fut).await {
            if resp.status() == reqwest::StatusCode::OK {
                if let Ok(payload) = resp.json::<Value>().await {
                    if let Some(video) = payload.get("video").and_then(|v| v.as_object()) {
                        let title = video
                            .get("title")
                            .and_then(|t| t.as_str())
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(str::to_string);
                        let mut cover = None;
                        for key in ["big_poster", "small_poster"] {
                            if let Some(c) = video.get(key).and_then(|v| v.as_str()) {
                                let c = c.trim();
                                if c.starts_with("https://") {
                                    cover = Some(c.to_string());
                                    break;
                                }
                            }
                        }
                        if title.is_some() {
                            return (title, cover);
                        }
                    }
                }
            }
        }
    }
    // og fallback on the watch page.
    let fut = client
        .get(watch_url)
        .header("user-agent", APARAT_USER_AGENT)
        .send();
    if let Ok(Ok(resp)) = tokio::time::timeout(std::time::Duration::from_secs(4), fut).await {
        if resp.status() == reqwest::StatusCode::OK {
            if let Ok(page) = resp.text().await {
                return (og_content(&page, "og:title"), og_content(&page, "og:image"));
            }
        }
    }
    (None, None)
}

/// `aparat.fetch_cover_bytes` — https only, 6 MB cap, best-effort.
async fn fetch_cover_bytes(client: &reqwest::Client, cover_url: &str) -> Option<Vec<u8>> {
    let trimmed = cover_url.trim();
    let (scheme, rest) = trimmed.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("https") || rest.is_empty() {
        return None;
    }
    let fut = client
        .get(trimmed)
        .header("user-agent", APARAT_USER_AGENT)
        .send();
    let resp = tokio::time::timeout(std::time::Duration::from_secs(4), fut)
        .await
        .ok()?
        .ok()?;
    if resp.status() != reqwest::StatusCode::OK {
        return None;
    }
    let data = resp.bytes().await.ok()?.to_vec();
    if data.is_empty() || data.len() > COVER_DOWNLOAD_CAP {
        return None;
    }
    Some(data)
}

/// `service._store_fetched_cover` — best-effort: any failure is `None`
/// and the item is created without a cover.
#[allow(clippy::too_many_arguments)]
async fn store_fetched_cover(
    state: &AppState,
    db: &Db,
    actor_id: Uuid,
    cover_url: &str,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> Option<String> {
    let data = fetch_cover_bytes(&state.client, cover_url).await?;
    let ext = [".jpg", ".png", ".webp"]
        .into_iter()
        .find(|e| check_magic(&data, e))?;
    let filename = format!("cover{ext}");
    if scan_upload(
        db,
        &state.config,
        "library.upload_rejected_infected",
        "library_item",
        ScanAudit {
            actor_id: Some(actor_id),
            actor_type: "admin",
            ip,
            user_agent,
        },
        &data,
        Some(&filename),
    )
    .await
    .is_err()
    {
        return None;
    }
    let (cext, _) = validate_cover(Some(&filename), &data).ok()?;
    let key = format!("library/covers/{}{}", Uuid::new_v4().simple(), cext);
    put_object(&state.config, &key, &data).await.ok()?;
    Some(key)
}

// --- metadata parsing (Pydantic-shaped, hand-rolled) -------------------------

/// Pydantic v2 lax `int`: JSON integer, integral float, or a digit
/// string (bool rejected).
fn pydantic_int(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.fract() == 0.0 && f.is_finite())
                .map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Pydantic v2 lax `bool`: bool, 0/1, or true/false/yes/no/on/off.
fn pydantic_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().and_then(|i| match i {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        }),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "y" | "on" => Some(true),
            "false" | "0" | "no" | "n" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn meta_str_list(v: &Value, field: &str) -> Result<Vec<String>, ApiError> {
    match v {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => {
                        return Err(invalid_metadata(&format!(
                            "{field} must be a list of strings"
                        )))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(invalid_metadata(&format!("{field} must be a list"))),
    }
}

fn meta_uuid(v: &Value, field: &str) -> Result<Uuid, ApiError> {
    match v {
        Value::String(s) => s
            .parse::<Uuid>()
            .map_err(|_| invalid_metadata(&format!("{field} must be a UUID"))),
        _ => Err(invalid_metadata(&format!("{field} must be a UUID"))),
    }
}

fn meta_opt_uuid(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Uuid>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => Ok(Some(meta_uuid(v, field)?)),
    }
}

fn meta_opt_localized(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Value>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(v) if is_localized_dict(v) => Ok(Some(v.clone())),
        Some(_) => Err(invalid_metadata(&format!(
            "{field} must be an object of strings"
        ))),
    }
}

fn meta_opt_str(
    map: &serde_json::Map<String, Value>,
    field: &str,
    max: usize,
) -> Result<Option<String>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(Value::String(s)) if s.chars().count() <= max => Ok(Some(s.clone())),
        Some(Value::String(_)) => Err(invalid_metadata(&format!("{field} is too long"))),
        Some(_) => Err(invalid_metadata(&format!("{field} must be a string"))),
    }
}

fn meta_opt_dict(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Value>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(v) if v.is_object() => Ok(Some(v.clone())),
        Some(_) => Err(invalid_metadata(&format!("{field} must be an object"))),
    }
}

fn meta_opt_str_list(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<Vec<String>>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => Ok(Some(meta_str_list(v, field)?)),
    }
}

fn meta_opt_bool(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<bool>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => pydantic_bool(v)
            .map(Some)
            .ok_or_else(|| invalid_metadata(&format!("{field} must be a boolean"))),
    }
}

fn meta_opt_int(
    map: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<i64>, ApiError> {
    match map.get(field) {
        Some(Value::Null) | None => Ok(None),
        Some(v) => pydantic_int(v)
            .map(Some)
            .ok_or_else(|| invalid_metadata(&format!("{field} must be an integer"))),
    }
}

/// `LibraryItemMetaIn` (multipart metadata JSON).
#[derive(Debug, Clone)]
struct CreateMeta {
    kind: String,
    title: Option<Value>,
    external_url: Option<String>,
    description: Option<Value>,
    scholar_id: Option<Uuid>,
    topic_tags: Vec<String>,
    related_lesson_id: Option<Uuid>,
    religious_place_ref: Option<String>,
    visibility: String,
    ai_scopes: Vec<String>,
    include_in_fatwa_rag: bool,
    download_policy: Option<Value>,
    duration_seconds: Option<i32>,
}

fn parse_create_meta(v: &Value) -> Result<CreateMeta, ApiError> {
    let Some(map) = v.as_object() else {
        return Err(invalid_metadata("metadata must be a JSON object"));
    };
    let kind = match map.get("kind") {
        Some(Value::String(s)) if KINDS.contains(&s.as_str()) => s.clone(),
        _ => {
            return Err(invalid_metadata(
                "kind must be one of video, video_360, pdf, audio, mindmap",
            ))
        }
    };
    let visibility = match map.get("visibility") {
        Some(Value::Null) | None => "registered".to_string(),
        Some(Value::String(s)) if VISIBILITIES.contains(&s.as_str()) => s.clone(),
        Some(_) => {
            return Err(invalid_metadata(
                "visibility must be public, registered or scholars_only",
            ))
        }
    };
    let duration = meta_opt_int(map, "duration_seconds")?;
    if let Some(d) = duration {
        if d < i32::MIN as i64 || d > i32::MAX as i64 {
            return Err(invalid_metadata("duration_seconds out of range"));
        }
    }
    Ok(CreateMeta {
        kind,
        title: meta_opt_localized(map, "title")?,
        external_url: meta_opt_str(map, "external_url", 600)?,
        description: meta_opt_localized(map, "description")?,
        scholar_id: meta_opt_uuid(map, "scholar_id")?,
        topic_tags: meta_opt_str_list(map, "topic_tags")?.unwrap_or_default(),
        related_lesson_id: meta_opt_uuid(map, "related_lesson_id")?,
        religious_place_ref: meta_opt_str(map, "religious_place_ref", 128)?,
        visibility,
        ai_scopes: meta_opt_str_list(map, "ai_scopes")?.unwrap_or_default(),
        include_in_fatwa_rag: meta_opt_bool(map, "include_in_fatwa_rag")?.unwrap_or(false),
        download_policy: meta_opt_dict(map, "download_policy")?,
        duration_seconds: duration.map(|d| d as i32),
    })
}

/// `LibraryItemUpdateIn` — every field optional; the outer `Option`
/// is presence (`exclude_unset`), the inner one is explicit null.
#[derive(Debug, Clone, Default)]
struct UpdatePatch {
    title: Option<Option<Value>>,
    description: Option<Option<Value>>,
    scholar_id: Option<Option<Uuid>>,
    topic_tags: Option<Option<Vec<String>>>,
    related_lesson_id: Option<Option<Uuid>>,
    religious_place_ref: Option<Option<String>>,
    visibility: Option<Option<String>>,
    ai_scopes: Option<Option<Vec<String>>>,
    include_in_fatwa_rag: Option<Option<bool>>,
    download_policy: Option<Option<Value>>,
    duration_seconds: Option<Option<i32>>,
    external_url: Option<Option<String>>,
    note: Option<Option<String>>,
}

fn parse_update_meta(v: &Value) -> Result<UpdatePatch, ApiError> {
    let Some(map) = v.as_object() else {
        return Err(invalid_metadata("metadata must be a JSON object"));
    };
    let mut patch = UpdatePatch::default();
    if map.contains_key("title") {
        patch.title = Some(meta_opt_localized(map, "title")?);
    }
    if map.contains_key("description") {
        patch.description = Some(meta_opt_localized(map, "description")?);
    }
    if map.contains_key("scholar_id") {
        patch.scholar_id = Some(meta_opt_uuid(map, "scholar_id")?);
    }
    if map.contains_key("topic_tags") {
        patch.topic_tags = Some(meta_opt_str_list(map, "topic_tags")?);
    }
    if map.contains_key("related_lesson_id") {
        patch.related_lesson_id = Some(meta_opt_uuid(map, "related_lesson_id")?);
    }
    if map.contains_key("religious_place_ref") {
        patch.religious_place_ref = Some(meta_opt_str(map, "religious_place_ref", 128)?);
    }
    if map.contains_key("visibility") {
        patch.visibility = Some(match map.get("visibility") {
            Some(Value::Null) => None,
            Some(Value::String(s)) if VISIBILITIES.contains(&s.as_str()) => Some(s.clone()),
            _ => {
                return Err(invalid_metadata(
                    "visibility must be public, registered or scholars_only",
                ))
            }
        });
    }
    if map.contains_key("ai_scopes") {
        patch.ai_scopes = Some(meta_opt_str_list(map, "ai_scopes")?);
    }
    if map.contains_key("include_in_fatwa_rag") {
        patch.include_in_fatwa_rag = Some(meta_opt_bool(map, "include_in_fatwa_rag")?);
    }
    if map.contains_key("download_policy") {
        patch.download_policy = Some(meta_opt_dict(map, "download_policy")?);
    }
    if map.contains_key("duration_seconds") {
        let d = meta_opt_int(map, "duration_seconds")?;
        if let Some(d) = d {
            if d < i32::MIN as i64 || d > i32::MAX as i64 {
                return Err(invalid_metadata("duration_seconds out of range"));
            }
        }
        patch.duration_seconds = Some(d.map(|x| x as i32));
    }
    if map.contains_key("external_url") {
        patch.external_url = Some(meta_opt_str(map, "external_url", usize::MAX)?);
    }
    if map.contains_key("note") {
        patch.note = Some(meta_opt_str(map, "note", usize::MAX)?);
    }
    Ok(patch)
}

// --- multipart collection ----------------------------------------------------

struct FilePart {
    bytes: Vec<u8>,
    filename: Option<String>,
    overflow: bool,
}

struct CollectedForm {
    metadata: Option<String>,
    file: Option<FilePart>,
    cover: Option<FilePart>,
}

/// Stream one file field with a cap; over-cap bytes are drained and
/// dropped, and `overflow` records that the true size exceeded it
/// (the caller raises the kind-specific 413 after metadata parsing,
/// preserving Python's router error order).
async fn read_field_capped(
    mut field: axum::extract::multipart::Field<'_>,
    cap: usize,
) -> ApiResult<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    while let Some(chunk) = field.chunk().await.map_err(|_| ApiError::internal())? {
        if overflow {
            continue;
        }
        if bytes.len() + chunk.len() > cap {
            overflow = true;
        } else {
            bytes.extend_from_slice(&chunk);
        }
    }
    Ok((bytes, overflow))
}

async fn collect_multipart(mut mp: Multipart) -> ApiResult<CollectedForm> {
    let mut out = CollectedForm {
        metadata: None,
        file: None,
        cover: None,
    };
    while let Some(field) = mp
        .next_field()
        .await
        .map_err(|_| ApiError::validation(vec![json!({ "msg": "invalid multipart body" })]))?
    {
        match field.name() {
            Some("metadata") => {
                if out.metadata.is_none() {
                    let text = field.text().await.map_err(|_| {
                        ApiError::validation(vec![json!({ "msg": "invalid multipart body" })])
                    })?;
                    out.metadata = Some(text);
                }
            }
            Some("file") => {
                if out.file.is_none() {
                    let filename = field.file_name().map(str::to_string);
                    let (bytes, overflow) = read_field_capped(field, MAX_FILE_BYTES).await?;
                    out.file = Some(FilePart {
                        bytes,
                        filename,
                        overflow,
                    });
                }
            }
            Some("cover") => {
                if out.cover.is_none() {
                    let filename = field.file_name().map(str::to_string);
                    let (bytes, overflow) = read_field_capped(field, MAX_COVER_BYTES).await?;
                    out.cover = Some(FilePart {
                        bytes,
                        filename,
                        overflow,
                    });
                }
            }
            _ => {
                // Drain unknown fields so the next field can be read.
                let mut field = field;
                while let Some(_chunk) = field.chunk().await.map_err(|_| ApiError::internal())? {}
            }
        }
    }
    Ok(out)
}

// --- lexical search + ingestion chunking --------------------------------------

/// `service._lexical_rank` — title 3 / tags 2 / description 1.5 /
/// transcript-or-OCR 1 per query token; `matched_in` is the sorted set.
fn lexical_rank(query: &str, item: &FullItem) -> (f64, Vec<String>) {
    let toks: Vec<String> = query
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(str::to_string)
        .collect();
    if toks.is_empty() {
        return (0.0, vec![]);
    }
    let title_b = py_json_dumps(&item.title).to_lowercase();
    let desc_b = item
        .description
        .as_ref()
        .map(py_json_dumps)
        .unwrap_or_default()
        .to_lowercase();
    let tags_b = item
        .topic_tags
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
        .to_lowercase();
    let text_b = [item.transcript.as_deref(), item.ocr_text.as_deref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    let transcript_lower = item.transcript.as_deref().map(|s| s.to_lowercase());
    let mut score = 0.0;
    let mut matched: Vec<String> = Vec::new();
    for t in &toks {
        if title_b.contains(t.as_str()) {
            score += 3.0;
            matched.push("title".to_string());
        }
        if tags_b.contains(t.as_str()) {
            score += 2.0;
            matched.push("tags".to_string());
        }
        if desc_b.contains(t.as_str()) {
            score += 1.5;
            matched.push("description".to_string());
        }
        if text_b.contains(t.as_str()) {
            score += 1.0;
            let in_transcript = transcript_lower
                .as_ref()
                .map(|tl| tl.contains(t.as_str()))
                .unwrap_or(false);
            matched.push(if in_transcript { "transcript" } else { "ocr" }.to_string());
        }
    }
    matched.sort();
    matched.dedup();
    (score, matched)
}

/// `ingestion.normalize_text` — whitespace-only normalization (not
/// the diacritic-stripping `embeddings.normalize_text`).
fn ingestion_normalize(raw: &str) -> String {
    let step = raw.replace("\r\n", "\n").replace('\r', "\n");
    let mut out = String::with_capacity(step.len());
    let mut in_space = false;
    for c in step.chars() {
        if c == ' ' || c == '\t' {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
        } else {
            in_space = false;
            out.push(c);
        }
    }
    let mut out2 = String::with_capacity(out.len());
    let mut nl_run = 0;
    for c in out.chars() {
        if c == '\n' {
            nl_run += 1;
            if nl_run <= 2 {
                out2.push('\n');
            }
        } else {
            nl_run = 0;
            out2.push(c);
        }
    }
    out2.trim().to_string()
}

/// `re.split(r"\n\s*\n", text)` — split on blank-line runs.
fn split_paragraphs(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\n' {
            let mut j = i;
            let mut newlines = 0;
            while j < chars.len() && matches!(chars[j], '\n' | ' ' | '\t' | '\x0b' | '\x0c') {
                if chars[j] == '\n' {
                    newlines += 1;
                }
                j += 1;
            }
            if newlines >= 2 {
                parts.push(chars[start..i].iter().collect());
                i = j;
                start = j;
                continue;
            }
        }
        i += 1;
    }
    parts.push(chars[start..].iter().collect());
    parts
}

/// `re.split(r"(?<=[.!?؟。])\s+", p)` — split after a sentence-ender
/// followed by whitespace (Rust `regex` has no lookbehind).
fn split_sentences(p: &str) -> Vec<String> {
    let chars: Vec<char> = p.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < chars.len() {
        if matches!(chars[i], '.' | '!' | '?' | '؟' | '。')
            && i + 1 < chars.len()
            && chars[i + 1].is_whitespace()
        {
            out.push(chars[start..=i].iter().collect());
            let mut j = i + 1;
            while j < chars.len() && chars[j].is_whitespace() {
                j += 1;
            }
            start = j;
            i = j;
            continue;
        }
        i += 1;
    }
    if start < chars.len() {
        out.push(chars[start..].iter().collect());
    }
    out
}

/// `fatwa.embeddings.chunk_text_paragraphs` — paragraph accumulation
/// to ~500 words, 50-word overlap, 20-word minimum.
fn chunk_text_paragraphs(text: &str) -> Vec<String> {
    if text.trim().is_empty() {
        return vec![];
    }
    let paragraphs: Vec<String> = split_paragraphs(text)
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    let mut units: Vec<String> = Vec::new();
    for p in &paragraphs {
        let words = p.split_whitespace().count();
        if words <= 500 {
            units.push(p.clone());
        } else {
            let sentences = split_sentences(p);
            let mut buf: Vec<String> = Vec::new();
            for s in sentences {
                buf.push(s);
                let total: usize = buf.iter().map(|x| x.split_whitespace().count()).sum();
                if total >= 500 {
                    units.push(buf.join(" "));
                    buf = Vec::new();
                }
            }
            if !buf.is_empty() {
                units.push(buf.join(" "));
            }
        }
    }
    let mut chunks: Vec<String> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    let mut current_words = 0usize;
    for u in &units {
        let w = u.split_whitespace().count();
        if !current.is_empty() && current_words + w > 500 {
            let text_chunk = current.join("\n\n");
            if text_chunk.split_whitespace().count() >= 20 {
                chunks.push(text_chunk.clone());
            }
            let words: Vec<&str> = text_chunk.split_whitespace().collect();
            let tail = if words.len() > 50 {
                &words[words.len() - 50..]
            } else {
                &words[..]
            };
            current = if tail.is_empty() {
                vec![]
            } else {
                vec![tail.join(" ")]
            };
            current_words = tail.len();
        }
        current.push(u.clone());
        current_words += w;
    }
    if !current.is_empty() {
        let text_chunk = current.join("\n\n");
        if text_chunk.split_whitespace().count() >= 20 {
            chunks.push(text_chunk);
        }
    }
    chunks
}

/// `service._indexable_text` — fa/ar/en title + description, then
/// transcript, then OCR text.
fn indexable_text(item: &FullItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    for loc in ["fa", "ar", "en"] {
        if let Some(t) = item.title.get(loc).and_then(|v| v.as_str()) {
            if !t.is_empty() {
                parts.push(t.to_string());
            }
        }
        if let Some(d) = item
            .description
            .as_ref()
            .and_then(|v| v.get(loc))
            .and_then(|v| v.as_str())
        {
            if !d.is_empty() {
                parts.push(d.to_string());
            }
        }
    }
    if let Some(t) = &item.transcript {
        if !t.is_empty() {
            parts.push(t.clone());
        }
    }
    if let Some(o) = &item.ocr_text {
        if !o.is_empty() {
            parts.push(o.clone());
        }
    }
    parts.join("\n\n").trim().to_string()
}

// --- shared service helpers --------------------------------------------------

/// `service._snapshot_of` — the archived version snapshot.
fn snapshot_of(item: &FullItem) -> Value {
    json!({
        "kind": item.kind,
        "title": item.title,
        "description": item.description.clone().unwrap_or(Value::Null),
        "topic_tags": item.topic_tags,
        "media_key": item.media_key,
        "external_url": item.external_url,
        "cover_media_key": item.cover_media_key,
        "visibility": item.visibility,
        "scholar_id": item.scholar_id.map(|u| u.to_string()),
        "ai_scopes": item.ai_scopes,
        "include_in_fatwa_rag": item.include_in_fatwa_rag,
        "download_policy": item.download_policy.clone().unwrap_or(Value::Null),
        "duration_seconds": item.duration_seconds,
    })
}

async fn archive_version(
    db: &Db,
    item: &FullItem,
    actor_id: Option<Uuid>,
    note: Option<String>,
) -> ApiResult<()> {
    db.execute(
        "INSERT INTO library_item_versions (id, item_id, version, snapshot, created_by, \
         created_at, note) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            new_id().into_arg(),
            item.id.into_arg(),
            item.version.into_arg(),
            snapshot_of(item).into_arg(),
            actor_id.into_arg(),
            utc_now().into_arg(),
            note.into_arg(),
        ],
    )
    .await?;
    Ok(())
}

fn new_item_base(meta_kind: &str, actor_id: Uuid, now: DateTime<Utc>) -> FullItem {
    FullItem {
        id: new_id(),
        kind: meta_kind.to_string(),
        title: json!({}),
        description: None,
        cover_media_key: None,
        media_key: None,
        external_url: None,
        scholar_id: None,
        topic_tags: Value::Array(vec![]),
        related_lesson_id: None,
        religious_place_ref: None,
        visibility: "registered".to_string(),
        workflow_status: "draft".to_string(),
        version: 1,
        ai_scopes: Value::Array(vec![]),
        include_in_fatwa_rag: false,
        ai_indexed: false,
        download_policy: None,
        transcript: None,
        transcript_status: "pending".to_string(),
        ocr_text: None,
        ocr_status: "pending".to_string(),
        processing_error: None,
        duration_seconds: None,
        file_size_bytes: None,
        mime_type: None,
        source_type: "upload".to_string(),
        source_ref: None,
        created_by: Some(actor_id),
        created_at: now,
        updated_at: now,
        published_at: None,
    }
}

async fn reload_item(db: &Db, item_id: Uuid) -> ApiResult<FullItem> {
    get_item_or_404(db, item_id).await
}

/// `service._create_external_item` — YouTube / Aparat / direct-file
/// video, no media file, no pipeline.
#[allow(clippy::too_many_arguments)]
async fn create_external_item(
    state: &AppState,
    db: &Db,
    meta: &CreateMeta,
    canonical_url: &str,
    cover: Option<(Vec<u8>, Option<String>)>,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> ApiResult<FullItem> {
    // Aparat metadata (title + og cover) is fetched only when needed:
    // no usable manual title, or no uploaded cover.
    let mut aparat_title: Option<String> = None;
    let mut aparat_cover: Option<String> = None;
    if parse_aparat_video_hash(Some(canonical_url)).is_some()
        && (!usable_title(meta.title.as_ref()) || cover.is_none())
    {
        let (t, c) = fetch_aparat_meta(&state.client, canonical_url).await;
        aparat_title = t;
        aparat_cover = c;
    }
    let title = if usable_title(meta.title.as_ref()) {
        meta.title.clone().unwrap()
    } else if parse_youtube_video_id(Some(canonical_url)).is_some() {
        match fetch_youtube_title(&state.client, canonical_url).await {
            Some(fetched) => json!({"fa": fetched, "ar": fetched, "en": fetched}),
            None => {
                return Err(ApiError::new(
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "title_required",
                    "عنوان ویدیو به‌دست نیامد؛ عنوان را دستی وارد کنید",
                ));
            }
        }
    } else if let Some(t) = aparat_title {
        json!({"fa": t, "ar": t, "en": t})
    } else {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "title_required",
            "عنوان ویدیو به‌دست نیامد؛ عنوان را دستی وارد کنید",
        ));
    };

    let mut cover_key: Option<String> = None;
    if let Some((cover_bytes, cover_filename)) = &cover {
        scan_upload(
            db,
            &state.config,
            "library.upload_rejected_infected",
            "library_item",
            ScanAudit {
                actor_id: Some(actor.user.id),
                actor_type: "admin",
                ip,
                user_agent,
            },
            cover_bytes,
            cover_filename.as_deref(),
        )
        .await?;
        let (cext, _) = validate_cover(cover_filename.as_deref(), cover_bytes)?;
        let key = format!("library/covers/{}{}", Uuid::new_v4().simple(), cext);
        put_object(&state.config, &key, cover_bytes).await?;
        cover_key = Some(key);
    } else if let Some(url) = aparat_cover {
        cover_key = store_fetched_cover(state, db, actor.user.id, &url, ip, user_agent).await;
    }

    let now = utc_now();
    let mut item = new_item_base(&meta.kind, actor.user.id, now);
    item.title = title;
    item.description = meta.description.clone();
    item.cover_media_key = cover_key;
    item.external_url = Some(canonical_url.to_string());
    item.scholar_id = meta.scholar_id;
    item.topic_tags = Value::Array(
        meta.topic_tags
            .iter()
            .map(|t| Value::String(t.clone()))
            .collect(),
    );
    item.related_lesson_id = meta.related_lesson_id;
    item.religious_place_ref = meta.religious_place_ref.clone();
    item.visibility = meta.visibility.clone();
    item.ai_scopes = Value::Array(
        meta.ai_scopes
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    item.include_in_fatwa_rag = meta.include_in_fatwa_rag;
    item.download_policy = meta.download_policy.clone();
    item.duration_seconds = meta.duration_seconds;
    item.insert(db).await?;

    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        "library.item.created",
        json!({
            "kind": item.kind,
            "visibility": item.visibility,
            "source_type": "upload",
            "external_url": canonical_url,
        }),
    )
    .await?;
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "library.item.create",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({
            "kind": item.kind,
            "visibility": item.visibility,
            "external_url": canonical_url,
        })),
        ip,
        user_agent,
    )
    .await?;
    reload_item(db, item.id).await
}

/// Upload path of `service.create_item` — scan first, then validate,
/// then store, then insert (draft; pipeline statuses stay pending).
#[allow(clippy::too_many_arguments)]
async fn create_upload_item(
    state: &AppState,
    db: &Db,
    meta: &CreateMeta,
    content: Vec<u8>,
    filename: Option<String>,
    cover: Option<(Vec<u8>, Option<String>)>,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> ApiResult<FullItem> {
    if !usable_title(meta.title.as_ref()) {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "title_required",
            "عنوان الزامی است",
        ));
    }
    scan_upload(
        db,
        &state.config,
        "library.upload_rejected_infected",
        "library_item",
        ScanAudit {
            actor_id: Some(actor.user.id),
            actor_type: "admin",
            ip,
            user_agent,
        },
        &content,
        filename.as_deref(),
    )
    .await?;
    if let Some((cover_bytes, cover_filename)) = &cover {
        scan_upload(
            db,
            &state.config,
            "library.upload_rejected_infected",
            "library_item",
            ScanAudit {
                actor_id: Some(actor.user.id),
                actor_type: "admin",
                ip,
                user_agent,
            },
            cover_bytes,
            cover_filename.as_deref(),
        )
        .await?;
    }
    let (ext, mime) = validate_upload(&meta.kind, filename.as_deref(), &content)?;
    let media_key = format!("library/{}/{}{}", meta.kind, Uuid::new_v4().simple(), ext);
    put_object(&state.config, &media_key, &content).await?;
    let mut cover_key: Option<String> = None;
    if let Some((cover_bytes, cover_filename)) = &cover {
        let (cext, _) = validate_cover(cover_filename.as_deref(), cover_bytes)?;
        let key = format!("library/covers/{}{}", Uuid::new_v4().simple(), cext);
        put_object(&state.config, &key, cover_bytes).await?;
        cover_key = Some(key);
    }
    // The media pipeline (transcribe/OCR) is a Python worker: with the
    // auto-pipeline flag on, statuses go `pending` (they already are)
    // and the enqueue is a no-op returning false.
    let _ = auto_pipeline_enabled(db, &state.config).await?;

    let now = utc_now();
    let size = content.len() as i32;
    let mut item = new_item_base(&meta.kind, actor.user.id, now);
    item.title = meta.title.clone().unwrap();
    item.description = meta.description.clone();
    item.cover_media_key = cover_key;
    item.media_key = Some(media_key);
    item.scholar_id = meta.scholar_id;
    item.topic_tags = Value::Array(
        meta.topic_tags
            .iter()
            .map(|t| Value::String(t.clone()))
            .collect(),
    );
    item.related_lesson_id = meta.related_lesson_id;
    item.religious_place_ref = meta.religious_place_ref.clone();
    item.visibility = meta.visibility.clone();
    item.ai_scopes = Value::Array(
        meta.ai_scopes
            .iter()
            .map(|s| Value::String(s.clone()))
            .collect(),
    );
    item.include_in_fatwa_rag = meta.include_in_fatwa_rag;
    item.download_policy = meta.download_policy.clone();
    item.duration_seconds = meta.duration_seconds;
    item.file_size_bytes = Some(size);
    item.mime_type = Some(mime.clone());
    item.insert(db).await?;

    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        "library.item.created",
        json!({
            "kind": item.kind,
            "visibility": item.visibility,
            "source_type": "upload",
            "file_size_bytes": content.len(),
        }),
    )
    .await?;
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "library.item.create",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({
            "kind": item.kind,
            "visibility": item.visibility,
            "file_size_bytes": content.len(),
            "mime_type": mime,
        })),
        ip,
        user_agent,
    )
    .await?;
    reload_item(db, item.id).await
}

/// Shared workflow transition (transition route + archive/delete).
async fn do_transition(
    db: &Db,
    item: &mut FullItem,
    to: &str,
    note: Option<String>,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> ApiResult<()> {
    let current = item.workflow_status.clone();
    validate_transition(&current, to)?;
    item.workflow_status = to.to_string();
    let now = utc_now();
    item.updated_at = now;
    if to == PUBLISHED && item.published_at.is_none() {
        item.published_at = Some(now);
    }
    item.save(db).await?;
    let note_flag = note.as_ref().map(|n| !n.is_empty()).unwrap_or(false);
    let event_type = if to == PUBLISHED {
        "library.item.published"
    } else {
        "library.item.transition"
    };
    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        event_type,
        json!({"from": current, "to": to, "version": item.version, "note": note_flag}),
    )
    .await?;
    let mut new_values = json!({"workflow_status": to, "version": item.version});
    if let Some(n) = &note {
        if !n.is_empty() {
            new_values["note"] = json!(n);
        }
    }
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "library.item.transition",
        "library_item",
        Some(&item.id.to_string()),
        Some(json!({"workflow_status": current})),
        Some(new_values),
        ip,
        user_agent,
    )
    .await?;
    Ok(())
}

async fn has_permission(db: &Db, user_id: Uuid, key: &str) -> ApiResult<bool> {
    let row = db
        .fetch_optional(
            "SELECT p.id FROM user_roles ur \
             JOIN role_permissions rp ON rp.role_id = ur.role_id \
             JOIN permissions p ON p.id = rp.permission_id \
             WHERE ur.user_id = ? AND p.key = ? LIMIT 1",
            &[user_id.into_arg(), key.into_arg()],
        )
        .await?;
    Ok(row.is_some())
}

// --- public ------------------------------------------------------------------

/// `GET /api/v1/library/items` — published items, visibility-filtered,
/// cursor-paginated (`limit` 1–100, default 20).
pub async fn public_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let params = parse_query(&uri);
    let kind = match params.get("kind") {
        Some(k) if KINDS.contains(&k.as_str()) => Some(k.clone()),
        Some(_) => return Err(validation_err("kind", "invalid kind")),
        None => None,
    };
    let scholar_id = match params.get("scholar_id") {
        Some(s) => Some(parse_path_uuid(s, "scholar_id")?),
        None => None,
    };
    let tag = params.get("tag").cloned();
    let q = params.get("q").cloned();
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let cursor = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;
    let privileged = is_privileged(db, user.as_ref()).await?;
    let mut sql = format!(
        "SELECT {FULL_COLUMNS} FROM library_items WHERE {}",
        public_where(privileged, user.is_some())
    );
    let mut args: Vec<Arg> = Vec::new();
    if let Some(k) = &kind {
        sql.push_str(" AND kind = ?");
        args.push(k.clone().into_arg());
    }
    if let Some(s) = scholar_id {
        sql.push_str(" AND scholar_id = ?");
        args.push(s.into_arg());
    }
    if let Some(t) = &tag {
        sql.push_str(" AND LOWER(CAST(topic_tags AS TEXT)) LIKE ?");
        args.push(format!("%{}%", t.to_lowercase()).into_arg());
    }
    if let Some(qq) = &q {
        let like = format!("%{}%", qq.to_lowercase());
        sql.push_str(
            " AND (LOWER(CAST(title AS TEXT)) LIKE ? \
             OR LOWER(CAST(description AS TEXT)) LIKE ? \
             OR LOWER(CAST(topic_tags AS TEXT)) LIKE ?)",
        );
        args.push(like.clone().into_arg());
        args.push(like.clone().into_arg());
        args.push(like.into_arg());
    }
    if let Some(c) = &cursor {
        let (ts, lid) = decode_cursor(c)?;
        sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    for row in rows {
        items.push(FullItem::from_row(&row)?);
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let last = &items[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(last.created_at, last.id));
        items.truncate(limit as usize);
    }
    let mut out = Vec::new();
    for item in &items {
        out.push(item_out_with_urls(&state, item, user.as_ref(), privileged).await?);
    }
    Ok(Json(json!({ "items": out, "next_cursor": next_cursor })))
}

/// `GET /api/v1/library/search` — lexical search over title/tags/
/// description/transcript/OCR, Backend-B style scan of the newest 500
/// visible items, `limit` 1–50, default 20.
pub async fn search_items(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let user = optional_user(&state, &headers).await;
    let params = parse_query(&uri);
    let q = match params.get("q") {
        Some(v) if v.chars().count() >= 2 => v.clone(),
        _ => return Err(validation_err("q", "must be at least 2 characters")),
    };
    let limit = query_int(&params, "limit", 20, 1, 50)?;
    let db = state.db_or_500()?;
    let privileged = is_privileged(db, user.as_ref()).await?;
    let sql = format!(
        "SELECT {FULL_COLUMNS} FROM library_items WHERE {} \
         ORDER BY created_at DESC, id DESC LIMIT 500",
        public_where(privileged, user.is_some())
    );
    let rows = db.fetch_all(&sql, &[]).await?;
    let mut hits: Vec<(f64, Vec<String>, FullItem)> = Vec::new();
    for row in rows {
        let item = FullItem::from_row(&row)?;
        let (score, matched) = lexical_rank(&q, &item);
        if score > 0.0 {
            hits.push((score, matched, item));
        }
    }
    // Stable sort by score desc (ties keep the created_at DESC order).
    hits.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    hits.truncate(limit.clamp(1, 50) as usize);
    let mut out = Vec::new();
    for (score, matched, item) in &hits {
        // Rust rounds half away from zero; Python `round(x, 3)` is
        // banker's — an exact-half edge difference only (ADR-024 class).
        let rounded = (score * 1000.0).round() / 1000.0;
        out.push(json!({
            "item": item_out_with_urls(&state, item, user.as_ref(), privileged).await?,
            "score": rounded,
            "matched_in": matched,
        }));
    }
    Ok(Json(json!({ "query": q, "hits": out })))
}

/// `GET /api/v1/library/items/{item_id}` — published + visible only.
pub async fn public_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let item = get_detail_item(db, item_id, user.as_ref()).await?;
    let privileged = is_privileged(db, user.as_ref()).await?;
    Ok(Json(
        item_out_with_urls(&state, &item, user.as_ref(), privileged).await?,
    ))
}

/// `GET /api/v1/library/items/{item_id}/stream` — 302 to the signed
/// media URL; rate-limited 60/min per IP (distributed limiter,
/// auth-sensitive); access is audited.
pub async fn stream_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
) -> ApiResult<Response<Body>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let ip_key = ip.clone().unwrap_or_else(|| "unknown".to_string());
    if !state
        .limiter
        .is_allowed(
            &format!("library_stream:{ip_key}"),
            60,
            60,
            true,
            &state.config.app_env,
        )
        .await
    {
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "تعداد درخواست بیش از حد مجاز است",
        ));
    }
    let user = optional_user(&state, &headers).await;
    let db = state.db_or_500()?;
    let item = get_detail_item(db, item_id, user.as_ref()).await?;
    let privileged = is_privileged(db, user.as_ref()).await?;
    let (media_url, _) = urls_for(&state, &item, user.is_some(), privileged)?;
    let Some(media_url) = media_url else {
        return Err(not_found_item());
    };
    log_audit(
        db,
        user.as_ref().map(|c| c.user.id),
        if user.is_some() { "user" } else { "anonymous" },
        "library.item.stream",
        "library_item",
        Some(&item_id.to_string()),
        None,
        Some(json!({ "kind": item.kind })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let res = Response::builder()
        .status(StatusCode::FOUND)
        .header("location", media_url)
        .body(Body::empty())
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}

/// `GET /api/v1/library/download-by-ref` — the signed download: the
/// signature (purpose `library`) is the authentication. No visibility
/// or workflow check — the capability is the URL itself.
pub async fn download_by_ref(State(state): State<AppState>, uri: Uri) -> ApiResult<Response<Body>> {
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
    if purpose != SIGNED_PURPOSE_LIBRARY
        || !verify_download_sig(&state.config, &key, exp, &sig, SIGNED_PURPOSE_LIBRARY)
    {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "invalid_signature",
            "امضای دانلود نامعتبر یا منقضی است",
        ));
    }
    let db = state.db_or_500()?;
    let row = db
        .fetch_optional(
            &format!(
                "SELECT {FULL_COLUMNS} FROM library_items \
                 WHERE media_key = ? OR cover_media_key = ? LIMIT 1"
            ),
            &[key.clone().into_arg(), key.clone().into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found("key_not_found", "فایل یافت نشد"));
    };
    let item = FullItem::from_row(&row)?;
    let mime = if item.cover_media_key.as_deref() == Some(key.as_str()) {
        ext_mime(&file_ext(Some(&key)))
            .unwrap_or("application/octet-stream")
            .to_string()
    } else {
        item.mime_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_string())
    };
    // Python raises FileNotFoundError here → the generic 500 handler.
    let data = read_bytes(&state.config, &key)
        .await
        .map_err(|_| ApiError::internal())?;
    let res = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", mime)
        .body(Body::from(data))
        .map_err(|_| ApiError::internal())?;
    Ok(res)
}

// --- admin -------------------------------------------------------------------

/// `GET /api/v1/admin/library/items` — every workflow status; admin
/// items carry transcript/OCR/error but `media_url`/`cover_url` are
/// null in the list, exactly like Python's `_to_out(..., None, None)`.
/// With `q`, filtering happens in Rust over the newest 500 rows
/// (LIKE on JSON is unreliable for Persian/Arabic).
pub async fn admin_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
) -> ApiResult<Json<Value>> {
    let _ctx = admin_ctx(&state, &headers).await?;
    let params = parse_query(&uri);
    let kind = match params.get("kind") {
        Some(k) if KINDS.contains(&k.as_str()) => Some(k.clone()),
        Some(_) => return Err(validation_err("kind", "invalid kind")),
        None => None,
    };
    let status = match params.get("status") {
        Some(s) if WORKFLOW_STATUSES.contains(&s.as_str()) => Some(s.clone()),
        Some(_) => return Err(validation_err("status", "invalid status")),
        None => None,
    };
    let source_type = params.get("source_type").cloned();
    let q = params.get("q").cloned();
    let limit = query_int(&params, "limit", 20, 1, 100)?;
    let cursor = params.get("after").filter(|s| !s.is_empty()).cloned();
    let db = state.db_or_500()?;

    if let Some(qq) = &q {
        let mut sql = format!("SELECT {FULL_COLUMNS} FROM library_items WHERE 1 = 1");
        let mut args: Vec<Arg> = Vec::new();
        if let Some(k) = &kind {
            sql.push_str(" AND kind = ?");
            args.push(k.clone().into_arg());
        }
        if let Some(s) = &status {
            sql.push_str(" AND workflow_status = ?");
            args.push(s.clone().into_arg());
        }
        if let Some(st) = &source_type {
            sql.push_str(" AND source_type = ?");
            args.push(st.clone().into_arg());
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT 500");
        let rows = db.fetch_all(&sql, &args).await?;
        let needle = qq.to_lowercase();
        let mut items: Vec<FullItem> = Vec::new();
        for row in rows {
            let item = FullItem::from_row(&row)?;
            let dumped = py_json_dumps(&json!({
                "t": item.title,
                "d": item.description.clone().unwrap_or(Value::Null),
            }));
            if dumped.to_lowercase().contains(&needle) {
                items.push(item);
            }
        }
        if let Some(c) = &cursor {
            let (ts, lid) = decode_cursor(c)?;
            items.retain(|it| it.created_at < ts || (it.created_at == ts && it.id < lid));
        }
        let mut next_cursor: Option<String> = None;
        if items.len() > limit as usize {
            let last = &items[(limit as usize) - 1];
            next_cursor = Some(encode_cursor(last.created_at, last.id));
            items.truncate(limit as usize);
        }
        let out: Vec<Value> = items.iter().map(|it| admin_json(it, None, None)).collect();
        return Ok(Json(json!({ "items": out, "next_cursor": next_cursor })));
    }

    let mut sql = format!("SELECT {FULL_COLUMNS} FROM library_items WHERE 1 = 1");
    let mut args: Vec<Arg> = Vec::new();
    if let Some(k) = &kind {
        sql.push_str(" AND kind = ?");
        args.push(k.clone().into_arg());
    }
    if let Some(s) = &status {
        sql.push_str(" AND workflow_status = ?");
        args.push(s.clone().into_arg());
    }
    if let Some(st) = &source_type {
        sql.push_str(" AND source_type = ?");
        args.push(st.clone().into_arg());
    }
    if let Some(c) = &cursor {
        let (ts, lid) = decode_cursor(c)?;
        sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
        args.push(ts.into_arg());
        args.push(ts.into_arg());
        args.push(lid.into_arg());
    }
    sql.push_str(" ORDER BY created_at DESC, id DESC LIMIT ?");
    args.push((limit + 1).into_arg());
    let rows = db.fetch_all(&sql, &args).await?;
    let mut items = Vec::new();
    for row in rows {
        items.push(FullItem::from_row(&row)?);
    }
    let mut next_cursor: Option<String> = None;
    if items.len() > limit as usize {
        let last = &items[(limit as usize) - 1];
        next_cursor = Some(encode_cursor(last.created_at, last.id));
        items.truncate(limit as usize);
    }
    let out: Vec<Value> = items.iter().map(|it| admin_json(it, None, None)).collect();
    Ok(Json(json!({ "items": out, "next_cursor": next_cursor })))
}

/// `GET /api/v1/admin/library/items/{item_id}` — admin detail with
/// transcript/OCR/error and signed URLs.
pub async fn admin_get(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let item = get_item_or_404(db, item_id).await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok(Json(
        admin_out_with_urls(&state, &item, Some(&ctx), privileged).await?,
    ))
}

/// `POST /api/v1/admin/library/items` — multipart create (201).
/// Metadata JSON errors are 422 `invalid_metadata`; the file is read
/// capped by kind *after* metadata parsing, like the Python router.
pub async fn admin_create(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    multipart: Multipart,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let form = collect_multipart(multipart).await?;
    let meta_raw = form
        .metadata
        .ok_or_else(|| validation_err("metadata", "field is required"))?;
    let meta_value: Value =
        serde_json::from_str(&meta_raw).map_err(|e| invalid_metadata(&e.to_string()))?;
    let meta = parse_create_meta(&meta_value)?;
    // Size gate (Python's capped read): the file must fit the kind
    // limit; the cover must fit 5 MB.
    if let Some(f) = &form.file {
        let limit = upload_limit_for_kind(&meta.kind)?;
        if f.overflow || f.bytes.len() as i64 > limit {
            return Err(file_too_large(limit));
        }
    }
    if let Some(c) = &form.cover {
        if c.overflow || c.bytes.len() > MAX_COVER_BYTES {
            return Err(file_too_large(5 * 1024 * 1024));
        }
    }
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let file = form.file.map(|f| (f.bytes, f.filename));
    let cover = form.cover.map(|f| (f.bytes, f.filename));
    let item = match &file {
        None if meta
            .external_url
            .as_deref()
            .map(|s| !s.is_empty())
            .unwrap_or(false) =>
        {
            let canonical =
                canonical_external_url(&meta.kind, meta.external_url.as_ref().unwrap())?;
            create_external_item(
                &state,
                db,
                &meta,
                &canonical,
                cover,
                &ctx,
                ip.as_deref(),
                ua.as_deref(),
            )
            .await?
        }
        None => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "file_required",
                "فایل رسانه الزامی است",
            ));
        }
        Some((content, filename)) => {
            create_upload_item(
                &state,
                db,
                &meta,
                content.clone(),
                filename.clone(),
                cover,
                &ctx,
                ip.as_deref(),
                ua.as_deref(),
            )
            .await?
        }
    };
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok((
        StatusCode::CREATED,
        Json(item_out_with_urls(&state, &item, Some(&ctx), privileged).await?),
    ))
}

/// `PUT /api/v1/admin/library/items/{item_id}` — metadata patch +
/// optional replacement file; the previous version is archived, never
/// deleted.
pub async fn admin_update(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
    multipart: Multipart,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let form = collect_multipart(multipart).await?;
    let meta_raw = form
        .metadata
        .ok_or_else(|| validation_err("metadata", "field is required"))?;
    let meta_value: Value =
        serde_json::from_str(&meta_raw).map_err(|e| invalid_metadata(&e.to_string()))?;
    let patch = parse_update_meta(&meta_value)?;
    // Kind lookup first (404), like Python's `get_item_kind`, then the
    // size gate on the replacement file.
    let mut item = get_item_or_404(db, item_id).await?;
    if let Some(f) = &form.file {
        let limit = upload_limit_for_kind(&item.kind)?;
        if f.overflow || f.bytes.len() as i64 > limit {
            return Err(file_too_large(limit));
        }
    }
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    let new_content = form.file.map(|f| (f.bytes, f.filename));
    update_item_inner(
        &state,
        db,
        &mut item,
        &patch,
        new_content,
        &ctx,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let item = reload_item(db, item_id).await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok(Json(
        item_out_with_urls(&state, &item, Some(&ctx), privileged).await?,
    ))
}

/// `service.update_item` — scan before archiving (a rejected file
/// must not create a version), external link validated before
/// archiving, file wins over the link.
#[allow(clippy::too_many_arguments)]
async fn update_item_inner(
    state: &AppState,
    db: &Db,
    item: &mut FullItem,
    patch: &UpdatePatch,
    new_content: Option<(Vec<u8>, Option<String>)>,
    actor: &AuthCtx,
    ip: Option<&str>,
    user_agent: Option<&str>,
) -> ApiResult<()> {
    let old_version = item.version;
    let old_title = item.title.clone();
    let old_visibility = item.visibility.clone();

    if let Some((content, filename)) = &new_content {
        scan_upload(
            db,
            &state.config,
            "library.upload_rejected_infected",
            "library_item",
            ScanAudit {
                actor_id: Some(actor.user.id),
                actor_type: "admin",
                ip,
                user_agent,
            },
            content,
            filename.as_deref(),
        )
        .await?;
    }

    // External link in the patch: validated before archiving; not
    // allowed on an item that has a file, and a fresh file wins.
    let external_in_patch = patch.external_url.is_some();
    let mut new_external: Option<Option<String>> = None;
    if external_in_patch && new_content.is_none() {
        match &patch.external_url {
            Some(Some(url)) => {
                if item.media_key.is_some() {
                    return Err(ApiError::new(
                        StatusCode::UNPROCESSABLE_ENTITY,
                        "external_url_not_allowed",
                        "این آیتم فایل دارد؛ لینک خارجی فقط روی آیتم بدون فایل مجاز است",
                    ));
                }
                new_external = Some(Some(canonical_external_url(&item.kind, url)?));
            }
            Some(None) => new_external = Some(None),
            None => {}
        }
    }

    archive_version(db, item, Some(actor.user.id), patch.note.clone().flatten()).await?;

    if let Some((content, filename)) = new_content {
        let (ext, mime) = validate_upload(&item.kind, filename.as_deref(), &content)?;
        let media_key = format!("library/{}/{}{}", item.kind, Uuid::new_v4().simple(), ext);
        put_object(&state.config, &media_key, &content).await?;
        item.media_key = Some(media_key);
        item.external_url = None;
        item.file_size_bytes = Some(content.len() as i32);
        item.mime_type = Some(mime);
        // Replaced content invalidates the old transcript/OCR/AI index.
        item.transcript = None;
        item.transcript_status = "pending".to_string();
        item.ocr_text = None;
        item.ocr_status = "pending".to_string();
        item.processing_error = None;
        item.ai_indexed = false;
        let _ = auto_pipeline_enabled(db, &state.config).await?;
    }

    if let Some(Some(v)) = &patch.title {
        item.title = v.clone();
    }
    if let Some(v) = &patch.description {
        item.description = v.clone();
    }
    if let Some(v) = &patch.scholar_id {
        item.scholar_id = *v;
    }
    if let Some(Some(v)) = &patch.topic_tags {
        item.topic_tags = Value::Array(v.iter().map(|t| Value::String(t.clone())).collect());
    }
    if let Some(v) = &patch.related_lesson_id {
        item.related_lesson_id = *v;
    }
    if let Some(v) = &patch.religious_place_ref {
        item.religious_place_ref = v.clone();
    }
    if let Some(Some(v)) = &patch.visibility {
        item.visibility = v.clone();
    }
    if let Some(Some(v)) = &patch.ai_scopes {
        item.ai_scopes = Value::Array(v.iter().map(|s| Value::String(s.clone())).collect());
    }
    if let Some(Some(v)) = &patch.include_in_fatwa_rag {
        item.include_in_fatwa_rag = *v;
    }
    if let Some(v) = &patch.download_policy {
        item.download_policy = v.clone();
    }
    if let Some(v) = &patch.duration_seconds {
        item.duration_seconds = *v;
    }
    if let Some(v) = new_external {
        item.external_url = v;
    }

    item.version = item.version.saturating_add(1);
    item.updated_at = utc_now();
    item.save(db).await?;

    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        "library.item.updated",
        json!({"version": item.version, "previous_version": old_version}),
    )
    .await?;
    log_audit(
        db,
        Some(actor.user.id),
        "admin",
        "library.item.update",
        "library_item",
        Some(&item.id.to_string()),
        Some(json!({"version": old_version, "title": old_title, "visibility": old_visibility})),
        Some(json!({"version": item.version, "title": item.title, "visibility": item.visibility})),
        ip,
        user_agent,
    )
    .await?;
    Ok(())
}

/// `POST /api/v1/admin/library/items/{item_id}/transition` — a step
/// on the shared workflow machine (§9); invalid → 422.
pub async fn transition_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let to = match parsed.get("to") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("to", "field is required")),
    };
    let note = match parsed.get("note") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err(validation_err("note", "must be a string")),
    };
    let db = state.db_or_500()?;
    let mut item = get_item_or_404(db, item_id).await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    do_transition(db, &mut item, &to, note, &ctx, ip.as_deref(), ua.as_deref()).await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok(Json(
        item_out_with_urls(&state, &item, Some(&ctx), privileged).await?,
    ))
}

/// `DELETE /api/v1/admin/library/items/{item_id}` — delete is archive
/// (the record is never physically deleted).
pub async fn admin_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let mut item = get_item_or_404(db, item_id).await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    do_transition(
        db,
        &mut item,
        ARCHIVED,
        None,
        &ctx,
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok(Json(
        item_out_with_urls(&state, &item, Some(&ctx), privileged).await?,
    ))
}

/// `GET /api/v1/admin/library/items/{item_id}/versions` — archived
/// versions, newest first.
pub async fn list_versions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let _ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    get_item_or_404(db, item_id).await?;
    let rows = db
        .fetch_all(
            "SELECT version, snapshot, created_by, created_at, note \
             FROM library_item_versions WHERE item_id = ? ORDER BY version DESC",
            &[item_id.into_arg()],
        )
        .await?;
    let mut out = Vec::new();
    for row in rows {
        let version: i32 = row.try_get("version")?;
        let snapshot: Value = row.try_get("snapshot")?;
        let created_by: Option<Uuid> = row.try_get("created_by")?;
        let created_at: DateTime<Utc> = row.try_get("created_at")?;
        let note: Option<String> = row.try_get("note")?;
        out.push(json!({
            "version": version,
            "snapshot": snapshot,
            "created_by": created_by.map(|u| u.to_string()),
            "created_at": created_at.to_rfc3339(),
            "note": note,
        }));
    }
    Ok(Json(Value::Array(out)))
}

/// `POST /api/v1/admin/library/items/{item_id}/versions/{v}/revert` —
/// archive the current state first, then restore the snapshot.
pub async fn revert_version(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path((item_id, version)): Path<(String, String)>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let target: i64 = version
        .parse()
        .map_err(|_| validation_err("version", "must be an integer"))?;
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = if body.is_empty() {
        Value::Object(Default::default())
    } else {
        parse_body(&body)?
    };
    let note = match parsed.get("note") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err(validation_err("note", "must be a string")),
    };
    let db = state.db_or_500()?;
    let mut item = get_item_or_404(db, item_id).await?;
    if target < i32::MIN as i64 || target > i32::MAX as i64 {
        return Err(ApiError::not_found(
            "version_not_found",
            "نسخهٔ درخواستی یافت نشد",
        ));
    }
    let row = db
        .fetch_optional(
            "SELECT snapshot FROM library_item_versions WHERE item_id = ? AND version = ?",
            &[item_id.into_arg(), (target as i32).into_arg()],
        )
        .await?;
    let Some(row) = row else {
        return Err(ApiError::not_found(
            "version_not_found",
            "نسخهٔ درخواستی یافت نشد",
        ));
    };
    let snapshot: Value = row.try_get("snapshot")?;
    let note = note.or_else(|| Some(format!("revert → v{target}")));
    archive_version(db, &item, Some(ctx.user.id), note).await?;

    let snap = &snapshot;
    if let Some(v) = snap.get("title").filter(|v| v.is_object()) {
        item.title = v.clone();
    }
    if snap.get("description").is_some() {
        item.description = snap.get("description").filter(|v| !v.is_null()).cloned();
    }
    if let Some(v) = snap.get("topic_tags").filter(|v| v.is_array()) {
        item.topic_tags = v.clone();
    }
    if snap.get("media_key").is_some() {
        item.media_key = snap
            .get("media_key")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    if snap.get("external_url").is_some() {
        item.external_url = snap
            .get("external_url")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    if snap.get("cover_media_key").is_some() {
        item.cover_media_key = snap
            .get("cover_media_key")
            .and_then(|v| v.as_str())
            .map(str::to_string);
    }
    if let Some(v) = snap.get("visibility").and_then(|v| v.as_str()) {
        item.visibility = v.to_string();
    }
    item.scholar_id = snap
        .get("scholar_id")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<Uuid>().ok());
    if let Some(v) = snap.get("ai_scopes").filter(|v| v.is_array()) {
        item.ai_scopes = v.clone();
    }
    if let Some(v) = snap.get("include_in_fatwa_rag").and_then(|v| v.as_bool()) {
        item.include_in_fatwa_rag = v;
    }
    if snap.get("download_policy").is_some() {
        item.download_policy = snap
            .get("download_policy")
            .filter(|v| !v.is_null())
            .cloned();
    }
    if snap.get("duration_seconds").is_some() {
        item.duration_seconds = snap
            .get("duration_seconds")
            .and_then(|v| v.as_i64())
            .map(|d| d as i32);
    }
    item.version = item.version.saturating_add(1);
    item.ai_indexed = false;
    item.updated_at = utc_now();
    item.save(db).await?;

    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        "library.item.reverted",
        json!({"to_version": target, "new_version": item.version}),
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "library.item.revert",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({"reverted_to": target, "version": item.version})),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok(Json(
        item_out_with_urls(&state, &item, Some(&ctx), privileged).await?,
    ))
}

/// `fatwa.ingestion.run_ingestion` for a library item — normalize,
/// paragraph-chunk, hash-embed, insert draft chunks, mark the run done.
/// (No ARQ, no pgvector: `embedding_vec` is never named — ADR-030.)
async fn run_library_ingestion(
    state: &AppState,
    db: &Db,
    run_id: Uuid,
    item: &FullItem,
    text: &str,
    scopes: &[String],
) -> ApiResult<Vec<String>> {
    db.execute(
        "UPDATE ingestion_runs SET status = 'running', started_at = ? WHERE id = ?",
        &[utc_now().into_arg(), run_id.into_arg()],
    )
    .await?;
    let normalized = ingestion_normalize(text);
    resolve_provider(state).await?;
    let chunks = chunk_text_paragraphs(&normalized);
    let source_ref = json!({
        "type": "library_item",
        "id": item.id.to_string(),
        "version": item.version.to_string(),
    });
    let citation = json!({
        "fa": item.title.get("fa").cloned().unwrap_or(Value::Null),
        "ar": item.title.get("ar").cloned().unwrap_or(Value::Null),
        "en": item.title.get("en").cloned().unwrap_or(Value::Null),
        "url": format!("/library/items/{}", item.id),
        "library_item_id": item.id.to_string(),
        "kind": item.kind,
    });
    let mut chunk_ids: Vec<String> = Vec::new();
    let now = utc_now();
    for chunk_text in &chunks {
        let vector = hash_embed(chunk_text);
        let chunk_id = new_id();
        db.execute(
            "INSERT INTO fiqh_knowledge_chunks (id, chunk_text, embedding, \
             embedding_provider, embedding_model, embedding_projection, source_type, \
             source_ref, source_library_item_ref, source_citation, topic_category, \
             madhab_scope, workflow_status, approved_by, ai_scopes, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'ingested', ?, NULL, ?, NULL, NULL, 'draft', NULL, ?, ?, ?)",
            &[
                chunk_id.into_arg(),
                chunk_text.clone().into_arg(),
                Value::Array(vector.iter().map(|v| json!(v)).collect()).into_arg(),
                HASH_PROVIDER.into_arg(),
                HASH_MODEL.into_arg(),
                PROJECTION_ID.into_arg(),
                source_ref.clone().into_arg(),
                citation.clone().into_arg(),
                Value::Array(scopes.iter().map(|s| Value::String(s.clone())).collect()).into_arg(),
                now.into_arg(),
                now.into_arg(),
            ],
        )
        .await?;
        chunk_ids.push(chunk_id.to_string());
    }
    let stats = json!({"stages": {
        "extract": {"chars": text.chars().count()},
        "normalize": {"chars": normalized.chars().count()},
        "provenance": source_ref,
        "chunk": {"count": chunks.len()},
        "embed": {"provider": HASH_PROVIDER, "model": HASH_MODEL, "count": chunks.len()},
        "index": {"chunk_ids": chunk_ids},
    }});
    db.execute(
        "UPDATE ingestion_runs SET status = 'done', stats = ?, finished_at = ? WHERE id = ?",
        &[stats.into_arg(), utc_now().into_arg(), run_id.into_arg()],
    )
    .await?;
    Ok(chunk_ids)
}

/// `POST /api/v1/admin/library/items/{item_id}/index-ai` — explicit AI
/// indexing, separate from publication; only approved/published items.
/// Chunk approval is governed by `fatwa.approve` (ADR-0005): without
/// it the chunks stay draft and `ai_indexed` is false.
pub async fn index_ai(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let db = state.db_or_500()?;
    let mut item = get_item_or_404(db, item_id).await?;
    if item.workflow_status != APPROVED && item.workflow_status != PUBLISHED {
        return Err(ApiError::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "item_not_indexable",
            "ایندکس AI فقط روی آیتم تأییدشده/منتشرشده مجاز است (RAG هرگز draft نمی‌بیند)",
            json!({ "workflow_status": item.workflow_status }),
        ));
    }
    let mut scopes: Vec<String> = item
        .ai_scopes
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| AI_SCOPES.contains(s))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if item.include_in_fatwa_rag && !scopes.iter().any(|s| s == "fatwa") {
        scopes.push("fatwa".to_string());
    }
    if scopes.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "no_ai_scopes",
            "برای این آیتم هیچ scope هوش مصنوعی فعالی نیست (ai_scopes خالی و include_in_fatwa_rag=false)",
        ));
    }
    let text = indexable_text(&item);
    if text.is_empty() {
        return Err(ApiError::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "nothing_to_index",
            "متن قابل‌ایندکسی (عنوان/توضیح/transcript/OCR) وجود ندارد",
        ));
    }
    let can_approve = has_permission(db, ctx.user.id, "fatwa.approve").await?;

    let run_id = new_id();
    let now = utc_now();
    db.execute(
        "INSERT INTO ingestion_runs (id, source_type, status, stats, started_at, \
         finished_at, created_at) VALUES (?, 'library_item', 'queued', ?, NULL, NULL, ?)",
        &[
            run_id.into_arg(),
            json!({"run_config": {"item_id": item.id.to_string()}}).into_arg(),
            now.into_arg(),
        ],
    )
    .await?;
    let chunk_ids = match run_library_ingestion(&state, db, run_id, &item, &text, &scopes).await {
        Ok(ids) => ids,
        Err(e) => {
            // Python's except path: the run is marked failed and the
            // error re-raised (partial draft chunks are dropped there;
            // here the failure happens before any chunk is written).
            db.execute(
                "UPDATE ingestion_runs SET status = 'failed', stats = ?, finished_at = ? WHERE id = ?",
                &[
                    json!({"error": format!("{}: {}", e.code, e.message)}).into_arg(),
                    utc_now().into_arg(),
                    run_id.into_arg(),
                ],
            )
            .await?;
            return Err(e);
        }
    };

    let mut approved_count = 0;
    for cid in &chunk_ids {
        let chunk_uuid = Uuid::parse_str(cid).map_err(|_| ApiError::internal())?;
        db.execute(
            "UPDATE fiqh_knowledge_chunks SET source_library_item_ref = ? WHERE id = ?",
            &[item.id.to_string().into_arg(), chunk_uuid.into_arg()],
        )
        .await?;
        if can_approve {
            db.execute(
                "UPDATE fiqh_knowledge_chunks SET workflow_status = 'approved', approved_by = ? WHERE id = ?",
                &[ctx.user.id.into_arg(), chunk_uuid.into_arg()],
            )
            .await?;
            approved_count += 1;
        }
    }
    item.ai_indexed = approved_count == chunk_ids.len() && !chunk_ids.is_empty();
    item.updated_at = utc_now();
    item.save(db).await?;

    let detail = if item.ai_indexed {
        "chunkها تأیید و در RAG قابل‌استناد شدند"
    } else {
        "chunkها ساخته شدند ولی draft ماندند — تأیید نهایی با مجوز fatwa.approve لازم است"
    };
    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        "library.item.ai_indexed",
        json!({
            "chunks": chunk_ids.len(),
            "approved": approved_count,
            "scopes": scopes,
            "ai_indexed": item.ai_indexed,
        }),
    )
    .await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "library.item.index_ai",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({
            "ai_indexed": item.ai_indexed,
            "chunks": chunk_ids.len(),
            "chunks_approved": approved_count,
            "scopes": scopes,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    Ok(Json(json!({
        "item_id": item.id.to_string(),
        "ai_indexed": item.ai_indexed,
        "chunks_created": chunk_ids.len(),
        "chunks_approved": approved_count,
        "chunk_ids": chunk_ids,
        "scopes": scopes,
        "detail": detail,
    })))
}

/// `POST /api/v1/admin/library/import` — scheduled import (Telegram
/// ready): always lands in `in_review`, media key `pending:telegram/…`.
/// (201)
pub async fn import_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    body: axum::body::Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let kind = match parsed.get("kind") {
        Some(Value::String(s)) if KINDS.contains(&s.as_str()) => s.clone(),
        _ => return Err(validation_err("kind", "invalid kind")),
    };
    let title = match parsed.get("title") {
        Some(v) if is_localized_dict(v) => v.clone(),
        _ => return Err(validation_err("title", "field is required")),
    };
    let description = match parsed.get("description") {
        Some(v) if is_localized_dict(v) => Some(v.clone()),
        Some(Value::Null) | None => None,
        Some(_) => {
            return Err(validation_err(
                "description",
                "must be an object of strings",
            ))
        }
    };
    let scholar_id = match parsed.get("scholar_id") {
        Some(Value::Null) | None => None,
        Some(v) => Some(
            crate::validate::uuid_value(v, "scholar_id")
                .map_err(|_| validation_err("scholar_id", "must be a UUID"))?,
        ),
    };
    let topic_tags = match parsed.get("topic_tags") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(validation_err("topic_tags", "must be a list of strings")),
                }
            }
            out
        }
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err(validation_err("topic_tags", "must be a list of strings")),
    };
    let visibility = match parsed.get("visibility") {
        Some(Value::Null) | None => "registered".to_string(),
        Some(Value::String(s)) if VISIBILITIES.contains(&s.as_str()) => s.clone(),
        Some(_) => return Err(validation_err("visibility", "invalid visibility")),
    };
    let ai_scopes = match parsed.get("ai_scopes") {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(validation_err("ai_scopes", "must be a list of strings")),
                }
            }
            out
        }
        Some(Value::Null) | None => Vec::new(),
        Some(_) => return Err(validation_err("ai_scopes", "must be a list of strings")),
    };
    let include_in_fatwa_rag = match parsed.get("include_in_fatwa_rag") {
        Some(v) => pydantic_bool(v)
            .ok_or_else(|| validation_err("include_in_fatwa_rag", "must be a boolean"))?,
        None => false,
    };
    let download_policy = match parsed.get("download_policy") {
        Some(v) if v.is_object() => Some(v.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err(validation_err("download_policy", "must be an object")),
    };
    let duration_seconds = match parsed.get("duration_seconds") {
        Some(v) => match pydantic_int(v) {
            Some(d) if (i32::MIN as i64..=i32::MAX as i64).contains(&d) => Some(d as i32),
            _ => return Err(validation_err("duration_seconds", "must be an integer")),
        },
        None => None,
    };
    let file_size_bytes = match parsed.get("file_size_bytes") {
        Some(v) => match pydantic_int(v) {
            Some(d) if (i32::MIN as i64..=i32::MAX as i64).contains(&d) => Some(d as i32),
            _ => return Err(validation_err("file_size_bytes", "must be an integer")),
        },
        None => None,
    };
    let mime_type = match parsed.get("mime_type") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Null) | None => None,
        Some(_) => return Err(validation_err("mime_type", "must be a string")),
    };
    let source_ref = match parsed.get("source_ref") {
        Some(v) if v.is_object() => v.clone(),
        _ => return Err(validation_err("source_ref", "field is required")),
    };
    let db = state.db_or_500()?;
    let now = utc_now();
    let mut item = new_item_base(&kind, ctx.user.id, now);
    item.title = title;
    item.description = description;
    item.media_key = Some(format!("pending:telegram/{}", Uuid::new_v4().simple()));
    item.scholar_id = scholar_id;
    item.topic_tags = Value::Array(
        topic_tags
            .iter()
            .map(|t| Value::String(t.clone()))
            .collect(),
    );
    item.visibility = visibility;
    item.workflow_status = "in_review".to_string();
    item.ai_scopes = Value::Array(ai_scopes.iter().map(|s| Value::String(s.clone())).collect());
    item.include_in_fatwa_rag = include_in_fatwa_rag;
    item.download_policy = download_policy;
    item.duration_seconds = duration_seconds;
    item.file_size_bytes = file_size_bytes;
    item.mime_type = mime_type;
    item.source_type = "telegram_import".to_string();
    item.source_ref = Some(source_ref.clone());
    item.insert(db).await?;

    emit_event(
        db,
        "library_item",
        &item.id.to_string(),
        "library.item.created",
        json!({
            "source_type": "telegram_import",
            "workflow_status": "in_review",
            "source_ref": source_ref,
        }),
    )
    .await?;
    // The audit copy drops `session`-bearing keys (Python filters them
    // before logging; the audit scrub would otherwise mask them).
    let audit_source_ref = match &source_ref {
        Value::Object(map) => {
            let mut kept = serde_json::Map::new();
            for (k, v) in map {
                if !k.to_lowercase().contains("session") {
                    kept.insert(k.clone(), v.clone());
                }
            }
            Value::Object(kept)
        }
        other => other.clone(),
    };
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "library.item.import",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({
            "kind": item.kind,
            "source_type": "telegram_import",
            "workflow_status": "in_review",
            "source_ref": audit_source_ref,
        })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok((
        StatusCode::CREATED,
        Json(item_out_with_urls(&state, &item, Some(&ctx), privileged).await?),
    ))
}

/// `POST /api/v1/admin/library/items/{item_id}/reprocess` — re-queue
/// STT/OCR on the media queue. The worker is a Python sidecar, so the
/// enqueue honestly reports `enqueued: false` and the status stays
/// `pending` until the worker runs.
pub async fn reprocess_item(
    State(state): State<AppState>,
    headers: HeaderMap,
    peer: ConnectInfo<std::net::SocketAddr>,
    Path(item_id): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<Value>> {
    let item_id = parse_path_uuid(&item_id, "item_id")?;
    let ctx = admin_ctx(&state, &headers).await?;
    let parsed = parse_body(&body)?;
    let job = match parsed.get("job") {
        Some(Value::String(s)) => s.clone(),
        _ => return Err(validation_err("job", "field is required")),
    };
    let db = state.db_or_500()?;
    let mut item = get_item_or_404(db, item_id).await?;
    match job.as_str() {
        "transcribe" => {
            item.transcript_status = "pending".to_string();
            item.processing_error = None;
        }
        "ocr" => {
            item.ocr_status = "pending".to_string();
            item.processing_error = None;
        }
        _ => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "bad_job",
                "job باید transcribe یا ocr باشد",
            ));
        }
    }
    item.updated_at = utc_now();
    item.save(db).await?;
    let (ip, ua) = client_ip_ua(&headers, peer, &state);
    log_audit(
        db,
        Some(ctx.user.id),
        "admin",
        "library.item.reprocess",
        "library_item",
        Some(&item.id.to_string()),
        None,
        Some(json!({ "job": job, "enqueued": false })),
        ip.as_deref(),
        ua.as_deref(),
    )
    .await?;
    let privileged = is_privileged(db, Some(&ctx)).await?;
    Ok(Json(
        item_out_with_urls(&state, &item, Some(&ctx), privileged).await?,
    ))
}
