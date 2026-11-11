//! [`FolderStore`]: a journal object store in one directory, for the standalone profile and for
//! tests that need a real filesystem.
//!
//! # How a write lands
//!
//! A write never appears half-done. The bytes go to a file under `<root>/.staging/`, are flushed
//! and synced, and only then does the object become visible at its key:
//!
//! * [`FolderStore::put_new`] links the staged file into place with `hard_link`, which fails with
//!   `AlreadyExists` instead of overwriting. That gives create-new semantics *and* an atomic
//!   appearance, so of two processes or threads racing for one key exactly one wins. On a
//!   filesystem without hard links (a FAT mount, some network mounts) the store falls back to a
//!   existence check followed by a rename, which is atomic in appearance but not in the check.
//! * [`FolderStore::put_replace`] renames the staged file over the manifest after comparing the
//!   current etag, and `rename` within one filesystem is atomic.
//!
//! The compare and the rename are two steps: two writers replacing one manifest at the same
//! instant can both pass the compare on a local directory, so this backend gives the replace its
//! *guard* but not a compare-and-swap. One writer per tenant's manifest is the journal's model
//! (ADR-061 s10: one relay), and a provider with conditional writes (S3, Azure) gets the swap
//! from the provider. The contract suite checks the guard, which is what the caller relies on.
//!
//! Listing walks the directory tree on every call: right for a standalone tenant's journal, and
//! wrong for a directory holding millions of objects — an object store's LIST is the answer
//! there.
//!
//! A process that dies between the staged write and the link leaves a file in `.staging/` and
//! nothing at the key: `get` returns `None` and `list` never shows it, because `.staging/` is not
//! part of the key space. `tests::crash` proves exactly that.
//!
//! The store owns `<root>/.staging/`; nothing outside this crate may write there.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::StoreError;
use crate::layout::Key;
use crate::store::{
    JournalStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, etag, guard_delete,
    guard_put_replace, length, page,
};

/// The directory under the root the store owns for staged writes. It is not part of the key
/// space, so a staged file is never a visible object.
pub const STAGING_DIR: &str = ".staging";

/// A store over one directory: one file per object, the key as the relative path.
#[derive(Debug)]
pub struct FolderStore {
    root: PathBuf,
    staged: AtomicU64,
}

impl FolderStore {
    /// Opens (and creates) a store rooted at `root`.
    ///
    /// # Errors
    /// [`StoreError::Io`] when the directory cannot be created.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        std::fs::create_dir_all(root.join(STAGING_DIR)).map_err(|error| StoreError::Io {
            operation: "create",
            key: root.display().to_string(),
            message: error.to_string(),
        })?;
        Ok(Self {
            root,
            staged: AtomicU64::new(0),
        })
    }

    /// The directory the objects live in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The filesystem path of a key. Only a canonical key reaches this: every entry point parses
    /// the key first, so no component here can be `..` or absolute.
    fn path_of(&self, key: &str) -> PathBuf {
        self.root.join(key)
    }

    /// Writes `bytes` under `.staging/` and returns the staged path, synced and closed.
    ///
    /// The name is a per-store counter and the process id: no clock and no randomness, both of
    /// which this crate is not allowed to read.
    fn stage(&self, key: &str, bytes: &[u8]) -> Result<PathBuf, StoreError> {
        let serial = self.staged.fetch_add(1, Ordering::Relaxed);
        let staged = self
            .root
            .join(STAGING_DIR)
            .join(format!("{}-{serial}.part", std::process::id()));
        let outcome = (|| -> std::io::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        match outcome {
            Ok(()) => Ok(staged),
            Err(error) => {
                // A half-written staged file is never a visible object; drop it anyway.
                drop(std::fs::remove_file(&staged));
                Err(io_error("write", key, &error))
            }
        }
    }

    /// Makes the parent directory of `path` exist.
    fn prepare(&self, key: &str, path: &Path) -> Result<(), StoreError> {
        let Some(parent) = path.parent() else {
            return Ok(());
        };
        std::fs::create_dir_all(parent).map_err(|error| io_error("create", key, &error))
    }

    /// The bytes of the object at `path`, or `None` when there is no such file.
    fn read_at(&self, key: &str, path: &Path) -> Result<Option<Vec<u8>>, StoreError> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("read", key, &error)),
        }
    }

    /// Collects every visible object under `dir` into `out`.
    ///
    /// `prefix` is the key prefix of `dir`. Directories are walked, files become entries, and
    /// `.staging/` at the root is skipped because it holds no object.
    fn collect(
        &self,
        dir: &Path,
        prefix: &str,
        out: &mut Vec<ObjectEntry>,
    ) -> Result<(), StoreError> {
        let listing = std::fs::read_dir(dir).map_err(|error| io_error("list", prefix, &error))?;
        for entry in listing {
            let entry = entry.map_err(|error| io_error("list", prefix, &error))?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| StoreError::Io {
                    operation: "list",
                    key: prefix.to_owned(),
                    message: "a file name in the store is not valid UTF-8".to_owned(),
                })?;
            let key = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = entry
                .file_type()
                .map_err(|error| io_error("list", &key, &error))?;
            if file_type.is_dir() {
                if prefix.is_empty() && key == STAGING_DIR {
                    continue;
                }
                self.collect(&entry.path(), &key, out)?;
            } else if file_type.is_file() {
                let size = entry
                    .metadata()
                    .map_err(|error| io_error("list", &key, &error))?
                    .len();
                out.push(ObjectEntry { key, size });
            }
        }
        Ok(())
    }
}

/// One filesystem failure as a store error; the message is the operating system's, the key is the
/// journal key (never the absolute path, which is host configuration).
fn io_error(operation: &'static str, key: &str, error: &std::io::Error) -> StoreError {
    StoreError::Io {
        operation,
        key: key.to_owned(),
        message: error.to_string(),
    }
}

impl JournalStore for FolderStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            let key = key.to_owned();
            Key::parse(&key)?;
            let digest = etag(&bytes);
            let size = length(&bytes);
            let path = self.path_of(&key);
            self.prepare(&key, &path)?;
            let staged = self.stage(&key, &bytes)?;
            match std::fs::hard_link(&staged, &path) {
                Ok(()) => {
                    drop(std::fs::remove_file(&staged));
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    drop(std::fs::remove_file(&staged));
                    return Err(StoreError::AlreadyExists { key });
                }
                Err(_) => {
                    // No hard links on this filesystem: check, then rename.
                    if path.exists() {
                        drop(std::fs::remove_file(&staged));
                        return Err(StoreError::AlreadyExists { key });
                    }
                    if let Err(error) = std::fs::rename(&staged, &path) {
                        drop(std::fs::remove_file(&staged));
                        return Err(io_error("place", &key, &error));
                    }
                }
            }
            Ok(ObjectMeta {
                key,
                etag: digest,
                size,
            })
        })
    }

    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            let key = key.to_owned();
            let expected = expected_etag.to_owned();
            guard_put_replace(&key)?;
            let digest = etag(&bytes);
            let size = length(&bytes);
            let path = self.path_of(&key);
            let current = self
                .read_at(&key, &path)?
                .ok_or_else(|| StoreError::NotFound { key: key.clone() })?;
            if etag(&current) != expected {
                return Err(StoreError::EtagMismatch { key, expected });
            }
            self.prepare(&key, &path)?;
            let staged = self.stage(&key, &bytes)?;
            if let Err(error) = std::fs::rename(&staged, &path) {
                drop(std::fs::remove_file(&staged));
                return Err(io_error("replace", &key, &error));
            }
            Ok(ObjectMeta {
                key,
                etag: digest,
                size,
            })
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        Box::pin(async move {
            let key = key.to_owned();
            Key::parse(&key)?;
            let path = self.path_of(&key);
            let Some(bytes) = self.read_at(&key, &path)? else {
                return Ok(None);
            };
            Ok(Some(StoredObject {
                meta: ObjectMeta {
                    key,
                    etag: etag(&bytes),
                    size: length(&bytes),
                },
                bytes,
            }))
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        Box::pin(async move {
            let key = key.to_owned();
            Key::parse(&key)?;
            let path = self.path_of(&key);
            let Some(bytes) = self.read_at(&key, &path)? else {
                return Ok(None);
            };
            Ok(Some(ObjectMeta {
                key,
                etag: etag(&bytes),
                size: length(&bytes),
            }))
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        Box::pin(async move {
            let mut entries = Vec::new();
            self.collect(&self.root, "", &mut entries)?;
            Ok(page(entries, prefix, after, limit))
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let key = key.to_owned();
            guard_delete(&key)?;
            let path = self.path_of(&key);
            match std::fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(StoreError::NotFound { key })
                }
                Err(error) => Err(io_error("delete", &key, &error)),
            }
        })
    }
}
