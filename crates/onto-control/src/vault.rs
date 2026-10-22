//! Encryption at rest of project LLM keys (docs/CONTROL_PLANE.md section
//! 8.2): an AES-256-GCM envelope under `ONTO_MASTER_KEY`, the key-encryption
//! key (KEK). Each secret gets its own random data key (DEK):
//!
//! ```text
//! blob = 0x01                              version
//!      ‖ kek_id[8]                         first 8 bytes of SHA-256(KEK)
//!      ‖ nonce_k[12] ‖ AES-256-GCM(KEK, nonce_k, DEK[32],  aad = "onto/dek/v1/" ‖ project_id)   (48 bytes)
//!      ‖ nonce_d[12] ‖ AES-256-GCM(DEK, nonce_d, secret,   aad = "onto/llm-key/v1/" ‖ project_id)
//! ```
//!
//! The AAD binds a blob to its project and purpose. `kek_id` picks
//! `ONTO_MASTER_KEY` or `ONTO_MASTER_KEY_PREVIOUS` (decryption only);
//! [`Vault::rewrap`] moves a blob to the current KEK without touching the
//! secret's ciphertext.

use aws_lc_rs::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

use onto_core::{Error, Result};

use crate::keys::{parse_master_key, random_bytes, sha256};

/// The envelope format this build writes.
pub const VERSION: u8 = 0x01;
const KEK_ID_LEN: usize = 8;
const TAG_LEN: usize = 16;
const DEK_LEN: usize = 32;
/// Version, kek_id, nonce and the wrapped DEK with its tag.
const HEADER_LEN: usize = 1 + KEK_ID_LEN + NONCE_LEN + DEK_LEN + TAG_LEN;
const DEK_AAD: &str = "onto/dek/v1/";
const SECRET_AAD: &str = "onto/llm-key/v1/";

/// The message of a refused key store without a master key.
pub const NO_MASTER_KEY: &str = "ONTO_MASTER_KEY is not set; refusing to store LLM API keys";

/// One key-encryption key.
struct Kek {
    id: [u8; KEK_ID_LEN],
    bytes: Vec<u8>,
}

impl Kek {
    fn new(bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() != DEK_LEN {
            return Err(Error::Config("a master key must be 32 bytes".into()));
        }
        let mut id = [0u8; KEK_ID_LEN];
        id.copy_from_slice(&sha256(&bytes)[..KEK_ID_LEN]);
        Ok(Kek { id, bytes })
    }
}

/// The KEKs of this process: the current one, and the previous one during
/// a rotation.
pub struct Vault {
    current: Kek,
    previous: Option<Kek>,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vault")
            .field("kek_id", &crate::keys::hex(&self.current.id))
            .field("previous", &self.previous.as_ref().map(|k| crate::keys::hex(&k.id)))
            .finish()
    }
}

fn key(bytes: &[u8]) -> Result<LessSafeKey> {
    let unbound = UnboundKey::new(&AES_256_GCM, bytes).map_err(|_| Error::Other("AES-256-GCM key setup".into()))?;
    Ok(LessSafeKey::new(unbound))
}

fn nonce_of(bytes: &[u8]) -> Result<Nonce> {
    Nonce::try_assume_unique_for_key(bytes).map_err(|_| Error::Other("AES-256-GCM nonce".into()))
}

fn seal(k: &[u8], aad: &str, plain: &[u8]) -> Result<Vec<u8>> {
    let nonce = random_bytes(NONCE_LEN);
    let mut buf = plain.to_vec();
    key(k)?
        .seal_in_place_append_tag(nonce_of(&nonce)?, Aad::from(aad.as_bytes()), &mut buf)
        .map_err(|_| Error::Other("AES-256-GCM seal".into()))?;
    let mut out = nonce;
    out.extend_from_slice(&buf);
    Ok(out)
}

/// `nonce ‖ ciphertext ‖ tag` → plaintext; `None` when it does not authenticate.
fn open(k: &[u8], aad: &str, sealed: &[u8]) -> Option<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        return None;
    }
    let (nonce, ct) = sealed.split_at(NONCE_LEN);
    let mut buf = ct.to_vec();
    let plain = key(k).ok()?.open_in_place(nonce_of(nonce).ok()?, Aad::from(aad.as_bytes()), &mut buf).ok()?;
    Some(plain.to_vec())
}

impl Vault {
    /// `current` and the optional previous KEK, 32 bytes each.
    pub fn new(current: Vec<u8>, previous: Option<Vec<u8>>) -> Result<Self> {
        let current = Kek::new(current)?;
        let previous = previous.map(Kek::new).transpose()?.filter(|p| p.id != current.id);
        Ok(Vault { current, previous })
    }

    /// From `ONTO_MASTER_KEY` and `ONTO_MASTER_KEY_PREVIOUS`; `None` when
    /// `ONTO_MASTER_KEY` is unset or empty.
    pub fn from_env() -> Result<Option<Self>> {
        Self::from_values(std::env::var("ONTO_MASTER_KEY").ok(), std::env::var("ONTO_MASTER_KEY_PREVIOUS").ok())
    }

    pub fn from_values(current: Option<String>, previous: Option<String>) -> Result<Option<Self>> {
        let present = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
        let Some(current) = present(current) else { return Ok(None) };
        let previous = match present(previous) {
            Some(p) => Some(parse_master_key(&p).map_err(|_| {
                Error::Config("ONTO_MASTER_KEY_PREVIOUS must be 32 bytes, as base64 or 64 hex characters".into())
            })?),
            None => None,
        };
        Self::new(parse_master_key(&current)?, previous).map(Some)
    }

    /// Hex of the current KEK's id (shown by the operator CLI).
    pub fn kek_id(&self) -> String {
        crate::keys::hex(&self.current.id)
    }

    /// Encrypt `secret` for `project_id` under the current KEK.
    pub fn seal(&self, project_id: &str, secret: &[u8]) -> Result<Vec<u8>> {
        let dek = random_bytes(DEK_LEN);
        let mut blob = Vec::with_capacity(HEADER_LEN + NONCE_LEN + secret.len() + TAG_LEN);
        blob.push(VERSION);
        blob.extend_from_slice(&self.current.id);
        blob.extend_from_slice(&seal(&self.current.bytes, &format!("{DEK_AAD}{project_id}"), &dek)?);
        blob.extend_from_slice(&seal(&dek, &format!("{SECRET_AAD}{project_id}"), secret)?);
        Ok(blob)
    }

    fn kek_for(&self, blob: &[u8]) -> Result<&Kek> {
        if blob.len() < HEADER_LEN + NONCE_LEN + TAG_LEN || blob[0] != VERSION {
            return Err(Error::Invalid("the stored LLM API key is not a version 1 envelope".into()));
        }
        let id = &blob[1..1 + KEK_ID_LEN];
        [Some(&self.current), self.previous.as_ref()].into_iter().flatten().find(|k| k.id == id).ok_or_else(|| {
            Error::Invalid(format!(
                "the stored LLM API key was encrypted under master key {}, which is neither ONTO_MASTER_KEY \
                 nor ONTO_MASTER_KEY_PREVIOUS",
                crate::keys::hex(id)
            ))
        })
    }

    fn dek(&self, project_id: &str, blob: &[u8]) -> Result<Vec<u8>> {
        let kek = self.kek_for(blob)?;
        open(&kek.bytes, &format!("{DEK_AAD}{project_id}"), &blob[1 + KEK_ID_LEN..HEADER_LEN])
            .filter(|d| d.len() == DEK_LEN)
            .ok_or_else(|| {
                Error::Invalid("the stored LLM API key does not decrypt (altered, or another project's)".into())
            })
    }

    /// The secret in a blob sealed for `project_id`.
    pub fn open(&self, project_id: &str, blob: &[u8]) -> Result<Vec<u8>> {
        let dek = self.dek(project_id, blob)?;
        open(&dek, &format!("{SECRET_AAD}{project_id}"), &blob[HEADER_LEN..]).ok_or_else(|| {
            Error::Invalid("the stored LLM API key does not decrypt (altered, or another project's)".into())
        })
    }

    /// The blob with its DEK wrapped under the current KEK (the secret's
    /// ciphertext is kept); `None` when it already is.
    pub fn rewrap(&self, project_id: &str, blob: &[u8]) -> Result<Option<Vec<u8>>> {
        if self.kek_for(blob)?.id == self.current.id {
            return Ok(None);
        }
        let dek = self.dek(project_id, blob)?;
        let mut out = Vec::with_capacity(blob.len());
        out.push(VERSION);
        out.extend_from_slice(&self.current.id);
        out.extend_from_slice(&seal(&self.current.bytes, &format!("{DEK_AAD}{project_id}"), &dek)?);
        out.extend_from_slice(&blob[HEADER_LEN..]);
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault(k: u8, prev: Option<u8>) -> Vault {
        Vault::new(vec![k; 32], prev.map(|p| vec![p; 32])).unwrap()
    }

    #[test]
    fn round_trip_and_binding() {
        let v = vault(1, None);
        let blob = v.seal("prj_a", b"sk-secret-1234").unwrap();
        assert_eq!(blob[0], VERSION);
        assert_eq!(blob.len(), HEADER_LEN + NONCE_LEN + 14 + TAG_LEN);
        assert_eq!(v.open("prj_a", &blob).unwrap(), b"sk-secret-1234");
        // Two seals of one secret differ (fresh DEK and nonces).
        assert_ne!(v.seal("prj_a", b"sk-secret-1234").unwrap(), blob);
        // Another project's id (the AAD) does not decrypt.
        assert!(v.open("prj_b", &blob).is_err());
        // Any altered byte fails, in the header, the wrapped DEK or the secret.
        for i in [0, 3, 1 + KEK_ID_LEN + 2, HEADER_LEN - 1, HEADER_LEN + 3, blob.len() - 1] {
            let mut bad = blob.clone();
            bad[i] ^= 0x40;
            assert!(v.open("prj_a", &bad).is_err(), "byte {i}");
        }
        assert!(v.open("prj_a", &blob[..20]).is_err());
        // Another master key does not decrypt, and says which key it needs.
        let err = vault(2, None).open("prj_a", &blob).unwrap_err().to_string();
        assert!(err.contains(&crate::keys::hex(&blob[1..9])), "{err}");
    }

    #[test]
    fn rotation() {
        let old = vault(1, None);
        let blob = old.seal("prj_a", b"sk-rotate").unwrap();
        let new = vault(2, Some(1));
        // The previous key still decrypts.
        assert_eq!(new.open("prj_a", &blob).unwrap(), b"sk-rotate");
        let rewrapped = new.rewrap("prj_a", &blob).unwrap().unwrap();
        assert_eq!(&rewrapped[HEADER_LEN..], &blob[HEADER_LEN..], "the secret's ciphertext is kept");
        assert_eq!(new.rewrap("prj_a", &rewrapped).unwrap(), None, "already current");
        // Without the previous key only the rewrapped blob opens.
        let only_new = vault(2, None);
        assert_eq!(only_new.open("prj_a", &rewrapped).unwrap(), b"sk-rotate");
        assert!(only_new.open("prj_a", &blob).is_err());
        assert!(new.rewrap("prj_b", &blob).is_err());
    }

    #[test]
    fn master_keys_from_the_environment() {
        assert!(Vault::from_values(None, Some("x".into())).unwrap().is_none(), "no ONTO_MASTER_KEY: no vault");
        let hex64 = "ab".repeat(32);
        let v = Vault::from_values(Some(hex64.clone()), None).unwrap().unwrap();
        let b64 = crate::keys::b64(&[0xab; 32]);
        let same = Vault::from_values(Some(b64), None).unwrap().unwrap();
        assert_eq!(v.kek_id(), same.kek_id());
        assert!(Vault::from_values(Some("short".into()), None).is_err());
        let err = Vault::from_values(Some(hex64), Some("bad".into())).unwrap_err();
        assert!(err.to_string().contains("ONTO_MASTER_KEY_PREVIOUS"), "{err}");
    }
}
