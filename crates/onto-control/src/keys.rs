//! API keys and the small amount of cryptography the control store needs:
//! random bytes, SHA-256, HMAC-SHA256 and constant-time comparison, all from
//! `aws-lc-rs`.
//!
//! A key looks like `onto_sk_7f3k9q2m_Vb1wQ9…`: the prefix (`onto_sk_` and 8
//! characters of `[a-z0-9]`) is stored and shown; the secret (base64url of 32
//! random bytes, 43 characters) is never stored. The stored hash is
//! `<pepper id>:` and lowercase hex `SHA-256(pepper || full key)` ([`Pepper`]).

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
/// Label of the pepper earlier versions derived from `ONTO_MASTER_KEY`
/// (still accepted for the untagged hashes they wrote).
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

/// One pepper value and its id (the first 8 hex characters of
/// `SHA-256("onto pepper id" || pepper)`), which tags the hashes it makes.
#[derive(Clone)]
struct PepperKey {
    id: String,
    bytes: Vec<u8>,
}

impl PepperKey {
    fn new(bytes: Vec<u8>) -> Self {
        let mut data = b"onto pepper id".to_vec();
        data.extend_from_slice(&bytes);
        Self { id: sha256_hex(&data)[..8].to_string(), bytes }
    }

    fn digest(&self, full_key: &str) -> String {
        let mut data = self.bytes.clone();
        data.extend_from_slice(full_key.as_bytes());
        sha256_hex(&data)
    }
}

/// How a presented key matched its stored hash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PepperMatch {
    No,
    /// Under the current pepper.
    Current,
    /// Under a previous (or the legacy master-key derived) pepper: the hash
    /// should be rewritten with the current one.
    Stale,
}

/// The secret mixed into every key hash: `ONTO_KEY_PEPPER`, its own secret,
/// separate from `ONTO_MASTER_KEY`, so rotating the master key never breaks
/// a key. A stored hash is `<pepper id>:<hex SHA-256(pepper || key)>`;
/// verification tries the pepper the id names (the current one or
/// `ONTO_KEY_PEPPER_PREVIOUS`), and an untagged hash (written before
/// peppers had ids) every known pepper, including the one earlier versions
/// derived from `ONTO_MASTER_KEY` (and `ONTO_MASTER_KEY_PREVIOUS`).
#[derive(Clone)]
pub struct Pepper {
    current: PepperKey,
    previous: Vec<PepperKey>,
    /// Peppers of untagged hashes only (derived from the master keys).
    legacy: Vec<PepperKey>,
}

impl std::fmt::Debug for Pepper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pepper(****)")
    }
}

/// The pepper earlier versions derived from a master key.
fn derived(master_key: &str) -> Result<PepperKey> {
    Ok(PepperKey::new(hmac_sha256(&parse_master_key(master_key)?, PEPPER_LABEL)))
}

fn pepper_value(name: &str, v: String) -> Result<Vec<u8>> {
    let v = v.trim().to_string();
    if v.chars().count() < 32 {
        return Err(Error::Config(format!("{name} must have at least 32 characters")));
    }
    Ok(v.into_bytes())
}

impl Pepper {
    pub fn new(bytes: Vec<u8>) -> Self {
        Pepper { current: PepperKey::new(bytes), previous: Vec::new(), legacy: Vec::new() }
    }

    /// A random pepper for this process only (the memory control store).
    pub fn random() -> Self {
        Self::new(random_bytes(32))
    }

    /// This pepper's id, as stored in front of its hashes.
    pub fn id(&self) -> &str {
        &self.current.id
    }

    /// From `ONTO_KEY_PEPPER` (at least 32 characters) and
    /// `ONTO_KEY_PEPPER_PREVIOUS`; the master keys only verify untagged
    /// hashes. `None` when `ONTO_KEY_PEPPER` is not set.
    pub fn from_env() -> Result<Option<Self>> {
        let var = |n: &str| std::env::var(n).ok();
        Self::from_values(
            var("ONTO_KEY_PEPPER"),
            var("ONTO_KEY_PEPPER_PREVIOUS"),
            [var("ONTO_MASTER_KEY"), var("ONTO_MASTER_KEY_PREVIOUS")],
        )
    }

    pub fn from_values(
        pepper: Option<String>,
        previous: Option<String>,
        master_keys: [Option<String>; 2],
    ) -> Result<Option<Self>> {
        let some = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        let Some(p) = some(pepper) else {
            if some(previous).is_some() {
                return Err(Error::Config("ONTO_KEY_PEPPER_PREVIOUS is set without ONTO_KEY_PEPPER".into()));
            }
            return Ok(None);
        };
        let mut me = Self::new(pepper_value("ONTO_KEY_PEPPER", p)?);
        if let Some(prev) = some(previous) {
            me.previous.push(PepperKey::new(pepper_value("ONTO_KEY_PEPPER_PREVIOUS", prev)?));
        }
        for m in master_keys.into_iter().flatten().filter(|m| !m.trim().is_empty()) {
            me.legacy.push(derived(&m)?);
        }
        Ok(Some(me))
    }

    /// `<id>:<hex SHA-256(pepper || full key)>` under the current pepper.
    pub fn hash(&self, full_key: &str) -> String {
        format!("{}:{}", self.current.id, self.current.digest(full_key))
    }

    /// Does the presented key match the stored hash (constant time per
    /// candidate pepper), and under which pepper?
    pub fn check(&self, full_key: &str, stored_hash: &str) -> PepperMatch {
        let eq = |k: &PepperKey, hex: &str| constant_time_eq(k.digest(full_key).as_bytes(), hex.as_bytes());
        match stored_hash.split_once(':') {
            Some((id, hex)) => {
                if id == self.current.id {
                    return if eq(&self.current, hex) { PepperMatch::Current } else { PepperMatch::No };
                }
                match self.previous.iter().find(|k| k.id == id) {
                    Some(k) if eq(k, hex) => PepperMatch::Stale,
                    _ => PepperMatch::No,
                }
            }
            None => {
                let all = std::iter::once(&self.current).chain(&self.previous).chain(&self.legacy);
                if all.into_iter().any(|k| eq(k, stored_hash)) { PepperMatch::Stale } else { PepperMatch::No }
            }
        }
    }

    /// The presented key matches the stored hash.
    pub fn verify(&self, full_key: &str, stored_hash: &str) -> bool {
        self.check(full_key, stored_hash) != PepperMatch::No
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
        assert_eq!(h.len(), 8 + 1 + 64);
        assert!(h.starts_with(&format!("{}:", a.id())));
        assert_eq!(a.check(&key, &h), PepperMatch::Current);
        assert!(!b.verify(&key, &h), "another pepper gives another hash");
        assert!(!a.verify(&format!("{key}x"), &h));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn pepper_sources() {
        let none = || [None, None];
        assert!(Pepper::from_values(None, None, none()).unwrap().is_none());
        assert!(Pepper::from_values(Some("short".into()), None, none()).is_err());
        assert!(Pepper::from_values(None, Some("p".repeat(32)), none()).is_err(), "a previous one needs a current one");
        let p = Pepper::from_values(Some("p".repeat(32)), None, none()).unwrap().unwrap();
        assert_eq!(p.hash("k"), format!("{}:{}", p.id(), sha256_hex(format!("{}k", "p".repeat(32)).as_bytes())));
        // A master key alone no longer makes a pepper.
        assert!(Pepper::from_values(None, None, [Some("00".repeat(32)), None]).unwrap().is_none());
        assert!(Pepper::from_values(Some("p".repeat(32)), None, [Some("not a key".into()), None]).is_err());
        assert_eq!(format!("{p:?}"), "Pepper(****)");
    }

    #[test]
    fn rotating_the_master_key_keeps_keys_valid() {
        let (_, key) = generate();
        let old_master = "00".repeat(32);
        // A hash as earlier versions stored it: untagged, master-key derived.
        let legacy = derived(&old_master).unwrap().digest(&key);
        let pepper = || Some("p".repeat(32));
        let p = Pepper::from_values(pepper(), None, [Some(old_master.clone()), None]).unwrap().unwrap();
        assert_eq!(p.check(&key, &legacy), PepperMatch::Stale, "verified, and to be rewritten");
        // A key hashed now does not depend on the master key at all.
        let tagged = p.hash(&key);
        let new_master = "11".repeat(32);
        let rotated = Pepper::from_values(pepper(), None, [Some(new_master), Some(old_master)]).unwrap().unwrap();
        assert_eq!(rotated.check(&key, &tagged), PepperMatch::Current);
        let alone = Pepper::from_values(pepper(), None, [None, None]).unwrap().unwrap();
        assert_eq!(alone.check(&key, &tagged), PepperMatch::Current, "no master key needed");
        assert_eq!(alone.check(&key, &legacy), PepperMatch::No, "the legacy pepper is gone with its master key");
        // Rotating the pepper: the previous one still verifies its hashes.
        let next = Pepper::from_values(Some("q".repeat(32)), pepper(), [None, None]).unwrap().unwrap();
        assert_eq!(next.check(&key, &tagged), PepperMatch::Stale);
        assert_eq!(next.check(&key, &next.hash(&key)), PepperMatch::Current);
        let forgot = Pepper::from_values(Some("q".repeat(32)), None, [None, None]).unwrap().unwrap();
        assert_eq!(forgot.check(&key, &tagged), PepperMatch::No);
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
