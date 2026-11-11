//! Typed errors with stable [`codes`](crate::codes).
//!
//! A failed rehearsal or a failed verification is not just an error: the pipeline records the
//! evidence and moves the promotion to [`Failed`](crate::PromotionState::Failed) before it
//! returns. The error tells the caller why the transition was refused.

use crate::codes;

/// Anything the promotion pipeline refuses to do.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PromotionError {
    /// The promotion id is not usable as a run id in the migration audit store.
    #[error("{}: {message}", codes::BAD_ID)]
    BadId {
        /// What is wrong with the id.
        message: String,
    },
    /// No promotion with this id is stored.
    #[error("{}: no promotion {id}", codes::NOT_FOUND)]
    NotFound {
        /// The id that was looked up.
        id: String,
    },
    /// A promotion with this id already exists.
    #[error("{}: promotion {id} already exists", codes::DUPLICATE)]
    Duplicate {
        /// The id that was taken.
        id: String,
    },
    /// The promotion is in another state than the step requires.
    #[error(
        "{}: promotion {id} is {state}; this step starts from {required}",
        codes::BAD_STATE
    )]
    BadState {
        /// The promotion.
        id: String,
        /// Its current state.
        state: String,
        /// The state the step requires.
        required: String,
    },
    /// The bundle in hand is not the one the promotion was requested for.
    #[error(
        "{}: promotion {id} is for bundle {expected}, not {found}",
        codes::BUNDLE_MISMATCH
    )]
    BundleMismatch {
        /// The promotion.
        id: String,
        /// The digest the promotion records.
        expected: String,
        /// The digest of the bundle in hand.
        found: String,
    },
    /// The bundle could not be planned against the target.
    #[error("{}: {message}", codes::PLAN_FAILED)]
    PlanFailed {
        /// What the bundle planner reported (never content or a secret).
        message: String,
    },
    /// A blocking conflict stops the promotion before anything is written.
    #[error("{}: {count} blocking item(s), first: {first}", codes::BLOCKED)]
    Blocked {
        /// How many items block.
        count: usize,
        /// The first blocking item, for the message.
        first: String,
    },
    /// The bundle would change nothing on the target.
    #[error(
        "{}: the bundle matches the target; there is nothing to promote",
        codes::NOTHING_TO_PROMOTE
    )]
    NothingToPromote,
    /// The plan in hand is not the plan that was approved.
    #[error(
        "{}: the plan is {found}, but {expected} was approved",
        codes::PLAN_CHANGED
    )]
    PlanChanged {
        /// The approved digest.
        expected: String,
        /// The digest of the plan in hand.
        found: String,
    },
    /// The target environment's policy requires approvals that are missing, rejected or stale.
    #[error("{}: {refusal}", codes::APPROVAL_REQUIRED)]
    ApprovalRequired {
        /// The approval crate's refusal, in a sentence.
        refusal: String,
    },
    /// The approval service refused an approval or a rejection.
    #[error("{}: {reason}", codes::APPROVAL_REFUSED)]
    ApprovalRefused {
        /// The approval service's refusal (its stable code and a sentence, never a secret).
        reason: String,
    },
    /// The target environment's policy requires a rehearsal before apply.
    #[error(
        "{}: the {environment} environment requires a rehearsal first",
        codes::REHEARSAL_REQUIRED
    )]
    RehearsalRequired {
        /// The environment role the policy is for.
        environment: String,
    },
    /// The rehearsal reported a failure; apply is refused and the target was never written.
    #[error(
        "{}: the rehearsal reported {verdict} with {findings} finding(s)",
        codes::REHEARSAL_FAILED
    )]
    RehearsalFailed {
        /// The rehearsal verdict.
        verdict: String,
        /// How many findings it reported.
        findings: u64,
    },
    /// Writing the bundle to the target failed.
    #[error("{}: {message}", codes::APPLY_FAILED)]
    ApplyFailed {
        /// What the applier reported (never content or a secret).
        message: String,
    },
    /// Verification found the target does not match the approved plan.
    #[error(
        "{}: {findings} difference(s); the target was restored",
        codes::VERIFY_FAILED
    )]
    VerifyFailed {
        /// How many differences verification found.
        findings: usize,
    },
    /// Restoring the target to its pre-apply state failed.
    #[error("{}: {message}", codes::RESTORE_FAILED)]
    RestoreFailed {
        /// What the applier reported.
        message: String,
    },
    /// A transition could not be recorded in the migration audit store.
    #[error("{}: {message}", codes::AUDIT_FAILED)]
    AuditFailed {
        /// What the audit backend reported.
        message: String,
    },
    /// The promotion store could not be read or written.
    #[error("{}: {message}", codes::STORE_FAILED)]
    StoreFailed {
        /// What the store reported.
        message: String,
    },
    /// The environment approval or rehearsal policy is not usable.
    #[error("{}: {message}", codes::POLICY_INVALID)]
    PolicyInvalid {
        /// What is wrong, in a sentence.
        message: String,
    },
    /// The promotion does not advance the environment ladder by one role.
    #[error(
        "{}: a promotion runs from {from} to the next environment, not to {to}",
        codes::BAD_ROUTE
    )]
    BadRoute {
        /// The source environment role.
        from: String,
        /// The target environment role.
        to: String,
    },
    /// The target site's environment does not match the plan's target environment.
    #[error(
        "{}: the target is {found}, not {expected}",
        codes::ENVIRONMENT_MISMATCH
    )]
    EnvironmentMismatch {
        /// The environment the plan targets.
        expected: String,
        /// The environment the target site is in.
        found: String,
    },
}

impl PromotionError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadId { .. } => codes::BAD_ID,
            Self::NotFound { .. } => codes::NOT_FOUND,
            Self::Duplicate { .. } => codes::DUPLICATE,
            Self::BadState { .. } => codes::BAD_STATE,
            Self::BundleMismatch { .. } => codes::BUNDLE_MISMATCH,
            Self::PlanFailed { .. } => codes::PLAN_FAILED,
            Self::Blocked { .. } => codes::BLOCKED,
            Self::NothingToPromote => codes::NOTHING_TO_PROMOTE,
            Self::PlanChanged { .. } => codes::PLAN_CHANGED,
            Self::ApprovalRequired { .. } => codes::APPROVAL_REQUIRED,
            Self::ApprovalRefused { .. } => codes::APPROVAL_REFUSED,
            Self::RehearsalRequired { .. } => codes::REHEARSAL_REQUIRED,
            Self::RehearsalFailed { .. } => codes::REHEARSAL_FAILED,
            Self::ApplyFailed { .. } => codes::APPLY_FAILED,
            Self::VerifyFailed { .. } => codes::VERIFY_FAILED,
            Self::RestoreFailed { .. } => codes::RESTORE_FAILED,
            Self::AuditFailed { .. } => codes::AUDIT_FAILED,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
            Self::PolicyInvalid { .. } => codes::POLICY_INVALID,
            Self::BadRoute { .. } => codes::BAD_ROUTE,
            Self::EnvironmentMismatch { .. } => codes::ENVIRONMENT_MISMATCH,
        }
    }
}
