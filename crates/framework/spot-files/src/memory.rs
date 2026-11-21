//! [`MemoryFileStore`]: bytes in RAM for tests and development (WP-PLT-10.T1).
//!
//! Same contract as [`LocalFileStore`](crate::local::LocalFileStore): a `put` streams once
//! into a buffer while hashing, then swaps the complete record in under the store lock —
//! readers see the previous version or the new one, never a partial file, and a failed
//! (e.g. [`FileError::TooLarge`]) upload leaves the old state untouched. The state is an
//! `Arc`, so clones share one store while separate instances stay independent.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use spot_core::SiteId;
use tokio::io::AsyncWrite;

use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix};
use crate::store::{
    Body, FileFuture, FileMeta, FileResult, FileStore, PutOptions, StoredFile, StoredHash,
    stream_hashed,
};

/// One stored object's full state in RAM.
#[derive(Debug)]
struct Record {
    /// The bytes.
    bytes: Vec<u8>,
    /// SHA-256 over `bytes`, computed by the put that created this version.
    sha256: [u8; 32],
    /// The recorded content type.
    content_type: String,
    /// When this version was written (UTC).
    modified: time::OffsetDateTime,
}

/// Full state: site → (key → record).
type Files = BTreeMap<SiteId, BTreeMap<FileKey, Record>>;

/// The in-memory [`FileStore`]: a `BTreeMap` per site under one lock. `BTreeMap` keeps
/// `list` in key byte order for free.
#[derive(Clone, Debug, Default)]
pub struct MemoryFileStore {
    /// A site that exists always maps to `Some(files)` records — `entry().or_default()`
    /// keeps that shape.
    files: Arc<Mutex<Files>>,
}

impl MemoryFileStore {
    /// Creates an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Locks the file table, recovering from a poisoned lock: the table is still a valid
    /// map even when a panicking writer released it mid-update.
    fn lock(&self) -> MutexGuard<'_, Files> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The metadata of one record, as `head` and `list` report it.
    fn meta_of(key: &FileKey, record: &Record) -> FileMeta {
        FileMeta {
            key: key.clone(),
            size: record.bytes.len() as u64,
            sha256: record.sha256,
            content_type: record.content_type.clone(),
            modified: record.modified,
        }
    }
}

impl FileStore for MemoryFileStore {
    fn put<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
        mut body: Body,
        opts: PutOptions,
    ) -> FileFuture<'a, FileResult<StoredFile>> {
        Box::pin(async move {
            // Stream and hash before touching the table: nothing of this put is visible
            // until the complete record exists, so any failure leaves the old state alone.
            let mut sink = VecSink::default();
            let (StoredHash(sha256), size) =
                stream_hashed(&mut sink, &mut body, opts.max_bytes).await?;
            let modified = time::OffsetDateTime::now_utc();
            let mut files = self.lock();
            let site_files = files.entry(site.clone()).or_default();
            if !opts.overwrite && site_files.contains_key(&key) {
                return Err(FileError::AlreadyExists {
                    key: key.as_str().to_owned(),
                });
            }
            site_files.insert(
                key.clone(),
                Record {
                    bytes: sink.bytes,
                    sha256,
                    content_type: opts.content_type.clone(),
                    modified,
                },
            );
            tracing::debug!(site = %site, key = %key, size, "stored file");
            Ok(StoredFile {
                key,
                size,
                sha256,
                content_type: opts.content_type,
            })
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<(FileMeta, Body)>> {
        Box::pin(async move {
            let files = self.lock();
            let record = match files.get(site).and_then(|site_files| site_files.get(&key)) {
                Some(record) => record,
                None => {
                    return Err(FileError::NotFound {
                        key: key.as_str().to_owned(),
                    });
                }
            };
            let meta = Self::meta_of(&key, record);
            // Cloning while the lock is held is fine here: the memory provider is for
            // tests and development, and the clone IS the streaming copy.
            let body: Body = Box::pin(std::io::Cursor::new(record.bytes.clone()));
            Ok((meta, body))
        })
    }

    fn head<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<Option<FileMeta>>> {
        Box::pin(async move {
            let files = self.lock();
            Ok(files
                .get(site)
                .and_then(|site_files| site_files.get(&key))
                .map(|record| Self::meta_of(&key, record)))
        })
    }

    fn delete<'a>(&'a self, site: &'a SiteId, key: FileKey) -> FileFuture<'a, FileResult<bool>> {
        Box::pin(async move {
            let mut files = self.lock();
            let deleted = files
                .get_mut(site)
                .is_some_and(|site_files| site_files.remove(&key).is_some());
            if deleted {
                tracing::debug!(site = %site, key = %key, "deleted file");
            }
            Ok(deleted)
        })
    }

    fn list<'a>(
        &'a self,
        site: &'a SiteId,
        prefix: &'a FileKeyPrefix,
        after: Option<&'a FileKey>,
        limit: usize,
    ) -> FileFuture<'a, FileResult<Vec<FileMeta>>> {
        Box::pin(async move {
            let files = self.lock();
            let mut out = Vec::new();
            for (key, record) in files
                .get(site)
                .into_iter()
                .flat_map(|site_files| site_files.iter())
            {
                if after.is_some_and(|cursor| key <= cursor) {
                    continue;
                }
                if !key.starts_with(prefix) {
                    continue;
                }
                if out.len() == limit {
                    break;
                }
                out.push(Self::meta_of(key, record));
            }
            Ok(out)
        })
    }
}

/// An [`AsyncWrite`] sink that collects bytes, for [`stream_hashed`]'s hashing path.
#[derive(Default)]
struct VecSink {
    /// Bytes accepted so far, in order.
    bytes: Vec<u8>,
}

impl AsyncWrite for VecSink {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.get_mut().bytes.extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
