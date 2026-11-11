//! The administrator trust store: the public keys whose signatures the agent accepts,
//! with their protection (software or hardware), optional revocation instant and, for
//! hardware keys, the verified attestation behind that claim.
//!
//! The store is loaded from the enrolment result (the console pushes the administrators'
//! hardware-key public keys when the agent joins) and is the only key material the agent
//! ever trusts. It is turned into a [`spot_signed_command::TrustedKeys`] on every
//! decision, so revocation takes effect immediately.

use std::collections::BTreeMap;

use ed25519_dalek::VerifyingKey;
use spot_agent_enroll::keys::Protection;
use spot_signed_command::{KeyId, TrustedKeys};

use crate::attestation::{AdminAttestation, KeyAttestation};
use crate::error::CoreError;

/// One trusted administrator key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdminKey {
    /// The key id: the first 16 bytes of SHA-256 of the public key, hex (the derivation
    /// `KeyId::from_public_key` uses).
    pub key_id: KeyId,
    /// The raw Ed25519 public key.
    pub public_key: [u8; 32],
    /// How the private half is protected. A hardware key's signature proves possession of
    /// a key the administrator's device holds; a software key does not.
    pub protection: Protection,
    /// First instant the key is no longer trusted, or `None` while it is.
    pub revoked_at: Option<i64>,
}

impl AdminKey {
    /// Whether this key is hardware-backed.
    #[must_use]
    pub fn is_hardware(&self) -> bool {
        self.protection == Protection::Hardware
    }
}

/// The administrator trust store: the keys dangerous commands are checked against.
///
/// Keyed by [`KeyId`]; insertion derives the id from the public key, so a wire key id is
/// never trusted on its own.
#[derive(Clone, Debug, Default)]
pub struct TrustStore {
    keys: BTreeMap<KeyId, AdminKey>,
}

impl TrustStore {
    /// An empty store: nothing verifies.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds an administrator public key with its protection, deriving the key id.
    ///
    /// # Errors
    /// [`CoreError::InvalidKey`] when the bytes are not a valid Ed25519 public key.
    pub fn insert(
        &mut self,
        public_key: &[u8; 32],
        protection: Protection,
    ) -> Result<KeyId, CoreError> {
        VerifyingKey::from_bytes(public_key).map_err(|_| CoreError::InvalidKey)?;
        let key_id = KeyId::from_public_key(public_key);
        self.keys.insert(
            key_id.clone(),
            AdminKey {
                key_id: key_id.clone(),
                public_key: *public_key,
                protection,
                revoked_at: None,
            },
        );
        Ok(key_id)
    }

    /// Adds an administrator key only after `verifier` proves `statement` attests exactly
    /// that key as hardware-backed at `now`. The stored protection is
    /// [`Protection::Hardware`].
    ///
    /// # Errors
    /// [`CoreError::Attestation`] when the statement does not verify, or
    /// [`CoreError::InvalidKey`] when the key bytes are not a valid Ed25519 point.
    pub fn insert_attested(
        &mut self,
        statement: &AdminAttestation,
        verifier: &dyn KeyAttestation,
        now: i64,
    ) -> Result<KeyId, CoreError> {
        verifier.verify(statement, &statement.operator_key, now)?;
        self.insert(&statement.operator_key, Protection::Hardware)
    }

    /// Revokes `key_id` from `at` on. Returns whether the key was present.
    pub fn revoke(&mut self, key_id: &KeyId, at: i64) -> bool {
        match self.keys.get_mut(key_id) {
            Some(key) => {
                key.revoked_at = Some(at);
                true
            }
            None => false,
        }
    }

    /// The key entry for `key_id`, revoked or not.
    #[must_use]
    pub fn get(&self, key_id: &KeyId) -> Option<&AdminKey> {
        self.keys.get(key_id)
    }

    /// Whether `key_id` is present and hardware-backed.
    #[must_use]
    pub fn is_hardware(&self, key_id: &KeyId) -> bool {
        self.keys.get(key_id).is_some_and(AdminKey::is_hardware)
    }

    /// How many keys are trusted, revoked ones included.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Every key, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &AdminKey> {
        self.keys.values()
    }

    /// The trust anchor the signed-command verifier checks signatures against, carrying
    /// every key's revocation instant.
    #[must_use]
    pub fn trusted_keys(&self) -> TrustedKeys {
        let mut trusted = TrustedKeys::new();
        for key in self.keys.values() {
            // The key was validated when it entered the store, so this can only fail if
            // the same bytes are somehow unusable now; failing closed (leaving the key
            // out) is the safe outcome.
            if trusted.insert(key.key_id.clone(), &key.public_key).is_err() {
                continue;
            }
            if let Some(revoked_at) = key.revoked_at {
                trusted.revoke(&key.key_id, revoked_at);
            }
        }
        trusted
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use ed25519_dalek::SigningKey;

    use super::*;

    fn key(seed: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = seed;
        SigningKey::from_bytes(&bytes).verifying_key().to_bytes()
    }

    #[test]
    fn insert_derives_the_key_id_and_revocation_takes_effect() {
        let mut store = TrustStore::new();
        assert!(store.is_empty());
        let public = key(1);
        let id = store.insert(&public, Protection::Hardware).unwrap();
        assert_eq!(id, KeyId::from_public_key(&public));
        assert_eq!(store.len(), 1);
        assert!(store.is_hardware(&id));
        assert_eq!(store.get(&id).unwrap().public_key, public);
        let trusted = store.trusted_keys();
        assert_eq!(trusted.len(), 1);
        assert!(store.revoke(&id, 500));
        assert_eq!(store.get(&id).unwrap().revoked_at, Some(500));
        assert!(!store.revoke(&KeyId::from_public_key(&key(9)), 1));
    }

    #[test]
    fn a_non_point_is_refused() {
        // Half of all y encodings have no matching x, so some repeated-byte pattern is not
        // a point; find one and prove the store refuses it with the invalid-key error.
        let mut found = false;
        for byte in 0u8..=255 {
            let bytes = [byte; 32];
            if VerifyingKey::from_bytes(&bytes).is_err() {
                let mut store = TrustStore::new();
                assert_eq!(
                    store.insert(&bytes, Protection::Software),
                    Err(CoreError::InvalidKey)
                );
                assert!(store.is_empty());
                found = true;
                break;
            }
        }
        assert!(
            found,
            "no invalid repeated-byte encoding to exercise the error path"
        );
    }
}
