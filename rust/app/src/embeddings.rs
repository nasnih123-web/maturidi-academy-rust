//! Embedding + hybrid retrieval — mirrors
//! `app/modules/fatwa/embeddings.py` (Backend B path only).
//!
//! The Rust port implements the deterministic **hash** provider exactly:
//! hashing trick over word trigrams (SHA-256 → dimension + sign) with
//! L2 normalization, and the hybrid score `lexical + semantic` where
//! lexical is character-trigram overlap and semantic is cosine
//! similarity. `ChunkIndex.search` runs the Backend B full scan on
//! both backends (the Rust binary has no pgvector / pg_trgm, so the
//! Postgres in-DB candidate window of Backend A is not used — the
//! full scan is a superset of its candidates, and scores are identical
//! to Python's Backend B).
//!
//! Provider resolution (`resolve_provider_name`): the setting
//! `ai.embedding_provider` (JSON-encoded string, e.g. `"hash"`);
//! when unset, Python uses `hash` in dev and `local_model` in
//! production. The Rust port cannot run `local_model`
//! (sentence-transformers / torch) or `external_api` in Wave 3, so an
//! unset setting resolves to `hash` in every environment and any other
//! resolved name is a 500 `internal_error` (the same status Python
//! produces when its provider fails to construct) — see ADR-021.

use sha2::{Digest, Sha256};

use crate::{errors::ApiError, settings::get_setting, AppState};

pub const EMBEDDING_DIM: usize = 256;
pub const PROJECTION_ID: &str = "folding-v1";
pub const HASH_PROVIDER: &str = "hash";
pub const HASH_MODEL: &str = "hash-trigram-v1";

pub const AI_SCOPES: &[&str] = &[
    "fatwa",
    "assistant",
    "quiz",
    "verification",
    "semantic_search",
];

/// `_normalize`: strip, drop Arabic diacritics U+064B–U+0652 and U+0670,
/// unify ي→ی and ك→ک, collapse whitespace.
pub fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.trim().chars() {
        if ('\u{064b}'..='\u{0652}').contains(&c) || c == '\u{0670}' {
            continue;
        }
        match c {
            'ي' => out.push('ی'),
            'ك' => out.push('ک'),
            _ => out.push(c),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `_trigrams`: character trigrams of `  {normalized}  ` (boundary
/// padding); a too-short string yields the whole padded string.
fn trigrams(text: &str) -> std::collections::HashSet<String> {
    let padded = format!("  {}  ", normalize_text(text));
    let chars: Vec<char> = padded.chars().collect();
    let mut out = std::collections::HashSet::new();
    if chars.len() >= 3 {
        for w in chars.windows(3) {
            out.insert(w.iter().collect());
        }
    } else {
        out.insert(padded);
    }
    out
}

/// `lexical_score`: |A∩B| / |A| over character trigrams, in [0, 1].
pub fn lexical_score(query: &str, document: &str) -> f64 {
    let q = trigrams(query);
    if q.is_empty() {
        return 0.0;
    }
    let d = trigrams(document);
    let inter = q.intersection(&d).count();
    inter as f64 / q.len() as f64
}

/// `cosine_similarity`, clipped to [0, 1]; 0.0 on empty / mismatched /
/// zero-norm inputs.
pub fn cosine_similarity(a: &[f64], b: &[f64]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let dot: f64 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na * nb)).clamp(0.0, 1.0)
}

/// `HashEmbeddingProvider.embed` for one text: for every word, every
/// trigram of `<word>` picks a dimension from the first 4 SHA-256 bytes
/// and a sign from the 5th byte; L2-normalized at the end.
pub fn hash_embed(text: &str) -> Vec<f64> {
    let mut vec = vec![0.0f64; EMBEDDING_DIM];
    for word in normalize_text(text).split_whitespace() {
        let padded: Vec<char> = format!("<{word}>").chars().collect();
        if padded.len() < 3 {
            continue;
        }
        for tri in padded.windows(3) {
            let s: String = tri.iter().collect();
            let h = Sha256::digest(s.as_bytes());
            let idx = (u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize) % EMBEDDING_DIM;
            vec[idx] += if h[4] & 1 == 1 { 1.0 } else { -1.0 };
        }
    }
    let norm: f64 = vec.iter().map(|x| x * x).sum::<f64>().sqrt();
    if norm == 0.0 {
        vec
    } else {
        vec.iter().map(|x| x / norm).collect()
    }
}

pub fn hash_embed_many(texts: &[String]) -> Vec<Vec<f64>> {
    texts.iter().map(|t| hash_embed(t)).collect()
}

/// Resolve the embedding provider name: explicit setting
/// `ai.embedding_provider` (JSON-decoded when quoted), else `hash`.
/// Any name other than `hash` is a 500 (no other provider is ported).
pub async fn resolve_provider(state: &AppState) -> Result<(), ApiError> {
    let db = state.db_or_500()?;
    let name = match get_setting(db, &state.config, "ai.embedding_provider").await? {
        Some(raw) if !raw.trim().is_empty() => decode_provider_name(&raw),
        _ => HASH_PROVIDER.to_string(),
    };
    if name == HASH_PROVIDER {
        Ok(())
    } else {
        tracing::error!("embedding provider not ported: {name}");
        Err(ApiError::internal())
    }
}

/// `_decode_provider_name`: tolerate a JSON-encoded setting value.
fn decode_provider_name(raw: &str) -> String {
    let text = raw.trim();
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        if let Ok(serde_json::Value::String(decoded)) = serde_json::from_str(text) {
            return decoded;
        }
    }
    text.to_string()
}

/// One scored chunk — mirrors `ScoredChunk` in `embeddings.py`.
#[derive(Debug, Clone)]
pub struct ScoredChunk {
    pub chunk_id: uuid::Uuid,
    pub chunk_text: String,
    pub source_citation: Option<serde_json::Value>,
    pub topic_category: Option<String>,
    pub madhab_scope: Option<String>,
    pub score: f64,
    /// Lexical / semantic components of `score` (kept for parity with
    /// Python's `ScoredChunk`; no Wave 3 route reads them separately).
    #[allow(dead_code)]
    pub lexical: f64,
    #[allow(dead_code)]
    pub semantic: f64,
}

/// `ChunkIndex.search` — Backend B full scan: candidates are
/// `approved`/`published` chunks (optionally filtered by topic
/// category and madhab scope); chunks without the requested `ai_scope`
/// are skipped; a chunk whose recorded (provider, model, projection,
/// dim) identity differs from the query identity gets semantic 0.0.
/// Sorted by score descending (stable), top `top_k` returned.
pub async fn search_chunks(
    state: &AppState,
    query: &str,
    top_k: i64,
    ai_scope: &str,
    topic_category: Option<&str>,
    madhab_scope: Option<&str>,
) -> Result<Vec<ScoredChunk>, ApiError> {
    if !AI_SCOPES.contains(&ai_scope) {
        return Err(ApiError::internal());
    }
    resolve_provider(state).await?;
    let query_vec = hash_embed(query);
    let db = state.db_or_500()?;

    let mut sql = "SELECT id, chunk_text, source_citation, topic_category, \
                   madhab_scope, embedding, embedding_provider, embedding_model, \
                   embedding_projection, ai_scopes FROM fiqh_knowledge_chunks \
                   WHERE workflow_status IN ('approved', 'published')"
        .to_string();
    let mut args: Vec<crate::db::Arg> = Vec::new();
    if let Some(tc) = topic_category {
        sql.push_str(" AND topic_category = ?");
        args.push(crate::db::Arg::Str(tc.to_string()));
    }
    if let Some(ms) = madhab_scope {
        sql.push_str(" AND (madhab_scope IS NULL OR madhab_scope = ?)");
        args.push(crate::db::Arg::Str(ms.to_string()));
    }
    let rows = db.fetch_all(&sql, &args).await?;

    let mut scored: Vec<ScoredChunk> = Vec::new();
    for row in rows {
        let scopes: Vec<String> = match row.try_get::<Option<serde_json::Value>, _>("ai_scopes")? {
            Some(serde_json::Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect(),
            _ => Vec::new(),
        };
        if !scopes.iter().any(|s| s == ai_scope) {
            continue;
        }
        let chunk_text: String = row.try_get("chunk_text")?;
        let lex = lexical_score(query, &chunk_text);
        let embedding: Vec<f64> = match row.try_get::<Option<serde_json::Value>, _>("embedding")? {
            Some(serde_json::Value::Array(items)) if items.iter().all(|i| i.is_number()) => {
                items.iter().filter_map(|i| i.as_f64()).collect()
            }
            _ => Vec::new(),
        };
        let provider: Option<String> = row.try_get("embedding_provider")?;
        let model: Option<String> = row.try_get("embedding_model")?;
        let projection: Option<String> = row.try_get("embedding_projection")?;
        let identity_ok = provider.as_deref() == Some(HASH_PROVIDER)
            && model.as_deref().unwrap_or("") == HASH_MODEL
            && projection.as_deref() == Some(PROJECTION_ID)
            && embedding.len() == EMBEDDING_DIM;
        let sem = if identity_ok {
            cosine_similarity(&query_vec, &embedding)
        } else {
            0.0
        };
        scored.push(ScoredChunk {
            chunk_id: row.try_get("id")?,
            chunk_text,
            source_citation: row.try_get("source_citation")?,
            topic_category: row.try_get("topic_category")?,
            madhab_scope: row.try_get("madhab_scope")?,
            score: lex + sem,
            lexical: lex,
            semantic: sem,
        });
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(top_k.max(0) as usize);
    Ok(scored)
}

/// `_chunk_to_citation`: first 400 chars + "…" when longer; score
/// rounded to 4 decimals.
pub fn chunk_to_citation(c: &ScoredChunk) -> serde_json::Value {
    let chars: Vec<char> = c.chunk_text.chars().collect();
    let excerpt: String = if chars.len() > 400 {
        let mut s: String = chars[..400].iter().collect();
        s.push('…');
        s
    } else {
        c.chunk_text.clone()
    };
    serde_json::json!({
        "chunk_id": c.chunk_id.to_string(),
        "excerpt": excerpt,
        "source_citation": c.source_citation,
        "topic_category": c.topic_category,
        "madhab_scope": c.madhab_scope,
        "score": (c.score * 10000.0).round() / 10000.0,
    })
}

/// Read a chunk's stored embedding as `Vec<f64>` (empty when absent or
/// malformed) — used by the verify `conflicting` heuristic.
pub fn stored_embedding(value: Option<serde_json::Value>) -> Vec<f64> {
    match value {
        Some(serde_json::Value::Array(items)) if items.iter().all(|i| i.is_number()) => {
            items.iter().filter_map(|i| i.as_f64()).collect()
        }
        _ => Vec::new(),
    }
}
