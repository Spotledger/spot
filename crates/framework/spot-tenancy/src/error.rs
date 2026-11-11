//! Typed errors with stable `tenancy.*` codes (catalogued in `spot-errors/catalogue/tenancy.toml`).
//!
//! No message carries a secret, a credential or raw SQL: ports report a `&'static str` reason,
//! never free text.

use crate::model::TenantState;

/// Stable error codes of this crate.
pub mod codes {
    /// The tenant name breaks the naming rules.
    pub const INVALID_NAME: &str = "tenancy.invalid_name";
    /// A live tenant already uses the name.
    pub const NAME_TAKEN: &str = "tenancy.name_taken";
    /// The name of a deleted tenant is still reserved.
    pub const NAME_RESERVED: &str = "tenancy.name_reserved";
    /// No such tenant.
    pub const NOT_FOUND: &str = "tenancy.not_found";
    /// The record changed since it was read.
    pub const VERSION_CONFLICT: &str = "tenancy.version_conflict";
    /// The state machine does not allow the transition.
    pub const INVALID_TRANSITION: &str = "tenancy.invalid_transition";
    /// The operation is not allowed in the tenant's current state.
    pub const WRONG_STATE: &str = "tenancy.wrong_state";
    /// No cluster satisfies the placement policy.
    pub const NO_PLACEMENT: &str = "tenancy.no_placement";
    /// A placement coordinate or identifier is malformed or already in use.
    pub const INVALID_PLACEMENT: &str = "tenancy.invalid_placement";
    /// A request field is malformed.
    pub const INVALID_REQUEST: &str = "tenancy.invalid_request";
    /// A provisioning or deprovisioning step failed.
    pub const STEP_FAILED: &str = "tenancy.step_failed";
    /// The workflow journal disagrees with the tenant's state.
    pub const JOURNAL_CORRUPT: &str = "tenancy.journal_corrupt";
    /// The two-person approval is missing or invalid.
    pub const APPROVAL_REQUIRED: &str = "tenancy.approval_required";
    /// The retention delay has not elapsed.
    pub const RETENTION_PENDING: &str = "tenancy.retention_pending";
    /// The final backup is missing or did not verify.
    pub const ARCHIVE_UNVERIFIED: &str = "tenancy.archive_unverified";
    /// A legal hold forbids the operation.
    pub const LEGAL_HOLD: &str = "tenancy.legal_hold";
    /// The audit trail could not record the event, so the operation did not proceed.
    pub const AUDIT_FAILED: &str = "tenancy.audit_failed";
    /// The control store failed.
    pub const STORE_FAILED: &str = "tenancy.store_failed";
    /// The placement's database already holds as many live tenants as it may.
    pub const CAPACITY_EXCEEDED: &str = "tenancy.capacity_exceeded";
}

/// Anything the control plane can refuse to do.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The tenant name breaks the naming rules.
    #[error("the tenant name is not valid: {reason}")]
    InvalidName {
        /// Which rule it breaks.
        reason: &'static str,
    },
    /// A live tenant already uses the name.
    #[error("the tenant name {name} is already in use")]
    NameTaken {
        /// The name.
        name: String,
    },
    /// The name of a deleted tenant is still reserved.
    #[error("the tenant name {name} is reserved until {until}")]
    NameReserved {
        /// The name.
        name: String,
        /// Unix milliseconds the reservation ends.
        until: u64,
    },
    /// No such tenant.
    #[error("tenant {tenant} was not found")]
    NotFound {
        /// The tenant id.
        tenant: String,
    },
    /// The record changed since it was read.
    #[error(
        "tenant {tenant} changed since it was read (expected version {expected}, found {found})"
    )]
    VersionConflict {
        /// The tenant id.
        tenant: String,
        /// The version the caller read.
        expected: u64,
        /// The version stored.
        found: u64,
    },
    /// The state machine does not allow the transition.
    #[error("tenant {tenant} cannot move from {from:?} to {to:?}")]
    InvalidTransition {
        /// The tenant id.
        tenant: String,
        /// Current state.
        from: TenantState,
        /// Requested state.
        to: TenantState,
    },
    /// The operation is not allowed in the tenant's current state.
    #[error("tenant {tenant} is {state:?}, which does not allow {operation}")]
    WrongState {
        /// The tenant id.
        tenant: String,
        /// Current state.
        state: TenantState,
        /// The refused operation.
        operation: &'static str,
    },
    /// No cluster satisfies the placement policy.
    #[error("no cluster in region {region} can take the tenant")]
    NoPlacement {
        /// The residency region that could not be satisfied.
        region: String,
    },
    /// A placement coordinate is malformed or already used by another tenant.
    #[error("the placement is not valid: {reason}")]
    InvalidPlacement {
        /// What is wrong.
        reason: &'static str,
    },
    /// A request field is malformed.
    #[error("the request is not valid: {reason}")]
    InvalidRequest {
        /// What is wrong.
        reason: &'static str,
    },
    /// A step failed; the journal records where, so the run can resume or roll back.
    #[error("step {step} of tenant {tenant} failed: {reason}")]
    StepFailed {
        /// The tenant id.
        tenant: String,
        /// The step name.
        step: &'static str,
        /// A fixed reason from the port, never free text.
        reason: &'static str,
    },
    /// The workflow journal disagrees with the tenant's state.
    #[error("the journal of tenant {tenant} is inconsistent: {reason}")]
    JournalCorrupt {
        /// The tenant id.
        tenant: String,
        /// What disagrees.
        reason: &'static str,
    },
    /// The two-person approval is missing or invalid.
    #[error("deleting tenant {tenant} needs two distinct approvers other than the requester")]
    ApprovalRequired {
        /// The tenant id.
        tenant: String,
    },
    /// The retention delay has not elapsed.
    #[error("tenant {tenant} is retained until {until}")]
    RetentionPending {
        /// The tenant id.
        tenant: String,
        /// Unix milliseconds the retention ends.
        until: u64,
    },
    /// The final backup is missing or did not verify.
    #[error("tenant {tenant} has no verified final backup")]
    ArchiveUnverified {
        /// The tenant id.
        tenant: String,
    },
    /// A legal hold forbids the operation.
    #[error("tenant {tenant} is under legal hold")]
    LegalHold {
        /// The tenant id.
        tenant: String,
    },
    /// The audit trail could not record the event.
    #[error("the audit trail could not record {action}; re-read the tenant before retrying")]
    AuditFailed {
        /// The action that was refused.
        action: &'static str,
    },
    /// The control store failed.
    #[error("the control store failed: {reason}")]
    StoreFailed {
        /// A fixed reason.
        reason: &'static str,
    },
    /// The placement's database already holds as many live tenants as it may hold.
    #[error("database {database} on cluster {cluster} is at capacity")]
    CapacityExceeded {
        /// The cluster.
        cluster: String,
        /// The database in it.
        database: String,
    },
}

impl Error {
    /// The stable dotted code of this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => codes::INVALID_NAME,
            Self::NameTaken { .. } => codes::NAME_TAKEN,
            Self::NameReserved { .. } => codes::NAME_RESERVED,
            Self::NotFound { .. } => codes::NOT_FOUND,
            Self::VersionConflict { .. } => codes::VERSION_CONFLICT,
            Self::InvalidTransition { .. } => codes::INVALID_TRANSITION,
            Self::WrongState { .. } => codes::WRONG_STATE,
            Self::NoPlacement { .. } => codes::NO_PLACEMENT,
            Self::InvalidPlacement { .. } => codes::INVALID_PLACEMENT,
            Self::InvalidRequest { .. } => codes::INVALID_REQUEST,
            Self::StepFailed { .. } => codes::STEP_FAILED,
            Self::JournalCorrupt { .. } => codes::JOURNAL_CORRUPT,
            Self::ApprovalRequired { .. } => codes::APPROVAL_REQUIRED,
            Self::RetentionPending { .. } => codes::RETENTION_PENDING,
            Self::ArchiveUnverified { .. } => codes::ARCHIVE_UNVERIFIED,
            Self::LegalHold { .. } => codes::LEGAL_HOLD,
            Self::AuditFailed { .. } => codes::AUDIT_FAILED,
            Self::StoreFailed { .. } => codes::STORE_FAILED,
            Self::CapacityExceeded { .. } => codes::CAPACITY_EXCEEDED,
        }
    }
}

/// The result type of this crate.
pub type Result<T> = std::result::Result<T, Error>;
