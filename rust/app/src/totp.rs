//! TOTP (RFC 6238) — mirrors `app/core/security.py` (`pyotp`).
//!
//! SHA-1, 6 digits, 30-second step, acceptance window ±1 step.
//! `verify_totp_step` returns the matched step counter so callers can
//! claim it atomically against replay (`totp_last_step`).

use hmac::{Hmac, Mac};
use sha1::Sha1;

use crate::crypto::token_hex;

pub const TOTP_STEP_SECONDS: i64 = 30;
pub const TOTP_DIGITS: u32 = 6;

/// `pyotp.random_base32()` — 20 random bytes, base32, no padding.
pub fn generate_totp_secret() -> String {
    let mut buf = [0u8; 20];
    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut buf);
    base32_encode(&buf)
}

const BASE32_ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &b in data {
        buffer = (buffer << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32_ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32_ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Decode a base32 secret (case-insensitive, padding tolerated).
/// Returns `None` for malformed input.
pub fn base32_decode(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for c in input.chars() {
        if c == '=' || c.is_whitespace() {
            continue;
        }
        let v = match c.to_ascii_uppercase() {
            'A'..='Z' => c.to_ascii_uppercase() as u32 - 'A' as u32,
            '2'..='7' => c as u32 - '2' as u32 + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn hotp(secret: &[u8], counter: u64) -> String {
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("hmac key");
    mac.update(&counter.to_be_bytes());
    let hash = mac.finalize().into_bytes();
    let offset = (hash[19] & 0x0f) as usize;
    let code = ((u32::from(hash[offset] & 0x7f) << 24)
        | (u32::from(hash[offset + 1]) << 16)
        | (u32::from(hash[offset + 2]) << 8)
        | u32::from(hash[offset + 3]))
        % 10u32.pow(TOTP_DIGITS);
    format!("{code:06}")
}

/// Verify a TOTP code like `pyotp.TOTP(secret).verify(code, valid_window=1)`,
/// returning the matched step counter, or `None` when invalid/expired.
/// Steps are checked oldest-first, matching `verify_totp_step`.
pub fn verify_totp_step(secret: &str, code: &str, now_unix: i64) -> Option<i64> {
    let key = base32_decode(secret)?;
    if key.is_empty() {
        return None;
    }
    let code = code.trim();
    if code.len() != TOTP_DIGITS as usize || !code.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let current = now_unix.div_euclid(TOTP_STEP_SECONDS);
    ((current - 1)..=(current + 1)).find(|&step| hotp(&key, step as u64) == code)
}

/// `pyotp.TOTP(secret).provisioning_uri(name=account, issuer_name=...)`.
/// pyotp URL-quotes the label with `urllib.parse.quote` (default safe `/`).
pub fn totp_provisioning_uri(secret: &str, account: &str) -> String {
    let issuer = "Maturidi Academy";
    let label = format!("{}:{}", url_quote(issuer), url_quote(account));
    format!(
        "otpauth://totp/{label}?secret={secret}&issuer={}",
        url_quote(issuer)
    )
}

fn url_quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b'@' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// `generate_recovery_codes(n=10)` — `XXXX-XXXX` uppercase hex pairs.
pub fn generate_recovery_codes(n: usize) -> Vec<String> {
    (0..n)
        .map(|_| {
            format!(
                "{}-{}",
                token_hex(2).to_uppercase(),
                token_hex(2).to_uppercase()
            )
        })
        .collect()
}
