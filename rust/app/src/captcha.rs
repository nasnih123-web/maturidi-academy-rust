//! Registration captcha — mirrors `app/core/captcha.py` (Turnstile).
//!
//! State machine: `CAPTCHA_ENABLED=false` → disabled, requests pass.
//! Enabled + no `TURNSTILE_SECRET_KEY` → fail-closed 503
//! `captcha_not_configured`. Enabled + secret → token required, verified
//! against Cloudflare siteverify; missing/invalid → 403 `captcha_failed`.

use axum::http::StatusCode;

use crate::{errors::ApiError, AppState};

const SITEVERIFY_URL: &str = "https://challenges.cloudflare.com/turnstile/v0/siteverify";

pub async fn enforce_captcha(
    state: &AppState,
    token: Option<&str>,
    remote_ip: Option<&str>,
) -> Result<(), ApiError> {
    if !state.config.captcha_enabled {
        return Ok(());
    }
    if state.config.turnstile_secret_key.trim().is_empty() {
        tracing::error!("captcha enabled but TURNSTILE_SECRET_KEY is not configured");
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "captcha_not_configured",
            "کپچا فعال است ولی پیکربندی نشده است؛ ثبت‌نام موقتاً ممکن نیست",
        ));
    }
    let Some(token) = token.map(str::trim).filter(|t| !t.is_empty()) else {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "captcha_failed",
            "تکمیل کپچا برای ثبت‌نام الزامی است",
        ));
    };
    if !verify_turnstile(state, token, remote_ip).await {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "captcha_failed",
            "راستی‌آزمایی کپچا ناموفق بود؛ دوباره تلاش کنید",
        ));
    }
    Ok(())
}

async fn verify_turnstile(state: &AppState, token: &str, remote_ip: Option<&str>) -> bool {
    let mut form: Vec<(&str, String)> = vec![
        ("secret", state.config.turnstile_secret_key.clone()),
        ("response", token.to_string()),
    ];
    if let Some(ip) = remote_ip {
        form.push(("remoteip", ip.to_string()));
    }
    let resp = match state
        .client
        .post(SITEVERIFY_URL)
        .form(&form)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return false,
    };
    if resp.status() != StatusCode::OK {
        return false;
    }
    match resp.json::<serde_json::Value>().await {
        Ok(v) => v.get("success").and_then(|s| s.as_bool()).unwrap_or(false),
        Err(_) => false,
    }
}
