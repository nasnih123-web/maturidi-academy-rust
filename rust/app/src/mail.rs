//! Outbound email / SMS — stubbed in Wave 2 (see ADR-015).
//!
//! Python sends verification / reset emails (Resend HTTPS API, SMTP
//! fallback) and SMS codes best-effort: a delivery failure is logged
//! and never breaks the auth flow, because the token is already stored
//! and the user can ask for a resend. The Rust mailer is not ported
//! yet, so these functions log and succeed — the observable behaviour
//! (token stored, flow continues, resend works) is identical, only the
//! actual delivery is pending a Wave 7 mailer.

/// Best-effort email send. Currently a no-op that logs the intent.
pub async fn send_email(_to: &str, _subject: &str, _text: &str, _html: &str) {
    tracing::warn!("email delivery not ported yet (ADR-015); message not sent");
}

/// Best-effort SMS send. Currently a no-op that logs the intent.
pub async fn send_sms(_phone: &str, _body: &str) {
    tracing::warn!("sms delivery not ported yet (ADR-015); message not sent");
}
