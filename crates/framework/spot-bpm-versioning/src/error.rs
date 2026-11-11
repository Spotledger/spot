//! Typed errors for `spot-bpm-versioning`: one enum with stable `bpm_versioning.*` codes.
//!
//! Callers match on [`VersioningError::code`], never on the message. The codes are this crate's
//! public vocabulary; every one has an entry in `spot-errors`' `bpm_versioning.toml`, all with
//! `class = "none"` because these errors never ride an [`AppError`](spot_app_api::AppError) and
//! are raised by a host-driven migration, batch or compensation path.

use spot_bpm_engine::EngineError;

/// Result of every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, VersioningError>;

/// One reason the publish guard refused a node: the node and the rule, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedNode {
    /// The offending node id.
    pub node: String,
    /// The stable rule code that refused it (a `bpm_documents.*` code).
    pub rule: String,
}

/// One problem while pinning, migrating or compensating a Case.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum VersioningError {
    /// A node the Case is still standing on has no mapping in the target version.
    #[error(
        "case `{case}` stands on node `{node}`, which the target version does not carry and the mapping does not name"
    )]
    UnmappedNode {
        /// The Case being migrated.
        case: String,
        /// The node with no mapping.
        node: String,
    },
    /// A mapping names a node the target version does not carry.
    #[error("the mapping names target node `{node}`, which the target version does not carry")]
    UnknownTargetNode {
        /// The target node.
        node: String,
    },
    /// Two source nodes map onto the same target node.
    #[error("source nodes `{first}` and `{second}` both map onto target node `{node}`")]
    MappingCollision {
        /// The target node.
        node: String,
        /// The first source node.
        first: String,
        /// The second source node.
        second: String,
    },
    /// The target version requires a variable the Case does not carry, with no default.
    #[error("the target version requires variable `{variable}`, which the Case does not carry")]
    VariableMissing {
        /// The missing variable path.
        variable: String,
    },
    /// The Case is not running, so it cannot be migrated.
    #[error("case `{case}` is {status}, not running")]
    CaseNotRunning {
        /// The Case id.
        case: String,
        /// Its run status, spelled as the model spells it.
        status: String,
    },
    /// The Case is not pinned to the version the caller names.
    #[error("case `{case}` is pinned to `{found}`, not `{expected}`")]
    VersionMismatch {
        /// The Case id.
        case: String,
        /// The version the caller named.
        expected: String,
        /// The version the Case actually pins.
        found: String,
    },
    /// The process registry does not hold the pinned definition, or its hash differs.
    #[error(
        "the pinned process `{key}` version {version} is missing from the registry or its hash differs"
    )]
    ProcessMismatch {
        /// The pinned process name.
        key: String,
        /// The pinned version number.
        version: u32,
    },
    /// The Case changed after the plan was made.
    #[error("case `{case}` moved from version {planned} to {current} after the plan was made")]
    StalePlan {
        /// The Case id.
        case: String,
        /// The Case's store version when the plan was made.
        planned: u64,
        /// The Case's store version now.
        current: u64,
    },
    /// A batch run with this identity already completed.
    #[error("batch run `{instance}` already completed; start a new scope instead")]
    BatchAlreadyCompleted {
        /// The identity key.
        instance: String,
    },
    /// A batch run with this identity must be resumed, not started again.
    #[error("batch run `{instance}` is {status}; resume it instead of starting a new run")]
    BatchRestartRequired {
        /// The identity key.
        instance: String,
        /// The run's status.
        status: String,
    },
    /// A batch run with this identity is already running.
    #[error("batch run `{instance}` is already running")]
    BatchInProgress {
        /// The identity key.
        instance: String,
    },
    /// No batch run has this id.
    #[error("no batch run has id `{id}`")]
    BatchUnknown {
        /// The batch run id.
        id: String,
    },
    /// The batch run is not in a state this operation may move.
    #[error("batch run `{id}` is {status}, so this operation does not apply")]
    BatchNotRunning {
        /// The batch run id.
        id: String,
        /// The run's status.
        status: String,
    },
    /// A bulk migration item has no plan.
    #[error("case `{case}` has no migration plan in this batch")]
    PlanMissing {
        /// The Case id.
        case: String,
    },
    /// A compensation step could not be raised or a Case could not be parked.
    #[error("compensation failed: {detail}")]
    Compensation {
        /// A plain-language reason.
        detail: String,
    },
    /// The publish guard refused the version; nothing was registered.
    #[error("the publish guard refused the version: {} violation(s)", .violations.len())]
    PublishRefused {
        /// Every violation, in node order: node id and rule only.
        violations: Vec<RefusedNode>,
    },
    /// The unguarded publish was used for a version that needs the guard.
    #[error(
        "node `{node}` names a service principal, so the version must be published through the guard"
    )]
    GuardRequired {
        /// The first Document node that names a principal.
        node: String,
    },
    /// The engine refused an operation.
    #[error("the engine refused the operation: {detail}")]
    Engine {
        /// The engine's plain-language message.
        detail: String,
    },
    /// A store or port failed.
    #[error("the store refused the operation: {detail}")]
    StoreFailed {
        /// A plain-language reason.
        detail: String,
    },
}

impl VersioningError {
    /// The stable `bpm_versioning.*` code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnmappedNode { .. } => "bpm_versioning.unmapped_node",
            Self::UnknownTargetNode { .. } => "bpm_versioning.unknown_target_node",
            Self::MappingCollision { .. } => "bpm_versioning.mapping_collision",
            Self::VariableMissing { .. } => "bpm_versioning.variable_missing",
            Self::CaseNotRunning { .. } => "bpm_versioning.case_not_running",
            Self::VersionMismatch { .. } => "bpm_versioning.version_mismatch",
            Self::ProcessMismatch { .. } => "bpm_versioning.process_mismatch",
            Self::StalePlan { .. } => "bpm_versioning.stale_plan",
            Self::BatchAlreadyCompleted { .. } => "bpm_versioning.batch_already_completed",
            Self::BatchRestartRequired { .. } => "bpm_versioning.batch_restart_required",
            Self::BatchInProgress { .. } => "bpm_versioning.batch_in_progress",
            Self::BatchUnknown { .. } => "bpm_versioning.batch_unknown",
            Self::BatchNotRunning { .. } => "bpm_versioning.batch_not_running",
            Self::PlanMissing { .. } => "bpm_versioning.plan_missing",
            Self::Compensation { .. } => "bpm_versioning.compensation_failed",
            Self::PublishRefused { .. } => "bpm_versioning.publish_refused",
            Self::GuardRequired { .. } => "bpm_versioning.guard_required",
            Self::Engine { .. } => "bpm_versioning.engine",
            Self::StoreFailed { .. } => "bpm_versioning.store_failed",
        }
    }
}

impl From<EngineError> for VersioningError {
    fn from(error: EngineError) -> Self {
        Self::Engine {
            detail: error.to_string(),
        }
    }
}
