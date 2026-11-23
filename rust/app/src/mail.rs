//! Outbound email / SMS — real sending since Wave 9 PR-3 (ADR-158).
//!
//! Python's order and semantics (`app/core/email.py`,
//! `app/core/sms.py`), ported:
//!
//! - Email: Resend HTTPS API (`POST https://api.resend.com/emails`,
//!   `Authorization: Bearer <key>`) when `RESEND_API_KEY` is set
//!   (preferred — Railway blocks SMTP ports); else SMTP (STARTTLS,
//!   15 s, login when `SMTP_USER` is set) via lettre when `SMTP_HOST`
//!   is set; else an honest error.
//! - SMS: Kavenegar (`POST
//!   https://api.kavenegar.com/v1/{api_key}/sms/send.json`, form
//!   `receptor`/`sender`/`message`, 15 s) when `SMS_API_KEY` is set.
//!   HTTP 200 with a non-200 `return.status` in the JSON body is a
//!   delivery failure — the fake-success guard from Python's sms.py.
//!
//! There is no dev-mode log-and-pretend any more (ADR-158): Python's
//! dev fallbacks returned success without sending, and the Wave 2
//! stub did the same. When unset or unreachable, these functions
//! return `Err` — a real error, never a fake success. Callers decide
//! what that means: auth flows log it and continue (the token/code
//! is already stored, the user can ask for a resend); the admin
//! broadcast path records the real outcome.
//!
//! Errors never carry a key or a message body: a Resend failure
//! reports only the HTTP status, a Kavenegar failure only the HTTP
//! status and the API status code. Bodies are redacted before any
//! log line (`redact_secrets`, ported from Python).

use std::{fmt, time::Duration};

use lettre::transport::smtp::{
    authentication::Credentials,
    client::{Tls, TlsParameters},
};
use lettre::{message::MultiPart, AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::{json, Value};

use crate::Config;

pub const RESEND_API_URL: &str = "https://api.resend.com/emails";
const KAVENEGAR_URL_TEMPLATE: &str = "https://api.kavenegar.com/v1/{api_key}/sms/send.json";
const MAIL_TIMEOUT_SECS: u64 = 15;

/// What went wrong sending a message. `Display` never includes a
/// credential or a message body — only the kind of failure and, for
/// delivery failures, the HTTP/API status.
#[derive(Debug)]
pub enum MailError {
    NotConfigured(&'static str),
    Delivery(String),
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MailError::NotConfigured(msg) => write!(f, "{msg}"),
            MailError::Delivery(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for MailError {}

impl MailError {
    pub fn is_not_configured(&self) -> bool {
        matches!(self, MailError::NotConfigured(_))
    }
}

/// `redact_secrets` — ported from Python's `email.redact_secrets`:
/// `token=` values and 6-digit codes are masked before anything is
/// logged. (One deliberate difference: Python's `\b\d{6}\b` also
/// matches the last six digits of a longer run; this version only
/// masks digit runs of exactly six, so longer numbers are never
/// mangled.)
pub fn redact_secrets(text: &str) -> String {
    mask_token_values(&mask_six_digit_codes(text))
}

fn mask_token_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        // Case-insensitive `token=` / `token:` at this position
        // (ASCII-only comparison, so byte indices stay aligned with
        // the original UTF-8 string).
        let rest = &text[i..];
        let lower = rest.to_ascii_lowercase();
        let is_token = lower.starts_with("token=") || lower.starts_with("token:");
        if is_token {
            out.push_str(&text[i..i + 6]);
            let mut j = i + 6;
            while j < bytes.len()
                && !matches!(
                    bytes[j],
                    b' ' | b'\t' | b'\n' | b'\r' | b'&' | b'"' | b'\'' | b'<' | b'>'
                )
            {
                j += 1;
            }
            if j > i + 6 {
                out.push_str("[REDACTED]");
            }
            i = j;
        } else {
            // Advance one UTF-8 char.
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn utf8_char_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

fn mask_six_digit_codes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j - i == 6 {
                out.push_str("[CODE]");
            } else {
                out.push_str(&text[i..j]);
            }
            i = j;
        } else {
            let ch_len = utf8_char_len(bytes[i]);
            out.push_str(&text[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

/// Kavenegar send URL for an API key.
pub fn kavenegar_url(api_key: &str) -> String {
    KAVENEGAR_URL_TEMPLATE.replace("{api_key}", api_key)
}

/// A mail/SMS sender built from a `Config` (credentials from env
/// only). The HTTP client is built once per `Mailer`; call sites use
/// the free `send_email`/`send_sms` wrappers, which build one per
/// call — fine for transactional mail. `with_endpoints` exists so
/// tests can point the transports at in-process mock servers.
pub struct Mailer {
    resend_api_key: Option<String>,
    smtp_host: String,
    smtp_port: u16,
    smtp_user: String,
    smtp_password: String,
    smtp_from: String,
    sms_api_key: Option<String>,
    sms_sender: String,
    sms_provider: String,
    resend_url: String,
    kavenegar_url_template: String,
    client: reqwest::Client,
}

impl Mailer {
    pub fn from_config(config: &Config) -> Self {
        Self::with_endpoints(
            config,
            RESEND_API_URL.to_string(),
            KAVENEGAR_URL_TEMPLATE.to_string(),
        )
    }

    pub fn with_endpoints(
        config: &Config,
        resend_url: String,
        kavenegar_url_template: String,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(MAIL_TIMEOUT_SECS))
            .build()
            .expect("mail http client builds");
        Self {
            resend_api_key: config.resend_api_key.clone(),
            smtp_host: config.smtp_host.clone(),
            smtp_port: config.smtp_port,
            smtp_user: config.smtp_user.clone(),
            smtp_password: config.smtp_password.clone(),
            smtp_from: config.smtp_from.clone(),
            sms_api_key: config.sms_api_key.clone(),
            sms_sender: config.sms_sender.clone(),
            sms_provider: config.sms_provider.clone(),
            resend_url,
            kavenegar_url_template,
            client,
        }
    }

    pub fn email_configured(&self) -> bool {
        self.resend_api_key.is_some() || !self.smtp_host.trim().is_empty()
    }

    pub fn sms_configured(&self) -> bool {
        self.sms_api_key.is_some()
    }

    pub async fn send_email(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), MailError> {
        if let Some(key) = &self.resend_api_key {
            return self.send_via_resend(key, to, subject, text, html).await;
        }
        if !self.smtp_host.trim().is_empty() {
            return self.send_via_smtp(to, subject, text, html).await;
        }
        Err(MailError::NotConfigured(
            "email not configured: set RESEND_API_KEY or SMTP_HOST",
        ))
    }

    async fn send_via_resend(
        &self,
        key: &str,
        to: &str,
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), MailError> {
        let mut payload = json!({
            "from": self.smtp_from,
            "to": [to],
            "subject": subject,
            "text": text,
        });
        if !html.is_empty() {
            payload["html"] = json!(html);
        }
        let resp = self
            .client
            .post(&self.resend_url)
            .header("Authorization", format!("Bearer {key}"))
            .json(&payload)
            .send()
            .await
            .map_err(|_| {
                MailError::Delivery("resend delivery failed: network error".to_string())
            })?;
        // §8 in Python: the key and the response body never appear in
        // the error — only the HTTP status.
        if !resp.status().is_success() {
            return Err(MailError::Delivery(format!(
                "resend delivery failed: http={}",
                resp.status().as_u16()
            )));
        }
        Ok(())
    }

    async fn send_via_smtp(
        &self,
        to: &str,
        subject: &str,
        text: &str,
        html: &str,
    ) -> Result<(), MailError> {
        let from: lettre::message::Mailbox = self.smtp_from.parse().map_err(|_| {
            MailError::Delivery("smtp delivery failed: invalid from address".to_string())
        })?;
        let recipient: lettre::message::Mailbox = to.parse().map_err(|_| {
            MailError::Delivery("smtp delivery failed: invalid recipient".to_string())
        })?;
        let message = if html.is_empty() {
            Message::builder()
                .from(from)
                .to(recipient)
                .subject(subject)
                .body(text.to_string())
        } else {
            Message::builder()
                .from(from)
                .to(recipient)
                .subject(subject)
                .multipart(MultiPart::alternative_plain_html(
                    text.to_string(),
                    html.to_string(),
                ))
        }
        .map_err(|_| MailError::Delivery("smtp delivery failed: invalid message".to_string()))?;
        // STARTTLS is required (Python's `starttls()`), verified
        // against the SMTP host's certificate.
        let tls = TlsParameters::new(self.smtp_host.clone()).map_err(|_| {
            MailError::Delivery("smtp delivery failed: invalid smtp host".to_string())
        })?;
        let mut builder = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.smtp_host)
            .port(self.smtp_port)
            .tls(Tls::Required(tls))
            .timeout(Some(Duration::from_secs(MAIL_TIMEOUT_SECS)));
        if !self.smtp_user.is_empty() {
            builder = builder.credentials(Credentials::new(
                self.smtp_user.clone(),
                self.smtp_password.clone(),
            ));
        }
        builder
            .build()
            .send(message)
            .await
            .map_err(|_| MailError::Delivery("smtp delivery failed".to_string()))?;
        Ok(())
    }

    pub async fn send_sms(&self, phone: &str, body: &str) -> Result<(), MailError> {
        if self.sms_provider != "kavenegar" {
            return Err(MailError::NotConfigured(
                "sms provider not implemented: only kavenegar is supported; set SMS_PROVIDER=kavenegar",
            ));
        }
        let Some(key) = &self.sms_api_key else {
            return Err(MailError::NotConfigured(
                "sms not configured: set SMS_API_KEY",
            ));
        };
        let url = self.kavenegar_url_template.replace("{api_key}", key);
        let resp = self
            .client
            .post(&url)
            .form(&[
                ("receptor", phone),
                ("sender", self.sms_sender.as_str()),
                ("message", body),
            ])
            .send()
            .await
            .map_err(|_| {
                MailError::Delivery("kavenegar delivery failed: network error".to_string())
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(MailError::Delivery(format!(
                "kavenegar delivery failed: http={}",
                status.as_u16()
            )));
        }
        let value: Value = resp.json().await.map_err(|_| {
            MailError::Delivery("kavenegar delivery failed: bad response".to_string())
        })?;
        // §8 in Python: HTTP 200 with a non-200 return.status is a
        // delivery failure — a fake success is forbidden.
        let api_status = value
            .get("return")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_i64());
        if api_status != Some(200) {
            return Err(MailError::Delivery(format!(
                "kavenegar delivery failed: api_status={}",
                api_status
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "unknown".to_string())
            )));
        }
        Ok(())
    }
}

/// Send an email with the config's credentials. Honest failure:
/// `Err` when unconfigured or undeliverable.
pub async fn send_email(
    config: &Config,
    to: &str,
    subject: &str,
    text: &str,
    html: &str,
) -> Result<(), MailError> {
    Mailer::from_config(config)
        .send_email(to, subject, text, html)
        .await
}

/// Send an SMS with the config's credentials. Honest failure:
/// `Err` when unconfigured or undeliverable.
pub async fn send_sms(config: &Config, phone: &str, body: &str) -> Result<(), MailError> {
    Mailer::from_config(config).send_sms(phone, body).await
}
