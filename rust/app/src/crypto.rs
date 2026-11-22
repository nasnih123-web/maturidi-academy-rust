//! Cryptography — mirrors `app/core/security.py` and `app/core/secrets.py`.
//!
//! - Argon2id with argon2-cffi `PasswordHasher()` defaults: memory 65536,
//!   time 3, parallelism 4, hash length 32, salt 16. The PHC string format
//!   is identical, so hashes created by Python verify here and vice versa.
//! - `hash_token`: SHA-256 hex (opaque refresh tokens, reset tokens, ...).
//! - `token_urlsafe`: `secrets.token_urlsafe(n)` equivalent.
//! - Fernet (`app/core/secrets.py`): key material is
//!   `base64url(sha256(key_material))`; envelope `{"v": version, "data":
//!   "<fernet token>"}`. Implemented directly (AES-128-CBC + HMAC-SHA256)
//!   so the wire format matches Python `cryptography` byte for byte.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::errors::ApiError;

type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;
type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;
type HmacSha256 = Hmac<Sha256>;

/// SHA-256 hex digest — `hash_token` in `security.py`.
pub fn hash_token(token: &str) -> String {
    let mut h = Sha256::new();
    h.update(token.as_bytes());
    hex_encode(&h.finalize())
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// `secrets.token_urlsafe(n)` — n random bytes, base64url without padding.
pub fn token_urlsafe(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

/// `secrets.token_hex(n)` — n random bytes as hex.
pub fn token_hex(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    rand::thread_rng().fill_bytes(&mut buf);
    hex_encode(&buf)
}

/// Hash a password with Argon2id, argon2-cffi defaults
/// (m=65536, t=3, p=4, hash_len=32). Returns the PHC string.
pub fn hash_password(password: &str) -> Result<String, ApiError> {
    use argon2::{Argon2, Params, PasswordHasher};
    let params = Params::new(65_536, 3, 4, Some(32)).map_err(|_| ApiError::internal())?;
    let argon2 = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let salt = argon2::password_hash::SaltString::generate(&mut rand::thread_rng());
    argon2
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| ApiError::internal())
}

/// Verify a password against a PHC Argon2 hash. Unknown/mismatched hashes
/// are simply `false`, like `verify_password` in `security.py`.
pub fn verify_password(password_hash: &str, password: &str) -> bool {
    use argon2::{Argon2, PasswordHash, PasswordVerifier};
    let Ok(parsed) = PasswordHash::new(password_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

/// HMAC-SHA256 hex — storage signing and embed tokens.
pub fn hmac_sha256_hex(key: &[u8], msg: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    hex_encode(&mac.finalize().into_bytes())
}

pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac key");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// Fernet key for a key-material string: `base64url(sha256(material))`,
/// then split into signing key (first 16 bytes) and encryption key
/// (last 16 bytes), exactly like Python `cryptography.fernet`.
fn fernet_keys(key_material: &str) -> ([u8; 16], [u8; 16]) {
    let digest = Sha256::digest(key_material.as_bytes());
    let mut signing = [0u8; 16];
    let mut encryption = [0u8; 16];
    signing.copy_from_slice(&digest[..16]);
    encryption.copy_from_slice(&digest[16..]);
    (signing, encryption)
}

/// Encrypt `plaintext` with Fernet (version-independent part of
/// `encrypt_value`): returns the base64url token string.
pub fn fernet_encrypt(key_material: &str, plaintext: &[u8]) -> String {
    let (signing, encryption) = fernet_keys(key_material);
    let mut iv = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut iv);
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ciphertext = Aes128CbcEnc::new(&encryption.into(), &iv.into())
        .encrypt_padded_vec_mut::<Pkcs7>(plaintext);
    let mut signed = Vec::with_capacity(1 + 8 + 16 + ciphertext.len());
    signed.push(0x80);
    signed.extend_from_slice(&timestamp.to_be_bytes());
    signed.extend_from_slice(&iv);
    signed.extend_from_slice(&ciphertext);
    let tag = hmac_sha256(&signing, &signed);
    signed.extend_from_slice(&tag);
    URL_SAFE_NO_PAD.encode(signed)
}

/// Decrypt a Fernet token. Returns `None` on any failure (bad token,
/// bad signature, bad padding) — the caller maps that to its own error.
pub fn fernet_decrypt(key_material: &str, token: &str) -> Option<Vec<u8>> {
    let (signing, encryption) = fernet_keys(key_material);
    let raw = URL_SAFE_NO_PAD.decode(token).ok()?;
    if raw.len() < 1 + 8 + 16 + 32 || raw[0] != 0x80 {
        return None;
    }
    let (signed, tag) = raw.split_at(raw.len() - 32);
    let expected = hmac_sha256(&signing, signed);
    if !constant_time_eq(&expected, tag) {
        return None;
    }
    let iv: [u8; 16] = signed[9..25].try_into().ok()?;
    let ciphertext = &signed[25..];
    Aes128CbcDec::new(&encryption.into(), &iv.into())
        .decrypt_padded_vec_mut::<Pkcs7>(ciphertext)
        .ok()
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}
