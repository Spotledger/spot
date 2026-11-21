//! The journal's keys, derived from the site's key provider. Nothing here is stored: the
//! provider holds the key encryption key, and the manifest signing key is derived from it on
//! demand, so no key byte is in a config file, a log, an error or a readiness text.

use std::fmt;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use spot_journal_manifest::{Manifest, SignedManifest, VerifyingKeys, sign};
use spot_journal_relay::{ManifestSigner, TailKeys};
use spot_secrets::KeyProvider;

use crate::error::ShipFault;

/// The `derive_key` purpose of the manifest signing key.
pub const SIGNING_PURPOSE: &[u8] = b"journal-manifest-sign";
const SEED_LEN: usize = 32;

/// A manifest signer over a key derived from the provider. Not `Clone`; `Debug` shows the key
/// id only.
pub struct DerivedSigner {
    key_id: String,
    key: SigningKey,
}

impl fmt::Debug for DerivedSigner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DerivedSigner")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl ManifestSigner for DerivedSigner {
    fn sign(&self, manifest: &Manifest) -> SignedManifest {
        sign(manifest, &self.key, &self.key_id)
    }

    fn key_id(&self) -> &str {
        &self.key_id
    }
}

/// The signing and verifying material of one tenant's journal, derived once at start.
pub struct JournalKeys {
    provider: Arc<dyn KeyProvider>,
    key_id: String,
    verifying: VerifyingKeys,
    seed: [u8; SEED_LEN],
}

impl fmt::Debug for JournalKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JournalKeys")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl JournalKeys {
    /// Derives the manifest signing key from `provider`'s current key version.
    ///
    /// The key id names the provider key and version (`<id>-v<version>`), so a rotation gives a
    /// new id; the verifying set holds the key for every generation (`1..=u64::MAX`).
    ///
    /// # Errors
    /// [`ShipFault::Config`] when the provider refuses to derive (the fixed sentence never
    /// carries the provider's message).
    pub async fn derive(provider: Arc<dyn KeyProvider>) -> Result<Self, ShipFault> {
        let current = provider.current();
        let derived = provider
            .derive_key(&current, SIGNING_PURPOSE, SEED_LEN)
            .await
            .map_err(|_| ShipFault::Config("the key provider cannot derive the journal key"))?;
        let seed: [u8; SEED_LEN] = derived
            .expose()
            .try_into()
            .map_err(|_| ShipFault::Config("the derived journal key has an unusable length"))?;
        let key_id = format!("{}-v{}", current.id, current.version);
        let signing = SigningKey::from_bytes(&seed);
        let mut verifying = VerifyingKeys::new();
        verifying
            .add_key(&key_id, signing.verifying_key(), 1, u64::MAX)
            .map_err(|_| ShipFault::Config("the journal verifying key is unusable"))?;
        Ok(Self {
            provider,
            key_id,
            verifying,
            seed,
        })
    }

    /// A fresh signer (the relay takes ownership of one per generation).
    #[must_use]
    pub fn signer(&self) -> Box<dyn ManifestSigner> {
        Box::new(DerivedSigner {
            key_id: self.key_id.clone(),
            key: SigningKey::from_bytes(&self.seed),
        })
    }

    /// The public keys manifests verify against.
    #[must_use]
    pub fn verifying(&self) -> VerifyingKeys {
        self.verifying.clone()
    }

    /// The tail-chunk keys (data keys wrapped by the tenant journal key).
    #[must_use]
    pub fn tail_keys(&self) -> TailKeys {
        TailKeys::new(Arc::clone(&self.provider))
    }
}
