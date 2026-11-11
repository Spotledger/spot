//! The ports the control plane drives. Every `apply`/`undo` pair must be idempotent: the
//! workflow re-runs a step after a crash without knowing whether its effect landed.
//!
//! A port reports a fixed `&'static str` reason, never free text, so a failure can neither leak a
//! credential nor carry raw SQL into logs, errors or the audit trail. Credentials of the tenant
//! admin and break-glass users are created and delivered inside the IAM port; they never cross
//! this boundary.

use crate::model::{Tenant, TenantId};

/// Why a port call failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepFailure {
    /// Whether retrying the same call may succeed.
    pub retryable: bool,
    /// A fixed reason.
    pub reason: &'static str,
}

/// The result of a port call.
pub type PortResult<T = ()> = std::result::Result<T, StepFailure>;

/// Creates and drops the tenant schema (the `spot-pg-migrate` port).
pub trait SchemaPort {
    /// Creates the schema named by `tenant.placement`; succeeds if it already exists and is empty
    /// or was created for this same tenant.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn create(&self, tenant: &Tenant) -> PortResult;
    /// Drops the schema; succeeds if it is already gone.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn drop_schema(&self, tenant: &Tenant) -> PortResult;
}

/// Runs the baseline migrations and the seed (the `spot-cli` new-site steps).
pub trait SiteSeeder {
    /// Applies baseline migrations and seed data; re-running changes nothing.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn seed(&self, tenant: &Tenant) -> PortResult;
    /// Removes seeded data (the schema drop removes the rest).
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn unseed(&self, tenant: &Tenant) -> PortResult;
}

/// Creates the tenant admin and break-glass users (IAM ports).
pub trait IamPort {
    /// Creates the users; re-running does not rotate or duplicate them.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn create_users(&self, tenant: &Tenant) -> PortResult;
    /// Removes the users and their credentials.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn remove_users(&self, tenant: &Tenant) -> PortResult;
}

/// Issues and revokes the licence (the `spot-license` issuing port).
pub trait LicencePort {
    /// Issues the licence for the tenant's edition; re-running reuses the issued one.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn issue(&self, tenant: &Tenant) -> PortResult;
    /// Revokes the licence.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn revoke(&self, tenant: &Tenant) -> PortResult;
}

/// Registers the tenant with telemetry.
pub trait TelemetryPort {
    /// Registers the tenant.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn register(&self, tenant: &Tenant) -> PortResult;
    /// Deregisters the tenant.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn deregister(&self, tenant: &Tenant) -> PortResult;
}

/// The five provisioning ports together.
#[derive(Clone, Copy)]
pub struct Ports<'a> {
    /// Schema creation.
    pub schema: &'a dyn SchemaPort,
    /// Baseline migrations and seed.
    pub seeder: &'a dyn SiteSeeder,
    /// Admin and break-glass users.
    pub iam: &'a dyn IamPort,
    /// Licence issuing.
    pub licence: &'a dyn LicencePort,
    /// Telemetry registration.
    pub telemetry: &'a dyn TelemetryPort,
}

/// The final backup of a tenant (the `spot-snapshot` port).
pub trait Archiver {
    /// Takes a final backup and returns its reference.
    ///
    /// # Errors
    /// A [`StepFailure`].
    fn final_backup(&self, tenant: &Tenant) -> PortResult<String>;
    /// Verifies the backup (restorable and complete); `Ok(false)` means it did not verify.
    ///
    /// # Errors
    /// A [`StepFailure`] when verification could not be attempted.
    fn verify(&self, tenant: &Tenant, backup_ref: &str) -> PortResult<bool>;
}

/// The two-person approval hook. It answers for one tenant's deletion; the control plane decides
/// whether the answer is enough and treats any failure as no approval.
pub trait ApprovalHook {
    /// The principals who have approved deleting `tenant`.
    fn approvers(&self, tenant: &Tenant) -> Vec<String>;
}

/// What an audited operation came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// It succeeded.
    Success,
    /// It failed with this stable error code.
    Failure(&'static str),
}

/// One audit record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEvent {
    /// The tenant concerned.
    pub tenant: TenantId,
    /// What was attempted (a fixed word such as `provision_step` or `delete_begin`).
    pub action: &'static str,
    /// The step, when the action is a workflow step.
    pub step: Option<&'static str>,
    /// Who asked.
    pub actor: String,
    /// What came of it.
    pub outcome: Outcome,
    /// Unix milliseconds.
    pub at: u64,
}

/// The audit trail (an adapter over `spot-audit-chain`).
pub trait AuditSink {
    /// Records one event.
    ///
    /// # Errors
    /// A [`StepFailure`] when the event could not be made durable.
    fn record(&self, event: &AuditEvent) -> PortResult;
}
