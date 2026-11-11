//! [`LockedKey`]: key material in locked, zeroised memory.
//!
//! The material is wiped on drop, cannot be cloned (an explicit [`LockedKey::duplicate`] is the
//! only copy path), and implements neither `Debug` nor `Display` nor `serde::Serialize`, so it
//! cannot reach a log, an error or a backup by accident.
//!
//! `mlock` and `PR_SET_DUMPABLE` are platform calls; the crate forbids `unsafe` (the project
//! coding rules), so they sit behind the [`MemoryLock`] seam. The default, [`NoLock`], is a
//! no-op, which is what tests and hosts without the privilege use; a host that may call the
//! platform supplies its own implementation.

use crate::crypto;
use crate::error::ProtectedKeyError;
use zeroize::Zeroizing;

/// The length of a key in bytes (256-bit).
pub const KEY_LEN: usize = 32;

/// The seam for pinning key memory and excluding it from core dumps.
pub trait MemoryLock: Send + Sync {
    /// Pins `region` so it cannot be swapped out (best effort). Returns whether it succeeded.
    fn lock(&self, region: &[u8]) -> bool;

    /// Releases a region pinned by [`MemoryLock::lock`].
    fn unlock(&self, region: &[u8]);

    /// Whether this process has core dumps disabled for key material.
    fn core_dumps_excluded(&self) -> bool {
        false
    }
}

/// The default lock: no platform call is made.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoLock;

impl MemoryLock for NoLock {
    fn lock(&self, _region: &[u8]) -> bool {
        false
    }

    fn unlock(&self, _region: &[u8]) {}
}

/// Key material in memory. Zeroised on drop.
///
/// It is deliberately **not** `Clone`, not `Debug`, not `Display` and not `Serialize`, so it
/// cannot be copied by accident or reach a log, an error or a backup. The only copy path is
/// the explicit [`LockedKey::duplicate`].
///
/// ```compile_fail
/// # use spot_protected_keys::LockedKey;
/// let key = LockedKey::from_bytes(&[0u8; 32]).unwrap();
/// let copy = key.clone(); // no `Clone`
/// ```
///
/// ```compile_fail
/// # use spot_protected_keys::LockedKey;
/// let key = LockedKey::from_bytes(&[0u8; 32]).unwrap();
/// println!("{key:?}"); // no `Debug`
/// ```
pub struct LockedKey(Zeroizing<[u8; KEY_LEN]>);

impl LockedKey {
    /// Copies exactly `KEY_LEN` bytes into locked memory. Any other length is a
    /// [`ProtectedKeyError::Malformed`].
    ///
    /// This is how a host loads a key from a KMS, an HSM or a key file; the bytes are copied
    /// immediately and zeroised on drop.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, ProtectedKeyError> {
        let key: [u8; KEY_LEN] = bytes
            .try_into()
            .map_err(|_| ProtectedKeyError::Malformed("key is not 32 bytes"))?;
        Ok(Self(Zeroizing::new(key)))
    }

    /// A fresh random key from the operating system CSPRNG.
    pub(crate) fn random() -> Result<Self, ProtectedKeyError> {
        crypto::random_key()
    }

    /// An explicit, auditable copy of the material. Both copies are zeroised on drop.
    ///
    /// This is the only way to copy a key; there is no `Clone`.
    #[must_use]
    pub fn duplicate(&self) -> Self {
        let mut out = Zeroizing::new([0u8; KEY_LEN]);
        out.copy_from_slice(self.expose());
        Self(out)
    }

    /// Asks `lock` to pin this key's memory. Returns whether the platform pinned it.
    pub fn pin(&self, lock: &dyn MemoryLock) -> bool {
        lock.lock(self.expose())
    }

    /// The key length, always [`KEY_LEN`].
    #[must_use]
    pub fn len(&self) -> usize {
        KEY_LEN
    }

    /// Never true; a key always has [`KEY_LEN`] bytes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// The raw bytes, for the AEAD layer and the root provider only.
    pub(crate) fn expose(&self) -> &[u8; KEY_LEN] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn rejects_a_wrong_length() {
        assert!(LockedKey::from_bytes(&[0u8; 31]).is_err());
        assert!(LockedKey::from_bytes(&[0u8; 33]).is_err());
        assert!(LockedKey::from_bytes(&[0u8; 32]).is_ok());
    }

    #[test]
    fn duplicate_copies_the_material() {
        let key = LockedKey::random().expect("key");
        let copy = key.duplicate();
        assert_eq!(key.expose(), copy.expose());
    }

    #[test]
    fn no_lock_is_a_no_op() {
        let key = LockedKey::random().expect("key");
        assert!(!key.pin(&NoLock));
        assert!(!NoLock.core_dumps_excluded());
    }
}
