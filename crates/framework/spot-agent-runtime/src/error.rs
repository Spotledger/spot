//! The runtime module error vocabulary, with stable `agent_runtime.*` codes.
//!
//! Every failure a database or runtime operation reports is a [`RuntimeError`] carrying a
//! stable, machine-readable code ([`RuntimeError::code`]). The codes are catalogued in
//! `spot-errors/catalogue/agent_runtime.toml`; they are a library-internal vocabulary (the
//! host agent bridges them onto the wire result), so every catalogue entry is class `none`.
//!
//! Messages name the parameter, the operation or the service at fault and never a secret, a
//! raw SQL string or the text of a setting value.

use thiserror::Error;

/// The host's PostgreSQL build or service is not one this module can drive.
pub const UNSUPPORTED: &str = "agent_runtime.unsupported";
/// A required tool, file, service or port is missing on this host.
pub const UNAVAILABLE: &str = "agent_runtime.unavailable";
/// An argument failed the module's own validation, after the catalogue's schema check.
pub const INVALID_ARGUMENT: &str = "agent_runtime.invalid_argument";
/// The parameter is not on the typed PostgreSQL allow-list.
pub const UNKNOWN_PARAMETER: &str = "agent_runtime.unknown_parameter";
/// A parameter value is outside the allow-listed range or shape.
pub const VALUE_OUT_OF_RANGE: &str = "agent_runtime.value_out_of_range";
/// `pg_reload_conf()` (or the equivalent reload) failed.
pub const RELOAD_FAILED: &str = "agent_runtime.reload_failed";
/// Restarting PostgreSQL to apply a parameter failed.
pub const RESTART_FAILED: &str = "agent_runtime.restart_failed";
/// A promotion was refused because the primary is still reachable.
pub const PROMOTE_PRIMARY_REACHABLE: &str = "agent_runtime.promote_primary_reachable";
/// A node's role set is not a valid combination.
pub const ROLE_COMBINATION: &str = "agent_runtime.role_combination";
/// The named role is not one of the catalogue's roles.
pub const UNKNOWN_ROLE: &str = "agent_runtime.unknown_role";
/// The named profile is not one of the catalogue's profiles.
pub const UNKNOWN_PROFILE: &str = "agent_runtime.unknown_profile";
/// The settings diff could not be validated or written.
pub const SETTINGS: &str = "agent_runtime.settings";
/// The approval step rejected the release plan.
pub const UPGRADE_REJECTED: &str = "agent_runtime.upgrade_rejected";
/// The release plan was submitted but is not approved yet.
pub const UPGRADE_NOT_APPROVED: &str = "agent_runtime.upgrade_not_approved";
/// The apply engine failed before it could report a usable outcome.
pub const UPGRADE_FAILED: &str = "agent_runtime.upgrade_failed";
/// Verification failed and the engine restored the old version.
pub const UPGRADE_ROLLED_BACK: &str = "agent_runtime.upgrade_rolled_back";
/// A replica status, lag, standby or promotion step failed.
pub const REPLICA_FAILED: &str = "agent_runtime.replica_failed";
/// A backup could not be started or listed.
pub const BACKUP_FAILED: &str = "agent_runtime.backup_failed";
/// A backup's integrity check failed.
pub const BACKUP_VERIFY_FAILED: &str = "agent_runtime.backup_verify_failed";
/// The runtime did not drain its in-flight work within the window.
pub const DRAIN_TIMEOUT: &str = "agent_runtime.drain_timeout";
/// The runtime service could not be started or stopped.
pub const SERVICE_FAILED: &str = "agent_runtime.service_failed";

/// Why a database or runtime operation could not be planned or applied.
///
/// Every variant carries a stable code ([`RuntimeError::code`]) and a short, secret-free
/// description. `Debug` prints the same fields, so a dumped error never leaks a value.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum RuntimeError {
    /// The PostgreSQL build or service is unsupported (see [`UNSUPPORTED`]).
    #[error("{what} is not a supported PostgreSQL build or service")]
    Unsupported {
        /// What was not recognised.
        what: String,
    },
    /// A required tool, service or port is missing (see [`UNAVAILABLE`]).
    #[error("required resource is unavailable: {what}")]
    Unavailable {
        /// What is missing.
        what: String,
    },
    /// An argument failed the module's validation (see [`INVALID_ARGUMENT`]).
    #[error("{op}: parameter {param} is invalid: {detail}")]
    InvalidArgument {
        /// The operation id.
        op: String,
        /// The parameter name.
        param: String,
        /// Why it was refused, in words (never the value).
        detail: String,
    },
    /// A parameter is not on the allow-list (see [`UNKNOWN_PARAMETER`]).
    #[error("{name} is not an allow-listed PostgreSQL parameter")]
    UnknownParameter {
        /// The parameter name.
        name: String,
    },
    /// A value is outside the allow-listed shape (see [`VALUE_OUT_OF_RANGE`]).
    #[error("{name}: {detail}")]
    ValueOutOfRange {
        /// The parameter name.
        name: String,
        /// Why the value was refused, in words (never the value).
        detail: String,
    },
    /// The configuration reload failed (see [`RELOAD_FAILED`]).
    #[error("PostgreSQL configuration reload failed: {detail}")]
    ReloadFailed {
        /// A short, secret-free detail.
        detail: String,
    },
    /// Restarting PostgreSQL failed (see [`RESTART_FAILED`]).
    #[error("PostgreSQL restart failed: {detail}")]
    RestartFailed {
        /// A short, secret-free detail.
        detail: String,
    },
    /// A promotion was refused because the primary is reachable (see
    /// [`PROMOTE_PRIMARY_REACHABLE`]).
    #[error("refusing to promote: the primary is still reachable and the critical flag is absent")]
    PromotePrimaryReachable,
    /// A role combination is invalid (see [`ROLE_COMBINATION`]).
    #[error("invalid node role combination: {detail}")]
    RoleCombination {
        /// Why the combination is refused.
        detail: String,
    },
    /// The named role is unknown (see [`UNKNOWN_ROLE`]).
    #[error("{name} is not a Spotledger node role")]
    UnknownRole {
        /// The role name.
        name: String,
    },
    /// The named profile is unknown (see [`UNKNOWN_PROFILE`]).
    #[error("{name} is not a Spotledger runtime profile")]
    UnknownProfile {
        /// The profile name.
        name: String,
    },
    /// The settings diff failed (see [`SETTINGS`]).
    #[error("settings change failed: {detail}")]
    Settings {
        /// The settings error's own secret-free description.
        detail: String,
    },
    /// Approval rejected the plan (see [`UPGRADE_REJECTED`]).
    #[error("release plan {digest} was rejected by {by}")]
    UpgradeRejected {
        /// The plan digest, hex.
        digest: String,
        /// The rejecting approvers, comma separated.
        by: String,
    },
    /// The plan is not approved yet (see [`UPGRADE_NOT_APPROVED`]).
    #[error("release plan {digest} is not approved yet")]
    UpgradeNotApproved {
        /// The plan digest, hex.
        digest: String,
    },
    /// The apply engine failed (see [`UPGRADE_FAILED`]).
    #[error("upgrade apply failed: {detail}")]
    UpgradeFailed {
        /// A short, secret-free detail.
        detail: String,
    },
    /// Verification failed and the engine restored the old version (see
    /// [`UPGRADE_ROLLED_BACK`]).
    #[error("verification failed; the engine restored the old version via {mechanism} ({cause})")]
    UpgradeRolledBack {
        /// Why the run rolled back, as a stable label.
        cause: String,
        /// How the rollback was achieved, as a stable label.
        mechanism: String,
    },
    /// A replica operation failed (see [`REPLICA_FAILED`]).
    #[error("replica operation failed: {detail}")]
    ReplicaFailed {
        /// A short, secret-free detail.
        detail: String,
    },
    /// A backup operation failed (see [`BACKUP_FAILED`]).
    #[error("backup operation failed: {detail}")]
    BackupFailed {
        /// A short, secret-free detail.
        detail: String,
    },
    /// A backup failed its integrity check (see [`BACKUP_VERIFY_FAILED`]).
    #[error("backup {id} failed verification: {detail}")]
    BackupVerifyFailed {
        /// The backup id.
        id: String,
        /// A short, secret-free detail.
        detail: String,
    },
    /// The runtime did not drain within the window (see [`DRAIN_TIMEOUT`]).
    #[error("the runtime did not drain within {seconds}s")]
    DrainTimeout {
        /// The window that elapsed, in whole seconds.
        seconds: u64,
    },
    /// The runtime service failed to start or stop (see [`SERVICE_FAILED`]).
    #[error("runtime service {action} failed: {detail}")]
    ServiceFailed {
        /// `start` or `stop`.
        action: String,
        /// A short, secret-free detail.
        detail: String,
    },
}

impl RuntimeError {
    /// The stable, machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported { .. } => UNSUPPORTED,
            Self::Unavailable { .. } => UNAVAILABLE,
            Self::InvalidArgument { .. } => INVALID_ARGUMENT,
            Self::UnknownParameter { .. } => UNKNOWN_PARAMETER,
            Self::ValueOutOfRange { .. } => VALUE_OUT_OF_RANGE,
            Self::ReloadFailed { .. } => RELOAD_FAILED,
            Self::RestartFailed { .. } => RESTART_FAILED,
            Self::PromotePrimaryReachable => PROMOTE_PRIMARY_REACHABLE,
            Self::RoleCombination { .. } => ROLE_COMBINATION,
            Self::UnknownRole { .. } => UNKNOWN_ROLE,
            Self::UnknownProfile { .. } => UNKNOWN_PROFILE,
            Self::Settings { .. } => SETTINGS,
            Self::UpgradeRejected { .. } => UPGRADE_REJECTED,
            Self::UpgradeNotApproved { .. } => UPGRADE_NOT_APPROVED,
            Self::UpgradeFailed { .. } => UPGRADE_FAILED,
            Self::UpgradeRolledBack { .. } => UPGRADE_ROLLED_BACK,
            Self::ReplicaFailed { .. } => REPLICA_FAILED,
            Self::BackupFailed { .. } => BACKUP_FAILED,
            Self::BackupVerifyFailed { .. } => BACKUP_VERIFY_FAILED,
            Self::DrainTimeout { .. } => DRAIN_TIMEOUT,
            Self::ServiceFailed { .. } => SERVICE_FAILED,
        }
    }

    /// The short, secret-free detail a handler reports, prefixed with the code.
    #[must_use]
    pub fn detail(&self) -> String {
        format!("{}: {self}", self.code())
    }
}

/// The crate's result alias.
pub type RuntimeResult<T> = Result<T, RuntimeError>;
