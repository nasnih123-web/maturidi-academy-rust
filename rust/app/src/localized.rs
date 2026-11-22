//! `LocalizedText` helpers — mirrors `_as_localized` / `_locale_texts`
//! in `app/modules/home/service.py`.
//!
//! A `LocalizedText` is a JSON object with all three of `fa`, `ar`,
//! `en`. Python coerces values with `str()`; rows whose title fails
//! the conversion are skipped silently, never an error.

use serde_json::Value;

/// `str()` coercion for a JSON scalar (Python semantics for the
/// values that actually occur: strings as-is, numbers/bools formatted).
fn coerce_str(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `_as_localized(raw)` — `Some((fa, ar, en))` only when `raw` is a
/// dict with all three keys coercible to strings.
pub fn as_localized(raw: &Value) -> Option<(String, String, String)> {
    let obj = raw.as_object()?;
    let fa = coerce_str(obj.get("fa")?)?;
    let ar = coerce_str(obj.get("ar")?)?;
    let en = coerce_str(obj.get("en")?)?;
    Some((fa, ar, en))
}

/// `{"fa": ..., "ar": ..., "en": ...}` JSON for a localized triple.
pub fn localized_json(fa: &str, ar: &str, en: &str) -> Value {
    serde_json::json!({ "fa": fa, "ar": ar, "en": en })
}

/// `_locale_texts(raw)` — the fa/ar/en values that are truthy
/// (non-empty string, non-zero number, `true`), in locale order.
pub fn locale_texts(raw: &Value) -> Vec<String> {
    let Some(obj) = raw.as_object() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for loc in ["fa", "ar", "en"] {
        match obj.get(loc) {
            Some(Value::String(s)) if !s.is_empty() => out.push(s.clone()),
            Some(Value::Number(n)) if n.as_f64() != Some(0.0) => out.push(n.to_string()),
            Some(Value::Bool(true)) => out.push("true".to_string()),
            _ => {}
        }
    }
    out
}
