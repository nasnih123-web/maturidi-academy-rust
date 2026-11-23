//! AWS Signature Version 4 for the S3-compatible storage provider
//! (ADR-162) — the same signing the MinIO client does in Python's
//! `S3StorageProvider`, implemented directly over reqwest so the Rust
//! binary needs no S3 SDK.
//!
//! Two uses: header signing for server-side PUT/GET/DELETE, and
//! query signing for the presigned GET URLs browsers receive (the
//! signature is computed locally, with no network call — the MinIO
//! client's `presigned_get_object` works the same way). URLs are
//! **path-style** (`{endpoint}/{bucket}/{key}`), matching the MinIO
//! client's default.
//!
//! The signing itself is proven against an independent Python
//! reference implementation (stdlib `hashlib`/`hmac` only) — see the
//! tests in `wave9_pr3.rs`, where the expected signatures are
//! hardcoded from that reference. No live S3 exists in this
//! environment; a live round-trip is a PR-4 staging proof.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// `kSigning` chain: `kDate → kRegion → kService → kSigning`.
fn signing_key(secret: &str, date_stamp: &str, region: &str) -> Vec<u8> {
    let k_date = hmac_sha256(format!("AWS4{secret}").as_bytes(), date_stamp.as_bytes());
    let k_region = hmac_sha256(&k_date, region.as_bytes());
    let k_service = hmac_sha256(&k_region, b"s3");
    hmac_sha256(&k_service, b"aws4_request")
}

/// RFC 3986 percent-encoding for S3 canonical URIs and query values:
/// unreserved `A-Z a-z 0-9 - _ . ~` pass through; `/` passes through
/// only inside a URI path (never inside a query value).
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (!encode_slash && b == b'/');
        if keep {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Host (with port when present) of an `http(s)://…` endpoint.
pub fn host_of(endpoint: &str) -> String {
    let rest = endpoint.split("://").nth(1).unwrap_or(endpoint);
    rest.split('/').next().unwrap_or("").to_string()
}

fn canonical_uri(bucket: &str, key: &str) -> String {
    // Slashes stay as path separators in a canonical URI (the AWS
    // spec) — only the query value of the presigned credential encodes
    // them (`uri_encode(..., true)` at the call site).
    format!("/{}/{}", uri_encode(bucket, false), uri_encode(key, false))
}

fn string_to_sign(amz_date: &str, scope: &str, canonical_request: &str) -> String {
    format!(
        "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
        sha256_hex(canonical_request.as_bytes())
    )
}

/// Sign a server-side request (PUT/GET/DELETE). Returns the
/// `Authorization` header value plus the `x-amz-date` and
/// `x-amz-content-sha256` header values the caller must send with it.
/// `amz_date` is `YYYYMMDDTHHMMSSZ`, `date_stamp` is `YYYYMMDD`.
/// Ten arguments, allowed: they are the SigV4 signing inputs in AWS
/// spec order, and the tests pin this exact signature.
#[allow(clippy::too_many_arguments)]
pub fn sign_request(
    method: &str,
    endpoint: &str,
    bucket: &str,
    key: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    payload: &[u8],
    amz_date: &str,
    date_stamp: &str,
) -> (String, String, String) {
    let payload_hash = sha256_hex(payload);
    let host = host_of(endpoint);
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let canonical_headers =
        format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
    let canonical_request = format!(
        "{method}\n{}\n\n{}\nhost;x-amz-content-sha256;x-amz-date\n{}",
        canonical_uri(bucket, key),
        canonical_headers,
        payload_hash
    );
    let to_sign = string_to_sign(amz_date, &scope, &canonical_request);
    let signing = signing_key(secret_key, date_stamp, region);
    let signature = hex_encode(&hmac_sha256(&signing, to_sign.as_bytes()));
    let authorization = format!(
        "AWS4-HMAC-SHA256 Credential={access_key}/{scope}, SignedHeaders=host;x-amz-content-sha256;x-amz-date, Signature={signature}"
    );
    (authorization, amz_date.to_string(), payload_hash)
}

/// Build a presigned GET URL against the *public* endpoint (the host
/// browsers see — never the internal endpoint). `expires_secs` is the
/// URL lifetime in seconds (default 300). The signature covers the
/// sorted query string with `UNSIGNED-PAYLOAD`, per the AWS spec for
/// presigned URLs. Nine arguments, allowed: same SigV4 inputs as
/// `sign_request`, and the tests pin this exact signature.
#[allow(clippy::too_many_arguments)]
pub fn presigned_url(
    public_endpoint: &str,
    bucket: &str,
    key: &str,
    access_key: &str,
    secret_key: &str,
    region: &str,
    expires_secs: i64,
    amz_date: &str,
    date_stamp: &str,
) -> String {
    let host = host_of(public_endpoint);
    let scope = format!("{date_stamp}/{region}/s3/aws4_request");
    let credential = format!("{access_key}/{scope}");
    let mut params: Vec<(String, String)> = vec![
        (
            "X-Amz-Algorithm".to_string(),
            "AWS4-HMAC-SHA256".to_string(),
        ),
        (
            "X-Amz-Credential".to_string(),
            uri_encode(&credential, true),
        ),
        ("X-Amz-Date".to_string(), amz_date.to_string()),
        ("X-Amz-Expires".to_string(), expires_secs.to_string()),
        ("X-Amz-SignedHeaders".to_string(), "host".to_string()),
    ];
    params.sort();
    let canonical_query = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical_headers = format!("host:{host}\n");
    let canonical_request = format!(
        "GET\n{}\n{}\n{}\nhost\nUNSIGNED-PAYLOAD",
        canonical_uri(bucket, key),
        canonical_query,
        canonical_headers
    );
    let to_sign = string_to_sign(amz_date, &scope, &canonical_request);
    let signing = signing_key(secret_key, date_stamp, region);
    let signature = hex_encode(&hmac_sha256(&signing, to_sign.as_bytes()));
    let base = public_endpoint.trim_end_matches('/');
    format!(
        "{base}{}?{}&X-Amz-Signature={}",
        canonical_uri(bucket, key),
        canonical_query,
        signature
    )
}

/// Current UTC time in the two SigV4 formats.
pub fn now_amz_date() -> (String, String) {
    let now = chrono::Utc::now();
    (
        now.format("%Y%m%dT%H%M%SZ").to_string(),
        now.format("%Y%m%d").to_string(),
    )
}
