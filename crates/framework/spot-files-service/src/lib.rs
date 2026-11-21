//! `spot-files-service`: the file service over `spot-files` (WP-PLT-10.T3).
//!
//! `spot-files` stores bytes behind [`spot_files::store::FileStore`]; this crate is the
//! *service* the native API's `/api/v1/files` operations and the document engine's `attach`
//! fields run on. It uploads a stream to the store with a running digest and a hard,
//! streaming size limit, writes the `File` record in state `pending`, and downloads it back
//! only to a caller who may read the owning document (or the protected field the file hangs
//! off), with an `ETag`, a safe `Content-Disposition`, byte ranges and conditional requests.
//!
//! # Shape
//!
//! - [`error`]: the stable [`FileServiceError`] vocabulary (codes, statuses, retryability);
//! - [`record`]: [`FileRecord`], the [`FileState`] lifecycle, [`AttachTarget`] and the
//!   [`FileRecordStore`] seam plus its in-memory implementation;
//! - [`policy`]: [`FilePolicy`], content-type allow/deny, the always-attachment list, the
//!   magic-number sniff and display-name sanitising;
//! - [`range`]: single byte ranges and `If-None-Match`;
//! - [`access`]: [`Caller`], the [`AttachAccess`] permission seam, the [`QuotaCheck`] hook and
//!   the [`TableAttachAccess`] TOML fake;
//! - [`scan`]: the `file.scan` [`Scanner`] seam and its no-op default;
//! - [`service`]: [`FilesService`] and the [`FilesApi`] trait;
//! - [`hook`]: [`FileAttachHook`], the committed-phase attach lifecycle.
//!
//! # Boundaries
//!
//! The service never derives a key from the client's name (the key is `visibility/yyyy/mm/<id>`
//! built from a fresh UUIDv7), never buffers a whole upload, and never names a path, a key or a
//! provider in a response or an error. A file's existence is only revealed to a caller who may
//! read it: every refusal is `document.not_found`.

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(missing_docs)]

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;

pub mod access;
pub mod ctx;
pub mod error;
pub mod hook;
pub mod policy;
pub mod range;
pub mod record;
pub mod scan;
pub mod service;

pub use access::{
    AttachAccess, ByteLimitQuota, Caller, CallerSource, FixedCaller, QuotaCheck, TableAttachAccess,
    TokenCallers, UnlimitedQuota,
};
pub use ctx::FileCtx;
pub use error::FileServiceError;
pub use hook::FileAttachHook;
pub use policy::{Disposition, FilePolicy};
pub use range::{ByteRange, ContentRange, RangeOutcome};
pub use record::{
    AttachTarget, FileRecord, FileRecordStore, FileState, FileView, MemoryFileRecordStore,
};
pub use scan::{FakeScanner, NoopScanner, ScanVerdict, Scanner};
pub use service::{
    Download, DownloadRequest, DownloadStatus, FilesApi, FilesService, PurgeReport, UploadRequest,
    storage_key,
};

/// A boxed, pinned, `Send` future, the idiom the object-safe service traits use (ADR-057).
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Lower-case hex, for a SHA-256 digest in an `ETag` or a `FileView`.
pub fn hex_lower(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        // Writing into a `String` cannot fail.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn hex_is_lower_case_and_padded() {
        assert_eq!(super::hex_lower(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
        assert_eq!(super::hex_lower(&[]), "");
    }
}
