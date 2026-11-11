//! The Ed25519 snapshot signer.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey};
use spot_journal_snapshot::SnapshotSigner;

/// Signs a snapshot index and verified marker with the tenant journal key.
///
/// The key is the caller's argument. The type is deliberately not `Clone`, and its `Debug` shows
/// the key id only.
pub struct Ed25519SnapshotSigner {
    /// The id the signed objects name; the verifying side looks the public key up by it.
    key_id: String,
    /// The secret key; never printed, logged or returned.
    signing_key: SigningKey,
}

impl Ed25519SnapshotSigner {
    /// A signer that signs under `key_id` with `signing_key`.
    #[must_use]
    pub fn new(key_id: &str, signing_key: SigningKey) -> Self {
        Self {
            key_id: key_id.to_owned(),
            signing_key,
        }
    }
}

impl fmt::Debug for Ed25519SnapshotSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ed25519SnapshotSigner")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl SnapshotSigner for Ed25519SnapshotSigner {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.signing_key.sign(message).to_bytes().to_vec()
    }
}
