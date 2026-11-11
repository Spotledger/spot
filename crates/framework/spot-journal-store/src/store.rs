//! The [`JournalStore`] trait: the journal's one seam to object storage.
//!
//! The methods are `async` only in the sense that they return a boxed future
//! ([`StoreFuture`]): this crate depends on no async runtime, so a caller on any executor — or a
//! synchronous test with a hand-rolled block-on — can drive them. Both backends here complete
//! their futures without waiting for an external wakeup.

use std::future::Future;
use std::pin::Pin;

use crate::error::StoreError;
use crate::layout::Key;

/// The boxed future of every store method (keeps the trait object-safe and runtime-free).
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, StoreError>> + Send + 'a>>;

/// What a store knows about an object without reading its bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectMeta {
    /// The canonical key.
    pub key: String,
    /// The content etag: the lower-case hex SHA-256 of the bytes, as [`etag`] computes it.
    pub etag: String,
    /// The length of the bytes.
    pub size: u64,
}

/// An object's metadata and bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredObject {
    /// Size and etag.
    pub meta: ObjectMeta,
    /// The object's bytes.
    pub bytes: Vec<u8>,
}

/// One entry of a listing: the key and the object's size.
///
/// A listing carries no etag on purpose: a provider serves etags with a HEAD, and a folder
/// backend would have to read every object's bytes to fill one in, which for a directory of
/// 64 MB segments is not a listing any more. Ask [`JournalStore::head`] for the etag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectEntry {
    /// The canonical key.
    pub key: String,
    /// The length of the object's bytes.
    pub size: u64,
}

/// The bytes' length as a `u64`, saturating rather than wrapping on a host whose `usize` is wider.
pub(crate) fn length(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// The etag of `bytes`: the lower-case hex SHA-256, unquoted, of exactly the bytes stored.
///
/// One rule for every backend, so a caller can compare an etag it computed with one a provider
/// returned. Content addressing gives the journal a second property for free: two identical
/// manifests of two generations have the same etag, which makes "unchanged generation" cheap.
#[must_use]
pub fn etag(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    hex
}

/// The journal's object store: five immutable object shapes, one mutable (the manifest).
///
/// # Rules every backend must hold (the suite in [`crate::contract`] checks them)
///
/// * **Keys are canonical.** Every key is parsed by [`crate::Layout::parse`] first, so a key of
///   any other shape, or a non-canonical spelling of a journal key, is refused with
///   [`StoreError::KeyInvalid`] rather than stored. One object, one name.
/// * **Writes are immutable.** [`JournalStore::put_new`] fails with
///   [`StoreError::AlreadyExists`] when the key is taken, and it holds that under concurrency:
///   of two writers racing for one key exactly one wins. Segments, tail chunks, blobs and
///   manifests are written once per generation.
/// * **The manifest is replaced, guarded.** [`JournalStore::put_replace`] takes the etag the
///   caller last saw; a different etag is [`StoreError::EtagMismatch`] and nothing is written.
///   Only a manifest may be replaced ([`crate::ObjectKind::is_replaceable`]); any other key is
///   [`StoreError::ReplaceRefused`], and replacing a key that does not exist is
///   [`StoreError::NotFound`].
/// * **Listing is ordered and resumable.** [`JournalStore::list`] returns at most `limit` objects
///   whose keys start with `prefix` and sort strictly after `after`, in ascending byte order. A
///   page shorter than `limit` is the end of the listing; asking again with the last key as
///   `after` continues it.
/// * **Deletion is narrow.** [`JournalStore::delete`] exists for tail chunks after compaction.
///   Anything under `segments/` or `snapshots/` is [`StoreError::DeleteRefused`]: the sealed
///   record is kept forever.
/// * **A key that is not there is `None`, not an error** for [`JournalStore::get`] and
///   [`JournalStore::head`]; a failed [`JournalStore::delete`] of a missing key is
///   [`StoreError::NotFound`].
pub trait JournalStore: Send + Sync {
    /// Stores `bytes` under `key`, refusing a key that is already taken.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] for a key outside the layout,
    /// [`StoreError::AlreadyExists`] when the key is taken, [`StoreError::Io`] when the backend
    /// fails.
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta>;

    /// Replaces the manifest at `key` if its current etag is `expected_etag`.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] outside the layout, [`StoreError::ReplaceRefused`] for a key
    /// that is not a manifest, [`StoreError::NotFound`] when the key does not exist,
    /// [`StoreError::EtagMismatch`] when the stored etag differs, [`StoreError::Io`] when the
    /// backend fails.
    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta>;

    /// The object at `key`, or `None` when there is none.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] outside the layout, [`StoreError::Io`] when the backend fails.
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>>;

    /// The metadata of the object at `key`, or `None` when there is none.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] outside the layout, [`StoreError::Io`] when the backend fails.
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>>;

    /// Up to `limit` objects under `prefix`, sorted, strictly after `after`.
    ///
    /// # Errors
    /// [`StoreError::Io`] when the backend fails. `prefix` and `after` are prefixes and keys, not
    /// necessarily canonical objects, so they are not parsed.
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>>;

    /// Removes the tail chunk at `key`.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] outside the layout, [`StoreError::DeleteRefused`] under
    /// `segments/` or `snapshots/`, [`StoreError::NotFound`] when there is nothing to remove,
    /// [`StoreError::Io`] when the backend fails.
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()>;
}

/// Refuses a key that is not a canonical journal key, or a replacement the rules do not allow.
///
/// Shared by both backends so the guards live in one place and a provider cannot forget one.
pub(crate) fn guard_put_new(key: &str) -> Result<Key, StoreError> {
    Key::parse(key)
}

/// The guard [`JournalStore::put_replace`] applies before touching the backend.
pub(crate) fn guard_put_replace(key: &str) -> Result<Key, StoreError> {
    let parsed = Key::parse(key)?;
    if !parsed.kind().is_replaceable() {
        return Err(StoreError::ReplaceRefused {
            key: key.to_owned(),
        });
    }
    Ok(parsed)
}

/// The guard [`JournalStore::delete`] applies before touching the backend.
pub(crate) fn guard_delete(key: &str) -> Result<Key, StoreError> {
    let parsed = Key::parse(key)?;
    if !parsed.kind().is_deletable() {
        return Err(StoreError::DeleteRefused {
            key: key.to_owned(),
        });
    }
    Ok(parsed)
}

/// Applies [`JournalStore::list`]'s filter-and-page rule to a listing of keys.
///
/// Both backends gather their keys, filter and sort once here, so paging order cannot drift
/// between them: ascending byte order, strictly after `after`, at most `limit` entries.
pub(crate) fn page(
    mut entries: Vec<ObjectEntry>,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> Vec<ObjectEntry> {
    entries.retain(|entry| entry.key.starts_with(prefix));
    if let Some(after) = after {
        entries.retain(|entry| entry.key.as_str() > after);
    }
    entries.sort_by(|left, right| left.key.cmp(&right.key));
    entries.truncate(limit);
    entries
}
