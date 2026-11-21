//! The snapshot key set over the manifest's key windows.

use spot_journal_manifest::VerifyingKeys;
use spot_journal_snapshot::SnapshotKeys;

/// The public keys a snapshot reader trusts, with the rotation windows of the manifest.
///
/// The set is one tenant's: the caller builds it from that tenant's public keys, so another
/// tenant's signature never verifies.
#[derive(Clone, Debug)]
pub struct ManifestSnapshotKeys {
    /// The tenant's key windows.
    keys: VerifyingKeys,
}

impl ManifestSnapshotKeys {
    /// Wraps a tenant's key set.
    #[must_use]
    pub fn new(keys: VerifyingKeys) -> Self {
        Self { keys }
    }
}

impl SnapshotKeys for ManifestSnapshotKeys {
    /// Whether `signature` verifies over `message` under `key_id`, with the key's window covering
    /// `generation`. An unknown key, a key outside its window and a bad signature all answer
    /// `false`, with no hint which.
    fn verify(&self, key_id: &str, generation: u64, message: &[u8], signature: &[u8]) -> bool {
        self.keys
            .verify_message(key_id, generation, message, signature)
            .is_ok()
    }
}
