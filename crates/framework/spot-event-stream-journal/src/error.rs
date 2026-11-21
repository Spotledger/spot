//! Typed errors with stable `eventstream_journal.*` codes. An error never carries a segment's
//! content, an envelope line or key material.

/// Why history could not be served. Every variant fails closed: a partial or unverified range is
/// never returned.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HistoryError {
    /// The position is in history the journal no longer serves as one stream (an abandoned
    /// generation, or the outbox horizon moved past the seam): the consumer must resync.
    #[error("resync required: generation {generation}, seq {seq} cannot be continued")]
    ResyncRequired {
        /// The generation of the position that cannot be continued.
        generation: u64,
        /// Its sequence number.
        seq: u64,
    },
    /// A manifest, segment or chunk is missing, does not verify or breaks the chain: nothing is
    /// served from the affected range.
    #[error("journal history is unavailable: {reason}")]
    HistoryUnavailable {
        /// What failed, in words (never content).
        reason: &'static str,
    },
    /// A key or an object names another tenant, another source or an unsafe path.
    #[error("the journal object is not in this tenant's journal")]
    ForeignKey,
    /// The request itself is not acceptable.
    #[error("the history request is invalid: {0}")]
    RequestInvalid(&'static str),
}

impl HistoryError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ResyncRequired { .. } => "eventstream_journal.resync_required",
            Self::HistoryUnavailable { .. } => "eventstream_journal.history_unavailable",
            Self::ForeignKey => "eventstream_journal.foreign_key",
            Self::RequestInvalid(_) => "eventstream_journal.request_invalid",
        }
    }
}

pub(crate) fn unavailable(reason: &'static str) -> HistoryError {
    HistoryError::HistoryUnavailable { reason }
}
