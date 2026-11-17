//! Errors: the `sync_node_pg.*` codes this crate adds, and the mapping of bridge refusals and
//! database failures onto fixed sentences.
//!
//! No failure carries SQL text, a connection string, a driver message, a record image or a row
//! value: a bridge refusal or a database error becomes a fixed sentence with the bridge's code.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use spot_pg_bridge::BridgeError;

/// Stable code: the node tables could not be created.
pub const PREPARE_FAILED: &str = "sync_node_pg.prepare_failed";
/// Stable code: the store cannot be built in this context.
pub const UNSUPPORTED_CONTEXT: &str = "sync_node_pg.unsupported_context";
/// Stable code: a node table is missing; the host must not receive.
pub const TABLES_MISSING: &str = "sync_node_pg.tables_missing";
/// Stable code: stored node state cannot be read back (damaged, disordered or over its bound).
pub const STATE_INVALID: &str = "sync_node_pg.state_invalid";
/// Stable code: a checkpoint ahead of the applied ledger was refused.
pub const CHECKPOINT_AHEAD: &str = "sync_node_pg.checkpoint_ahead";
/// Stable code: a bundle was refused; nothing of it was stored.
pub const APPLY_REFUSED: &str = "sync_node_pg.apply_refused";
/// Stable code: the store failed (database or bridge).
pub const STORE_FAILED: &str = "sync_node_pg.store_failed";

/// Why a store could not be built, prepared or called.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PgNodeError {
    /// `sync_node_pg.prepare_failed`.
    PrepareFailed,
    /// `sync_node_pg.unsupported_context`.
    UnsupportedContext {
        /// A fixed sentence saying what is wrong.
        reason: &'static str,
    },
    /// `sync_node_pg.tables_missing`.
    TablesMissing {
        /// The missing tables, by name.
        tables: Vec<&'static str>,
    },
    /// `sync_node_pg.state_invalid`.
    StateInvalid,
    /// `sync_node_pg.checkpoint_ahead`.
    CheckpointAhead,
    /// `sync_node_pg.apply_refused`: the bundle was refused and nothing was stored. The text
    /// carries a fixed sentence or a `replica_apply.*` reason code, never a value.
    ApplyRefused {
        /// The reason text.
        reason: String,
    },
    /// `sync_node_pg.store_failed`: the database or the bridge failed; the fixed text carries the
    /// bridge code when there is one.
    StoreFailed {
        /// A fixed sentence.
        reason: &'static str,
    },
}

impl PgNodeError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PrepareFailed => PREPARE_FAILED,
            Self::UnsupportedContext { .. } => UNSUPPORTED_CONTEXT,
            Self::TablesMissing { .. } => TABLES_MISSING,
            Self::StateInvalid => STATE_INVALID,
            Self::CheckpointAhead => CHECKPOINT_AHEAD,
            Self::ApplyRefused { .. } => APPLY_REFUSED,
            Self::StoreFailed { .. } => STORE_FAILED,
        }
    }
}

impl fmt::Display for PgNodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrepareFailed => {
                write!(f, "{PREPARE_FAILED}: the node tables could not be created")
            }
            Self::UnsupportedContext { reason } => write!(f, "{UNSUPPORTED_CONTEXT}: {reason}"),
            Self::TablesMissing { tables } => {
                write!(f, "{TABLES_MISSING}: missing {}", tables.join(", "))
            }
            Self::StateInvalid => {
                write!(f, "{STATE_INVALID}: the stored node state cannot be read")
            }
            Self::CheckpointAhead => write!(
                f,
                "{CHECKPOINT_AHEAD}: the checkpoint is ahead of what the ledger holds"
            ),
            Self::ApplyRefused { reason } => write!(f, "{APPLY_REFUSED}: {reason}"),
            Self::StoreFailed { reason } => write!(f, "{STORE_FAILED}: {reason}"),
        }
    }
}

impl std::error::Error for PgNodeError {}

/// What went wrong inside one bridged operation. Never shown: it is mapped to a fixed reason.
#[derive(Debug)]
pub(crate) enum OpError {
    /// The driver failed (its message is dropped: it can quote a key or a value).
    Db,
    /// A stored row could not be decoded.
    Decode,
    /// A domain refusal; the transaction is rolled back and the error returned as is.
    Refused(PgNodeError),
}

impl From<tokio_postgres::Error> for OpError {
    fn from(_error: tokio_postgres::Error) -> Self {
        Self::Db
    }
}

/// The fixed sentence of a bridge refusal.
fn bridge_reason(error: &BridgeError<OpError>) -> &'static str {
    match error {
        BridgeError::WrongThread => "pg_bridge.wrong_thread: called inside an async runtime",
        BridgeError::Busy => "pg_bridge.busy: the node store queue is full",
        BridgeError::Timeout => {
            "pg_bridge.timeout: the call missed its deadline; treat it as not done"
        }
        BridgeError::Closed => "pg_bridge.closed: the bridge is shut down",
        BridgeError::Connect => "pg_bridge.connect: no database connection",
        BridgeError::Config { .. } => "pg_bridge.config: the bridge is misconfigured",
        BridgeError::Store(OpError::Db) => "the node store query failed",
        BridgeError::Store(OpError::Decode) => "a node store row could not be decoded",
        BridgeError::Store(OpError::Refused(_)) => "the node store refused the call",
        _ => "the node store bridge refused the call",
    }
}

/// The store's own error for a bridged failure; a domain refusal comes back as itself.
pub(crate) fn map(error: BridgeError<OpError>) -> PgNodeError {
    match error {
        BridgeError::Store(OpError::Refused(inner)) => inner,
        BridgeError::Store(OpError::Decode) => PgNodeError::StateInvalid,
        other => PgNodeError::StoreFailed {
            reason: bridge_reason(&other),
        },
    }
}

/// Remembers the last failure of a trait method that cannot return one (`has`, `park`, `ready`,
/// `len`...), so the host can read it with `take` and show it. The method itself answers in the
/// safe direction (an unknown record is not applied, a parked bundle that was not stored is
/// redelivered by the hub).
#[derive(Clone, Debug, Default)]
pub struct FailureLog(Arc<Mutex<Option<PgNodeError>>>);

impl FailureLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn record(&self, error: PgNodeError) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(error);
    }

    /// The last failure since the previous call, if any.
    #[must_use]
    pub fn take(&self) -> Option<PgNodeError> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// Converts an unsigned value to the `bigint` the tables hold.
pub(crate) fn to_i64(value: u64) -> Result<i64, OpError> {
    i64::try_from(value).map_err(|_| OpError::Decode)
}

/// Converts a stored `bigint` back; a negative value is a damaged row.
pub(crate) fn to_u64(value: i64) -> Result<u64, OpError> {
    u64::try_from(value).map_err(|_| OpError::Decode)
}
