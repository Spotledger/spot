//! Where the verified bundle bytes of an installed app are read from at start-up.
//!
//! The registry (`spot-installer-pg`) holds an app's digest, not its bytes. The runtime reads the
//! bytes by digest from a [`BundleStore`] and re-verifies them, so a file changed on disk after the
//! install is caught here, not trusted because it was installed once.
//!
//! [`DirBundleStore`] is content addressed: `<dir>/<digest hex>/app.bundle`, with the detached
//! signature `app.sig` and the publisher certificate `app.cert` beside it (the two files
//! `spot-pack pack --sign` writes). A missing signature or certificate reads as empty; whether that
//! is acceptable is the site's `[trust] require_signed`, decided in [`super::verify`].

use std::path::{Path, PathBuf};

/// The sidecar files are small; anything larger is refused unread.
const SIDECAR_MAX_BYTES: u64 = 1024 * 1024;
/// A bundle larger than this is refused unread (the bundle reader's own limit applies later).
const BUNDLE_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// The stored files of one bundle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StoredBundle {
    /// The bundle's bytes.
    pub bytes: Vec<u8>,
    /// The detached signature file, empty when there is none.
    pub signature: Vec<u8>,
    /// The publisher certificate file, empty when there is none.
    pub certificate: Vec<u8>,
}

/// Why a bundle could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// No bundle is stored under this digest.
    Missing,
    /// The bytes cannot be read (permissions, size, not a regular file).
    Unreadable,
}

/// A source of bundle bytes by digest.
pub trait BundleStore: Send + Sync {
    /// The files stored for the bundle whose digest is `digest_hex`.
    fn read(&self, digest_hex: &str) -> Result<StoredBundle, StoreError>;
}

/// The content-addressed directory store.
#[derive(Clone, Debug)]
pub struct DirBundleStore {
    dir: PathBuf,
}

impl DirBundleStore {
    /// A store over `dir`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

fn plain_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn read_limited(path: &Path, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(StoreError::Unreadable),
        Ok(meta) if !meta.is_file() || meta.len() > max => Err(StoreError::Unreadable),
        Ok(_) => std::fs::read(path)
            .map(Some)
            .map_err(|_| StoreError::Unreadable),
    }
}

impl BundleStore for DirBundleStore {
    fn read(&self, digest_hex: &str) -> Result<StoredBundle, StoreError> {
        // The digest names a directory: only 64 lowercase hex digits are ever joined to a path.
        if !plain_hex(digest_hex) {
            return Err(StoreError::Missing);
        }
        let dir = self.dir.join(digest_hex);
        let bytes =
            read_limited(&dir.join("app.bundle"), BUNDLE_MAX_BYTES)?.ok_or(StoreError::Missing)?;
        Ok(StoredBundle {
            bytes,
            signature: read_limited(&dir.join("app.sig"), SIDECAR_MAX_BYTES)?.unwrap_or_default(),
            certificate: read_limited(&dir.join("app.cert"), SIDECAR_MAX_BYTES)?
                .unwrap_or_default(),
        })
    }
}

/// A store that holds bundles in memory (tests, and a host that embeds its bundles).
#[derive(Default)]
pub struct MemoryBundleStore {
    items: std::sync::Mutex<std::collections::BTreeMap<String, StoredBundle>>,
}

impl MemoryBundleStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores `bundle` under `digest_hex`.
    pub fn put(&self, digest_hex: &str, bundle: StoredBundle) {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(digest_hex.to_owned(), bundle);
    }

    /// Removes a stored bundle.
    pub fn remove(&self, digest_hex: &str) {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(digest_hex);
    }
}

impl BundleStore for MemoryBundleStore {
    fn read(&self, digest_hex: &str) -> Result<StoredBundle, StoreError> {
        self.items
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(digest_hex)
            .cloned()
            .ok_or(StoreError::Missing)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_digest_that_is_not_hex_never_reaches_the_file_system() {
        let store = DirBundleStore::new("/nonexistent");
        assert_eq!(store.read("../../etc/passwd"), Err(StoreError::Missing));
        assert_eq!(store.read(&"A".repeat(64)), Err(StoreError::Missing));
    }

    #[test]
    fn a_stored_bundle_round_trips_with_empty_sidecars() {
        let store = MemoryBundleStore::new();
        let digest = "a".repeat(64);
        store.put(
            &digest,
            StoredBundle {
                bytes: vec![1, 2, 3],
                ..StoredBundle::default()
            },
        );
        let found = store.read(&digest).expect("stored");
        assert_eq!(found.bytes, vec![1, 2, 3]);
        assert!(found.signature.is_empty());
        store.remove(&digest);
        assert_eq!(store.read(&digest), Err(StoreError::Missing));
    }
}
