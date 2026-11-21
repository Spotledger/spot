//! Typed errors with stable `journal_durability.*` codes. Messages never carry an event body.

use crate::{Generation, Position};

/// A durability refusal or failure a caller can map to an API error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DurabilityError {
    /// `remote-sync` with `on_unreachable: halt` while the object store cannot be reached.
    #[error("commits are halted while the journal object store is unreachable")]
    Halted,
    /// The commit is durable locally but the chunk acknowledgement did not arrive in time.
    #[error(
        "the outcome of commit {position} is unknown; retry with the same command id to query it"
    )]
    OutcomeUnknown {
        /// The position of the commit.
        position: Position,
    },
    /// Commits were acknowledged locally and have not been shipped within the grace period.
    #[error(
        "the recovery point objective is breached since {since} (unshipped from {unshipped_from})"
    )]
    RpoBreach {
        /// Millisecond time the exposure began.
        since: u64,
        /// The first position not yet in the object store.
        unshipped_from: Position,
    },
    /// The tenant's durability setting cannot be applied.
    #[error("the durability setting is invalid: {0}")]
    ConfigInvalid(&'static str),
    /// A commit of a generation lower than one already seen: a stale writer.
    #[error("commit {position} is from a stale generation (the gate is at generation {current})")]
    StaleGeneration {
        /// The refused commit.
        position: Position,
        /// The highest generation the gate has seen.
        current: Generation,
    },
    /// A commit whose position is not above the last one the gate accepted in its generation.
    #[error("commit {position} is not after the last accepted commit {last}")]
    OutOfOrder {
        /// The refused commit.
        position: Position,
        /// The last accepted position.
        last: Position,
    },
    /// A commit under a command id the gate already holds (not withdrawn).
    #[error("commit {position} reuses a command id that is already committed")]
    DuplicateCommand {
        /// The refused commit.
        position: Position,
    },
}

impl DurabilityError {
    /// The stable catalogue code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Halted => "journal_durability.halted",
            Self::OutcomeUnknown { .. } => "journal_durability.outcome_unknown",
            Self::RpoBreach { .. } => "journal_durability.rpo_breach",
            Self::ConfigInvalid(_) => "journal_durability.config_invalid",
            Self::StaleGeneration { .. } => "journal_durability.stale_generation",
            Self::OutOfOrder { .. } => "journal_durability.out_of_order",
            Self::DuplicateCommand { .. } => "journal_durability.duplicate_command",
        }
    }
}
