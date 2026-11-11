//! The one error type of the quarantine queue, with stable [`codes`](crate::codes).

use crate::codes;
use crate::item::{ItemId, State};

/// What a quarantine operation failed with.
///
/// It never carries a secret or a field value: an item is named by its id, and the store's own
/// explanation is the only free text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum QuarantineError {
    /// The item a caller named does not exist.
    #[error("quarantine item {id} was not found")]
    NotFound {
        /// The item the caller named.
        id: ItemId,
    },
    /// The acting principal may not perform the operation on the item.
    #[error("the principal may not {access} quarantine item {id}")]
    Denied {
        /// The operation that was refused.
        access: &'static str,
        /// The item it was refused on.
        id: ItemId,
    },
    /// A discard was requested without the reason it requires.
    #[error("discarding quarantine item {id} needs a reason")]
    ReasonRequired {
        /// The item the discard was refused on.
        id: ItemId,
    },
    /// The item is closed, so the operation does not apply.
    #[error("quarantine item {id} is {state} and cannot be {access}")]
    NotOpen {
        /// The item.
        id: ItemId,
        /// The item's state.
        state: State,
        /// The operation that was refused.
        access: &'static str,
    },
    /// The item is closed (released or discarded), so no change to it is accepted.
    #[error("quarantine item {id} is closed and cannot change")]
    ItemClosed {
        /// The item.
        id: ItemId,
    },
    /// A value is longer than the queue accepts; nothing was stored and nothing was truncated.
    #[error("the {what} is {actual} bytes, over the {limit} the quarantine queue accepts")]
    TooLarge {
        /// Which value: `payload`, `detail`, `reason code` or `source`.
        what: &'static str,
        /// The limit, in bytes.
        limit: usize,
        /// The size that was offered, in bytes.
        actual: usize,
    },
    /// The item only marks a record too large to store, so it cannot be retried or released.
    #[error("quarantine item {id} marks a record too large to store; it can only be discarded")]
    MarkerOnly {
        /// The item.
        id: ItemId,
    },
    /// The quarantine store failed.
    #[error("the quarantine store failed: {why}")]
    Store {
        /// What the store reported.
        why: String,
    },
    /// Recording a quarantine audit entry failed.
    #[error("recording the quarantine audit entry failed: {why}")]
    Audit {
        /// What the audit sink reported.
        why: String,
    },
}

impl QuarantineError {
    /// The stable code from [`codes`](crate::codes) that names this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            QuarantineError::NotFound { .. } => codes::NOT_FOUND,
            QuarantineError::Denied { .. } => codes::DENIED,
            QuarantineError::ReasonRequired { .. } => codes::REASON_REQUIRED,
            QuarantineError::NotOpen { .. } => codes::NOT_OPEN,
            QuarantineError::ItemClosed { .. } => codes::ITEM_CLOSED,
            QuarantineError::TooLarge { .. } => codes::TOO_LARGE,
            QuarantineError::MarkerOnly { .. } => codes::MARKER_ONLY,
            QuarantineError::Store { .. } => codes::STORE_FAILED,
            QuarantineError::Audit { .. } => codes::AUDIT_FAILED,
        }
    }
}
