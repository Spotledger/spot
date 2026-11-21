//! Stable error codes for the coordinator: the `coordinator.*` family of the native API error
//! registry. Callers match on [`Error::code`], never on the message.

use std::fmt;

/// A held lease refused `acquire`: another holder's lease is still live.
pub const LEASE_HELD: &str = "coordinator.lease_held";

/// A stale lease handle (expired, released or superseded) was refused by `renew`, `release` or
/// `check_fence`; the current holder is untouched.
pub const LEASE_LOST: &str = "coordinator.lease_lost";

/// `lock` waited its whole `wait` without getting the lock.
pub const LOCK_TIMEOUT: &str = "coordinator.lock_timeout";

/// A second node was detected beside the in-process coordinator, which is correct for one node
/// only: the coordinator refuses to start, or refuses every call after the node appeared.
pub const SECOND_NODE: &str = "coordinator.second_node";

/// A lease, lock, counter or rate-limit name is empty, longer than 200 bytes or not printable.
pub const INVALID_NAME: &str = "coordinator.invalid_name";

/// An argument is outside its range: a zero `by`, ttl or cost, an over-long ttl, an empty
/// rate-limit window, or a cost above the limit's capacity.
pub const INVALID_ARGUMENT: &str = "coordinator.invalid_argument";

/// A counter would pass `u64::MAX`.
pub const COUNTER_OVERFLOW: &str = "coordinator.counter_overflow";

/// What went wrong in one coordinator operation.
///
/// `code` is stable and callers match on it, not on the message. Every code belongs to the
/// `coordinator.*` family and is catalogued in `spot-errors`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// The stable machine-readable code (`coordinator.lease_lost`, ...).
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

    /// `acquire` found the lease held.
    pub fn lease_held(name: &str, holder: &str, held_by: &str, token: u64) -> Self {
        Self::new(
            LEASE_HELD,
            format!("lease {name}: {holder} cannot acquire, {held_by} holds token {token}"),
        )
    }

    /// A stale handle was refused.
    pub fn lease_lost(name: &str, holder: &str, token: u64, why: &str) -> Self {
        Self::new(
            LEASE_LOST,
            format!("lease {name}: holder {holder} lost token {token} ({why})"),
        )
    }

    /// An invalid argument, with what is wrong with it.
    pub fn invalid_argument(why: impl Into<String>) -> Self {
        Self::new(INVALID_ARGUMENT, why)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for Error {}

/// Result of every fallible coordinator operation.
pub type Result<T> = std::result::Result<T, Error>;
