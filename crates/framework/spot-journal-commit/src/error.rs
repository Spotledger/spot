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
}

impl CommitError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::SeqOutOfOrder { .. } => "journal_commit.seq_out_of_order",
            Self::UnknownSeq { .. } => "journal_commit.unknown_seq",
            Self::RelayStopped { .. } => "journal_commit.relay_stopped",
        }
    }
}
