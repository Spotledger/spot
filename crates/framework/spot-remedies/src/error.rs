//! The one error type for the remedy framework, with stable machine-readable `remedies.*` codes.
//!
//! Every failure a remedy run can produce is a [`RemedyError`]: an unknown or invalid remedy, a
//! refusal from the tier authoriser, parameters that fail the remedy's schema, a precondition
//! that is not met, a plan that is stale or forged, an undo that is unknown, expired or in
//! conflict, a typed operation that failed, and the hard floor's refusal of a direct edit to a
//! submitted financial record.
//!
//! The codes are the crate's stable vocabulary: they never change meaning within a major version
//! and each has one entry in `spot-errors`' `catalogue/remedies.toml`.

/// The stable `remedies.*` codes, in one place. Each has one catalogue entry in
/// `crates/framework/spot-errors/catalogue/remedies.toml`.
mod codes {
    /// No remedy with the requested id is in the registry.
    pub const UNKNOWN: &str = "remedies.unknown";
    /// The tier authoriser refused the remedy for this agent.
    pub const REFUSED: &str = "remedies.refused";
    /// The parameters do not satisfy the remedy's JSON Schema.
    pub const PARAMS_INVALID: &str = "remedies.params_invalid";
    /// The remedy's precondition is not met, so it must not run.
    pub const PRECONDITION_FAILED: &str = "remedies.precondition_failed";
    /// The plan is stale or forged: its hash does not bind it to this remedy and its parameters.
    pub const PLAN_STALE: &str = "remedies.plan_stale";
    /// No undo record exists for the token (or it was already undone).
    pub const UNDO_UNKNOWN: &str = "remedies.undo_unknown";
    /// The undo window has passed; the change stands.
    pub const UNDO_EXPIRED: &str = "remedies.undo_expired";
    /// A later change touched an affected target, so undo would clobber it.
    pub const UNDO_CONFLICT: &str = "remedies.undo_conflict";
    /// The remedy declares its undo irreversible and refuses it.
    pub const IRREVERSIBLE: &str = "remedies.irreversible";
    /// Two remedies declare the same id.
    pub const DUPLICATE: &str = "remedies.duplicate";
    /// A remedy id is not dotted lower-case.
    pub const INVALID_ID: &str = "remedies.invalid_id";
    /// A remedy's metadata breaks a rule (empty title, an irreversible undo above T3, ...).
    pub const META_INVALID: &str = "remedies.meta_invalid";
    /// The action catalogue the registry builds from the remedies did not load.
    pub const CATALOGUE_INVALID: &str = "remedies.catalogue_invalid";
    /// A corpus file did not parse or a case broke a rule.
    pub const CORPUS_INVALID: &str = "remedies.corpus_invalid";
    /// A typed operation over the storage/document/BPM ports failed.
    pub const OP_FAILED: &str = "remedies.op_failed";
    /// The context refused a direct edit of a submitted financial record (the hard floor).
    pub const HARD_FLOOR_FINANCIAL: &str = "remedies.hard_floor_financial";
}

/// Something failed in the remedy framework.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RemedyError {
    /// No remedy with the requested id is in the registry.
    #[error("no remedy `{remedy}` is in the registry")]
    Unknown {
        /// The id nothing is registered under.
        remedy: String,
    },

    /// The tier authoriser refused the remedy for this agent: the reason is the agent
    /// authoriser's own stable code and sentence, carried through unchanged.
    #[error("the remedy `{remedy}` is refused: {detail}")]
    Refused {
        /// The remedy id.
        remedy: String,
        /// The authoriser's stable `agent.*` code.
        code: String,
        /// The authoriser's plain-language sentence.
        detail: String,
    },

    /// The parameters do not satisfy the remedy's JSON Schema.
    #[error("the parameters for `{remedy}` are invalid at `{path}`: {detail}")]
    ParamsInvalid {
        /// The remedy id.
        remedy: String,
        /// The JSON pointer-ish path of the offending value.
        path: String,
        /// What was wrong.
        detail: String,
    },

    /// The remedy's precondition is not met, so it must not run.
    #[error("the precondition for `{remedy}` is not met: {detail}")]
    PreconditionFailed {
        /// The remedy id.
        remedy: String,
        /// The precondition's own explanation.
        detail: String,
    },

    /// The plan is stale or forged: its hash does not bind it to this remedy and its parameters.
    #[error("the plan for `{remedy}` is stale: {detail}")]
    PlanStale {
        /// The remedy id.
        remedy: String,
        /// What did not match.
        detail: String,
    },

    /// No undo record exists for the token (or it was already undone).
    #[error("no undo record exists for the token `{token}`")]
    UndoUnknown {
        /// The token nothing is recorded under.
        token: String,
    },

    /// The undo window has passed; the change stands.
    #[error("the undo window for `{token}` closed at {expired_at}; it is now {now}")]
    UndoExpired {
        /// The token.
        token: String,
        /// When the window closed (unix seconds).
        expired_at: i64,
        /// The caller's `now` (unix seconds).
        now: i64,
    },

    /// A later change touched an affected target, so undo would clobber it.
    #[error("undo `{token}` conflicts: `{target}` is `{actual}`, not the `{expected}` it applied")]
    UndoConflict {
        /// The token.
        token: String,
        /// The target whose state moved.
        target: String,
        /// The state the apply left.
        expected: String,
        /// The state now.
        actual: String,
    },

    /// The remedy declares its undo irreversible and refuses it.
    #[error("the remedy `{remedy}` cannot be undone: {reason}")]
    Irreversible {
        /// The remedy id.
        remedy: String,
        /// Why the change cannot be undone.
        reason: String,
    },

    /// Two remedies declare the same id.
    #[error("two remedies declare the id `{remedy}`")]
    Duplicate {
        /// The id declared twice.
        remedy: String,
    },

    /// A remedy id is not dotted lower-case.
    #[error("`{id}` is not a valid remedy id; a remedy id is dotted lower-case")]
    InvalidId {
        /// The invalid id.
        id: String,
    },

    /// A remedy's metadata breaks a rule.
    #[error("the metadata for `{remedy}` is invalid: {detail}")]
    MetaInvalid {
        /// The remedy id.
        remedy: String,
        /// What broke.
        detail: String,
    },

    /// The action catalogue the registry builds from the remedies did not load.
    #[error("the remedy action catalogue did not load: {detail}")]
    CatalogueInvalid {
        /// The parse or validation error.
        detail: String,
    },

    /// A corpus file did not parse or a case broke a rule.
    #[error("the corpus at `{at}` is invalid: {detail}")]
    CorpusInvalid {
        /// The file or case the problem came from.
        at: String,
        /// What was wrong.
        detail: String,
    },

    /// A typed operation over the storage/document/BPM ports failed.
    #[error("the operation `{operation}` failed: {detail}")]
    OpFailed {
        /// The operation that failed.
        operation: String,
        /// The underlying explanation.
        detail: String,
    },

    /// The context refused a direct edit of a submitted financial record (the hard floor).
    #[error(
        "the document `{document}` is a submitted financial record ({state}); it changes only \
         through cancel, amend or a journal entry"
    )]
    HardFloorFinancial {
        /// The document's reference.
        document: String,
        /// The document's state.
        state: String,
    },
}

impl RemedyError {
    /// The stable `remedies.*` code, for logs, API responses and the audit chain.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RemedyError::Unknown { .. } => codes::UNKNOWN,
            RemedyError::Refused { .. } => codes::REFUSED,
            RemedyError::ParamsInvalid { .. } => codes::PARAMS_INVALID,
            RemedyError::PreconditionFailed { .. } => codes::PRECONDITION_FAILED,
            RemedyError::PlanStale { .. } => codes::PLAN_STALE,
            RemedyError::UndoUnknown { .. } => codes::UNDO_UNKNOWN,
            RemedyError::UndoExpired { .. } => codes::UNDO_EXPIRED,
            RemedyError::UndoConflict { .. } => codes::UNDO_CONFLICT,
            RemedyError::Irreversible { .. } => codes::IRREVERSIBLE,
            RemedyError::Duplicate { .. } => codes::DUPLICATE,
            RemedyError::InvalidId { .. } => codes::INVALID_ID,
            RemedyError::MetaInvalid { .. } => codes::META_INVALID,
            RemedyError::CatalogueInvalid { .. } => codes::CATALOGUE_INVALID,
            RemedyError::CorpusInvalid { .. } => codes::CORPUS_INVALID,
            RemedyError::OpFailed { .. } => codes::OP_FAILED,
            RemedyError::HardFloorFinancial { .. } => codes::HARD_FLOOR_FINANCIAL,
        }
    }

    /// The plain-language sentence for the error.
    #[must_use]
    pub fn sentence(&self) -> String {
        self.to_string()
    }
}
