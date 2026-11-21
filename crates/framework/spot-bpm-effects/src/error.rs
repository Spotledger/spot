//! Typed errors for `spot-bpm-effects`: one enum with stable `bpm_effects.*` codes.
//!
//! Callers match on [`EffectsError::code`], never on the message. Every code has an entry in
//! `spot-errors`' `bpm_effects.toml`, all with `class = "none"` because these errors never ride
//! an `AppError`: they are raised on the host-driven path of a request's undo. No message holds
//! a field value, a secret or text a peer sent.

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, EffectsError>;

/// One problem while recording, rewinding or undoing a request.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EffectsError {
    /// An undo holds the request, so no forward step, rewind or second undo may run.
    #[error(
        "request `{request}` is held by an undo; a forward step, a rewind or another undo is refused"
    )]
    RequestLocked {
        /// The request.
        request: String,
    },
    /// The request has no record file yet, or the node is not in it.
    #[error("request `{request}` has no entry or visit for node `{node}`")]
    NodeUnknown {
        /// The request.
        request: String,
        /// The node asked for.
        node: String,
    },
    /// The request has no record file.
    #[error("request `{request}` has no record file")]
    RequestUnknown {
        /// The request.
        request: String,
    },
    /// A rewind would make the revised path create a record again.
    #[error(
        "rewinding request `{request}` to `{to}` would create again the record of node `{node}`; the process declares no revision mapping for it"
    )]
    RewindWouldDuplicate {
        /// The request.
        request: String,
        /// The rewind target.
        to: String,
        /// The node whose record would be created twice.
        node: String,
    },
    /// The plan was made for a file that has changed since.
    #[error(
        "the undo plan for request `{request}` is stale: the record file has changed since it was made"
    )]
    StalePlan {
        /// The request.
        request: String,
    },
    /// The dependency order of the entries has a cycle, so no safe order exists.
    #[error(
        "the entries of request `{request}` depend on one another in a cycle; no reversal order is safe"
    )]
    DependencyCycle {
        /// The request.
        request: String,
    },
    /// A bound was exceeded.
    #[error("request `{request}`: {what} exceeds the limit of {limit}")]
    LimitExceeded {
        /// The request.
        request: String,
        /// What is bounded.
        what: &'static str,
        /// The limit.
        limit: usize,
    },
    /// An entry is malformed (receipt and record disagree, or a field is out of shape).
    #[error("entry for request `{request}` is malformed: {what}")]
    EntryInvalid {
        /// The request.
        request: String,
        /// What is wrong.
        what: &'static str,
    },
    /// The lifecycle port could not be reached; the undo stays held and can be resumed.
    #[error(
        "the lifecycle port was unavailable ({code}); the undo of request `{request}` can be resumed"
    )]
    PortUnavailable {
        /// The request.
        request: String,
        /// A stable code the port reported.
        code: String,
    },
    /// The partner cancellation message could not be sent; the undo can be resumed.
    #[error(
        "the partner cancellation of request `{request}` failed ({code}); the undo can be resumed"
    )]
    CancellationFailed {
        /// The request.
        request: String,
        /// A stable code the port reported.
        code: String,
    },
    /// A decision was chosen that is not open, or the choice is not one of the offered ones.
    #[error("request `{request}`: no open decision accepts this choice")]
    NoSuchDecision {
        /// The request.
        request: String,
    },
    /// The business graph could not answer a dependants query.
    #[error("the business graph could not answer for request `{request}` ({code})")]
    GraphUnavailable {
        /// The request.
        request: String,
        /// A stable code the graph reported.
        code: String,
    },
    /// There is no undo in progress to act on.
    #[error("request `{request}` has no undo in progress")]
    NoUndo {
        /// The request.
        request: String,
    },
}

impl EffectsError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RequestLocked { .. } => codes::REQUEST_LOCKED,
            Self::NodeUnknown { .. } => codes::NODE_UNKNOWN,
            Self::RequestUnknown { .. } => codes::REQUEST_UNKNOWN,
            Self::RewindWouldDuplicate { .. } => codes::REWIND_WOULD_DUPLICATE,
            Self::StalePlan { .. } => codes::STALE_PLAN,
            Self::DependencyCycle { .. } => codes::DEPENDENCY_CYCLE,
            Self::LimitExceeded { .. } => codes::LIMIT_EXCEEDED,
            Self::EntryInvalid { .. } => codes::ENTRY_INVALID,
            Self::PortUnavailable { .. } => codes::PORT_UNAVAILABLE,
            Self::CancellationFailed { .. } => codes::CANCELLATION_FAILED,
            Self::NoSuchDecision { .. } => codes::NO_SUCH_DECISION,
            Self::GraphUnavailable { .. } => codes::GRAPH_UNAVAILABLE,
            Self::NoUndo { .. } => codes::NO_UNDO,
        }
    }
}

/// Every stable code this crate raises, and the two it reports inside a decision.
pub mod codes {
    /// An undo holds the request.
    pub const REQUEST_LOCKED: &str = "bpm_effects.request_locked";
    /// The node is not in the request's file.
    pub const NODE_UNKNOWN: &str = "bpm_effects.node_unknown";
    /// The request has no record file.
    pub const REQUEST_UNKNOWN: &str = "bpm_effects.request_unknown";
    /// A rewind would create a record twice.
    pub const REWIND_WOULD_DUPLICATE: &str = "bpm_effects.rewind_would_duplicate";
    /// The plan no longer matches the file.
    pub const STALE_PLAN: &str = "bpm_effects.stale_plan";
    /// The entries depend on one another in a cycle.
    pub const DEPENDENCY_CYCLE: &str = "bpm_effects.dependency_cycle";
    /// A bound was exceeded.
    pub const LIMIT_EXCEEDED: &str = "bpm_effects.limit_exceeded";
    /// An entry is malformed.
    pub const ENTRY_INVALID: &str = "bpm_effects.entry_invalid";
    /// The lifecycle port was unavailable.
    pub const PORT_UNAVAILABLE: &str = "bpm_effects.port_unavailable";
    /// The partner cancellation failed.
    pub const CANCELLATION_FAILED: &str = "bpm_effects.cancellation_failed";
    /// No open decision accepts the choice.
    pub const NO_SUCH_DECISION: &str = "bpm_effects.no_such_decision";
    /// The business graph was unavailable.
    pub const GRAPH_UNAVAILABLE: &str = "bpm_effects.graph_unavailable";
    /// No undo is in progress.
    pub const NO_UNDO: &str = "bpm_effects.no_undo";
    /// Blocker reason: the effect is a point of no return (reported in a decision).
    pub const POINT_OF_NO_RETURN: &str = "bpm_effects.point_of_no_return";
    /// Blocker reason: the lifecycle refused without a stable code of its own (reported in a
    /// decision).
    pub const REVERSAL_REFUSED: &str = "bpm_effects.reversal_refused";
}
