//! Stable error codes for the scheduler: the `scheduler.*` family of the native API error
//! registry. Callers match on [`Error::code`], never on the message.

use std::fmt;

/// The prefix every scheduler error code starts with.
pub const SCHEDULER_ERROR_PREFIX: &str = "scheduler.";

/// A lease stopped covering the caller: expired and taken by someone else, released, or
/// superseded by a newer acquisition. The operation it tried to fence was refused, so nothing
/// it guarded was written by it.
pub const LEASE_LOST: &str = "scheduler.lease_lost";

/// The write guard's stored token does not match the lease the caller holds: the write is from
/// an older generation and was rejected by [`crate::FencedWrite::check`].
pub const FENCE_MISMATCH: &str = "scheduler.fence_mismatch";

/// A cron expression cannot be run: wrong field count, or a field the scheduler cannot read.
pub const INVALID_CRON: &str = "scheduler.invalid_cron";

/// A time-zone name is not a known IANA zone, or the schedule's cron cannot run in it.
pub const INVALID_ZONE: &str = "scheduler.invalid_zone";

/// The high-water mark moved backwards: the store's mark is later than the state the caller
/// computed from, which means another writer advanced it in between.
pub const HIGH_WATER_REWOUND: &str = "scheduler.high_water_rewound";

/// What went wrong in one scheduler operation.
///
/// `code` is stable and callers match on it, not on the message. Every code belongs to the
/// `scheduler.*` family and is catalogued in `spot-errors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable machine-readable code (`scheduler.lease_lost`, ...).
    pub code: &'static str,
    /// What went wrong, with the names and tokens involved.
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

    /// A lost lease, with the name, the holder and the token that no longer covered it.
    pub fn lease_lost(name: &str, holder: &str, token: u64, why: &str) -> Self {
        Self::new(
            LEASE_LOST,
            format!("lease {name}: holder {holder} lost token {token} ({why})"),
        )
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// Result of every fallible scheduler operation.
pub type Result<T> = std::result::Result<T, Error>;
