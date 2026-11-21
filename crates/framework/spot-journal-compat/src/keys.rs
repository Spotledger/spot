//! The verifying side of the fixture's key: what a reader of the fixture's public-key file trusts.
//!
//! A fixture carries one public key and no secret. [`FixtureKeys`] is the public half only: it
//! verifies the manifest signature (through the manifest crate's key windows) and the snapshot
//! index signature (through the snapshot crate's [`SnapshotKeys`] seam). The generator's private
//! half is a fixed TEST-ONLY constant in [`crate::generator::key`] and never reaches a reader.

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use spot_journal_manifest::VerifyingKeys;
use spot_journal_snapshot::SnapshotKeys;

use crate::error::CompatError;
use crate::json::{hex, unhex32};

/// The key id every fixture object names: the id of the throwaway test key, never a tenant's.
pub const KEY_ID: &str = "journal-fixture-key-1";

/// The public key a fixture's the fixture's public-key file holds.
#[derive(Clone, Debug)]
pub struct FixtureKeys {
    /// The Ed25519 public key.
    key: VerifyingKey,
}

impl FixtureKeys {
    /// The keys a 64-digit hex public key names.
    ///
    /// # Errors
    /// [`CompatError`] when the text is not 64 hex digits of an Ed25519 public key.
    pub fn from_hex(text: &str) -> Result<Self, CompatError> {
        let bytes = unhex32(text.trim())
            .ok_or_else(|| CompatError::new("the public key is not 64 lower-case hex digits"))?;
        let key = VerifyingKey::from_bytes(&bytes)
            .map_err(|_| CompatError::new("key.pub is not an Ed25519 public key"))?;
        Ok(Self { key })
    }

    /// The keys of `key`.
    #[must_use]
    pub fn new(key: VerifyingKey) -> Self {
        Self { key }
    }

    /// The public key as the fixture's public-key file spells it.
    #[must_use]
    pub fn to_hex(&self) -> String {
        hex(self.key.as_bytes())
    }

    /// The manifest keys: this one key, trusted for every generation (`1..=u64::MAX`), since a
    /// frozen fixture holds one generation and a throwaway key that never rotates.
    #[must_use]
    pub fn manifest_keys(&self) -> VerifyingKeys {
        let mut keys = VerifyingKeys::new();
        // The window is valid by construction (a non-empty id, generation 1 to the maximum).
        if keys.add_key(KEY_ID, self.key, 1, u64::MAX).is_err() {
            return VerifyingKeys::new();
        }
        keys
    }
}

impl SnapshotKeys for FixtureKeys {
    fn verify(&self, key_id: &str, _generation: u64, message: &[u8], signature: &[u8]) -> bool {
        if key_id != KEY_ID {
            return false;
        }
        let Ok(bytes) = <[u8; 64]>::try_from(signature) else {
            return false;
        };
        self.key
            .verify(message, &Signature::from_bytes(&bytes))
            .is_ok()
    }
}
