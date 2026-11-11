//! Signed checkpoints, the signer trait and the key ring.

use std::collections::HashMap;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::canonical::put_field;
use crate::entry::Hash;
use crate::error::AuditError;

const CHECKPOINT_TAG: &[u8] = b"spot.audit.checkpoint.v1\0";

/// Signs checkpoint bodies. A local key in tests; a managed key later (WP-SEC-03).
pub trait CheckpointSigner: Send + Sync {
    /// The id of the signing key (see [`key_id_of`]).
    fn key_id(&self) -> String;

    /// An Ed25519 signature (64 bytes) over `message`.
    ///
    /// # Errors
    /// [`AuditError::Signer`] when the key is unavailable.
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, AuditError>;
}

/// The id of a public key: the first 16 bytes of its SHA-256 as 32 lower-case hex characters
/// (the derivation `spot-bundle-signing` uses).
#[must_use]
pub fn key_id_of(public_key: &[u8; 32]) -> String {
    let digest = Sha256::digest(public_key);
    let mut hex = String::with_capacity(32);
    for byte in &digest[..16] {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// A key held in memory, for tests and development. The caller supplies the 32 seed bytes.
pub struct LocalSigner {
    key: SigningKey,
}

impl LocalSigner {
    /// A signer from a 32-byte seed.
    #[must_use]
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(&seed),
        }
    }

    /// The public key, to add to a [`KeyRing`].
    #[must_use]
    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }
}

impl std::fmt::Debug for LocalSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LocalSigner({})", self.key_id())
    }
}

impl CheckpointSigner for LocalSigner {
    fn key_id(&self) -> String {
        key_id_of(&self.public_key())
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, AuditError> {
        Ok(self.key.sign(message).to_bytes().to_vec())
    }
}

/// Maps key ids to public keys, so checkpoints signed before a key rotation stay verifiable.
#[derive(Clone, Debug, Default)]
pub struct KeyRing {
    keys: HashMap<String, VerifyingKey>,
}

impl KeyRing {
    /// An empty ring.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a public key and returns its id.
    ///
    /// # Errors
    /// [`AuditError::KeyInvalid`] when the bytes are not an Ed25519 public key.
    pub fn add(&mut self, public_key: &[u8]) -> Result<String, AuditError> {
        let array: [u8; 32] = public_key.try_into().map_err(|_| AuditError::KeyInvalid)?;
        let key = VerifyingKey::from_bytes(&array).map_err(|_| AuditError::KeyInvalid)?;
        let id = key_id_of(&array);
        self.keys.insert(id.clone(), key);
        Ok(id)
    }

    /// Whether the ring knows `key_id`.
    #[must_use]
    pub fn contains(&self, key_id: &str) -> bool {
        self.keys.contains_key(key_id)
    }

    /// Checks `signature` over `message` under `key_id`: `None` when the key is unknown,
    /// otherwise whether the signature holds.
    pub(crate) fn check(&self, key_id: &str, message: &[u8], signature: &[u8]) -> Option<bool> {
        let key = self.keys.get(key_id)?;
        let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
            return Some(false);
        };
        Some(key.verify(message, &Signature::from_bytes(&bytes)).is_ok())
    }
}

/// A signed statement that the chain of `site` had head `head_hash` at `seq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    /// The site.
    pub site: String,
    /// The sequence number of the head it covers.
    pub seq: u64,
    /// The hash of that head entry.
    pub head_hash: Hash,
    /// Unix microseconds, supplied by the caller.
    pub at: i64,
    /// The id of the signing key.
    pub key_id: String,
    /// The Ed25519 signature (64 bytes) over [`Checkpoint::canonical_bytes`].
    pub signature: Vec<u8>,
}

impl Checkpoint {
    /// The canonical bytes the signature covers (everything but the signature).
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        body_bytes(&self.site, self.seq, &self.head_hash, self.at, &self.key_id)
    }

    /// Signs a checkpoint with `signer`.
    ///
    /// # Errors
    /// [`AuditError::Signer`] when the signer fails.
    pub fn sign(
        signer: &dyn CheckpointSigner,
        site: &str,
        seq: u64,
        head_hash: Hash,
        at: i64,
    ) -> Result<Self, AuditError> {
        let key_id = signer.key_id();
        let body = body_bytes(site, seq, &head_hash, at, &key_id);
        let signature = signer.sign(&body)?;
        Ok(Self {
            site: site.to_owned(),
            seq,
            head_hash,
            at,
            key_id,
            signature,
        })
    }
}

fn body_bytes(site: &str, seq: u64, head_hash: &Hash, at: i64, key_id: &str) -> Vec<u8> {
    let mut out = CHECKPOINT_TAG.to_vec();
    put_field(&mut out, site.as_bytes());
    out.extend_from_slice(&seq.to_be_bytes());
    out.extend_from_slice(head_hash);
    out.extend_from_slice(&at.to_be_bytes());
    put_field(&mut out, key_id.as_bytes());
    out
}
