//! The object store port: immutable puts, gets, listing and deletes.
//!
//! The trait is what the customer's machine writes to and what the service implements
//! over S3-compatible storage later (the real store is an adapter behind a feature; the
//! filesystem store and the in-memory store are enough for acceptance). Objects are
//! **immutable**: [`ObjectStore::put`] never replaces an existing key, and reports
//! [`PutOutcome::Exists`] instead, so a client retry must compare the stored bytes and
//! can never lose a chunk someone else already uploaded.
//!
//! Nothing in this crate stores plaintext. The stores here move opaque byte strings;
//! [`RecordingStore`] records every byte that crosses the port so a test can prove that
//! only ciphertext does.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Mutex;

use crate::error::{CloudBackupError, Result};
use crate::manifest::is_object_segment;

/// A boxed `Send` future, so [`ObjectStore`] stays object-safe.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// What a put did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object was written.
    Stored,
    /// The key already existed; nothing was written.
    Exists,
}

/// One object's key and size, as a listing reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectInfo {
    /// The object key.
    pub key: String,
    /// The object's size in bytes.
    pub bytes: u64,
}

/// Where encrypted chunks and manifests live.
///
/// Implementations must be safe to share between threads and must never overwrite an
/// existing key.
pub trait ObjectStore: Send + Sync {
    /// Writes `bytes` under `key`, or reports that the key already exists.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] for an unusable key or an underlying failure.
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome>;

    /// Reads the object at `key`.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] when the object is absent or unreadable.
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>>;

    /// Lists the objects whose keys start with `prefix`, sorted by key.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] for an underlying failure.
    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>>;

    /// Deletes the object at `key`; deleting an absent key is a success.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] for an underlying failure.
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()>;

    /// Whether an object exists at `key`.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] for an underlying failure other than absence.
    fn exists<'a>(&'a self, key: &'a str) -> StoreFuture<'a, bool> {
        Box::pin(async move { Ok(self.get(key).await.is_ok()) })
    }
}

/// The read half of a store, for verification: what the customer's machine re-reads to
/// check an upload. Every [`ObjectStore`] is an `ObjectReader`; the session's service
/// destination has its own implementation so verification reads back through the
/// service, not around it.
pub trait ObjectReader: Send + Sync {
    /// Reads the object at `key`.
    ///
    /// # Errors
    /// [`CloudBackupError::Store`] when the object is absent or unreadable.
    fn read<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>>;
}

impl<T: ObjectStore + ?Sized> ObjectReader for T {
    fn read<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        self.get(key)
    }
}

/// The in-memory store: the acceptance store and the backing of [`RecordingStore`].
#[derive(Debug, Default)]
pub struct MemoryObjectStore {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl MemoryObjectStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.objects
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every key, sorted.
    pub fn keys(&self) -> Vec<String> {
        self.guard().keys().cloned().collect()
    }

    /// Total bytes held.
    pub fn total_bytes(&self) -> u64 {
        self.guard()
            .values()
            .map(|bytes| u64::try_from(bytes.len()).unwrap_or(u64::MAX))
            .sum()
    }
}

impl ObjectStore for MemoryObjectStore {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        Box::pin(async move {
            if !valid_key(key) {
                return Err(CloudBackupError::Store {
                    reason: format!("the object key {key:?} is not usable"),
                });
            }
            let mut objects = self.guard();
            if objects.contains_key(key) {
                return Ok(PutOutcome::Exists);
            }
            objects.insert(key.to_owned(), bytes);
            Ok(PutOutcome::Stored)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.guard()
                .get(key)
                .cloned()
                .ok_or_else(|| CloudBackupError::Store {
                    reason: format!("the store holds no object at {key:?}"),
                })
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            Ok(self
                .guard()
                .iter()
                .filter(|(key, _)| key.starts_with(prefix))
                .map(|(key, bytes)| ObjectInfo {
                    key: key.clone(),
                    bytes: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                })
                .collect())
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.guard().remove(key);
            Ok(())
        })
    }
}

/// One operation [`RecordingStore`] saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordedOp {
    /// A put, with the exact bytes handed to the port.
    Put {
        /// The object key.
        key: String,
        /// The bytes the caller wrote.
        bytes: Vec<u8>,
    },
    /// A get.
    Get {
        /// The object key.
        key: String,
    },
    /// A listing.
    List {
        /// The key prefix.
        prefix: String,
    },
    /// A delete.
    Delete {
        /// The object key.
        key: String,
    },
}

/// A store that remembers every operation, for the "only ciphertext crosses the port"
/// proof and for checking that a resume downloads fewer chunks.
pub struct RecordingStore {
    inner: MemoryObjectStore,
    operations: Mutex<Vec<RecordedOp>>,
}

impl Default for RecordingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl RecordingStore {
    /// An empty recording store.
    pub fn new() -> Self {
        Self {
            inner: MemoryObjectStore::new(),
            operations: Mutex::new(Vec::new()),
        }
    }

    fn guard(&self) -> std::sync::MutexGuard<'_, Vec<RecordedOp>> {
        self.operations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Every operation, in order.
    pub fn operations(&self) -> Vec<RecordedOp> {
        self.guard().clone()
    }

    /// Every put, in order.
    pub fn puts(&self) -> Vec<(String, Vec<u8>)> {
        self.guard()
            .iter()
            .filter_map(|operation| match operation {
                RecordedOp::Put { key, bytes } => Some((key.clone(), bytes.clone())),
                _ => None,
            })
            .collect()
    }

    /// The keys of every get, in order.
    pub fn gets(&self) -> Vec<String> {
        self.guard()
            .iter()
            .filter_map(|operation| match operation {
                RecordedOp::Get { key } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    /// The total number of bytes written through the port.
    pub fn written_bytes(&self) -> u64 {
        self.guard()
            .iter()
            .map(|operation| match operation {
                RecordedOp::Put { bytes, .. } => u64::try_from(bytes.len()).unwrap_or(u64::MAX),
                _ => 0,
            })
            .sum()
    }

    /// Forgets the recorded operations, keeping the objects.
    pub fn clear(&self) {
        self.guard().clear();
    }

    /// The inner store, for assertions about stored keys.
    pub fn inner(&self) -> &MemoryObjectStore {
        &self.inner
    }
}

impl std::fmt::Debug for RecordingStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RecordingStore")
            .field("operations", &self.operations().len())
            .finish_non_exhaustive()
    }
}

impl ObjectStore for RecordingStore {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        Box::pin(async move {
            let outcome = self.inner.put(key, bytes.clone()).await?;
            self.guard().push(RecordedOp::Put {
                key: key.to_owned(),
                bytes,
            });
            Ok(outcome)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            self.guard().push(RecordedOp::Get {
                key: key.to_owned(),
            });
            self.inner.get(key).await
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            self.guard().push(RecordedOp::List {
                prefix: prefix.to_owned(),
            });
            self.inner.list(prefix).await
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.guard().push(RecordedOp::Delete {
                key: key.to_owned(),
            });
            self.inner.delete(key).await
        })
    }
}

/// A store rooted at one directory: the desktop-side service that has no S3 account yet
/// (and the adapter seam for the S3-compatible store).
///
/// Keys map to paths segment by segment; a key with an empty, `.` or `..` segment, a
/// backslash or a NUL is refused, so a tenant id can never escape the root.
#[derive(Clone, Debug)]
pub struct FileSystemStore {
    root: PathBuf,
}

impl FileSystemStore {
    /// A store rooted at `root`; the directory is created on the first put.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path_for(&self, key: &str) -> Result<PathBuf> {
        if !valid_key(key) {
            return Err(CloudBackupError::Store {
                reason: format!("the object key {key:?} is not usable"),
            });
        }
        let mut path = self.root.clone();
        for segment in key.split('/') {
            if !is_object_segment(segment) {
                return Err(CloudBackupError::Store {
                    reason: format!("the object key {key:?} has an unusable segment"),
                });
            }
            path.push(segment);
        }
        Ok(path)
    }
}

impl ObjectStore for FileSystemStore {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        Box::pin(async move {
            let path = self.path_for(key)?;
            if let Some(parent) = path.parent() {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(CloudBackupError::store)?;
            }
            // `create_new` is the immutability guarantee: an existing key is never
            // truncated, not even by a concurrent writer.
            match tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .await
            {
                Ok(mut file) => {
                    use tokio::io::AsyncWriteExt;
                    file.write_all(&bytes)
                        .await
                        .map_err(CloudBackupError::store)?;
                    file.flush().await.map_err(CloudBackupError::store)?;
                    file.sync_all().await.map_err(CloudBackupError::store)?;
                    Ok(PutOutcome::Stored)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    Ok(PutOutcome::Exists)
                }
                Err(error) => Err(CloudBackupError::store(error)),
            }
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let path = self.path_for(key)?;
            tokio::fs::read(&path)
                .await
                .map_err(|error| CloudBackupError::Store {
                    reason: if error.kind() == std::io::ErrorKind::NotFound {
                        format!("the store holds no object at {key:?}")
                    } else {
                        format!("reading {key:?} failed: {error}")
                    },
                })
        })
    }

    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        Box::pin(async move {
            let root = self.root.clone();
            let prefix = prefix.to_owned();
            tokio::task::spawn_blocking(move || list_tree(&root, &prefix))
                .await
                .map_err(CloudBackupError::store)?
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let path = self.path_for(key)?;
            match tokio::fs::remove_file(&path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(CloudBackupError::store(error)),
            }
        })
    }
}

/// Whether a key is relative, non-empty and has only usable segments.
fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 1024 && key.split('/').all(is_object_segment)
}

/// Walks `root`, returning the files whose relative key starts with `prefix`.
fn list_tree(root: &Path, prefix: &str) -> Result<Vec<ObjectInfo>> {
    let mut out = Vec::new();
    if !root.exists() {
        return Ok(out);
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(CloudBackupError::store(error)),
        };
        for entry in entries {
            let entry = entry.map_err(CloudBackupError::store)?;
            let path = entry.path();
            let file_type = entry.file_type().map_err(CloudBackupError::store)?;
            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                let Ok(relative) = path.strip_prefix(root) else {
                    continue;
                };
                let key = relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join("/");
                if key.starts_with(prefix) {
                    let len = entry.metadata().map_err(CloudBackupError::store)?.len();
                    // Object sizes are u64 in the manifest; `usize` is u64 on every
                    // supported target, so this is a widening conversion in name only.
                    #[allow(clippy::useless_conversion)]
                    let bytes = u64::try_from(len).unwrap_or(u64::MAX);
                    out.push(ObjectInfo { key, bytes });
                }
            }
        }
    }
    out.sort_by(|left, right| left.key.cmp(&right.key));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn memory_store_is_immutable_and_lists_by_prefix() {
        let store = MemoryObjectStore::new();
        assert_eq!(
            store
                .put("acme/bk-1/a", b"one".to_vec())
                .await
                .expect("put"),
            PutOutcome::Stored
        );
        assert_eq!(
            store
                .put("acme/bk-1/a", b"two".to_vec())
                .await
                .expect("put"),
            PutOutcome::Exists
        );
        assert_eq!(store.get("acme/bk-1/a").await.expect("get"), b"one");
        assert_eq!(store.list("acme/bk-1/").await.expect("list").len(), 1);
        assert_eq!(store.list("other/").await.expect("list").len(), 0);
        store.delete("acme/bk-1/a").await.expect("delete");
        store
            .delete("acme/bk-1/a")
            .await
            .expect("delete is idempotent");
        assert!(!store.exists("acme/bk-1/a").await.expect("exists"));
    }

    #[tokio::test]
    async fn recording_store_remembers_every_operation() {
        let store = RecordingStore::new();
        store
            .put("acme/bk-1/a", b"secret".to_vec())
            .await
            .expect("put");
        store.get("acme/bk-1/a").await.expect("get");
        store.list("acme/").await.expect("list");
        assert_eq!(store.puts().len(), 1);
        assert_eq!(store.puts()[0].1, b"secret");
        assert_eq!(store.gets(), vec!["acme/bk-1/a".to_owned()]);
        assert_eq!(store.written_bytes(), 6);
        assert_eq!(store.operations().len(), 3);
        store.clear();
        assert!(store.operations().is_empty());
    }

    #[tokio::test]
    async fn filesystem_store_round_trips_and_is_immutable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSystemStore::new(dir.path().join("objects"));
        let key = "acme.example/bk-1/chunk-00000000.bin";
        assert_eq!(
            store.put(key, b"first".to_vec()).await.expect("put"),
            PutOutcome::Stored
        );
        assert_eq!(
            store.put(key, b"second".to_vec()).await.expect("put"),
            PutOutcome::Exists
        );
        assert_eq!(store.get(key).await.expect("get"), b"first");
        let listing = store.list("acme.example/").await.expect("list");
        assert_eq!(listing.len(), 1);
        assert_eq!(listing[0].key, key);
        assert_eq!(listing[0].bytes, 5);
        store.delete(key).await.expect("delete");
        assert!(store.get(key).await.is_err());
    }

    #[tokio::test]
    async fn filesystem_store_refuses_escape_keys() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = FileSystemStore::new(dir.path());
        assert!(store.put("../outside", Vec::new()).await.is_err());
        assert!(store.put("/absolute", Vec::new()).await.is_err());
        assert!(store.put("a/./b", Vec::new()).await.is_err());
    }
}
