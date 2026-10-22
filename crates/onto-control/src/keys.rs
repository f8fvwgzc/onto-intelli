//! API keys and the small amount of cryptography the control store needs:
//! random bytes, SHA-256, HMAC-SHA256 and constant-time comparison, all from
//! `aws-lc-rs`.
//!
//! A key looks like `onto_sk_7f3k9q2m_Vb1wQ9…`: the prefix (`onto_sk_` and 8
//! characters of `[a-z0-9]`) is stored and shown; the secret (base64url of 32
//! random bytes, 43 characters) is never stored. The stored hash is lowercase
//! hex `SHA-256(pepper || full key)`.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use aws_lc_rs::rand::SecureRandom;
use base64::Engine;
use parking_lot::Mutex;

use onto_core::{Error, Result};

use crate::model::{ApiKey, Project};

/// Every key starts with this, so secret scanners recognise leaked keys.
pub const KEY_PREFIX: &str = "onto_sk_";
/// `onto_sk_` + 8 characters.
pub const PREFIX_LEN: usize = KEY_PREFIX.len() + 8;
/// Base64url (no padding) of 32 bytes.
pub const SECRET_LEN: usize = 43;
/// Label of the pepper derived from `ONTO_MASTER_KEY`.
pub const PEPPER_LABEL: &[u8] = b"onto api key pepper v1";

const ALNUM: &[u8; 36] = b"abcdefghijklmnopqrstuvwxyz0123456789";

/// `n` bytes from the CSPRNG.
pub fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    aws_lc_rs::rand::SystemRandom::new().fill(&mut buf).expect("the system random number generator failed");
    buf
}

/// `n` characters of `[a-z0-9]`, uniformly from the CSPRNG.
pub fn random_alnum(n: usize) -> String {
    let mut out = String::with_capacity(n);
    while out.len() < n {
        for b in random_bytes(n * 2) {
            // 252 = 7 × 36: rejecting the rest keeps every character equally likely.
            if b < 252 && out.len() < n {
                out.push(ALNUM[(b % 36) as usize] as char);
            }
        }
    }
    out
}

/// Base64url without padding of `n` random bytes.
pub fn random_token(n: usize) -> String {
    b64(&random_bytes(n))
}

pub fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn sha256(data: &[u8]) -> Vec<u8> {
    aws_lc_rs::digest::digest(&aws_lc_rs::digest::SHA256, data).as_ref().to_vec()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Lowercase hex SHA-256: how invitation codes, OIDC states and tickets are stored.
pub fn sha256_hex(data: &[u8]) -> String {
    hex(&sha256(data))
}

pub fn hmac_sha256(key: &[u8], data: &[u8]) -> Vec<u8> {
    let k = aws_lc_rs::hmac::Key::new(aws_lc_rs::hmac::HMAC_SHA256, key);
    aws_lc_rs::hmac::sign(&k, data).as_ref().to_vec()
}

/// Equal length and equal bytes, in time independent of where they differ.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && aws_lc_rs::constant_time::verify_slices_are_equal(a, b).is_ok()
}

/// A project ref (the tenant id): 20 characters of `[a-z0-9]`.
pub fn new_ref() -> String {
    random_alnum(20)
}

/// A new key: `(prefix, full key)`.
pub fn generate() -> (String, String) {
    let prefix = format!("{KEY_PREFIX}{}", random_alnum(8));
    let full = format!("{prefix}_{}", random_token(32));
    (prefix, full)
}

/// Split a presented key into `(prefix, secret)`; `None` when it is not a
/// well-formed key.
pub fn parse(full: &str) -> Option<(&str, &str)> {
    let rest = full.strip_prefix(KEY_PREFIX)?;
    if full.len() != PREFIX_LEN + 1 + SECRET_LEN || rest.as_bytes().get(8) != Some(&b'_') {
        return None;
    }
    let (prefix, secret) = (&full[..PREFIX_LEN], &full[PREFIX_LEN + 1..]);
    if !prefix[KEY_PREFIX.len()..].bytes().all(|b| ALNUM.contains(&b)) {
        return None;
    }
    match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(secret) {
        Ok(raw) if raw.len() == 32 => Some((prefix, secret)),
        _ => None,
    }
}

/// `ONTO_MASTER_KEY`: 32 bytes as base64 (standard or url-safe, padded or
/// not) or as 64 hex characters.
pub fn parse_master_key(v: &str) -> Result<Vec<u8>> {
    let v = v.trim();
    if v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok((0..32).map(|i| u8::from_str_radix(&v[2 * i..2 * i + 2], 16).unwrap_or(0)).collect());
    }
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        if let Ok(raw) = engine.decode(v)
            && raw.len() == 32
        {
            return Ok(raw);
        }
    }
    Err(Error::Config("ONTO_MASTER_KEY must be 32 bytes, as base64 or 64 hex characters".into()))
}

/// The secret mixed into every key hash.
#[derive(Clone)]
pub struct Pepper(Vec<u8>);

impl std::fmt::Debug for Pepper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pepper(****)")
    }
}

impl Pepper {
    pub fn new(bytes: Vec<u8>) -> Self {
        Pepper(bytes)
    }

    /// A random pepper for this process only (the memory control store).
    pub fn random() -> Self {
        Pepper(random_bytes(32))
    }

    /// From `ONTO_KEY_PEPPER` (at least 32 characters), else HMAC-SHA256 of
    /// `ONTO_MASTER_KEY` with the label `onto api key pepper v1`; `None`
    /// when neither is set.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_values(std::env::var("ONTO_KEY_PEPPER").ok(), std::env::var("ONTO_MASTER_KEY").ok())
    }

    pub fn from_values(pepper: Option<String>, master_key: Option<String>) -> Result<Option<Self>> {
        if let Some(p) = pepper.map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
            if p.chars().count() < 32 {
                return Err(Error::Config("ONTO_KEY_PEPPER must have at least 32 characters".into()));
            }
            return Ok(Some(Pepper(p.into_bytes())));
        }
        match master_key.filter(|m| !m.trim().is_empty()) {
            Some(m) => Ok(Some(Pepper(hmac_sha256(&parse_master_key(&m)?, PEPPER_LABEL)))),
            None => Ok(None),
        }
    }

    /// Lowercase hex SHA-256(pepper || full key).
    pub fn hash(&self, full_key: &str) -> String {
        let mut data = self.0.clone();
        data.extend_from_slice(full_key.as_bytes());
        sha256_hex(&data)
    }

    /// The presented key matches the stored hash (constant time).
    pub fn verify(&self, full_key: &str, stored_hash: &str) -> bool {
        constant_time_eq(self.hash(full_key).as_bytes(), stored_hash.as_bytes())
    }
}

/// How long a key looked up by prefix is trusted before it is read again.
pub const KEY_TTL: Duration = Duration::from_secs(10);
/// How long an unknown prefix is remembered.
pub const MISS_TTL: Duration = Duration::from_secs(5);

/// A key with its project, or a remembered miss (`None`).
pub type KeyEntry = Option<(ApiKey, Project)>;

/// Keys by prefix, with the project they belong to. Revocation on this
/// replica evicts at once; other replicas see it within [`KEY_TTL`].
#[derive(Default)]
pub struct KeyCache {
    entries: Mutex<HashMap<String, (KeyEntry, Instant)>>,
}

impl KeyCache {
    /// `Some(entry)` while fresh at `now` (`Some(None)`: a remembered miss).
    pub fn get(&self, prefix: &str, now: Instant) -> Option<KeyEntry> {
        let mut m = self.entries.lock();
        match m.get(prefix) {
            Some((v, until)) if *until > now => Some(v.clone()),
            Some(_) => {
                m.remove(prefix);
                None
            }
            None => None,
        }
    }

    pub fn put(&self, prefix: &str, v: KeyEntry, now: Instant) {
        let ttl = if v.is_some() { KEY_TTL } else { MISS_TTL };
        let mut m = self.entries.lock();
        if m.len() > 100_000 {
            m.retain(|_, (_, until)| *until > now);
        }
        m.insert(prefix.to_string(), (v, now + ttl));
    }

    pub fn evict(&self, prefix: &str) {
        self.entries.lock().remove(prefix);
    }

    /// Forget every key of a project (deleted).
    pub fn evict_project(&self, project_id: &str) {
        self.entries.lock().retain(|_, (v, _)| v.as_ref().is_none_or(|(k, _)| k.project_id != project_id));
    }
}

/// `last_used_at` is written at most once a minute per key.
pub const TOUCH_EVERY_MS: i64 = 60_000;

/// Key uses waiting to be written with `touch_keys`.
#[derive(Default)]
pub struct Touches {
    inner: Mutex<TouchState>,
}

#[derive(Default)]
struct TouchState {
    pending: HashMap<String, i64>,
    written: HashMap<String, i64>,
}

impl Touches {
    pub fn record(&self, key_id: &str, at: i64) {
        let mut s = self.inner.lock();
        let e = s.pending.entry(key_id.to_string()).or_insert(at);
        *e = (*e).max(at);
    }

    /// The uses due for writing at `now` (each key at most once a minute);
    /// the rest stay pending.
    pub fn due(&self, now: i64) -> Vec<(String, i64)> {
        let mut s = self.inner.lock();
        let TouchState { pending, written } = &mut *s;
        let mut out = Vec::new();
        pending.retain(|id, at| {
            if written.get(id).is_none_or(|w| now - w >= TOUCH_EVERY_MS) {
                out.push((id.clone(), *at));
                written.insert(id.clone(), now);
                false
            } else {
                true
            }
        });
        if written.len() > 100_000 {
            written.retain(|_, w| now - *w < TOUCH_EVERY_MS);
        }
        out.sort();
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_and_parse() {
        let (prefix, full) = generate();
        assert_eq!(prefix.len(), PREFIX_LEN);
        assert!(full.starts_with(&format!("{prefix}_")));
        assert_eq!(full.len(), PREFIX_LEN + 1 + SECRET_LEN);
        let (p, s) = parse(&full).unwrap();
        assert_eq!(p, prefix);
        assert_eq!(s.len(), SECRET_LEN);
        assert!(parse("onto_sk_short").is_none());
        assert!(parse(&full.replace("onto_sk_", "onto_pk_")).is_none());
        assert!(parse(&format!("{full}x")).is_none());
        assert!(parse(&format!("onto_sk_ABCDEFGH_{}", random_token(32))).is_none(), "prefix is [a-z0-9]");
        assert!(parse(&format!("onto_sk_abcdefgh_{}", random_token(31))).is_none(), "secret is 32 bytes");
        let mut bad_sep = full.clone().into_bytes();
        bad_sep[PREFIX_LEN] = b'-';
        assert!(parse(std::str::from_utf8(&bad_sep).unwrap()).is_none());
        let r = new_ref();
        assert_eq!(r.len(), 20);
        assert!(r.bytes().all(|b| ALNUM.contains(&b)));
        assert_ne!(new_ref(), r);
    }

    #[test]
    fn hashing_uses_the_pepper() {
        let a = Pepper::new(b"a".repeat(32));
        let b = Pepper::new(b"b".repeat(32));
        let (_, key) = generate();
        let h = a.hash(&key);
        assert_eq!(h.len(), 64);
        assert!(a.verify(&key, &h));
        assert!(!b.verify(&key, &h), "another pepper gives another hash");
        assert!(!a.verify(&format!("{key}x"), &h));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn pepper_sources() {
        assert!(Pepper::from_values(None, None).unwrap().is_none());
        assert!(Pepper::from_values(Some("short".into()), None).is_err());
        let p = Pepper::from_values(Some("p".repeat(32)), None).unwrap().unwrap();
        assert_eq!(p.hash("k"), sha256_hex(format!("{}k", "p".repeat(32)).as_bytes()));
        let hex_key = "00".repeat(32);
        let from_master = Pepper::from_values(None, Some(hex_key.clone())).unwrap().unwrap();
        let b64_key = base64::engine::general_purpose::STANDARD.encode([0u8; 32]);
        let same = Pepper::from_values(None, Some(b64_key)).unwrap().unwrap();
        assert_eq!(from_master.hash("k"), same.hash("k"));
        assert_eq!(from_master.0, hmac_sha256(&[0u8; 32], PEPPER_LABEL));
        assert!(Pepper::from_values(None, Some("not a key".into())).is_err());
        assert_eq!(format!("{p:?}"), "Pepper(****)");
    }

    #[test]
    fn touches_are_batched_once_a_minute() {
        let t = Touches::default();
        t.record("key_a", 1_000);
        t.record("key_a", 2_000);
        t.record("key_b", 1_500);
        assert_eq!(t.due(3_000), vec![("key_a".into(), 2_000), ("key_b".into(), 1_500)]);
        t.record("key_a", 4_000);
        assert!(t.due(10_000).is_empty(), "key_a was written less than a minute ago");
        assert_eq!(t.due(63_000), vec![("key_a".into(), 4_000)]);
        assert!(t.due(200_000).is_empty());
    }
}
