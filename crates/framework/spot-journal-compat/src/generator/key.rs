//! The fixture's signing key: a fixed, throwaway, **TEST-ONLY** key.
//!
//! # This is not a secret and not a tenant's key
//!
//! The 32 bytes below are a constant in the generator on purpose. Every fixture object is signed
//! with it, and any reader of the fixture can check the signature, so publishing the public half
//! in `key.pub` costs nothing: the private half signs invented documents in a test fixture, it is
//! never used by a running system, it is never a tenant's journal key, and rotating it would only
//! mean regenerating the fixture. Nothing here logs it, and the loader only ever sees `key.pub`.
//!
//! The generator is the only code that holds it; [`crate::FixtureKeys`] is the public half.

use ed25519_dalek::{Signer, SigningKey};
use spot_journal_snapshot::SnapshotSigner;

use crate::FixtureKeys;
use crate::keys::KEY_ID;

/// The TEST-ONLY Ed25519 seed every fixture object is signed with.
///
/// A seed, not a key anyone uses: see the module documentation. It is a constant so the fixture's
/// bytes are reproducible; a random key would make every regeneration a different fixture.
pub const TEST_ONLY_SIGNING_KEY: [u8; 32] = [
    0x9a, 0x1f, 0x5c, 0x27, 0x83, 0x4b, 0xd6, 0x0e, 0x71, 0x38, 0xac, 0x52, 0xe4, 0x19, 0x6d, 0xb0,
    0x2e, 0xf7, 0x43, 0x8c, 0x15, 0xa9, 0x60, 0xde, 0x37, 0x72, 0xc1, 0x04, 0x8b, 0x5e, 0xd2, 0x26,
];

/// Signs the fixture's manifest and snapshot index with the TEST-ONLY key.
pub struct FixtureSigner {
    /// The throwaway key.
    key: SigningKey,
}

impl FixtureSigner {
    /// The signer of [`TEST_ONLY_SIGNING_KEY`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            key: SigningKey::from_bytes(&TEST_ONLY_SIGNING_KEY),
        }
    }

    /// The throwaway key, for the manifest's `sign` call.
    #[must_use]
    pub fn signing_key(&self) -> &SigningKey {
        &self.key
    }

    /// The public half, as a reader of `key.pub` gets it.
    #[must_use]
    pub fn keys(&self) -> FixtureKeys {
        FixtureKeys::new(self.key.verifying_key())
    }
}

impl Default for FixtureSigner {
    fn default() -> Self {
        Self::new()
    }
}

impl SnapshotSigner for FixtureSigner {
    fn key_id(&self) -> &str {
        KEY_ID
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.key.sign(message).to_bytes().to_vec()
    }
}
