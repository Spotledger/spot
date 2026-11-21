//! The pipeline's one error type and its stable `fix_pipeline.*` vocabulary.
//!
//! A lifecycle refusal (an approval that never came, a rehearsal that failed) is a **state**,
//! not an error: it is journaled and returned in a [`FixOutcome`](crate::FixOutcome) so the
//! caller can show it. A [`FixError`] is an infrastructure or contract failure: an invalid plan,
//! a journal or audit sink that could not be written, a pre-image store that failed, or a kill
//! that fault injection injected.
//!
//! Every code here has exactly one entry in
//! `crates/framework/spot-errors/catalogue/fix_pipeline.toml`.

/// The stable `fix_pipeline.*` codes, in one place.
pub mod codes {
    /// A fix id is empty or holds a character an id cannot.
    pub const INVALID_ID: &str = "fix_pipeline.invalid_id";
    /// A plan is empty, malformed or its hash does not bind its contents.
    pub const INVALID_PLAN: &str = "fix_pipeline.invalid_plan";
    /// The plan belongs to another fix or is not the journaled one.
    pub const PLAN_MISMATCH: &str = "fix_pipeline.plan_mismatch";
    /// The agent's tier cannot make a change (T0 only reads).
    pub const TIER_NOT_FIXABLE: &str = "fix_pipeline.tier_not_fixable";
    /// The action is out of the agent's tier or the hard floor refuses it.
    pub const REFUSED: &str = "fix_pipeline.refused";
    /// A T3 fix needs a human approval and none was supplied.
    pub const APPROVAL_REQUIRED: &str = "fix_pipeline.approval_required";
    /// The approval port refused the plan.
    pub const APPROVAL_REFUSED: &str = "fix_pipeline.approval_refused";
    /// A T3 fix must be rehearsed and this path tried to skip it.
    pub const REHEARSAL_REQUIRED: &str = "fix_pipeline.rehearsal_required";
    /// The rehearsal failed; the fix ended with evidence and no change.
    pub const REHEARSAL_FAILED: &str = "fix_pipeline.rehearsal_failed";
    /// The write-ahead journal could not be written or read.
    pub const JOURNAL_FAILED: &str = "fix_pipeline.journal_failed";
    /// The audit sink could not record a transition; the run fails closed.
    pub const AUDIT_FAILED: &str = "fix_pipeline.audit_failed";
    /// The logical pre-image of an affected record could not be captured or restored.
    pub const PRE_IMAGE_FAILED: &str = "fix_pipeline.pre_image_failed";
    /// The snapshot could not be taken, restored or the maintenance gate moved.
    pub const SNAPSHOT_FAILED: &str = "fix_pipeline.snapshot_failed";
    /// A fix step failed while it was running.
    pub const STEP_FAILED: &str = "fix_pipeline.step_failed";
    /// Verification failed; the pipeline rolled back.
    pub const VERIFY_FAILED: &str = "fix_pipeline.verify_failed";
    /// The automatic rollback itself failed.
    pub const ROLLBACK_FAILED: &str = "fix_pipeline.rollback_failed";
    /// The rollback left a residue: the state is not byte-identical to the pre-state.
    pub const RESIDUE_FOUND: &str = "fix_pipeline.residue_found";
    /// A target changed after the fix, so undo would clobber the later change.
    pub const UNDO_CONFLICT: &str = "fix_pipeline.undo_conflict";
    /// The undo window has closed; the fix stands.
    pub const UNDO_EXPIRED: &str = "fix_pipeline.undo_expired";
    /// The fix is not verified, so there is nothing to undo.
    pub const UNDO_UNAVAILABLE: &str = "fix_pipeline.undo_unavailable";
    /// The fix cannot be undone: its source declares no inverse.
    pub const IRREVERSIBLE: &str = "fix_pipeline.irreversible";
    /// The requested transition is not allowed from the journaled state.
    pub const WRONG_STATE: &str = "fix_pipeline.wrong_state";
    /// Fault injection killed the run at the named boundary.
    pub const KILLED: &str = "fix_pipeline.killed";
    /// Something read the wall clock, used a float or otherwise broke a pipeline rule.
    pub const INTERNAL: &str = "fix_pipeline.internal";
}

/// Something failed in the fix pipeline.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FixError {
    /// A fix id is malformed.
    #[error("the fix id `{id}` is not valid: {detail}")]
    InvalidId {
        /// The offending id.
        id: String,
        /// What is wrong.
        detail: String,
    },

    /// A plan is empty, malformed or forged.
    #[error("the fix plan is invalid: {detail}")]
    InvalidPlan {
        /// What is wrong.
        detail: String,
    },

    /// The plan does not match the journaled one.
    #[error("the fix plan does not match the plan the journal holds")]
    PlanMismatch,

    /// The agent's tier cannot make changes.
    #[error("an agent at tier {tier} cannot run a fix; T0 only reads")]
    TierNotFixable {
        /// The refusing tier.
        tier: String,
    },

    /// The action is out of tier or the hard floor refuses it.
    #[error("the fix is refused: {detail}")]
    Refused {
        /// Why.
        detail: String,
    },

    /// A T3 fix has no human approval.
    #[error("the T3 fix needs a human approval and none was supplied")]
    ApprovalRequired,

    /// The approval port refused.
    #[error("the approval for the fix was refused: {detail}")]
    ApprovalRefused {
        /// Why.
        detail: String,
    },

    /// A T3 path tried to skip rehearsal.
    #[error("the T3 fix must be rehearsed before it is applied")]
    RehearsalRequired,

    /// The rehearsal failed.
    #[error("the rehearsal failed: {detail}")]
    RehearsalFailed {
        /// Why.
        detail: String,
    },

    /// The journal failed.
    #[error("the fix journal failed: {detail}")]
    JournalFailed {
        /// The backend's message, never a secret.
        detail: String,
    },

    /// The audit sink failed.
    #[error("the audit sink failed: {detail}")]
    AuditFailed {
        /// The sink's message, never a secret.
        detail: String,
    },

    /// The pre-image store failed.
    #[error("the pre-image store failed: {detail}")]
    PreImageFailed {
        /// The store's message, never a secret.
        detail: String,
    },

    /// The snapshot port failed.
    #[error("the snapshot port failed: {detail}")]
    SnapshotFailed {
        /// The port's message, never a secret.
        detail: String,
    },

    /// A step failed.
    #[error("fix step {index} (`{id}`) failed: {detail}")]
    StepFailed {
        /// The step index.
        index: usize,
        /// The step id.
        id: String,
        /// Why.
        detail: String,
    },

    /// Verification failed.
    #[error("verification failed: {detail}")]
    VerifyFailed {
        /// Why.
        detail: String,
    },

    /// The automatic rollback failed.
    #[error("the rollback failed: {detail}")]
    RollbackFailed {
        /// Why.
        detail: String,
    },

    /// The rollback left a residue.
    #[error("the rollback left {diffs} residual difference(s)")]
    ResidueFound {
        /// How many fingerprint differences remain.
        diffs: usize,
    },

    /// A target changed after the fix.
    #[error("undo conflicts because `{target}` changed after the fix")]
    UndoConflict {
        /// The changed target.
        target: String,
    },

    /// The undo window closed.
    #[error("the undo window for the fix closed at {expires_at}")]
    UndoExpired {
        /// The closing instant (unix seconds).
        expires_at: i64,
    },

    /// There is nothing to undo.
    #[error("the fix is in state {state} and cannot be undone")]
    UndoUnavailable {
        /// The journaled state.
        state: String,
    },

    /// The fix is irreversible.
    #[error("the fix is irreversible: {detail}")]
    Irreversible {
        /// Why.
        detail: String,
    },

    /// The transition is not allowed.
    #[error("the fix is in state {state} and cannot {action}")]
    WrongState {
        /// The journaled state.
        state: String,
        /// The attempted transition.
        action: String,
    },

    /// Fault injection killed the run.
    #[error("the fix was killed at `{at}`")]
    Killed {
        /// The boundary label.
        at: String,
    },

    /// An internal rule was broken.
    #[error("the fix pipeline refused an internal rule: {detail}")]
    Internal {
        /// What was wrong.
        detail: String,
    },
}

/// The pipeline's result type.
pub type Result<T> = std::result::Result<T, FixError>;

impl FixError {
    /// The stable code this error maps onto.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => codes::INVALID_ID,
            Self::InvalidPlan { .. } => codes::INVALID_PLAN,
            Self::PlanMismatch => codes::PLAN_MISMATCH,
            Self::TierNotFixable { .. } => codes::TIER_NOT_FIXABLE,
            Self::Refused { .. } => codes::REFUSED,
            Self::ApprovalRequired => codes::APPROVAL_REQUIRED,
            Self::ApprovalRefused { .. } => codes::APPROVAL_REFUSED,
            Self::RehearsalRequired => codes::REHEARSAL_REQUIRED,
            Self::RehearsalFailed { .. } => codes::REHEARSAL_FAILED,
            Self::JournalFailed { .. } => codes::JOURNAL_FAILED,
            Self::AuditFailed { .. } => codes::AUDIT_FAILED,
            Self::PreImageFailed { .. } => codes::PRE_IMAGE_FAILED,
            Self::SnapshotFailed { .. } => codes::SNAPSHOT_FAILED,
            Self::StepFailed { .. } => codes::STEP_FAILED,
            Self::VerifyFailed { .. } => codes::VERIFY_FAILED,
            Self::RollbackFailed { .. } => codes::ROLLBACK_FAILED,
            Self::ResidueFound { .. } => codes::RESIDUE_FOUND,
            Self::UndoConflict { .. } => codes::UNDO_CONFLICT,
            Self::UndoExpired { .. } => codes::UNDO_EXPIRED,
            Self::UndoUnavailable { .. } => codes::UNDO_UNAVAILABLE,
            Self::Irreversible { .. } => codes::IRREVERSIBLE,
            Self::WrongState { .. } => codes::WRONG_STATE,
            Self::Killed { .. } => codes::KILLED,
            Self::Internal { .. } => codes::INTERNAL,
        }
    }
}
