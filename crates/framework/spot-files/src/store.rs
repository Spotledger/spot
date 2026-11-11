//! The [`FileStore`] trait: object-safe byte storage scoped to a [`SiteId`].
//!
//! Every method takes `&SiteId` first (ADR-34: one tenant = one site = one namespace of
//! keys). The trait is object-safe so services hold `Arc<dyn FileStore>`; async methods
//! return pinned boxed `Send` futures for the same reason.

use std::future::Future;
use std::pin::Pin;

use sha2::{Digest, Sha256};
use spot_core::SiteId;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix};

/// A boxed, pinned, `Send` async-read body: the streaming shape every provider accepts and
/// returns, chosen so the trait stays object-safe.
pub type Body = Pin<Box<dyn AsyncRead + Send + 'static>>;

/// The future returned by trait methods: boxed and pinned so `Arc<dyn FileStore>` works,
/// `Send` so a caller may move it across tasks.
pub type FileFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Result of a file operation.
pub type FileResult<T> = Result<T, FileError>;

/// The size of the streaming window used by [`stream_hashed`].
const STREAM_CHUNK: usize = 64 * 1024;

/// Options for a `put`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PutOptions {
    /// MIME type recorded with the object, served back by `head` and `get`.
    pub content_type: String,
    /// Hard limit on the body size, enforced *while streaming*: the provider stops and
    /// stores nothing once the counter passes this value.
    pub max_bytes: u64,
    /// When `false`, an existing key fails with [`FileError::AlreadyExists`]; when `true`,
    /// the new content replaces the old one atomically.
    pub overwrite: bool,
}

impl PutOptions {
    /// Builds the options.
    pub fn new(content_type: impl Into<String>, max_bytes: u64, overwrite: bool) -> Self {
        Self {
            content_type: content_type.into(),
            max_bytes,
            overwrite,
        }
    }
}

/// What a successful `put` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredFile {
    /// The key the bytes were stored under.
    pub key: FileKey,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the stored bytes.
    pub sha256: [u8; 32],
    /// The recorded content type.
    pub content_type: String,
}

/// Metadata for an existing file: what `head` answers and `get` returns first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileMeta {
    /// The key (filled by every method so `list` pages can be read without the store).
    pub key: FileKey,
    /// Size in bytes.
    pub size: u64,
    /// SHA-256 of the stored bytes.
    pub sha256: [u8; 32],
    /// The recorded content type.
    pub content_type: String,
    /// Last-modified time (UTC).
    pub modified: time::OffsetDateTime,
}

/// Byte storage for attachments and assets.
///
/// Implementations: [`LocalFileStore`](crate::local::LocalFileStore) and
/// [`MemoryFileStore`](crate::memory::MemoryFileStore). Every implementation must pass the
/// [`contract_tests`](crate::contract_tests) suite. Keys are always validated [`FileKey`]s,
/// so a provider never sees a path it must defend against.
pub trait FileStore: Send + Sync + 'static {
    /// Streams `body` under `key`, hashing the content while writing it.
    ///
    /// - A body longer than `opts.max_bytes` fails with [`FileError::TooLarge`] and leaves
    ///   nothing behind.
    /// - With `opts.overwrite == false`, an existing key fails with
    ///   [`FileError::AlreadyExists`] and the old content is untouched.
    /// - The write is atomic: readers see the old content or the new, never a partial file.
    fn put<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
        body: Body,
        opts: PutOptions,
    ) -> FileFuture<'a, FileResult<StoredFile>>;

    /// Opens `key` for reading: its metadata plus a body positioned at the start.
    /// Fails with [`FileError::NotFound`] when the key is absent.
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<(FileMeta, Body)>>;

    /// Metadata for `key`, or `Ok(None)` when it does not exist.
    fn head<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<Option<FileMeta>>>;

    /// Removes `key`, returning whether anything was deleted.
    fn delete<'a>(&'a self, site: &'a SiteId, key: FileKey) -> FileFuture<'a, FileResult<bool>>;

    /// Lists keys under `prefix` in key byte order: at most `limit` entries, starting
    /// strictly after `after` when given. An empty prefix lists the whole site.
    fn list<'a>(
        &'a self,
        site: &'a SiteId,
        prefix: &'a FileKeyPrefix,
        after: Option<&'a FileKey>,
        limit: usize,
    ) -> FileFuture<'a, FileResult<Vec<FileMeta>>>;
}

/// Wraps an in-memory slice as a streaming [`Body`].
pub fn body_from_vec(data: Vec<u8>) -> Body {
    Box::pin(std::io::Cursor::new(data))
}

/// Wraps a string's bytes as a streaming [`Body`].
pub fn body_from_str(text: &str) -> Body {
    body_from_vec(text.as_bytes().to_vec())
}

/// Streams `body` into `sink`, hashing every byte read and enforcing `max_bytes`.
///
/// Used by both providers: they call it with their temp file / buffer as the sink, and on
/// [`FileError::TooLarge`] or a stream failure they discard the sink, which is what gives
/// "nothing is left behind".
pub(crate) async fn stream_hashed<W: AsyncWrite + Unpin>(
    sink: &mut W,
    body: &mut Body,
    max_bytes: u64,
) -> FileResult<(StoredHash, u64)> {
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut buf = vec![0u8; STREAM_CHUNK];
    loop {
        let n = body
            .read(&mut buf)
            .await
            .map_err(|e| FileError::io(format!("reading upload stream: {e}")))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > max_bytes {
            return Err(FileError::TooLarge { max_bytes });
        }
        hasher.update(&buf[..n]);
        sink.write_all(&buf[..n])
            .await
            .map_err(|e| FileError::io(format!("writing stored bytes: {e}")))?;
    }
    sink.flush().await.map_err(FileError::from)?;
    Ok((StoredHash(hasher.finalize().into()), total))
}

/// The SHA-256 digest computed by [`stream_hashed`], newtyped so providers can name it.
pub(crate) struct StoredHash(pub [u8; 32]);
