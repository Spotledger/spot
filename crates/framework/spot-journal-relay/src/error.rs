//! Typed errors with stable `journal_relay.*` codes. A message never carries an entry line.

use spot_journal_store::StoreError;

/// Every error the relay returns.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelayError {
    /// The stored chain (manifest, segments, tail chunks) disagrees with itself or with the
    /// manifest. The relay never repairs this by overwriting; an operator looks at it.
    #[error("the stored journal chain is broken: {reason}")]
    ChainBroken {
        /// What disagrees, in words (never an entry line).
        reason: &'static str,
    },
    /// The manifest, a segment or a tail chunk was written by someone else: another relay owns
    /// this source. The relay has stopped and writes nothing further.
    #[error("another relay owns this source: {reason}")]
    SplitBrain {
        /// What revealed the second writer.
        reason: &'static str,
    },
    /// A batch repeats or goes back over positions the relay already holds, or reuses a batch id.
    #[error("position {offered} is not the expected {expected}: a repeat or an out-of-order entry")]
    EntryOutOfOrder {
        /// The sequence number the relay expects next.
        expected: u64,
        /// The sequence number offered.
        offered: u64,
    },
    /// A batch skips positions, inside the batch or after the last position the relay holds.
    #[error("position {offered} leaves a gap after the expected {expected}")]
    BatchGap {
        /// The sequence number the relay expects next.
        expected: u64,
        /// The sequence number offered.
        offered: u64,
    },
    /// An entry or a batch is not acceptable by itself (empty batch, wrong generation, a line
    /// that is not one line of UTF-8 or is too large).
    #[error("the batch is invalid: {0}")]
    EntryInvalid(&'static str),
    /// The relay configuration is unusable.
    #[error("the relay configuration is invalid: {0}")]
    ConfigInvalid(&'static str),
    /// An earlier call failed part-way, so the relay does not know what the store holds. Call
    /// `resume` before anything else.
    #[error("an earlier call failed part-way: resume the relay before appending")]
    ResumeRequired,
    /// The object store failed or refused.
    #[error("the journal store failed: {0}")]
    Store(StoreError),
}

impl RelayError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ChainBroken { .. } => "journal_relay.chain_broken",
            Self::SplitBrain { .. } => "journal_relay.split_brain",
            Self::EntryOutOfOrder { .. } => "journal_relay.entry_out_of_order",
            Self::BatchGap { .. } => "journal_relay.batch_gap",
            Self::EntryInvalid(_) => "journal_relay.entry_invalid",
            Self::ConfigInvalid(_) => "journal_relay.config_invalid",
            Self::ResumeRequired => "journal_relay.resume_required",
            Self::Store(_) => "journal_relay.store_failed",
        }
    }
}

impl From<StoreError> for RelayError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}
