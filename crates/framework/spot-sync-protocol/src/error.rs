//! Stable error codes for the sync protocol: the `sync_protocol.*` family of the native API error
//! registry. Callers match on [`Error::code`], never on the message.
//!
//! Every code here is library-internal for now: the transport binding (WP-EDG-02.T2) maps them
//! onto the app-facing surface. They are catalogued in
//! `crates/framework/spot-errors/catalogue/sync_protocol.toml`.

use std::fmt;

/// An argument passed to a constructor, a config or a state-machine call is missing, out of
/// range or self-contradictory.
pub const INVALID_ARGUMENT: &str = "sync_protocol.invalid_argument";

/// A batch's sequence range is malformed: `from_seq > to_seq`, records that are not the
/// contiguous range the header claims, or a record count that does not match the range.
pub const INVALID_SEQUENCE: &str = "sync_protocol.invalid_sequence";

/// A batch failed its SHA-256 checksum (or was otherwise unreadable); the receiver refuses it and
/// the sender re-requests it.
pub const CORRUPT_BATCH: &str = "sync_protocol.corrupt_batch";

/// A batch arrived that does not start at the receiver's next expected sequence: something is
/// missing, so the receiver refuses it and the sender re-sends from the checkpoint.
pub const GAP: &str = "sync_protocol.gap";

/// No protocol version is common to both peers' declared ranges.
pub const VERSION_MISMATCH: &str = "sync_protocol.version_mismatch";

/// The peers' metadata epochs differ, so the edge's software is out of the skew the hub allows.
pub const METADATA_EPOCH: &str = "sync_protocol.metadata_epoch";

/// A batch is larger than the flow-control limit the receiver or session allows.
pub const BATCH_TOO_LARGE: &str = "sync_protocol.batch_too_large";

/// A session's deadline passed before its step completed.
pub const DEADLINE_EXCEEDED: &str = "sync_protocol.deadline_exceeded";

/// Writing a checkpoint to a [`CheckpointStore`](crate::store::CheckpointStore) failed.
pub const STORE_FAILED: &str = "sync_protocol.store_failed";

/// Applying a record to an [`ApplySink`](crate::store::ApplySink) failed.
pub const APPLY_FAILED: &str = "sync_protocol.apply_failed";

/// The `sync.*` vocabulary this crate defines, so the error-code scan sees every code exactly
/// where it is used. Each code's literal also appears at the constructor that raises it.
pub const CODES: [&str; 10] = [
    INVALID_ARGUMENT,
    INVALID_SEQUENCE,
    CORRUPT_BATCH,
    GAP,
    VERSION_MISMATCH,
    METADATA_EPOCH,
    BATCH_TOO_LARGE,
    DEADLINE_EXCEEDED,
    STORE_FAILED,
    APPLY_FAILED,
];

/// What went wrong in one protocol operation.
///
/// `code` is stable and callers match on it, not on the message. Every code belongs to the
/// `sync.*` family and is catalogued in `spot-errors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable machine-readable code (`sync_protocol.gap`, ...).
    pub code: &'static str,
    /// What went wrong, with the sequence numbers and names involved. Never a secret.
    pub message: String,
}

impl Error {
    /// An error with the given code and message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An argument is missing or out of range.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(INVALID_ARGUMENT, message)
    }

    /// A batch header and its records do not describe one contiguous range.
    pub fn invalid_sequence(message: impl Into<String>) -> Self {
        Self::new(INVALID_SEQUENCE, message)
    }

    /// A batch's checksum does not match its contents.
    pub fn corrupt_batch(message: impl Into<String>) -> Self {
        Self::new(CORRUPT_BATCH, message)
    }

    /// The two peers have no protocol version in common.
    pub fn version_mismatch(ours: impl fmt::Display, theirs: impl fmt::Display) -> Self {
        Self::new(
            VERSION_MISMATCH,
            format!("no common protocol version: ours {ours}, theirs {theirs}"),
        )
    }

    /// The two peers' metadata epochs differ beyond the allowed skew.
    pub fn metadata_epoch(ours: u64, theirs: u64) -> Self {
        Self::new(
            METADATA_EPOCH,
            format!("metadata epoch differs: ours {ours}, theirs {theirs}"),
        )
    }

    /// A batch exceeded the flow-control limit.
    pub fn batch_too_large(limit: u64, got: u64) -> Self {
        Self::new(
            BATCH_TOO_LARGE,
            format!("batch spans {got} sequences, more than the limit of {limit}"),
        )
    }

    /// A session step would run past its deadline.
    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Self::new(DEADLINE_EXCEEDED, message)
    }

    /// The checkpoint store refused a write.
    pub fn store_failed(message: impl Into<String>) -> Self {
        Self::new(STORE_FAILED, message)
    }

    /// The apply sink refused a record.
    pub fn apply_failed(message: impl Into<String>) -> Self {
        Self::new(APPLY_FAILED, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// Result of every fallible sync-protocol operation.
pub type Result<T> = std::result::Result<T, Error>;
