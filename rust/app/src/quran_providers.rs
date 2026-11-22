//! Quran text/translation/tafsir/audio providers — a port of
//! `app/modules/quran/providers.py`.
//!
//! Providers: `alquran_cloud` (AlQuran Cloud HTTP, combined editions
//! endpoint, 10 s timeout, 3 attempts, 600 s cache + stale fallback),
//! `self_hosted` (bundled Tanzil Uthmani text, `data/quran/`), `bundled`
//! (bundled translation/tafsir JSON, `data/quran/`), plus custom
//! AlQuran-compatible providers from the registry. Unknown provider ids
//! fail with 503, never a silent fallback.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::errors::ApiError;

/// One ayah's text in one edition.
#[derive(Debug, Clone)]
pub struct EditionText {
    pub text: String,
    pub audio_url: Option<String>,
    pub juz: Option<i64>,
    pub page: Option<i64>,
}

impl EditionText {
    fn text_only(text: String) -> Self {
        Self {
            text,
            audio_url: None,
            juz: None,
            page: None,
        }
    }
}

/// A fetched surah: per-ayah edition maps (insertion order) + the
/// edition ids in request order.
#[derive(Debug, Clone)]
pub struct SurahFetch {
    pub ayahs: Vec<(i64, Vec<(String, EditionText)>)>,
    pub edition_ids: Vec<String>,
}

/// A fetched ayah: edition map (insertion order) + edition ids.
#[derive(Debug, Clone)]
pub struct AyahFetch {
    pub editions: Vec<(String, EditionText)>,
}

fn insert_first(map: &mut Vec<(String, EditionText)>, id: &str, text: EditionText) {
    if !map.iter().any(|(k, _)| k == id) {
        map.push((id.to_string(), text));
    }
}

pub fn provider_unavailable(provider_id: &str) -> ApiError {
    ApiError::with_details(
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        "quran_provider_unavailable",
        "سرویس متن قرآن در دسترس نیست؛ لطفاً کمی بعد تلاش کنید",
        serde_json::json!({ "provider": provider_id }),
    )
}

fn proxy_from_env() -> Option<String> {
    for var in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            }
        }
    }
    None
}

fn provider_client() -> reqwest::Client {
    let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(10));
    if let Some(proxy) = proxy_from_env() {
        if let Ok(p) = reqwest::Proxy::all(&proxy) {
            builder = builder.proxy(p);
        }
    }
    builder.build().unwrap_or_else(|_| reqwest::Client::new())
}

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn http_client() -> &'static reqwest::Client {
    HTTP_CLIENT.get_or_init(provider_client)
}

// --- in-process provider cache (600 s TTL + stale fallback) -----------

struct CacheEntry {
    stored_at: Instant,
    surah: Option<SurahFetch>,
    ayah: Option<AyahFetch>,
}

static CACHE: OnceLock<Mutex<HashMap<String, CacheEntry>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<String, CacheEntry>> {
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cache_get_surah(key: &str) -> Option<SurahFetch> {
    let map = cache().lock().ok()?;
    let e = map.get(key)?;
    if e.stored_at.elapsed() > Duration::from_secs(600) {
        return None;
    }
    e.surah.clone()
}

fn cache_get_ayah(key: &str) -> Option<AyahFetch> {
    let map = cache().lock().ok()?;
    let e = map.get(key)?;
    if e.stored_at.elapsed() > Duration::from_secs(600) {
        return None;
    }
    e.ayah.clone()
}

fn cache_get_stale_surah(key: &str) -> Option<SurahFetch> {
    let map = cache().lock().ok()?;
    map.get(key).and_then(|e| e.surah.clone())
}

fn cache_get_stale_ayah(key: &str) -> Option<AyahFetch> {
    let map = cache().lock().ok()?;
    map.get(key).and_then(|e| e.ayah.clone())
}

fn cache_put_surah(key: &str, value: SurahFetch) {
    if let Ok(mut map) = cache().lock() {
        map.insert(
            key.to_string(),
            CacheEntry {
                stored_at: Instant::now(),
                surah: Some(value),
                ayah: None,
            },
        );
    }
}

fn cache_put_ayah(key: &str, value: AyahFetch) {
    if let Ok(mut map) = cache().lock() {
        map.insert(
            key.to_string(),
            CacheEntry {
                stored_at: Instant::now(),
                surah: None,
                ayah: Some(value),
            },
        );
    }
}

// --- AlQuran Cloud (and AlQuran-compatible custom providers) -----------

pub struct AlquranProvider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
}

impl AlquranProvider {
    fn edition_ids(
        &self,
        text: Option<&str>,
        translation: Option<&str>,
        tafsir: Option<&str>,
        reciter: Option<&str>,
    ) -> Result<Vec<String>, ApiError> {
        let mut ids: Vec<String> = Vec::new();
        for ed in [text, translation, tafsir, reciter] {
            if let Some(e) = ed.filter(|e| !e.is_empty()) {
                if !ids.iter().any(|x| x == e) {
                    ids.push(e.to_string());
                }
            }
        }
        if ids.is_empty() {
            return Err(provider_unavailable(&self.id));
        }
        Ok(ids)
    }

    async fn get(&self, path: &str) -> Result<Value, ApiError> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);
        let mut last_err = String::new();
        for attempt in 0..3 {
            let mut req = http_client().get(&url);
            if let Some(key) = &self.api_key {
                req = req.header("Authorization", format!("Bearer {key}"));
            }
            match req.send().await {
                Ok(resp) => {
                    let status = resp.status();
                    if status.as_u16() == 429 || status.is_server_error() {
                        last_err = format!("upstream {status}");
                        tokio::time::sleep(Duration::from_millis((500 * (attempt + 1)) as u64))
                            .await;
                        continue;
                    }
                    if !status.is_success() {
                        return Err(provider_unavailable(&self.id));
                    }
                    let body: Value = resp
                        .json()
                        .await
                        .map_err(|_| provider_unavailable(&self.id))?;
                    if !body.is_object() || body.get("code").and_then(|c| c.as_i64()) != Some(200) {
                        return Err(provider_unavailable(&self.id));
                    }
                    return Ok(body["data"].clone());
                }
                Err(e) => {
                    last_err = e.to_string();
                    tokio::time::sleep(Duration::from_millis((500 * (attempt + 1)) as u64)).await;
                }
            }
        }
        let _ = last_err;
        Err(provider_unavailable(&self.id))
    }

    fn merge_edition_ayahs(data: &Value) -> HashMap<i64, Vec<(String, EditionText)>> {
        let mut merged: HashMap<i64, Vec<(String, EditionText)>> = HashMap::new();
        let Some(editions) = data.as_array() else {
            return merged;
        };
        for edition in editions {
            let edition_id = edition
                .get("edition")
                .and_then(|e| e.get("identifier"))
                .and_then(|i| i.as_str())
                .unwrap_or("unknown");
            let Some(ayahs) = edition.get("ayahs").and_then(|a| a.as_array()) else {
                continue;
            };
            for ayah in ayahs {
                let Some(n) = ayah.get("numberInSurah").and_then(|v| v.as_i64()) else {
                    continue;
                };
                let slot = merged.entry(n).or_default();
                insert_first(
                    slot,
                    edition_id,
                    EditionText {
                        text: ayah
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        audio_url: ayah
                            .get("audio")
                            .and_then(|a| a.as_str())
                            .map(|s| s.to_string()),
                        juz: ayah.get("juz").and_then(|v| v.as_i64()),
                        page: ayah.get("page").and_then(|v| v.as_i64()),
                    },
                );
            }
        }
        merged
    }

    pub async fn fetch_surah(
        &self,
        number: i64,
        text: Option<&str>,
        translation: Option<&str>,
        tafsir: Option<&str>,
        reciter: Option<&str>,
    ) -> Result<SurahFetch, ApiError> {
        let ids = self.edition_ids(text, translation, tafsir, reciter)?;
        let cache_key = format!("surah:{}:{number}:{}", self.id, ids.join(","));
        let stale = cache_get_stale_surah(&cache_key);
        if let Some(cached) = cache_get_surah(&cache_key) {
            return Ok(cached);
        }
        let live = self.fetch_surah_live(number, &ids, &cache_key).await;
        match live {
            Ok(v) => Ok(v),
            Err(e) => {
                if let Some(s) = stale {
                    tracing::warn!(
                        "provider {} failed for surah {number}; serving stale cache",
                        self.id
                    );
                    Ok(s)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn fetch_surah_live(
        &self,
        number: i64,
        ids: &[String],
        cache_key: &str,
    ) -> Result<SurahFetch, ApiError> {
        let data = self
            .get(&format!("/surah/{number}/editions/{}", ids.join(",")))
            .await?;
        if !data.is_array() {
            return Err(provider_unavailable(&self.id));
        }
        let merged = Self::merge_edition_ayahs(&data);
        if merged.is_empty() {
            return Err(provider_unavailable(&self.id));
        }
        let mut keys: Vec<i64> = merged.keys().copied().collect();
        keys.sort_unstable();
        let result = SurahFetch {
            ayahs: keys.into_iter().map(|n| (n, merged[&n].clone())).collect(),
            edition_ids: ids.to_vec(),
        };
        cache_put_surah(cache_key, result.clone());
        Ok(result)
    }

    pub async fn fetch_ayah(
        &self,
        surah: i64,
        ayah: i64,
        text: Option<&str>,
        translation: Option<&str>,
        tafsir: Option<&str>,
        reciter: Option<&str>,
    ) -> Result<AyahFetch, ApiError> {
        let ids = self.edition_ids(text, translation, tafsir, reciter)?;
        let cache_key = format!("ayah:{}:{surah}:{ayah}:{}", self.id, ids.join(","));
        let stale = cache_get_stale_ayah(&cache_key);
        if let Some(cached) = cache_get_ayah(&cache_key) {
            return Ok(cached);
        }
        let live = self.fetch_ayah_live(surah, ayah, &ids, &cache_key).await;
        match live {
            Ok(v) => Ok(v),
            Err(e) => {
                if let Some(s) = stale {
                    tracing::warn!(
                        "provider {} failed for ayah {surah}:{ayah}; serving stale cache",
                        self.id
                    );
                    Ok(s)
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn fetch_ayah_live(
        &self,
        surah: i64,
        ayah: i64,
        ids: &[String],
        cache_key: &str,
    ) -> Result<AyahFetch, ApiError> {
        let data = self
            .get(&format!("/ayah/{surah}:{ayah}/editions/{}", ids.join(",")))
            .await?;
        if !data.is_array() {
            return Err(provider_unavailable(&self.id));
        }
        let mut editions: Vec<(String, EditionText)> = Vec::new();
        for edition in data.as_array().unwrap_or(&Vec::new()) {
            let edition_id = edition
                .get("edition")
                .and_then(|e| e.get("identifier"))
                .and_then(|i| i.as_str())
                .unwrap_or("unknown");
            insert_first(
                &mut editions,
                edition_id,
                EditionText {
                    text: edition
                        .get("text")
                        .and_then(|t| t.as_str())
                        .unwrap_or("")
                        .to_string(),
                    audio_url: edition
                        .get("audio")
                        .and_then(|a| a.as_str())
                        .map(|s| s.to_string()),
                    juz: edition.get("juz").and_then(|v| v.as_i64()),
                    page: edition.get("page").and_then(|v| v.as_i64()),
                },
            );
        }
        if editions.is_empty() {
            return Err(provider_unavailable(&self.id));
        }
        let result = AyahFetch { editions };
        cache_put_ayah(cache_key, result.clone());
        Ok(result)
    }
}

// --- Self-hosted (Tanzil) -----------------------------------------------

/// (surah, ayah) -> Uthmani text.
type TanzilMap = HashMap<(i64, i64), String>;

fn tanzil_data() -> Result<&'static TanzilMap, ApiError> {
    static TANZIL: OnceLock<Result<TanzilMap, ()>> = OnceLock::new();
    let loaded = TANZIL.get_or_init(|| {
        let raw = include_str!("../data/quran/tanzil_uthmani.txt");
        let mut data: HashMap<(i64, i64), String> = HashMap::new();
        for line in raw.lines() {
            let mut parts = line.splitn(3, '|');
            let (Some(s), Some(a), Some(t)) = (parts.next(), parts.next(), parts.next()) else {
                continue;
            };
            let (Ok(surah), Ok(ayah)) = (s.parse::<i64>(), a.parse::<i64>()) else {
                continue;
            };
            data.insert((surah, ayah), t.to_string());
        }
        let surahs: std::collections::HashSet<i64> = data.keys().map(|(s, _)| *s).collect();
        if surahs.len() != 114 || data.len() != 6236 {
            return Err(());
        }
        Ok(data)
    });
    match loaded {
        Ok(d) => Ok(d),
        Err(_) => Err(provider_unavailable("self_hosted")),
    }
}

fn self_hosted_surah(number: i64, text_edition: &str) -> Result<SurahFetch, ApiError> {
    let data = tanzil_data()?;
    let mut ayahs: Vec<(i64, Vec<(String, EditionText)>)> = Vec::new();
    let mut keys: Vec<(i64, i64)> = data.keys().filter(|(s, _)| *s == number).copied().collect();
    keys.sort_unstable();
    if keys.is_empty() {
        return Err(provider_unavailable("self_hosted"));
    }
    for (_, ayah) in keys {
        ayahs.push((
            ayah,
            vec![(
                text_edition.to_string(),
                EditionText::text_only(data[&(number, ayah)].clone()),
            )],
        ));
    }
    Ok(SurahFetch {
        ayahs,
        edition_ids: vec![text_edition.to_string()],
    })
}

fn self_hosted_ayah(surah: i64, ayah: i64, text_edition: &str) -> Result<AyahFetch, ApiError> {
    let data = tanzil_data()?;
    let Some(text) = data.get(&(surah, ayah)) else {
        return Err(provider_unavailable("self_hosted"));
    };
    Ok(AyahFetch {
        editions: vec![(
            text_edition.to_string(),
            EditionText::text_only(text.clone()),
        )],
    })
}

// --- Bundled editions -----------------------------------------------------

fn bundled_data(edition: &str) -> Result<&'static HashMap<String, String>, ApiError> {
    // Parsed editions are `Box::leak`ed once, so the `&'static` reference
    // stays valid forever (the map only ever gains entries).
    static BUNDLED: OnceLock<Mutex<HashMap<String, &'static HashMap<String, String>>>> =
        OnceLock::new();
    let map = BUNDLED.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(guard) = map.lock() {
        if let Some(d) = guard.get(edition) {
            return Ok(*d);
        }
    }
    let raw: &str = match edition {
        "fa.rowwad" => include_str!("../data/quran/fa_rowwad.json"),
        "fa.taji" => include_str!("../data/quran/fa_taji.json"),
        "fa.mokhtasar" => include_str!("../data/quran/tafsir/fa.mokhtasar.json"),
        "fa.saadi" => include_str!("../data/quran/tafsir/fa.saadi.json"),
        _ => return Err(provider_unavailable("bundled")),
    };
    let parsed: Value = serde_json::from_str(raw).map_err(|_| provider_unavailable("bundled"))?;
    let Some(obj) = parsed.as_object() else {
        return Err(provider_unavailable("bundled"));
    };
    if obj.len() < 6000
        || !obj
            .iter()
            .all(|(_, v)| v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false))
    {
        return Err(provider_unavailable("bundled"));
    }
    let data: HashMap<String, String> = obj
        .iter()
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    let leaked: &'static HashMap<String, String> = Box::leak(Box::new(data));
    if let Ok(mut guard) = map.lock() {
        guard.insert(edition.to_string(), leaked);
    }
    Ok(leaked)
}

fn bundled_surah(
    number: i64,
    translation: Option<&str>,
    tafsir: Option<&str>,
) -> Result<SurahFetch, ApiError> {
    let requested: Vec<&str> = [translation, tafsir].into_iter().flatten().collect();
    if requested.is_empty() {
        return Err(provider_unavailable("bundled"));
    }
    let mut merged: HashMap<i64, Vec<(String, EditionText)>> = HashMap::new();
    for edition in &requested {
        let data = bundled_data(edition)?;
        for (key, text) in data.iter() {
            let Some((s, a)) = key.split_once(':') else {
                continue;
            };
            let (Ok(surah), Ok(ayah)) = (s.parse::<i64>(), a.parse::<i64>()) else {
                continue;
            };
            if surah == number {
                insert_first(
                    merged.entry(ayah).or_default(),
                    edition,
                    EditionText::text_only(text.clone()),
                );
            }
        }
    }
    if merged.is_empty() {
        return Err(provider_unavailable("bundled"));
    }
    let mut keys: Vec<i64> = merged.keys().copied().collect();
    keys.sort_unstable();
    Ok(SurahFetch {
        ayahs: keys
            .into_iter()
            .map(|n| (n, merged.remove(&n).unwrap()))
            .collect(),
        edition_ids: requested.iter().map(|s| s.to_string()).collect(),
    })
}

fn bundled_ayah(
    surah: i64,
    ayah: i64,
    translation: Option<&str>,
    tafsir: Option<&str>,
) -> Result<AyahFetch, ApiError> {
    let requested: Vec<&str> = [translation, tafsir].into_iter().flatten().collect();
    if requested.is_empty() {
        return Err(provider_unavailable("bundled"));
    }
    let mut editions: Vec<(String, EditionText)> = Vec::new();
    for edition in &requested {
        let data = bundled_data(edition)?;
        let Some(text) = data.get(&format!("{surah}:{ayah}")) else {
            return Err(provider_unavailable("bundled"));
        };
        insert_first(&mut editions, edition, EditionText::text_only(text.clone()));
    }
    Ok(AyahFetch { editions })
}

// --- Provider registry -----------------------------------------------------

/// A resolvable provider id.
pub enum Provider {
    Alquran(AlquranProvider),
    SelfHosted,
    Bundled,
}

impl Provider {
    pub fn id(&self) -> &str {
        match self {
            Provider::Alquran(p) => &p.id,
            Provider::SelfHosted => "self_hosted",
            Provider::Bundled => "bundled",
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Provider::Alquran(p) => &p.name,
            Provider::SelfHosted => "Self-hosted (Tanzil)",
            Provider::Bundled => "Bundled (server-side data)",
        }
    }

    pub fn base_url(&self) -> &str {
        match self {
            Provider::Alquran(p) => &p.base_url,
            Provider::SelfHosted => "",
            Provider::Bundled => "",
        }
    }

    pub async fn fetch_surah(
        &self,
        number: i64,
        text: Option<&str>,
        translation: Option<&str>,
        tafsir: Option<&str>,
        reciter: Option<&str>,
    ) -> Result<SurahFetch, ApiError> {
        match self {
            Provider::Alquran(p) => {
                p.fetch_surah(number, text, translation, tafsir, reciter)
                    .await
            }
            Provider::SelfHosted => {
                let Some(t) = text else {
                    return Err(provider_unavailable("self_hosted"));
                };
                if translation.is_some() || tafsir.is_some() || reciter.is_some() {
                    return Err(provider_unavailable("self_hosted"));
                }
                self_hosted_surah(number, t)
            }
            Provider::Bundled => {
                if text.is_some() || reciter.is_some() {
                    return Err(provider_unavailable("bundled"));
                }
                bundled_surah(number, translation, tafsir)
            }
        }
    }

    pub async fn fetch_ayah(
        &self,
        surah: i64,
        ayah: i64,
        text: Option<&str>,
        translation: Option<&str>,
        tafsir: Option<&str>,
        reciter: Option<&str>,
    ) -> Result<AyahFetch, ApiError> {
        match self {
            Provider::Alquran(p) => {
                p.fetch_ayah(surah, ayah, text, translation, tafsir, reciter)
                    .await
            }
            Provider::SelfHosted => {
                let Some(t) = text else {
                    return Err(provider_unavailable("self_hosted"));
                };
                if translation.is_some() || tafsir.is_some() || reciter.is_some() {
                    return Err(provider_unavailable("self_hosted"));
                }
                self_hosted_ayah(surah, ayah, t)
            }
            Provider::Bundled => {
                if text.is_some() || reciter.is_some() {
                    return Err(provider_unavailable("bundled"));
                }
                bundled_ayah(surah, ayah, translation, tafsir)
            }
        }
    }
}

/// Custom provider entry, validated exactly like Python's
/// `validate_custom_provider_entry` (canonical output).
pub fn validate_custom_provider_entry(entry: &Value) -> Result<Value, ()> {
    let obj = entry.as_object().ok_or(())?;
    let pid = obj.get("id").and_then(|v| v.as_str()).ok_or(())?;
    let id_ok = !pid.is_empty()
        && pid.len() <= 64
        && pid
            .chars()
            .next()
            .map(|c| c.is_ascii_alphanumeric())
            .unwrap_or(false)
        && pid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !id_ok {
        return Err(());
    }
    let name = obj.get("name").and_then(|v| v.as_str()).ok_or(())?;
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 120 {
        return Err(());
    }
    let base_url = obj.get("base_url").and_then(|v| v.as_str()).ok_or(())?;
    let base_url = base_url.trim();
    if !(base_url.starts_with("https://") || base_url.starts_with("http://")) {
        return Err(());
    }
    let ptype = obj.get("type").and_then(|v| v.as_str()).ok_or(())?;
    if !["audio", "text", "translation", "tafsir"].contains(&ptype) {
        return Err(());
    }
    Ok(serde_json::json!({
        "id": pid,
        "name": name,
        "base_url": base_url.trim_end_matches('/'),
        "type": ptype,
    }))
}

/// Clean a stored custom-provider list (invalid entries dropped —
/// the read path never 500s; strict validation happens at write).
pub fn clean_custom_list(raw: &Value) -> Vec<Value> {
    let Some(arr) = raw.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| validate_custom_provider_entry(item).ok())
        .collect()
}

pub fn clean_keys_map(raw: &Value) -> HashMap<String, String> {
    let Some(obj) = raw.as_object() else {
        return HashMap::new();
    };
    obj.iter()
        .filter_map(|(k, v)| {
            v.as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| (k.clone(), s.to_string()))
        })
        .collect()
}

/// Build the servable provider map: built-ins + registered custom ones.
/// A `Vec` (not a `HashMap`) so the `/providers` response keeps
/// Python's insertion order: alquran_cloud, self_hosted, bundled,
/// then customs in registry order.
pub fn build_provider_map(
    custom: &[Value],
    keys: &HashMap<String, String>,
) -> Vec<(String, Provider)> {
    let mut map: Vec<(String, Provider)> = Vec::new();
    map.push((
        "alquran_cloud".to_string(),
        Provider::Alquran(AlquranProvider {
            id: "alquran_cloud".to_string(),
            name: "AlQuranCloud".to_string(),
            base_url: "https://api.alquran.cloud/v1".to_string(),
            api_key: None,
        }),
    ));
    map.push(("self_hosted".to_string(), Provider::SelfHosted));
    map.push(("bundled".to_string(), Provider::Bundled));
    for entry in custom {
        let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let base_url = entry
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        map.push((
            id.to_string(),
            Provider::Alquran(AlquranProvider {
                id: id.to_string(),
                name,
                base_url,
                api_key: keys.get(id).cloned(),
            }),
        ));
    }
    map
}

/// The known-but-unconnected provider catalog — metadata only.
pub fn catalog_entries() -> Vec<Value> {
    vec![
        serde_json::json!({
            "id": "everyayah",
            "name": "EveryAyah",
            "base_url": "https://everyayah.com/data",
            "types": ["audio"],
            "implemented": false,
            "note": "",
        }),
        serde_json::json!({
            "id": "fawazahmed0",
            "name": "fawazahmed0",
            "base_url": "https://cdn.jsdelivr.net/gh/fawazahmed0/quran-api@1",
            "types": ["translation"],
            "implemented": false,
            "note": "",
        }),
        serde_json::json!({
            "id": "quran_foundation",
            "name": "Quran Foundation v4",
            "base_url": "https://api.quran.com/api/v4",
            "types": ["audio", "text", "translation", "tafsir"],
            "implemented": false,
            "note": "",
        }),
    ]
}

/// Editions whose bundled data file is installed (all four are
/// bundled in this repo).
pub fn bundled_available() -> (Vec<String>, Vec<String>) {
    (
        crate::quran_data::BUNDLED_TRANSLATIONS
            .iter()
            .map(|s| s.to_string())
            .collect(),
        crate::quran_data::BUNDLED_TAFSIRS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    )
}
