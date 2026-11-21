//! The file store seam of the `/api/v1/files` routes (ADR-055 "Files", WP-PLT-10.T5).
//!
//! `spot-http` does not depend on the files service. It defines [`FilesPort`], the two
//! operations in the shapes the router needs, and the host (WP-PRD-01.T8) writes a thin adapter
//! from `spot-files-service`'s `FilesApi` onto it. Bodies cross the port as [`ByteStream`]s,
//! never as whole buffers: an upload is pulled chunk by chunk by the port, a download is pulled
//! chunk by chunk by the router. [`MemoryFilesPort`] is the in-memory fake the tests (and a
//! host's smoke tests) use.
//!
//! A port never returns a path or a storage key: [`FileRecord`] has no field for one.

use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};

use axum::body::Bytes;
use serde_json::Value as Json;
use sha2::{Digest as _, Sha256};
use spot_storage_contract::RecordId;

use super::registry::Code;
use super::service::{ApiError, BoxFuture, RequestCtx};
use super::storage_error::RETRY_AFTER_DETAIL;

/// A stream of body bytes, polled like `futures::Stream` (the crate keeps its dependency list
/// short, so the trait is local). An `Err` ends the stream: the port must abandon the upload
/// and report the failure, storing nothing.
pub trait ByteStream: Send {
    /// The next chunk, `None` at the end of the body.
    fn poll_chunk(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, ApiError>>>;
}

/// A boxed [`ByteStream`].
pub type BoxByteStream = Pin<Box<dyn ByteStream>>;

/// The next chunk of `stream`, or `None` at its end.
pub async fn next_chunk(stream: &mut BoxByteStream) -> Option<Result<Bytes, ApiError>> {
    std::future::poll_fn(|cx| stream.as_mut().poll_chunk(cx)).await
}

/// A stream over chunks already in memory (tests and small bodies).
pub struct ChunkStream {
    chunks: std::vec::IntoIter<Bytes>,
}

impl ChunkStream {
    /// A stream of `chunks`, in order.
    pub fn new(chunks: Vec<Bytes>) -> Self {
        ChunkStream {
            chunks: chunks.into_iter(),
        }
    }

    /// A stream of one chunk.
    pub fn once(bytes: impl Into<Bytes>) -> Self {
        ChunkStream::new(vec![bytes.into()])
    }
}

impl ByteStream for ChunkStream {
    fn poll_chunk(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, ApiError>>> {
        Poll::Ready(self.chunks.next().map(Ok))
    }
}

/// What an upload carries besides its bytes. The name is the client's; the port sanitises it
/// for display and never derives a storage key from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileUpload {
    /// The display name from `Content-Disposition`.
    pub name: String,
    /// The declared content type, `None` when the upload named none (the port then stores
    /// `application/octet-stream`).
    pub content_type: Option<String>,
}

/// The stored file's record (the contract's `FileRecord`). It names no path or storage key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRecord {
    /// The file's id.
    pub id: RecordId,
    /// The display name, at most 255 characters.
    pub name: String,
    /// The size in bytes.
    pub size: u64,
    /// The stored content type.
    pub content_type: String,
    /// SHA-256 of the bytes, hex, lower-case.
    pub digest: String,
    /// True when the file is unattached and only its uploader may read it.
    pub private: bool,
}

/// The conditional and range headers of a download, raw as they arrived.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileGetRequest {
    /// The `Range` header.
    pub range: Option<String>,
    /// The `If-None-Match` header.
    pub if_none_match: Option<String>,
}

/// What a download answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileGetStatus {
    /// `200`: the whole file.
    Full,
    /// `206`: the requested range.
    Partial,
    /// `304`: the validator matched.
    NotModified,
    /// `416`: the range cannot be satisfied.
    RangeNotSatisfiable,
}

/// A download: the status, the response headers and, for `200` and `206`, the body.
pub struct FileDownload {
    /// The status to answer with.
    pub status: FileGetStatus,
    /// Response headers. The router passes only `ETag`, `Content-Type`, `Content-Disposition`,
    /// `Content-Range`, `Content-Length` and `Accept-Ranges` through.
    pub headers: Vec<(String, String)>,
    /// The bytes, for `Full` and `Partial`.
    pub body: Option<BoxByteStream>,
}

/// A failure at the port, in terms of HTTP and the registry: the router maps it to the
/// contract's error envelope without importing the service's error type. A `code` the registry
/// does not know is reported as `http.internal`, with the code under `details.app_code`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FilePortError {
    /// The HTTP status the service intends (the registry's status wins for a known code).
    pub status: u16,
    /// The stable dotted code, for example `file.scan_failed`.
    pub code: String,
    /// Plain-language message; never a path, a key or file content.
    pub message: String,
    /// Whether the same request may succeed later.
    pub retryable: bool,
    /// The wait in whole seconds a store reports (a throttle, a pool wait), if any. The router
    /// sends it as `Retry-After` only when the final status is 429 or 503.
    pub retry_after_secs: Option<u64>,
}

impl FilePortError {
    /// An error with the given parts.
    pub fn new(
        status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        FilePortError {
            status,
            code: code.into(),
            message: message.into(),
            retryable,
            retry_after_secs: None,
        }
    }

    /// Advertises a wait of `secs` seconds (stored as at least 1: a zero invites a hot loop).
    #[must_use]
    pub fn with_retry_after(mut self, secs: u64) -> Self {
        self.retry_after_secs = Some(secs.max(1));
        self
    }

    /// The unknown-or-forbidden answer of `file_get`.
    pub fn not_found() -> Self {
        FilePortError::new(
            404,
            Code::DocumentNotFound.as_str(),
            Code::DocumentNotFound.default_message(),
            false,
        )
    }

    /// The router's error for this port error.
    ///
    /// The wait travels as the `retry_after` detail, and only when the final status is 429 or
    /// 503: a code the registry does not know becomes a 500, and any other status has no use
    /// for a `Retry-After`.
    pub(crate) fn into_api_error(self) -> ApiError {
        let wait = self.retry_after_secs;
        let error = ApiError::app(self.code, self.message);
        let waits = matches!(error.status().as_u16(), 429 | 503);
        match wait {
            Some(secs) if waits => error.detail(RETRY_AFTER_DETAIL, Json::from(secs.max(1))),
            _ => error,
        }
    }
}

impl From<ApiError> for FilePortError {
    fn from(error: ApiError) -> Self {
        FilePortError {
            status: error.status().as_u16(),
            code: error.code.as_str().to_owned(),
            message: error.message.clone(),
            retryable: error.retryable(),
            retry_after_secs: super::storage_error::retry_after_secs(&error),
        }
    }
}

/// The file store behind `file_upload` and `file_get`. Object-safe with boxed futures like the
/// other services; every method decides permission itself from the request context.
pub trait FilesPort: Send + Sync + 'static {
    /// The largest upload in bytes (the registry's `files.max_upload_bytes`). The router
    /// stops reading and answers `413 file.too_large` past it.
    fn max_upload_bytes(&self) -> u64;

    /// Stores one file. The port pulls `body` to its end; if the stream yields an error it
    /// stores nothing and returns that failure. The id and digest are the port's.
    fn upload<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        upload: FileUpload,
        body: BoxByteStream,
    ) -> BoxFuture<'a, Result<FileRecord, FilePortError>>;

    /// Reads one file. Unknown and forbidden files both fail as [`FilePortError::not_found`].
    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: RecordId,
        request: FileGetRequest,
    ) -> BoxFuture<'a, Result<FileDownload, FilePortError>>;
}

struct StoredFile {
    record: FileRecord,
    owner: Option<String>,
    shared: bool,
    bytes: Vec<u8>,
}

#[derive(Default)]
struct MemoryInner {
    files: BTreeMap<RecordId, StoredFile>,
    fail_next_upload: Option<FilePortError>,
    fail_next_download: Option<FilePortError>,
}

/// An in-memory [`FilesPort`]: files belong to the bearer token that uploaded them, until
/// [`MemoryFilesPort::share`] makes one readable by every caller (as if attached to a document
/// everyone may read). Chunks are collected in memory; this is a test double.
pub struct MemoryFilesPort {
    max_upload_bytes: u64,
    inner: Mutex<MemoryInner>,
}

impl MemoryFilesPort {
    /// A store whose uploads may be at most `max_upload_bytes`.
    pub fn new(max_upload_bytes: u64) -> Self {
        MemoryFilesPort {
            max_upload_bytes,
            inner: Mutex::new(MemoryInner::default()),
        }
    }

    /// Makes `id` readable by every caller.
    pub fn share(&self, id: RecordId) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(file) = inner.files.get_mut(&id)
        {
            file.shared = true;
            file.record.private = false;
        }
    }

    /// The next upload fails with `error` after its stream was consumed; nothing is stored.
    pub fn fail_next_upload(&self, error: FilePortError) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.fail_next_upload = Some(error);
        }
    }

    /// The next download fails with `error`.
    pub fn fail_next_download(&self, error: FilePortError) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.fail_next_download = Some(error);
        }
    }

    /// How many files are stored.
    pub fn file_count(&self) -> usize {
        self.inner.lock().map_or(0, |inner| inner.files.len())
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}

fn internal(message: &str) -> FilePortError {
    FilePortError::new(500, Code::FileStoreFailed.as_str(), message, true)
}

/// Only these types are served inline; everything else is an attachment (the service's rule,
/// in miniature).
fn disposition(content_type: &str, name: &str) -> String {
    let safe_inline = matches!(
        content_type,
        "image/png" | "image/jpeg" | "application/pdf" | "text/plain"
    );
    let escaped: String = name
        .chars()
        .filter(|c| !c.is_control())
        .flat_map(|c| match c {
            '"' | '\\' => vec!['\\', c],
            _ => vec![c],
        })
        .collect();
    format!(
        "{}; filename=\"{escaped}\"",
        if safe_inline { "inline" } else { "attachment" }
    )
}

/// `bytes=a-b`, `bytes=a-`, `bytes=-n` against `size`: `Ok(Some((first, last)))`, `Ok(None)`
/// for a header to ignore, `Err(())` when unsatisfiable.
fn parse_range(raw: &str, size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return Ok(None);
    };
    if spec.contains(',') {
        return Ok(None);
    }
    let Some((from, to)) = spec.split_once('-') else {
        return Ok(None);
    };
    let (from, to) = (from.trim(), to.trim());
    if from.is_empty() {
        let Ok(n) = to.parse::<u64>() else {
            return Ok(None);
        };
        if n == 0 || size == 0 {
            return Err(());
        }
        return Ok(Some((size.saturating_sub(n), size - 1)));
    }
    let Ok(first) = from.parse::<u64>() else {
        return Ok(None);
    };
    let last = if to.is_empty() {
        size.saturating_sub(1)
    } else {
        match to.parse::<u64>() {
            Ok(last) => last.min(size.saturating_sub(1)),
            Err(_) => return Ok(None),
        }
    };
    if first >= size || first > last {
        return Err(());
    }
    Ok(Some((first, last)))
}

impl FilesPort for MemoryFilesPort {
    fn max_upload_bytes(&self) -> u64 {
        self.max_upload_bytes
    }

    fn upload<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        upload: FileUpload,
        mut body: BoxByteStream,
    ) -> BoxFuture<'a, Result<FileRecord, FilePortError>> {
        Box::pin(async move {
            let Some(credential) = &ctx.credential else {
                return Err(FilePortError::from(ApiError::from_code(Code::AuthRequired)));
            };
            let mut bytes = Vec::new();
            let mut hasher = Sha256::new();
            while let Some(chunk) = next_chunk(&mut body).await {
                let chunk = chunk?;
                hasher.update(&chunk);
                bytes.extend_from_slice(&chunk);
            }
            let mut inner = self.inner.lock().map_err(|_| internal("store poisoned"))?;
            if let Some(error) = inner.fail_next_upload.take() {
                return Err(error);
            }
            let id = RecordId::from_bytes(*uuid::Uuid::now_v7().as_bytes())
                .map_err(|_| internal("id generation failed"))?;
            let record = FileRecord {
                id,
                name: upload.name.chars().take(255).collect(),
                size: bytes.len() as u64,
                content_type: upload
                    .content_type
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                digest: hex_lower(&hasher.finalize()),
                private: true,
            };
            inner.files.insert(
                id,
                StoredFile {
                    record: record.clone(),
                    owner: Some(credential.token.clone()),
                    shared: false,
                    bytes,
                },
            );
            Ok(record)
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: RecordId,
        request: FileGetRequest,
    ) -> BoxFuture<'a, Result<FileDownload, FilePortError>> {
        Box::pin(async move {
            let token = ctx.credential.as_ref().map(|c| c.token.as_str());
            let mut inner = self.inner.lock().map_err(|_| internal("store poisoned"))?;
            if let Some(error) = inner.fail_next_download.take() {
                return Err(error);
            }
            let file = inner
                .files
                .get(&id)
                .filter(|f| f.shared || (token.is_some() && f.owner.as_deref() == token))
                .ok_or_else(FilePortError::not_found)?;
            let size = file.bytes.len() as u64;
            let etag = format!("\"{}\"", file.record.digest);
            let mut headers = vec![
                ("ETag".to_owned(), etag.clone()),
                ("Accept-Ranges".to_owned(), "bytes".to_owned()),
            ];
            let matched = request.if_none_match.as_deref().is_some_and(|raw| {
                raw.split(',')
                    .any(|c| c.trim().trim_start_matches("W/") == etag || c.trim() == "*")
            });
            if matched {
                return Ok(FileDownload {
                    status: FileGetStatus::NotModified,
                    headers,
                    body: None,
                });
            }
            headers.push(("Content-Type".to_owned(), file.record.content_type.clone()));
            headers.push((
                "Content-Disposition".to_owned(),
                disposition(&file.record.content_type, &file.record.name),
            ));
            let range = match request.range.as_deref().map(|raw| parse_range(raw, size)) {
                None | Some(Ok(None)) => None,
                Some(Ok(Some(range))) => Some(range),
                Some(Err(())) => {
                    headers.push(("Content-Range".to_owned(), format!("bytes */{size}")));
                    return Ok(FileDownload {
                        status: FileGetStatus::RangeNotSatisfiable,
                        headers,
                        body: None,
                    });
                }
            };
            let (status, slice) = match range {
                Some((first, last)) => {
                    headers.push((
                        "Content-Range".to_owned(),
                        format!("bytes {first}-{last}/{size}"),
                    ));
                    // The range was checked against `size`, so the slice is in bounds.
                    let slice = file.bytes[first as usize..=last as usize].to_vec();
                    (FileGetStatus::Partial, slice)
                }
                None => (FileGetStatus::Full, file.bytes.clone()),
            };
            // Two chunks, so the router's streaming path is exercised.
            let mid = slice.len() / 2;
            let chunks = vec![
                Bytes::copy_from_slice(&slice[..mid]),
                Bytes::copy_from_slice(&slice[mid..]),
            ];
            Ok(FileDownload {
                status,
                headers,
                body: Some(Box::pin(ChunkStream::new(chunks))),
            })
        })
    }
}
