//! Why an adapter failed, as a stable code and a port failure.
//!
//! Every variant is unit-like on purpose: a failure carries no free text, so no credential, no
//! licence key and no SQL statement can ever reach a log, an error or the audit trail. The codes
//! are catalogued in `spot-errors/catalogue/tenancy_adapters.toml`.

use spot_tenancy::ports::StepFailure;

/// Why an adapter refused or failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum AdapterError {
    /// The tenant schema could not be created or dropped.
    #[error("the tenant schema step failed")]
    Schema,
    /// The baseline migration or the seed could not be applied.
    #[error("the tenant baseline or seed step failed")]
    Seed,
    /// The final backup could not be taken.
    #[error("the tenant's final backup failed")]
    Backup,
    /// The backup reference is not this tenant's, or its manifest did not verify.
    #[error("the tenant's final backup did not verify")]
    BackupUnverified,
    /// The licence could not be minted or signed.
    #[error("the tenant licence could not be issued")]
    Licence,
    /// The licence store could not be read or written.
    #[error("the licence store failed")]
    LicenceStore,
    /// The approvers could not be resolved from the tenant's roles.
    #[error("the tenant's approvers could not be resolved")]
    Approval,
    /// The audit chain could not append the record.
    #[error("the audit trail could not append the record")]
    Audit,
    /// Telemetry registration is not configured or failed.
    #[error("the telemetry step failed")]
    Telemetry,
    /// The IAM user provisioning is not configured or failed.
    #[error("the user provisioning step failed")]
    Iam,
    /// The adapter has no implementation for this platform yet; it never succeeds.
    #[error("the adapter is not configured")]
    NotConfigured,
}

impl AdapterError {
    /// The stable dotted code of this failure.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Schema => "tenancy_adapters.schema_failed",
            Self::Seed => "tenancy_adapters.seed_failed",
            Self::Backup => "tenancy_adapters.backup_failed",
            Self::BackupUnverified => "tenancy_adapters.backup_unverified",
            Self::Licence => "tenancy_adapters.licence_failed",
            Self::LicenceStore => "tenancy_adapters.licence_store_failed",
            Self::Approval => "tenancy_adapters.approval_unavailable",
            Self::Audit => "tenancy_adapters.audit_failed",
            Self::Telemetry => "tenancy_adapters.telemetry_failed",
            Self::Iam => "tenancy_adapters.iam_failed",
            Self::NotConfigured => "tenancy_adapters.not_configured",
        }
    }

    /// The port failure a workflow step reports for this adapter error.
    ///
    /// `retryable` says whether replaying the same call may succeed; it is the caller's
    /// knowledge, because only it knows whether the cause was transient.
    #[must_use]
    pub fn failure(self, retryable: bool) -> StepFailure {
        StepFailure {
            retryable,
            reason: self.code(),
        }
    }
}
