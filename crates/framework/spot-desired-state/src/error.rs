//! Typed errors with stable `desired_state.*` codes.
//!
//! Every code raised here is catalogued in
//! `crates/framework/spot-errors/catalogue/desired_state.toml`; the repository's error-code
//! scan fails if one is missing and if a catalogued entry is never raised.
//!
//! Messages name the setting, the scope or the step and never echo a secret value or a raw
//! statement.

use thiserror::Error;

/// The stable codes of this crate, catalogued in `desired_state.toml`.
pub mod codes {
    /// Two desired-state layers describe the same scope and group.
    pub const DUPLICATE_LAYER: &str = "desired_state.duplicate_layer";
    /// A desired-state layer names no group.
    pub const EMPTY_GROUP: &str = "desired_state.empty_group";
    /// A setting key is not the dotted lower-case form the schema requires.
    pub const INVALID_KEY: &str = "desired_state.invalid_key";
    /// A desired setting is not in the registry.
    pub const UNKNOWN_SETTING: &str = "desired_state.unknown_setting";
    /// A desired setting is set at a scope its spec does not allow.
    pub const SCOPE_NOT_ALLOWED: &str = "desired_state.scope_not_allowed";
    /// A desired setting is read-only (observed, never set).
    pub const READ_ONLY: &str = "desired_state.read_only";
    /// A desired setting is a secret, and a desired-state document never carries one.
    pub const SECRET_SETTING: &str = "desired_state.secret_setting";
    /// A desired value fails the setting's own type check.
    pub const INVALID_VALUE: &str = "desired_state.invalid_value";
    /// A layer's scope identifier does not match its group.
    pub const GROUP_MISMATCH: &str = "desired_state.group_mismatch";
    /// A layer names a scope desired state does not cover (company or user).
    pub const UNSUPPORTED_SCOPE: &str = "desired_state.unsupported_scope";
    /// An operation id in the remediation operation set is not a catalogue id.
    pub const INVALID_OPERATION: &str = "desired_state.invalid_operation";
    /// A bulk plan has no step.
    pub const EMPTY_PLAN: &str = "desired_state.empty_plan";
    /// The plan has not been approved (pending or rejected).
    pub const NOT_APPROVED: &str = "desired_state.not_approved";
    /// The approval or rehearsal proof is for a different plan.
    pub const PLAN_CHANGED: &str = "desired_state.plan_changed";
    /// The rehearsal found a structural problem with the plan.
    pub const REHEARSAL_FAILED: &str = "desired_state.rehearsal_failed";
    /// An acknowledged-drift exemption has no reason, no actor or an expiry that is not later.
    pub const INVALID_EXEMPTION: &str = "desired_state.invalid_exemption";
    /// The wave/pause state machine refused or failed.
    pub const ROLLOUT: &str = "desired_state.rollout";
    /// A plan step names a server the plan's wave machinery does not know.
    pub const UNKNOWN_SERVER: &str = "desired_state.unknown_server";
}

/// What can go wrong while building, approving, rehearsing or running a desired state.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum Error {
    /// Two desired-state layers describe the same scope and identifier.
    #[error("{}: the scope {layer} is described twice", codes::DUPLICATE_LAYER)]
    DuplicateLayer {
        /// The canonical scope and identifier.
        layer: String,
    },
    /// A desired-state layer names no group.
    #[error("{}: a desired-state layer needs a group", codes::EMPTY_GROUP)]
    EmptyGroup,
    /// A setting key is not the dotted lower-case form the schema requires.
    #[error("{}: {key} is not a setting key: {reason}", codes::INVALID_KEY)]
    InvalidKey {
        /// The offending key.
        key: String,
        /// Why it was refused, without echoing anything else.
        reason: String,
    },
    /// A desired setting is not registered in the typed settings schema.
    #[error("{}: {key}", codes::UNKNOWN_SETTING)]
    UnknownSetting {
        /// The setting key.
        key: String,
    },
    /// A desired setting is set at a scope its spec does not allow.
    #[error("{}: {key} cannot be set at {scope}", codes::SCOPE_NOT_ALLOWED)]
    ScopeNotAllowed {
        /// The setting key.
        key: String,
        /// The scope that was attempted.
        scope: String,
    },
    /// A read-only setting was made part of a desired state.
    #[error("{}: {key} is observed, never set", codes::READ_ONLY)]
    ReadOnly {
        /// The setting key.
        key: String,
    },
    /// A secret setting was made part of a desired state.
    #[error(
        "{}: {key} is a secret and is never part of a desired state",
        codes::SECRET_SETTING
    )]
    SecretSetting {
        /// The setting key.
        key: String,
    },
    /// A desired value fails the setting's own type check.
    #[error("{}: {key}: {reason}", codes::INVALID_VALUE)]
    InvalidValue {
        /// The setting key.
        key: String,
        /// The first check failure, in the schema's own words.
        reason: String,
    },
    /// A layer's scope identifier does not match its group.
    #[error(
        "{}: group {group} is described by scope {scope}",
        codes::GROUP_MISMATCH
    )]
    GroupMismatch {
        /// The layer's group.
        group: String,
        /// The canonical scope identifier.
        scope: String,
    },
    /// A layer names a scope desired state does not cover (company or user).
    #[error("{}: {scope}", codes::UNSUPPORTED_SCOPE)]
    UnsupportedScope {
        /// The canonical scope identifier.
        scope: String,
    },
    /// An operation id in the remediation operation set is not a catalogue id.
    #[error("{}: {id}", codes::INVALID_OPERATION)]
    InvalidOperation {
        /// The refused operation id.
        id: String,
    },
    /// A bulk plan has no step.
    #[error("{}: the plan has no step", codes::EMPTY_PLAN)]
    EmptyPlan,
    /// The plan has not been approved.
    #[error("{}: {decision}", codes::NOT_APPROVED)]
    NotApproved {
        /// Whether the plan is pending or rejected, and by whom.
        decision: String,
    },
    /// The approval or rehearsal proof is for a different plan.
    #[error("{}: the proof does not match the plan", codes::PLAN_CHANGED)]
    PlanChanged,
    /// The rehearsal found a structural problem with the plan.
    #[error("{}: {problems} problem(s)", codes::REHEARSAL_FAILED)]
    RehearsalFailed {
        /// How many problems the rehearsal found.
        problems: usize,
    },
    /// An acknowledged-drift exemption is not usable.
    #[error("{}: {reason}", codes::INVALID_EXEMPTION)]
    InvalidExemption {
        /// Why the exemption was refused.
        reason: String,
    },
    /// The wave/pause state machine refused or failed.
    #[error("{}: {message}", codes::ROLLOUT)]
    Rollout {
        /// The state machine's own sentence.
        message: String,
    },
    /// A plan step names a server the plan's wave machinery does not know.
    #[error("{}: {server}", codes::UNKNOWN_SERVER)]
    UnknownServer {
        /// The server id.
        server: String,
    },
}

impl Error {
    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DuplicateLayer { .. } => codes::DUPLICATE_LAYER,
            Self::EmptyGroup => codes::EMPTY_GROUP,
            Self::InvalidKey { .. } => codes::INVALID_KEY,
            Self::UnknownSetting { .. } => codes::UNKNOWN_SETTING,
            Self::ScopeNotAllowed { .. } => codes::SCOPE_NOT_ALLOWED,
            Self::ReadOnly { .. } => codes::READ_ONLY,
            Self::SecretSetting { .. } => codes::SECRET_SETTING,
            Self::InvalidValue { .. } => codes::INVALID_VALUE,
            Self::GroupMismatch { .. } => codes::GROUP_MISMATCH,
            Self::UnsupportedScope { .. } => codes::UNSUPPORTED_SCOPE,
            Self::InvalidOperation { .. } => codes::INVALID_OPERATION,
            Self::EmptyPlan => codes::EMPTY_PLAN,
            Self::NotApproved { .. } => codes::NOT_APPROVED,
            Self::PlanChanged => codes::PLAN_CHANGED,
            Self::RehearsalFailed { .. } => codes::REHEARSAL_FAILED,
            Self::InvalidExemption { .. } => codes::INVALID_EXEMPTION,
            Self::Rollout { .. } => codes::ROLLOUT,
            Self::UnknownServer { .. } => codes::UNKNOWN_SERVER,
        }
    }
}

impl From<spot_fleet_rollout::Error> for Error {
    fn from(error: spot_fleet_rollout::Error) -> Self {
        Self::Rollout {
            message: error.to_string(),
        }
    }
}
