//! Typed errors with stable `journal_audit.*` codes.
//!
//! A message names a key (an entity, an id, a fingerprint name) or a reason phrase; it never
//! carries a record's content, and no record digest is echoed.

/// Why a comparison or a fingerprint was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    /// A stream is not sorted strictly ascending by entity then id, so the merge would silently
    /// report a record as missing and extra at the same time.
    #[error("the {side} stream is not sorted by entity then id at record {entity}/{id}")]
    StreamNotSorted {
        /// Which side the stream came from (`live` or `rebuilt`).
        side: &'static str,
        /// The entity of the record that broke the order (or repeated the one before it).
        entity: String,
        /// The id of that record.
        id: String,
    },
    /// A fingerprint is unusable: a name is empty or repeated, or its keys are empty or repeated.
    #[error("the {fingerprint} fingerprint is invalid: {reason}")]
    FingerprintInvalid {
        /// The fingerprint's name, or `(unnamed)` when the name itself is the problem.
        fingerprint: String,
        /// What is wrong with it.
        reason: &'static str,
    },
    /// A decimal amount string is not an exact decimal.
    #[error("the amount {text:?} is not an exact decimal: {reason}")]
    AmountInvalid {
        /// The text the caller gave.
        text: String,
        /// What is wrong with it.
        reason: &'static str,
    },
}

impl AuditError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::StreamNotSorted { .. } => "journal_audit.stream_not_sorted",
            Self::FingerprintInvalid { .. } => "journal_audit.fingerprint_invalid",
            Self::AmountInvalid { .. } => "journal_audit.amount_invalid",
        }
    }
}
