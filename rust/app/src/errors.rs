//! Unified error envelope — mirrors `app/core/errors.py` exactly.
//!
//! Every error (including validation 422s and 404s) is returned as:
//! `{"error": {"code": "...", "message": "...", "details": {...}},
//!   "request_id": "<uuid>"}`.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};

/// A domain error: HTTP status + stable machine code + Persian message.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub details: Value,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: json!({}),
        }
    }

    pub fn with_details(
        status: StatusCode,
        code: &'static str,
        message: impl Into<String>,
        details: Value,
    ) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details,
        }
    }

    /// 422 validation error, like FastAPI's `RequestValidationError` handler
    /// in `errors.py`: code `validation_error`, message
    /// «خطای اعتبارسنجی ورودی», details `{"errors": [...]}`.
    /// The individual field messages differ from Pydantic's wording
    /// (ADR-006); the status, code and shape are pinned instead.
    pub fn validation(errors: Vec<Value>) -> Self {
        Self::with_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            "validation_error",
            "خطای اعتبارسنجی ورودی",
            json!({ "errors": errors }),
        )
    }

    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "خطای داخلی سرور",
        )
    }

    pub fn unauthorized(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, code, message)
    }

    pub fn forbidden(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, code, message)
    }

    pub fn not_found(code: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let request_id = uuid::Uuid::new_v4().to_string();
        let body = json!({
            "error": {
                "code": self.code,
                "message": self.message,
                "details": self.details,
            },
            "request_id": request_id,
        });
        (self.status, Json(body)).into_response()
    }
}

/// Anything that can go wrong inside a handler. Database errors become a
/// 500 `internal_error` (like Python's unhandled-exception handler); all
/// expected failures are `ApiError`s.
pub type ApiResult<T> = Result<T, ApiError>;

impl From<sqlx::Error> for ApiError {
    fn from(err: sqlx::Error) -> Self {
        tracing::error!("database error: {err}");
        Self::internal()
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(_: serde_json::Error) -> Self {
        Self::validation(vec![json!({"msg": "invalid JSON body"})])
    }
}
