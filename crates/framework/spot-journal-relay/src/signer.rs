//! The signing seam: the key never enters this crate.

use spot_journal_manifest::{Manifest, SignedManifest};

/// Signs manifests with the tenant's journal key.
///
/// Synchronous on purpose: signing is a local computation. An implementation that calls a key
/// service wraps it itself. The relay never sees the key.
pub trait ManifestSigner: Send + Sync {
    /// Signs `manifest` under [`ManifestSigner::key_id`].
    fn sign(&self, manifest: &Manifest) -> SignedManifest;

    /// The id of the key `sign` uses (an id, never key material).
    fn key_id(&self) -> &str;
}
