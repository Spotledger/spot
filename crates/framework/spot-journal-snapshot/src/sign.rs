//! The seams the caller fills: the signer, the verifying keys and the part transform.
//!
//! The crate holds no key material and reads no randomness: a signer and a key set are traits.
//! An Ed25519 implementation over the manifest's key windows is an adapter outside this crate.

use crate::error::SnapshotError;

/// Signs a snapshot index (and a verified marker) with the tenant journal key.
pub trait SnapshotSigner: Send + Sync {
    /// The id of the signing key (an id, never key material).
    fn key_id(&self) -> &str;
    /// The signature over `message`.
    fn sign(&self, message: &[u8]) -> Vec<u8>;
}

/// The public keys a reader trusts.
pub trait SnapshotKeys: Send + Sync {
    /// Whether `signature` is valid over `message` for `key_id`, trusted for `generation`.
    fn verify(&self, key_id: &str, generation: u64, message: &[u8], signature: &[u8]) -> bool;
}

/// A reversible transform applied to each part after compression (WP-JRN-08.T4 supplies
/// encryption here later). The default is [`IdentityTransform`].
pub trait PartTransform: Send + Sync {
    /// Applied to the compressed part before it is stored.
    ///
    /// # Errors
    /// [`SnapshotError::TransformFailed`] when the transform cannot run.
    fn forward(&self, bytes: Vec<u8>) -> Result<Vec<u8>, SnapshotError>;
    /// Undoes [`PartTransform::forward`] on the stored bytes.
    ///
    /// # Errors
    /// [`SnapshotError::TransformFailed`] when the bytes cannot be restored.
    fn reverse(&self, bytes: Vec<u8>) -> Result<Vec<u8>, SnapshotError>;
}

/// The identity transform: the stored part is the compressed part.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityTransform;

impl PartTransform for IdentityTransform {
    fn forward(&self, bytes: Vec<u8>) -> Result<Vec<u8>, SnapshotError> {
        Ok(bytes)
    }
    fn reverse(&self, bytes: Vec<u8>) -> Result<Vec<u8>, SnapshotError> {
        Ok(bytes)
    }
}
