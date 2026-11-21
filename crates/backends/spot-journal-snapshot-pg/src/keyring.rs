//! The snapshot signer and the verifying keys, derived from the site's key provider exactly as
//! the journal's manifest key is (`spot_journal_ship::JournalKeys`): one key for manifests and
//! snapshots, never stored, never printed.

use std::fmt;
use std::sync::Arc;

use ed25519_dalek::SigningKey;
use spot_journal_manifest::VerifyingKeys;
use spot_journal_ship::keys::SIGNING_PURPOSE;
use spot_journal_sign::{Ed25519SnapshotSigner, ManifestSnapshotKeys};
use spot_secrets::KeyProvider;

use crate::error::SnapshotPgError;

/// The signing and verifying material of one tenant's snapshots.
pub struct SnapshotKeyring {
    key_id: String,
    seed: [u8; 32],
    verifying: VerifyingKeys,
}

impl fmt::Debug for SnapshotKeyring {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotKeyring")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl SnapshotKeyring {
    /// Derives the keys from `provider`'s current key version.
    ///
    /// # Errors
    /// [`SnapshotPgError::Key`] when the provider refuses (the fixed sentence never carries its
    /// message).
    pub async fn derive(provider: &Arc<dyn KeyProvider>) -> Result<Self, SnapshotPgError> {
        let current = provider.current();
        let derived = provider
            .derive_key(&current, SIGNING_PURPOSE, 32)
            .await
            .map_err(|_| SnapshotPgError::Key)?;
        let seed: [u8; 32] = derived
            .expose()
            .try_into()
            .map_err(|_| SnapshotPgError::Key)?;
        // The same key id format as the journal's manifest key, so one verifying set serves both.
        let key_id = format!("{}-v{}", current.id, current.version);
        let mut verifying = VerifyingKeys::new();
        verifying
            .add_key(
                &key_id,
                SigningKey::from_bytes(&seed).verifying_key(),
                1,
                u64::MAX,
            )
            .map_err(|_| SnapshotPgError::Key)?;
        Ok(Self {
            key_id,
            seed,
            verifying,
        })
    }

    /// A signer under the derived key.
    #[must_use]
    pub fn signer(&self) -> Ed25519SnapshotSigner {
        Ed25519SnapshotSigner::new(&self.key_id, SigningKey::from_bytes(&self.seed))
    }

    /// The verifying keys (`SnapshotKeys`).
    #[must_use]
    pub fn keys(&self) -> ManifestSnapshotKeys {
        ManifestSnapshotKeys::new(self.verifying.clone())
    }
}
