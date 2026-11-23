//! Branded auth email templates — ported from
//! `apps/api/app/core/mail_templates.py` (verbatim copy + shell).
//!
//! Verification and password-reset emails: one HTML shell (deep green
//! `#07231c` + gold `#d4af37`, the web palette) + a plain-text twin.
//! The locale rule is Python's `resolve_email_locale`: the country
//! comes from the phone number (IR → fa, Arab countries → ar, else
//! en); without a phone, the stored locale when it is fa/ar/en, else
//! en. The dial-code table here is the minimal set that decides the
//! rule — IR + the 22 Arab countries — because every other country
//! maps to en, exactly as Python's full table does.
//!
//! The plain-text body ends with a `{marker}={token}` line, exactly
//! like Python's `_render` (the token is already in the action URL;
//! the line is ported for parity, not added). Tokens must never
//! reach a *log* line — see `mail::redact_secrets` — but the email
//! body itself is the user's own inbox, and the link carries the
//! token by design.

/// Rendered email: subject + plain text + HTML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedEmail {
    pub subject: String,
    pub text: String,
    pub html: String,
}

struct Copy {
    subject: &'static str,
    academy: &'static str,
    title: &'static str,
    greeting: &'static str,
    greeting_named: &'static str,
    body: &'static str,
    cta: &'static str,
    expiry: &'static str,
    fallback: &'static str,
    ignore: &'static str,
}

const VERIFY_COPY: [(&str, Copy); 3] = [
    (
        "fa",
        Copy {
            subject: "تأیید ایمیل — آکادمی ماتریدی",
            academy: "آکادمی ماتریدی",
            title: "تأیید ایمیل",
            greeting: "سلام،",
            greeting_named: "سلام {name}،",
            body: "برای تکمیل ثبت‌نام‌تان در آکادمی ماتریدی و فعال‌شدن حسابتان، لطفاً ایمیل‌تان را تأیید کنید.",
            cta: "تأیید ایمیل",
            expiry: "این لینک یک ساعت اعتبار دارد.",
            fallback: "اگر دکمه کار نکرد، این لینک را کپی کنید و در مرورگر باز کنید:",
            ignore: "اگر این درخواست را شما نداده‌اید، این ایمیل را نادیده بگیرید.",
        },
    ),
    (
        "ar",
        Copy {
            subject: "تأكيد البريد الإلكتروني — أكاديمية ماتريدي",
            academy: "أكاديمية ماتريدي",
            title: "تأكيد البريد الإلكتروني",
            greeting: "مرحباً،",
            greeting_named: "مرحباً {name}،",
            body: "لإكمال تسجيلك في أكاديمية ماتريدي وتفعيل حسابك، يُرجى تأكيد بريدك الإلكتروني بالضغط على الزر أدناه.",
            cta: "تأكيد البريد الإلكتروني",
            expiry: "هذا الرابط صالح لمدة ساعة واحدة.",
            fallback: "إذا لم يعمل الزر، انسخ هذا الرابط وافتحه في متصفحك:",
            ignore: "إذا لم تطلب هذا، يمكنك تجاهل هذه الرسالة بأمان.",
        },
    ),
    (
        "en",
        Copy {
            subject: "Verify your email — Maturidi Academy",
            academy: "Maturidi Academy",
            title: "Verify your email",
            greeting: "Hello,",
            greeting_named: "Hello {name},",
            body: "To finish creating your Maturidi Academy account and activate it, please verify your email address by clicking the button below.",
            cta: "Verify email",
            expiry: "This link is valid for 1 hour.",
            fallback: "If the button doesn't work, copy this link and open it in your browser:",
            ignore: "If you didn't request this, you can safely ignore this email.",
        },
    ),
];

const RESET_COPY: [(&str, Copy); 3] = [
    (
        "fa",
        Copy {
            subject: "بازنشانی رمز عبور — آکادمی ماتریدی",
            academy: "آکادمی ماتریدی",
            title: "بازنشانی رمز عبور",
            greeting: "سلام،",
            greeting_named: "سلام {name}،",
            body: "برای حساب شما در آکادمی ماتریدی درخواست بازنشانی رمز عبور ثبت شده است. برای انتخاب رمز تازه، روی دکمه زیر کلیک کنید.",
            cta: "بازنشانی رمز عبور",
            expiry: "این لینک یک ساعت اعتبار دارد.",
            fallback: "اگر دکمه کار نکرد، این لینک را کپی کنید و در مرورگر باز کنید:",
            ignore: "اگر این درخواست را شما نداده‌اید، این ایمیل را نادیده بگیرید؛ رمز عبورتان بدون تغییر می‌ماند.",
        },
    ),
    (
        "ar",
        Copy {
            subject: "إعادة تعيين كلمة المرور — أكاديمية ماتريدي",
            academy: "أكاديمية ماتريدي",
            title: "إعادة تعيين كلمة المرور",
            greeting: "مرحباً،",
            greeting_named: "مرحباً {name}،",
            body: "تم استلام طلب لإعادة تعيين كلمة مرور حسابك في أكاديمية ماتريدي. لاختيار كلمة مرور جديدة، انقر الزر أدناه.",
            cta: "إعادة تعيين كلمة المرور",
            expiry: "هذا الرابط صالح لمدة ساعة واحدة.",
            fallback: "إذا لم يعمل الزر، انسخ هذا الرابط وافتحه في متصفحك:",
            ignore: "إذا لم تطلب هذا، تجاهل هذه الرسالة وستبقى كلمة مرورك دون تغيير.",
        },
    ),
    (
        "en",
        Copy {
            subject: "Reset your password — Maturidi Academy",
            academy: "Maturidi Academy",
            title: "Reset password",
            greeting: "Hello,",
            greeting_named: "Hello {name},",
            body: "We received a request to reset the password for your Maturidi Academy account. Click the button below to choose a new password.",
            cta: "Reset password",
            expiry: "This link is valid for 1 hour.",
            fallback: "If the button doesn't work, copy this link and open it in your browser:",
            ignore: "If you didn't request this, ignore this email — your password will stay unchanged.",
        },
    ),
];

/// Dial codes that decide the locale: (dial, locale). IR → fa, the
/// Arab countries (Python's `ARAB_COUNTRIES`) → ar. Longest match
/// wins; anything else → en (via the caller's fallback).
const LOCALE_DIAL_CODES: &[(&str, &str)] = &[
    ("98", "fa"),
    ("966", "ar"), // SA
    ("971", "ar"), // AE
    ("974", "ar"), // QA
    ("965", "ar"), // KW
    ("968", "ar"), // OM
    ("973", "ar"), // BH
    ("964", "ar"), // IQ
    ("20", "ar"),  // EG
    ("962", "ar"), // JO
    ("961", "ar"), // LB
    ("963", "ar"), // SY
    ("970", "ar"), // PS
    ("967", "ar"), // YE
    ("212", "ar"), // MA
    ("213", "ar"), // DZ
    ("216", "ar"), // TN
    ("218", "ar"), // LY
    ("249", "ar"), // SD
    ("222", "ar"), // MR
    ("253", "ar"), // DJ
    ("252", "ar"), // SO
    ("269", "ar"), // KM
];

/// `resolve_email_locale` — phone country first, stored locale
/// second, English last. `phone` may carry `+`, spaces or dashes.
/// The international prefix (`00`) and the national trunk prefix
/// (`0`) are stripped before matching, so a raw national-format
/// number (`0989…`) resolves to IR like its E.164 form — this is one
/// deliberate difference from Python's `country_iso_from_phone`,
/// which does not strip. Stored phones are E.164-normalised at
/// registration, so the strip only affects raw input and can never
/// change a real user's locale. A phone that matches neither IR nor
/// an Arab dial code is English, and never falls through to the
/// stored locale — the same outcome as Python for every country
/// Python resolves (all non-IR/non-Arab → en).
pub fn resolve_email_locale(phone: Option<&str>, stored_locale: Option<&str>) -> &'static str {
    if let Some(phone) = phone {
        let digits: String = phone.chars().filter(|c| c.is_ascii_digit()).collect();
        let digits = digits.strip_prefix("00").unwrap_or(&digits);
        let digits = digits.strip_prefix('0').unwrap_or(digits);
        // Longest dial prefix wins (the table is written longest-first
        // among the 2-digit codes, but sort the match explicitly).
        let mut best: Option<(&str, &str)> = None;
        for (dial, locale) in LOCALE_DIAL_CODES {
            if digits.starts_with(dial) && best.map(|(d, _)| d.len()).unwrap_or(0) < dial.len() {
                best = Some((dial, locale));
            }
        }
        if let Some((_, locale)) = best {
            return locale;
        }
        // A phone was present but its country is neither IR nor Arab:
        // Python returns "en" here (it never falls through to the
        // stored locale when a country was resolved).
        return "en";
    }
    let loc = stored_locale.unwrap_or("").trim().to_lowercase();
    match loc.as_str() {
        "fa" => "fa",
        "ar" => "ar",
        _ => "en",
    }
}

fn clean_name(name: Option<&str>) -> String {
    let Some(name) = name else {
        return String::new();
    };
    name.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
}

fn greeting(copy: &Copy, name: Option<&str>) -> String {
    let clean = clean_name(name);
    if clean.is_empty() {
        return copy.greeting.to_string();
    }
    copy.greeting_named.replace("{name}", &clean)
}

/// Minimal HTML escaping (`html.escape(quote=True)`).
fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            _ => out.push(c),
        }
    }
    out
}

const DEEP_GREEN: &str = "#07231c";
const DEEP_GREEN_2: &str = "#0d3b2c";
const GOLD: &str = "#d4af37";
const GOLD_DEEP: &str = "#b8912c";
const GOLD_SOFT: &str = "#e5c07b";
const ON_GOLD: &str = "#06120e";
const PAGE_BG: &str = "#f4efe3";
const CARD_BORDER: &str = "#e9e1cb";
const INK: &str = "#454037";
const MUTED: &str = "#7c7666";
const FONT_STACK: &str = "'Segoe UI', Tahoma, Arial, sans-serif";

fn render_html(
    copy: &Copy,
    locale: &str,
    greeting: &str,
    action_url: &str,
    logo_url: Option<&str>,
) -> String {
    let rtl = matches!(locale, "fa" | "ar");
    let direction = if rtl { "rtl" } else { "ltr" };
    let align = if rtl { "right" } else { "left" };
    let url_esc = esc(action_url);
    let logo_html = match logo_url {
        Some(url) => format!(
            "<img src=\"{}\" alt=\"{}\" height=\"54\" style=\"display:block;margin:0 auto 10px;border:0;height:54px;width:auto;\">",
            esc(url),
            esc(copy.academy)
        ),
        None => String::new(),
    };
    format!(
        "<!DOCTYPE html>\n<html lang=\"{locale}\" dir=\"{direction}\" xmlns=\"http://www.w3.org/1999/xhtml\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1.0\">\n<meta name=\"color-scheme\" content=\"light\">\n<title>{title}</title>\n<style>\n  @keyframes ma-rise {{\n    from {{ opacity: 0; transform: translateY(10px); }}\n    to {{ opacity: 1; transform: translateY(0); }}\n  }}\n  .ma-card {{ animation: ma-rise 0.6s ease-out; }}\n  .ma-btn-link:hover {{ filter: brightness(1.08); }}\n  @media (prefers-reduced-motion: reduce) {{\n    .ma-card {{ animation: none; }}\n  }}\n  @media only screen and (max-width: 620px) {{\n    .ma-wrap {{ width: 100% !important; }}\n    .ma-pad {{ padding: 26px 20px !important; }}\n    .ma-title {{ font-size: 20px !important; }}\n  }}\n</style>\n</head>\n<body style=\"margin:0;padding:0;background-color:{PAGE_BG};font-family:{FONT_STACK};\">\n<div style=\"display:none;max-height:0;overflow:hidden;opacity:0;\">{body_esc}</div>\n<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" style=\"background-color:{PAGE_BG};\">\n<tr><td align=\"center\" style=\"padding:28px 12px;\">\n<table role=\"presentation\" class=\"ma-wrap ma-card\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" style=\"width:600px;max-width:600px;\">\n<tr><td align=\"center\" bgcolor=\"{DEEP_GREEN}\" style=\"background-color:{DEEP_GREEN};background:linear-gradient(135deg,{DEEP_GREEN},{DEEP_GREEN_2});border-radius:14px 14px 0 0;border-bottom:3px solid {GOLD};padding:26px 32px;\">\n{logo_html}\n<div style=\"font-size:22px;font-weight:800;color:{GOLD_SOFT};font-family:{FONT_STACK};\">{academy}</div>\n</td></tr>\n<tr><td bgcolor=\"#ffffff\" style=\"background-color:#ffffff;border:1px solid {CARD_BORDER};border-top:none;border-radius:0 0 14px 14px;\">\n<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\">\n<tr><td class=\"ma-pad\" align=\"{align}\" style=\"padding:34px 38px;text-align:{align};\">\n<h1 class=\"ma-title\" style=\"margin:0 0 14px;font-size:23px;line-height:1.5;color:{DEEP_GREEN};font-family:{FONT_STACK};\">{title}</h1>\n<p style=\"margin:0 0 10px;font-size:15px;line-height:1.9;color:{INK};font-family:{FONT_STACK};\">{greeting_esc}</p>\n<p style=\"margin:0 0 26px;font-size:15px;line-height:1.9;color:{INK};font-family:{FONT_STACK};\">{body_esc}</p>\n<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" align=\"center\" style=\"margin:0 auto 24px;\">\n<tr><td align=\"center\" bgcolor=\"{GOLD}\" style=\"border-radius:999px;background:linear-gradient(135deg,{GOLD_DEEP},{GOLD});\">\n<a class=\"ma-btn-link\" href=\"{url_esc}\" target=\"_blank\" style=\"display:inline-block;padding:14px 42px;font-size:16px;font-weight:800;color:{ON_GOLD};text-decoration:none;border-radius:999px;font-family:{FONT_STACK};\">{cta}</a>\n</td></tr>\n</table>\n<p style=\"margin:0 0 22px;text-align:center;font-size:13px;line-height:1.8;color:{MUTED};font-family:{FONT_STACK};\">{expiry}</p>\n<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\"><tr><td style=\"border-top:1px solid {CARD_BORDER};padding-top:18px;\">\n<p style=\"margin:0 0 6px;font-size:12.5px;line-height:1.8;color:{MUTED};font-family:{FONT_STACK};\">{fallback}</p>\n<p style=\"margin:0;font-size:12.5px;line-height:1.8;word-break:break-all;font-family:{FONT_STACK};\"><a href=\"{url_esc}\" target=\"_blank\" style=\"color:#8a6d1f;\">{url_esc}</a></p>\n</td></tr></table>\n</td></tr>\n</table>\n</td></tr>\n<tr><td align=\"center\" style=\"padding:20px 24px 4px;\">\n<p style=\"margin:0;font-size:12px;line-height:1.9;color:#8b8574;font-family:{FONT_STACK};\">{ignore}</p>\n<p style=\"margin:8px 0 0;font-size:12px;color:#8b8574;font-family:{FONT_STACK};\">{academy}</p>\n</td></tr>\n</table>\n</td></tr>\n</table>\n</body>\n</html>",
        locale = locale,
        direction = direction,
        title = esc(copy.title),
        body_esc = esc(copy.body),
        logo_html = logo_html,
        academy = esc(copy.academy),
        align = align,
        greeting_esc = esc(greeting),
        cta = esc(copy.cta),
        expiry = esc(copy.expiry),
        fallback = esc(copy.fallback),
        ignore = esc(copy.ignore),
        url_esc = url_esc,
    )
}

fn render(
    table: &[(&str, Copy)],
    locale: &str,
    action_url: &str,
    token: &str,
    logo_url: Option<&str>,
    name: Option<&str>,
    marker: &str,
) -> RenderedEmail {
    let (key, copy) = table
        .iter()
        .find(|(k, _)| *k == locale)
        .map(|(k, c)| (*k, c))
        .unwrap_or(("en", &table.iter().find(|(k, _)| *k == "en").unwrap().1));
    let greet = greeting(copy, name);
    let text = [
        greet.clone(),
        copy.body.to_string(),
        action_url.to_string(),
        copy.expiry.to_string(),
        copy.ignore.to_string(),
        format!("{marker}={token}"),
    ]
    .join("\n");
    RenderedEmail {
        subject: copy.subject.to_string(),
        text,
        html: render_html(copy, key, &greet, action_url, logo_url),
    }
}

/// `render_verification_email` — one-hour link
/// (`EMAIL_VERIFY_EXPIRE_HOURS`). Unknown locale falls back to en.
pub fn render_verification_email(
    locale: &str,
    action_url: &str,
    token: &str,
    logo_url: Option<&str>,
    name: Option<&str>,
) -> RenderedEmail {
    render(
        &VERIFY_COPY,
        locale,
        action_url,
        token,
        logo_url,
        name,
        "verify_token",
    )
}

/// `render_password_reset_email` — one-hour link
/// (`RESET_TOKEN_EXPIRE_HOURS`). Unknown locale falls back to en.
pub fn render_password_reset_email(
    locale: &str,
    action_url: &str,
    token: &str,
    logo_url: Option<&str>,
    name: Option<&str>,
) -> RenderedEmail {
    render(
        &RESET_COPY,
        locale,
        action_url,
        token,
        logo_url,
        name,
        "reset_token",
    )
}

/// `public_url` — build a public link from `app_base_url` with
/// validation (Python `auth/service.py`): the host never comes from
/// request headers, only from server settings; scheme must be
/// http/https, hostname present, no userinfo. Returns `None` on an
/// invalid base instead of raising — the callers log and skip the
/// send (best-effort, like Python's try/except around the send).
pub fn public_url(app_base_url: &str, path: &str) -> Option<String> {
    let base = app_base_url.trim().trim_end_matches('/');
    let (scheme, rest) = base.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let host = authority
        .rsplit(':')
        .next()
        .unwrap_or(authority)
        .trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return None;
    }
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    Some(format!("{base}{path}"))
}
