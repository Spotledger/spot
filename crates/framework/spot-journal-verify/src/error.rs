//! Typed errors with stable `journal_verify.*` codes. A message never carries object bytes.

use spot_journal_store::StoreError;

/// Why a verification could not be carried out at all.
///
/// Anything that is wrong with the journal itself is a finding in the report, not an error: an
/// error means the question could not be asked or answered (the store failed, the caller asked
/// for the impossible).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VerifyError {
    /// The expected tenant, source or generation is not usable.
    #[error("the expectation is not usable: {0}")]
    ExpectedInvalid(&'static str),
    /// The store failed or refused a read.
    #[error("the journal store failed: {0}")]
    Store(#[from] StoreError),
    /// The byte budget cannot cover the next object, so no run could make progress.
    #[error("the byte budget {budget} cannot cover the {needed} bytes the next read needs")]
    BudgetTooSmall {
        /// The bytes the run would have to read to finish the next step.
        needed: u64,
        /// The configured budget.
        budget: u64,
    },
}

impl VerifyError {
    /// The stable catalogue code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::ExpectedInvalid(_) => "journal_verify.expected_invalid",
            Self::Store(_) => "journal_verify.store_failed",
            Self::BudgetTooSmall { .. } => "journal_verify.budget_too_small",
        }
    }
}
