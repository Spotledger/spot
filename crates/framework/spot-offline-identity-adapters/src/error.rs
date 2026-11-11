//! The one error type of the adapters, with stable `offline_identity_adapters.*` codes.
//!
//! No message carries key material, a wrapped key, a sealed blob or a payload: only ids,
//! journal positions, epochs and counts. Every code is catalogued in
//! `crates/framework/spot-errors/catalogue/offline_identity_adapters.toml`.

/// The queue store could not be read at start-up.
pub const QUARANTINE_READ: &str = "offline_identity_adapters.quarantine_read";
/// The queue store refused the item.
pub const QUARANTINE_WRITE: &str = "offline_identity_adapters.quarantine_write";
/// A stored envelope is not a queue item.
pub const QUARANTINE_CORRUPT: &str = "offline_identity_adapters.quarantine_corrupt";
/// The queue already holds a different payload for this node and journal position.
pub const QUARANTINE_CONFLICT: &str = "offline_identity_adapters.quarantine_conflict";
/// A node id cannot key a queue item (it is not a valid queue source id).
pub const ITEM_ID: &str = "offline_identity_adapters.item_id";
/// The sealed state could not be read.
pub const STATE_READ: &str = "offline_identity_adapters.state_read";
/// The sealed state could not be written.
pub const STATE_WRITE: &str = "offline_identity_adapters.state_write";
/// The sealed state did not authenticate: it was altered or sealed for another node.
pub const STATE_TAMPERED: &str = "offline_identity_adapters.state_tampered";
/// The stored epoch is lower than the highest epoch this node has already seen.
pub const STATE_ROLLBACK: &str = "offline_identity_adapters.state_rollback";
/// The unsealed state is not a `Persisted` of this format.
pub const STATE_MALFORMED: &str = "offline_identity_adapters.state_malformed";

/// A failure of an adapter. The caller matches on [`AdapterError::code`], never on the text.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// The queue store could not be read.
    #[error("the quarantine store could not be read ({code})", code = self.code())]
    QuarantineRead,
    /// The queue store refused the item.
    #[error("the quarantine store refused the item ({code})", code = self.code())]
    QuarantineWrite,
    /// A stored envelope is not a queue item.
    #[error("a stored quarantine item is not readable ({code})", code = self.code())]
    QuarantineCorrupt,
    /// The queue already holds a different payload for this position.
    #[error(
        "the quarantine queue holds a different payload for node {node} at journal position \
         {journal_seq} ({code})",
        code = self.code()
    )]
    QuarantineConflict {
        /// The node.
        node: String,
        /// The journal position.
        journal_seq: u64,
    },
    /// A node id cannot key a queue item.
    #[error("the node id cannot key a quarantine item ({code})", code = self.code())]
    ItemId,
    /// The sealed state could not be read.
    #[error("the sealed state could not be read ({code})", code = self.code())]
    StateRead,
    /// The sealed state could not be written.
    #[error("the sealed state could not be written ({code})", code = self.code())]
    StateWrite,
    /// The sealed state did not authenticate.
    #[error("the sealed state did not authenticate ({code})", code = self.code())]
    StateTampered,
    /// The stored epoch went backwards.
    #[error(
        "the stored epoch {stored} is lower than the epoch {seen} this node has already seen \
         ({code})",
        code = self.code()
    )]
    StateRollback {
        /// The epoch in the stored state.
        stored: u64,
        /// The highest epoch this node has seen.
        seen: u64,
    },
    /// The unsealed state is not a `Persisted`.
    #[error("the unsealed state is not readable ({code})", code = self.code())]
    StateMalformed,
}

impl AdapterError {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::QuarantineRead => QUARANTINE_READ,
            Self::QuarantineWrite => QUARANTINE_WRITE,
            Self::QuarantineCorrupt => QUARANTINE_CORRUPT,
            Self::QuarantineConflict { .. } => QUARANTINE_CONFLICT,
            Self::ItemId => ITEM_ID,
            Self::StateRead => STATE_READ,
            Self::StateWrite => STATE_WRITE,
            Self::StateTampered => STATE_TAMPERED,
            Self::StateRollback { .. } => STATE_ROLLBACK,
            Self::StateMalformed => STATE_MALFORMED,
        }
    }
}
