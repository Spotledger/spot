//! Errors: the `sync_hub_pg.*` codes this crate adds, and the mapping of bridge refusals and
//! database failures onto the traits' own errors.
//!
//! No failure carries SQL text, a connection string, a driver message, a payload or a row value:
//! a bridge refusal or a database error becomes a fixed sentence with the bridge's code.

use std::fmt;

use spot_pg_bridge::BridgeError;
use spot_sync_hub::HubError;

/// Stable code: the hub tables could not be created.
pub const PREPARE_FAILED: &str = "sync_hub_pg.prepare_failed";
/// Stable code: the store cannot be built in this context.
pub const UNSUPPORTED_CONTEXT: &str = "sync_hub_pg.unsupported_context";
/// Stable code: a hub table is missing; the host must not serve.
pub const TABLES_MISSING: &str = "sync_hub_pg.tables_missing";
/// Stable code: a destination's queue is at its row or byte bound; nothing was dropped.
pub const QUEUE_FULL: &str = "sync_hub_pg.queue_full";
/// Stable code: a node's stored subscription cannot be read; the node receives nothing.
pub const SUBSCRIPTION_INVALID: &str = "sync_hub_pg.subscription_invalid";
/// Stable code: stored relay state cannot be read back (damaged or over the size bound).
pub const STATE_INVALID: &str = "sync_hub_pg.state_invalid";
/// Stable code: a node lifecycle change was refused (unknown node, revoked node, bad state).
pub const NODE_REFUSED: &str = "sync_hub_pg.node_refused";
/// Stable code: the store failed (database or bridge).
pub const STORE_FAILED: &str = "sync_hub_pg.store_failed";

/// Why a store could not be built, prepared or called.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PgHubError {
    /// `sync_hub_pg.prepare_failed`.
    PrepareFailed,
    /// `sync_hub_pg.unsupported_context`.
    UnsupportedContext {
        /// A fixed sentence saying what is wrong.
        reason: &'static str,
    },
    /// `sync_hub_pg.tables_missing`.
    TablesMissing {
        /// The missing tables, by name.
        tables: Vec<&'static str>,
    },
    /// `sync_hub_pg.queue_full`.
    QueueFull,
    /// `sync_hub_pg.subscription_invalid`.
    SubscriptionInvalid,
    /// `sync_hub_pg.state_invalid`.
    StateInvalid,
    /// `sync_hub_pg.node_refused`.
    NodeRefused {
        /// A fixed sentence saying why.
        reason: &'static str,
    },
    /// `sync_hub_pg.store_failed`: the database or the bridge failed; the fixed text carries the
    /// bridge code when there is one.
    StoreFailed {
        /// A fixed sentence.
        reason: &'static str,
    },
}

impl PgHubError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::PrepareFailed => PREPARE_FAILED,
            Self::UnsupportedContext { .. } => UNSUPPORTED_CONTEXT,
            Self::TablesMissing { .. } => TABLES_MISSING,
            Self::QueueFull => QUEUE_FULL,
            Self::SubscriptionInvalid => SUBSCRIPTION_INVALID,
            Self::StateInvalid => STATE_INVALID,
            Self::NodeRefused { .. } => NODE_REFUSED,
            Self::StoreFailed { .. } => STORE_FAILED,
        }
    }
}

impl fmt::Display for PgHubError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PrepareFailed => {
                write!(f, "{PREPARE_FAILED}: the hub tables could not be created")
            }
            Self::UnsupportedContext { reason } => write!(f, "{UNSUPPORTED_CONTEXT}: {reason}"),
            Self::TablesMissing { tables } => {
                write!(f, "{TABLES_MISSING}: missing {}", tables.join(", "))
            }
            Self::QueueFull => write!(f, "{QUEUE_FULL}: the destination's queue is full"),
            Self::SubscriptionInvalid => write!(
                f,
                "{SUBSCRIPTION_INVALID}: the stored subscription cannot be read; the node receives nothing"
            ),
            Self::StateInvalid => {
                write!(f, "{STATE_INVALID}: the stored relay state cannot be read")
            }
            Self::NodeRefused { reason } => write!(f, "{NODE_REFUSED}: {reason}"),
            Self::StoreFailed { reason } => write!(f, "{STORE_FAILED}: {reason}"),
        }
    }
}

impl std::error::Error for PgHubError {}

/// What went wrong inside one bridged operation. Never shown: it is mapped to a fixed reason.
#[derive(Debug)]
pub(crate) enum OpError {
    /// The driver failed (its message is dropped: it can quote a key or a value).
    Db,
    /// A stored row could not be decoded.
    Decode,
    /// A domain refusal; the transaction is rolled back and the error returned as is.
    Refused(PgHubError),
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
        BridgeError::Busy => "pg_bridge.busy: the hub store queue is full",
        BridgeError::Timeout => {
            "pg_bridge.timeout: the call missed its deadline; treat it as not done"
        }
        BridgeError::Closed => "pg_bridge.closed: the bridge is shut down",
        BridgeError::Connect => "pg_bridge.connect: no database connection",
        BridgeError::Config { .. } => "pg_bridge.config: the bridge is misconfigured",
        BridgeError::Store(OpError::Db) => "the hub store query failed",
        BridgeError::Store(OpError::Decode) => "a hub store row could not be decoded",
        BridgeError::Store(OpError::Refused(_)) => "the hub store refused the call",
        _ => "the hub store bridge refused the call",
    }
}

/// The store's own error for a bridged failure; a domain refusal comes back as itself.
pub(crate) fn map(error: BridgeError<OpError>) -> PgHubError {
    match error {
        BridgeError::Store(OpError::Refused(inner)) => inner,
        BridgeError::Store(OpError::Decode) => PgHubError::StateInvalid,
        other => PgHubError::StoreFailed {
            reason: bridge_reason(&other),
        },
    }
}

/// The relay's error for a failed queue call: a fixed sentence, plus the code for a full queue.
pub(crate) fn queue_error(error: BridgeError<OpError>) -> HubError {
    let inner = map(error);
    HubError::QueueFailed {
        reason: inner.to_string(),
    }
}

/// The relay's error for a failed stream read.
pub(crate) fn stream_error(error: BridgeError<OpError>) -> HubError {
    let inner = map(error);
    HubError::StreamFailed {
        reason: inner.to_string(),
    }
}
