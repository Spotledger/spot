//! Typed errors with stable `journal_durability.*` codes. Messages never carry an event body.

use crate::Seq;

/// A durability refusal or failure a caller can map to an API error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurabilityError {
    /// `remote-sync` with `on_unreachable: halt` while the object store cannot be reached.
    #[error("commits are halted while the journal object store is unreachable")]
    Halted,
    /// The commit is durable locally but the chunk acknowledgement did not arrive in time.
    #[error("the outcome of commit {seq} is unknown; retry with the same command id to query it")]
    OutcomeUnknown {
        /// The sequence number of the commit.
        seq: Seq,
    },
    /// Commits were acknowledged locally and have not been shipped within the grace period.
    #[error(
        "the recovery point objective is breached since {since} (unshipped from {unshipped_from_seq})"
    )]
    RpoBreach {
        /// Millisecond time the exposure began.
        since: u64,
        /// The first sequence number not yet in the object store.
        unshipped_from_seq: Seq,
    },
    /// The tenant's durability setting cannot be applied.
    #[error("the durability setting is invalid: {0}")]
    ConfigInvalid(&'static str),
}

impl DurabilityError {
    /// The stable catalogue code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Halted => "journal_durability.halted",
            Self::OutcomeUnknown { .. } => "journal_durability.outcome_unknown",
            Self::RpoBreach { .. } => "journal_durability.rpo_breach",
            Self::ConfigInvalid(_) => "journal_durability.config_invalid",
        }
    }
}
