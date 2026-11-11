//! The one error type of the sync-layer wiring, with stable [`codes`](crate::codes).

use spot_quarantine::QuarantineError;
use spot_shared_balance::{BalanceError, EntryId, NodeId};

use crate::codes;

/// Why a batch was not accepted.
///
/// A refusal never carries a secret: nodes are named by id and entries by their id. It also
/// never carries an amount or a balance value beyond what the receipt already names.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SyncError {
    /// The transport authenticated `authenticated`, and the batch (or one of its entries) named
    /// `claimed`. Nothing in the batch was applied.
    #[error("the batch claims node {claimed} but the connection authenticated as {authenticated}")]
    SenderMismatch {
        /// The node the transport authenticated (mutual TLS through the node directory).
        authenticated: NodeId,
        /// The node the batch or one of its entries named.
        claimed: NodeId,
        /// The entry that named the other node, when the batch header itself matched.
        entry: Option<EntryId>,
    },
    /// The node is not in the hub's registry, so the hub cannot vouch for it.
    #[error("node {node} is not registered")]
    NodeNotRegistered {
        /// The authenticated node.
        node: NodeId,
    },
    /// The node is registered but suspended or decommissioned.
    #[error("node {node} is {state}")]
    NodeNotActive {
        /// The authenticated node.
        node: NodeId,
        /// The registry's state: `suspended` or `decommissioned`.
        state: &'static str,
    },
    /// The audit of a refusal could not be written. The refusal itself still stands.
    #[error("the refusal could not be audited: {code} was not recorded")]
    AuditFailed {
        /// The code of the refusal that is now unrecorded.
        code: &'static str,
    },
    /// A ledger would not take an entry (a reused id or an overflow). Everything handled before
    /// the failure stays handled, and a repeat of the batch resumes safely.
    #[error(transparent)]
    Ledger(#[from] BalanceError),
    /// The quarantine queue could not take a refused entry, so the batch must not be
    /// acknowledged: the sender sends it again and the adapter retries the placement.
    #[error(transparent)]
    Quarantine(#[from] QuarantineError),
}

impl SyncError {
    /// The stable code from [`codes`](crate::codes) that names this error.
    ///
    /// A ledger refusal keeps the `shared_balance.*` code of the operation that failed.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            SyncError::SenderMismatch { .. } => codes::SENDER_MISMATCH,
            SyncError::NodeNotRegistered { .. } => codes::NODE_NOT_REGISTERED,
            SyncError::NodeNotActive { .. } => codes::NODE_NOT_ACTIVE,
            SyncError::AuditFailed { .. } => codes::AUDIT_FAILED,
            SyncError::Ledger(error) => error.code(),
            SyncError::Quarantine(_) => codes::QUARANTINE_FAILED,
        }
    }
}
