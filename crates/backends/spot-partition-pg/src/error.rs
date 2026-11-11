//! Errors: the two `partition_pg.*` codes this crate adds, and the mapping of bridge refusals and
//! database failures onto the trait's own [`PartitionError`].
//!
//! No failure carries SQL text, a connection string, a driver message or a row value: a bridge
//! refusal or a database error becomes [`PartitionError::Store`] with a fixed sentence (the
//! `partition.store` code), and the construction errors name a fixed sentence only.

use std::fmt;

use spot_partition::PartitionError;
use spot_pg_bridge::BridgeError;

/// Stable code: the partition tables could not be created.
pub const PREPARE_FAILED: &str = "partition_pg.prepare_failed";
/// Stable code: the store cannot be built in this context.
pub const UNSUPPORTED_CONTEXT: &str = "partition_pg.unsupported_context";

/// Why a store could not be built or prepared.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PgPartitionError {
    /// `partition_pg.prepare_failed`: the DDL did not apply or no connection was available.
    PrepareFailed,
    /// `partition_pg.unsupported_context`: the thread is inside an async runtime, or the bridge
    /// is configured for more calls in flight than the pool can serve.
    UnsupportedContext {
        /// A fixed sentence saying what is wrong.
        reason: &'static str,
    },
}

impl PgPartitionError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PrepareFailed => PREPARE_FAILED,
            Self::UnsupportedContext { .. } => UNSUPPORTED_CONTEXT,
        }
    }
}

impl fmt::Display for PgPartitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrepareFailed => write!(
                f,
                "{PREPARE_FAILED}: the partition registry and range tables could not be created"
            ),
            Self::UnsupportedContext { reason } => write!(f, "{UNSUPPORTED_CONTEXT}: {reason}"),
        }
    }
}

impl std::error::Error for PgPartitionError {}

/// What went wrong inside one bridged operation. Never shown: it is mapped to a fixed reason or,
/// for a domain refusal, to the trait's own error.
#[derive(Debug)]
pub(crate) enum OpError {
    /// The driver failed (its message is dropped: it can quote a key or a value).
    Db,
    /// A stored row could not be decoded.
    Decode,
    /// A domain refusal; the transaction is rolled back and the error is returned as is.
    Refused(PartitionError),
}

impl From<tokio_postgres::Error> for OpError {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

fn store(reason: &str) -> PartitionError {
    PartitionError::Store {
        reason: reason.to_owned(),
    }
}

/// Maps a bridged operation's failure to the trait's error. The reason names the bridge code or
/// the kind of failure, never a statement or a value.
pub(crate) fn map(error: BridgeError<OpError>) -> PartitionError {
    match error {
        BridgeError::WrongThread => {
            store("pg_bridge.wrong_thread: the partition store was called inside an async runtime")
        }
        BridgeError::Busy => store("pg_bridge.busy: the partition store queue is full"),
        BridgeError::Timeout => store(
            "pg_bridge.timeout: the partition store call missed its deadline; treat it as not done",
        ),
        BridgeError::Closed => store("pg_bridge.closed: the partition store bridge is shut down"),
        BridgeError::Connect => {
            store("pg_bridge.connect: no database connection for the partition store")
        }
        BridgeError::Config { .. } => {
            store("pg_bridge.config: the partition store bridge is misconfigured")
        }
        BridgeError::Store(OpError::Db) => store("the partition store query failed"),
        BridgeError::Store(OpError::Decode) => store("a partition store row could not be decoded"),
        BridgeError::Store(OpError::Refused(error)) => error,
        // The bridge error is non-exhaustive: an unknown refusal is still a visible failure.
        _ => store("the partition store bridge refused the call"),
    }
}
