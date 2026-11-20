//! Typed errors with stable `journal_commit.*` codes. A message never carries a command id or an
//! entry line.

/// A refusal of a call to the driver.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CommitError {
    /// A request or a local commit confirmation is not the next sequence number in order.
    #[error("sequence number {offered} is not the expected {expected}")]
    SeqOutOfOrder {
        /// The sequence number the driver expects.
        expected: u64,
        /// The sequence number offered.
        offered: u64,
    },
    /// A local commit confirmation named a sequence number that was never submitted, or was
    /// confirmed already.
    #[error("sequence number {seq} is not waiting for a local commit confirmation")]
    UnknownSeq {
        /// The sequence number named.
        seq: u64,
    },
    /// Shipping stopped for a reason a retry cannot cure (another writer, a broken chain, a bad
    /// batch); the driver takes no new commit.
    #[error("the journal relay stopped ({cause}); no new commit is taken")]
    RelayStopped {
        /// The relay's own stable error code.
        cause: &'static str,
    },
    /// The entry alone would make a tail chunk over the relay's size limit, so it is refused
    /// before it is queued: nothing was shipped and the sequence number is not consumed.
    #[error(
        "the entry would make a tail chunk of up to {bytes} bytes, over the {limit} byte limit"
    )]
    EntryTooLarge {
        /// The size bound of a chunk holding just this entry.
        bytes: u64,
        /// The chunk limit.
        limit: u64,
    },
    /// A setting is outside its allowed range.
    #[error("the setting {name} is invalid: it must be at least the acknowledgement deadline")]
    InvalidSetting {
        /// The setting's name.
        name: &'static str,
    },
}

impl CommitError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SeqOutOfOrder { .. } => "journal_commit.seq_out_of_order",
            Self::UnknownSeq { .. } => "journal_commit.unknown_seq",
            Self::RelayStopped { .. } => "journal_commit.relay_stopped",
            Self::EntryTooLarge { .. } => "journal_commit.entry_too_large",
            Self::InvalidSetting { .. } => "journal_commit.invalid_setting",
        }
    }
}
