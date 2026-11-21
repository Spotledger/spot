//! The typed executor: one async method per journal operation.
//!
//! The site host (WP-RTB-04.T3) implements [`JournalOps`] over the journal services; the
//! management API server and the CLI reach it through [`crate::dispatch`].
//!
//! # Contract for implementers
//!
//! - An implementation is **bound to one tenant and one authenticated caller** when it is
//!   created. No parameter of any operation names a tenant, so a caller cannot reach another
//!   tenant's journal through the arguments.
//! - Every method checks the caller's permission itself and **fails closed**: when it cannot
//!   decide, it returns [`crate::ExecKind::Denied`].
//! - Every method writes an audit record for the outcome, **including a refusal or a failure**
//!   (`sys_journal_admin`, `sys_journal_grant`); this crate has no audit port, so it cannot do
//!   it for the executor.
//! - Results carry counts, positions, ids and reason codes only.

use async_trait::async_trait;

use crate::args::{
    PolicySetting, ReaderGrantArgs, ReaderRevokeArgs, RebuildPlanArgs, RebuildRunArgs,
    SnapshotListArgs, SubjectEraseArgs,
};
use crate::error::ExecError;
use crate::results::{
    AuditResult, PolicyResult, ReaderGrantResult, ReaderListResult, ReaderRevokeResult,
    RebuildPlanResult, RebuildRunResult, RecoveryReportResult, ReshipResult, SnapshotListResult,
    SnapshotTakeResult, StatusResult, SubjectEraseResult, VerifyResult,
};

/// The journal operations, one method each, with typed arguments and typed results.
#[async_trait]
pub trait JournalOps: Send + Sync {
    /// `journal.status`.
    async fn status(&self) -> Result<StatusResult, ExecError>;
    /// `journal.recovery.report`.
    async fn recovery_report(&self) -> Result<RecoveryReportResult, ExecError>;
    /// `journal.snapshot.list`.
    async fn snapshot_list(&self, args: SnapshotListArgs) -> Result<SnapshotListResult, ExecError>;
    /// `journal.snapshot.take`.
    async fn snapshot_take(&self) -> Result<SnapshotTakeResult, ExecError>;
    /// `journal.verify`.
    async fn verify(&self) -> Result<VerifyResult, ExecError>;
    /// `journal.audit.last`.
    async fn audit_last(&self) -> Result<AuditResult, ExecError>;
    /// `journal.audit.run`.
    async fn audit_run(&self) -> Result<AuditResult, ExecError>;
    /// `journal.policy.get`.
    async fn policy_get(&self) -> Result<PolicyResult, ExecError>;
    /// `journal.policy.set`.
    async fn policy_set(&self, policy: PolicySetting) -> Result<PolicyResult, ExecError>;
    /// `journal.reship`.
    async fn reship(&self) -> Result<ReshipResult, ExecError>;
    /// `journal.rebuild.plan`.
    async fn rebuild_plan(&self, args: RebuildPlanArgs) -> Result<RebuildPlanResult, ExecError>;
    /// `journal.rebuild.run`: plans again and refuses with [`crate::ExecKind::Conflict`] when
    /// the plan's hash is not `args.confirm_plan_hash`, so a run executes only the plan a
    /// person has seen.
    async fn rebuild_run(&self, args: RebuildRunArgs) -> Result<RebuildRunResult, ExecError>;
    /// `journal.reader.list`.
    async fn reader_list(&self) -> Result<ReaderListResult, ExecError>;
    /// `journal.reader.grant`.
    async fn reader_grant(&self, args: ReaderGrantArgs) -> Result<ReaderGrantResult, ExecError>;
    /// `journal.reader.revoke`.
    async fn reader_revoke(&self, args: ReaderRevokeArgs) -> Result<ReaderRevokeResult, ExecError>;
    /// `journal.subject.erase`.
    async fn subject_erase(&self, args: SubjectEraseArgs) -> Result<SubjectEraseResult, ExecError>;
}
