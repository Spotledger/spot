//! The subject-key trait, the key handle it yields, and its error.
//!
//! Keys are arguments: this crate creates none, stores none and backs up none. Where a key
//! lives (the key provider, its backup, its rotation) is a later task's business.

use std::fmt;

/// The length of a subject or journal key, in bytes.
pub const KEY_LEN: usize = 32;

/// A 32-byte AEAD key.
///
/// `Debug` prints a redaction and `Drop` overwrites the bytes, so a handle cannot leak a key
/// through a log line or a panic message. [`KeyHandle::expose`] is the one way out, for the key
/// store that already owns the key. A clone is a second copy of the key and each copy is wiped
/// when it drops, so clone one only when a call genuinely needs its own handle.
pub struct KeyHandle([u8; KEY_LEN]);

impl KeyHandle {
    /// A handle over `bytes`.
    #[must_use]
    pub fn new(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw key bytes.
    ///
    /// For the key store that owns the key, and nothing else: never log, serialise or return
    /// the slice.
    #[must_use]
    pub fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

impl Clone for KeyHandle {
    fn clone(&self) -> Self {
        Self(self.0)
    }
}

impl Drop for KeyHandle {
    fn drop(&mut self) {
        self.0.fill(0);
    }
}

impl fmt::Debug for KeyHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("KeyHandle(<redacted>)")
    }
}

/// Why a [`SubjectKeys`] implementation could not answer.
///
/// The text of [`KeyStoreError::Unavailable`] comes from the key store, so it must describe the
/// store's failure and never the key or the value it protects.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KeyStoreError {
    /// The key store could not be reached, or refused the call.
    #[error("the key store is unavailable: {0}")]
    Unavailable(String),
    /// The key store answered with a key of the wrong length.
    #[error("the key store returned a key of the wrong length")]
    KeyLength,
}

/// Where a data subject's key lives: the one thing this crate asks of the platform.
///
/// Synchronous by design: sealing one image is a loop over its fields, not a round trip per
/// field, so an adapter that wraps `spot-secrets`' async `KeyProvider` caches or unwraps keys
/// before it calls in. Destruction is what erasure means, so [`SubjectKeys::destroy`] takes
/// `&self` (an interior lock, not a mutable borrow) and [`SubjectKeys::key_for`] answers `None`
/// afterwards.
pub trait SubjectKeys {
    /// The key that protects `subject`'s erasable personal fields, or `None` once the key has
    /// been destroyed.
    fn key_for(&self, subject: &str) -> Result<Option<KeyHandle>, KeyStoreError>;

    /// Destroys `subject`'s key, which is what erasure is. Idempotent: destroying a key that is
    /// already gone succeeds.
    fn destroy(&self, subject: &str) -> Result<(), KeyStoreError>;

    /// The tenant journal key that protects fields declared `erasable = false` (a legal
    /// retention field), so that erasing a subject never touches them.
    fn retained_key(&self) -> Result<KeyHandle, KeyStoreError>;
}
