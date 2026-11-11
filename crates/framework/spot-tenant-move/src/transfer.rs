//! The chunked transfer seam and its checksums and size accounting.
//!
//! A [`TransferPort`] streams a per-tenant logical dump: one [`Chunk`] at a time, each
//! carrying a SHA-256 of its own bytes and the relative file and offset it belongs to.
//! The engine reads the stream twice — once to export and account, once to import — so
//! the port must be able to resume at `after` (a stable, deterministic dump), and
//! [`TransferPort::import_chunk`] must be idempotent because a crash between the import
//! and the checkpoint re-sends one chunk.
//!
//! The production adapter is [`crate::snapshot_transfer::SnapshotTransfer`], which takes
//! a `spot-snapshot` logical dump and restores it through the same provider.

use sha2::{Digest, Sha256};

use crate::BoxFuture;
use crate::error::Result;
use crate::plan::MovePlan;

/// One checksummed piece of the logical dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Zero-based position in the dump stream.
    pub sequence: u64,
    /// The safe relative path of the file inside the dump.
    pub file: String,
    /// The byte offset of this chunk inside the file.
    pub offset: u64,
    /// The chunk's bytes.
    pub bytes: Vec<u8>,
    /// Lowercase hex SHA-256 of `bytes`, as the producer computed it.
    pub sha256: String,
    /// Whether this is the last chunk of the dump.
    pub last: bool,
}

impl Chunk {
    /// Builds a chunk and computes its checksum.
    #[must_use]
    pub fn new(
        sequence: u64,
        file: impl Into<String>,
        offset: u64,
        bytes: Vec<u8>,
        last: bool,
    ) -> Self {
        let sha256 = sha256_hex(&bytes);
        Self {
            sequence,
            file: file.into(),
            offset,
            bytes,
            sha256,
            last,
        }
    }

    /// Whether the bytes still match the recorded checksum.
    #[must_use]
    pub fn verify(&self) -> bool {
        self.sha256 == sha256_hex(&self.bytes)
    }

    /// The chunk's size, in bytes.
    #[must_use]
    pub fn len(&self) -> u64 {
        u64::try_from(self.bytes.len()).unwrap_or(u64::MAX)
    }

    /// Whether the chunk carries no byte.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

/// Lowercase hex SHA-256 of a byte slice.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Integer size accounting for one transfer pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TransferTotals {
    /// Chunks consumed.
    pub chunks: u64,
    /// Bytes consumed.
    pub bytes: u64,
}

impl TransferTotals {
    /// Accounts one chunk of `bytes`.
    pub(crate) fn add(&mut self, bytes: u64) {
        self.chunks = self.chunks.saturating_add(1);
        self.bytes = self.bytes.saturating_add(bytes);
    }
}

/// The per-tenant logical dump/restore provider.
///
/// `export_chunk(plan, after)` returns the chunk immediately after `after` (the first
/// chunk when `after` is `None`), or `None` at the end of the stream. The same call
/// serves both the export and the import pass; the engine never asks the port to
/// remember a cursor, so a restarted move resumes from its persisted sequence.
pub trait TransferPort: Send + Sync {
    /// The chunk after `after`, or `None` when the dump is exhausted.
    ///
    /// # Errors
    /// [`Error::TransferFailed`] with a fixed reason.
    fn export_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        after: Option<u64>,
    ) -> BoxFuture<'a, Result<Option<Chunk>>>;

    /// Imports one chunk into the target. Re-importing the same sequence changes nothing.
    ///
    /// # Errors
    /// [`Error::TransferFailed`] with a fixed reason.
    fn import_chunk<'a>(
        &'a self,
        plan: &'a MovePlan,
        chunk: &'a Chunk,
    ) -> BoxFuture<'a, Result<()>>;

    /// Finishes the import: closes the imported dump and asks the provider to restore it.
    /// Called once after the last chunk; re-running it is harmless.
    ///
    /// # Errors
    /// [`Error::TransferFailed`] with a fixed reason.
    fn finish_import<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, Result<()>>;

    /// Discards the transfer staging. Called after cut-over and on abort; idempotent.
    ///
    /// # Errors
    /// [`Error::TransferFailed`] with a fixed reason.
    fn cleanup<'a>(&'a self, plan: &'a MovePlan) -> BoxFuture<'a, Result<()>>;
}
