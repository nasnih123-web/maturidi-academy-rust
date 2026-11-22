//! Request validation helpers.
//!
//! Bodies are parsed as `serde_json::Value` and validated by hand so the
//! failure mode matches Python: any bad field → 422 with the unified
//! envelope, code `validation_error`, message «خطای اعتبارسنجی ورودی»
//! (the per-field messages differ from Pydantic's wording — ADR-006).
//! Length/range bounds are copied from the Pydantic schemas.

use serde_json::{json, Value};

use crate::errors::ApiError;

pub type VResult<T> = Result<T, ApiError>;

fn err(field: &str, msg: &str) -> ApiError {
    ApiError::validation(vec![json!({ "field": field, "msg": msg })])
}

/// Parse a request body as JSON. Non-object bodies and invalid JSON are
/// 422 validation errors (Python never returns a bare 400 here).
pub fn parse_body(bytes: &[u8]) -> VResult<Value> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| ApiError::validation(vec![json!({ "msg": "invalid JSON body" })]))?;
    if !v.is_object() {
        return Err(ApiError::validation(vec![
            json!({ "msg": "body must be a JSON object" }),
        ]));
    }
    Ok(v)
}

fn get<'a>(body: &'a Value, field: &str) -> Option<&'a Value> {
    body.get(field).filter(|v| !v.is_null())
}

pub fn req_str(body: &Value, field: &str, min: usize, max: usize) -> VResult<String> {
    let Some(v) = get(body, field) else {
        return Err(err(field, "field is required"));
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    let s = s.trim().to_string();
    if s.chars().count() < min || s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(s)
}

pub fn opt_str(body: &Value, field: &str, min: usize, max: usize) -> VResult<Option<String>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    let s = s.trim().to_string();
    if s.chars().count() < min || s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(Some(s))
}

/// Optional string with no minimum (empty string is significant for
/// phone-clearing in `PATCH /users/me`).
pub fn opt_str_raw(body: &Value, field: &str, max: usize) -> VResult<Option<String>> {
    let Some(v) = get(body, field) else {
        return Ok(None);
    };
    let Some(s) = v.as_str() else {
        return Err(err(field, "must be a string"));
    };
    if s.chars().count() > max {
        return Err(err(field, "length out of range"));
    }
    Ok(Some(s.to_string()))
}

#[allow(dead_code)] // Later waves: boolean body fields
pub fn req_bool(body: &Value, field: &str) -> VResult<bool> {
    match get(body, field) {
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(err(field, "must be a boolean")),
        None => Err(err(field, "field is required")),
    }
}

pub fn opt_bool(body: &Value, field: &str) -> VResult<Option<bool>> {
    match get(body, field) {
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(err(field, "must be a boolean")),
        None => Ok(None),
    }
}

pub fn opt_int(body: &Value, field: &str, min: i64, max: i64) -> VResult<Option<i64>> {
    match get(body, field) {
        Some(v) => {
            let Some(n) = v.as_i64() else {
                return Err(err(field, "must be an integer"));
            };
            if n < min || n > max {
                return Err(err(field, "out of range"));
            }
            Ok(Some(n))
        }
        None => Ok(None),
    }
}

pub fn req_int(body: &Value, field: &str, min: i64, max: i64, default: i64) -> VResult<i64> {
    Ok(opt_int(body, field, min, max)?.unwrap_or(default))
}

pub fn opt_str_list(body: &Value, field: &str) -> VResult<Option<Vec<String>>> {
    match get(body, field) {
        Some(Value::Array(items)) => {
            let mut out = Vec::new();
            for item in items {
                let Some(s) = item.as_str() else {
                    return Err(err(field, "must be a list of strings"));
                };
                out.push(s.to_string());
            }
            Ok(Some(out))
        }
        Some(_) => Err(err(field, "must be a list of strings")),
        None => Ok(None),
    }
}

/// Minimal email check — mirrors `EmailStr` for realistic addresses.
/// (Pydantic's full RFC validation is not reproduced; see ADR-006.)
pub fn is_valid_email(s: &str) -> bool {
    if s.len() > 320 || s.contains(char::is_whitespace) {
        return false;
    }
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && !domain.is_empty() && domain.contains('.') && !domain.starts_with('.')
}

pub fn opt_uuid(body: &Value, field: &str) -> VResult<Option<uuid::Uuid>> {
    match get(body, field) {
        Some(Value::String(s)) => s
            .parse::<uuid::Uuid>()
            .map(Some)
            .map_err(|_| err(field, "must be a UUID")),
        Some(_) => Err(err(field, "must be a UUID")),
        None => Ok(None),
    }
}
