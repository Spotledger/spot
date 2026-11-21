//! The typed executor: one async method per sync operation.
//!
//! The site host (WP-RTB-04.T3) implements [`SyncOps`] over the sync services; the management
//! API server and the CLI reach it through [`crate::dispatch`].
//!
//! # Contract for implementers
//!
//! - An implementation is **bound to one tenant and one authenticated caller** when it is
//!   created. No parameter of any operation names a tenant, so a caller cannot reach another
//!   tenant's nodes, items or policies through the arguments.
//! - Every method checks the caller's permission itself and **fails closed**: when it cannot
//!   decide, it returns [`crate::ExecKind::Denied`].
//! - Every method writes an audit record for the outcome, **including a refusal or a failure**
//!   (`sys_sync_node_admin`, `sys_sync_decision`); this crate has no audit port, so it cannot
//!   do it for the executor. For the destructive operations (`revoke`, `role.change`,
//!   `quarantine.discard`) the audit record is written before the action.
//! - `node_enrol` creates the one-time code; the audit record holds the enrolment id only.
//! - Results carry counts, positions, ids and reason codes only.

use async_trait::async_trait;
use spot_sync_health::HealthView;

use crate::args::{
    EnrolArgs, NodeArgs, PolicyShowArgs, QuarantineDiscardArgs, QuarantineRetryArgs, ResendArgs,
    RoleChangeArgs,
};
use crate::error::ExecError;
use crate::results::{
    BootstrapStatusResult, ConditionListResult, EnrolResult, NodeListResult, NodeShowResult,
    NodeStateResult, NotifyContactResult, PolicyLintResult, PolicyShowResult,
    QuarantineDiscardResult, QuarantineListResult, QuarantineRetryResult, ResendResult,
    RoleChangeResult, RotateCertificateResult,
};

/// The sync operations, one method each, with typed arguments and typed results.
#[async_trait]
pub trait SyncOps: Send + Sync {
    /// `sync.health`: the health view of `spot-sync-health`, unchanged (subjects and
    /// parameters already redacted by that crate).
    async fn health(&self) -> Result<HealthView, ExecError>;
    /// `sync.node.list`.
    async fn node_list(&self) -> Result<NodeListResult, ExecError>;
    /// `sync.node.show`.
    async fn node_show(&self, args: NodeArgs) -> Result<NodeShowResult, ExecError>;
    /// `sync.node.enrol`.
    async fn node_enrol(&self, args: EnrolArgs) -> Result<EnrolResult, ExecError>;
    /// `sync.node.suspend`.
    async fn node_suspend(&self, args: NodeArgs) -> Result<NodeStateResult, ExecError>;
    /// `sync.node.resume`.
    async fn node_resume(&self, args: NodeArgs) -> Result<NodeStateResult, ExecError>;
    /// `sync.node.revoke`: the dispatcher has checked that `confirm` equals the node.
    async fn node_revoke(&self, args: NodeArgs) -> Result<NodeStateResult, ExecError>;
    /// `sync.node.rotate_certificate`.
    async fn node_rotate_certificate(
        &self,
        args: NodeArgs,
    ) -> Result<RotateCertificateResult, ExecError>;
    /// `sync.node.resend`.
    async fn node_resend(&self, args: ResendArgs) -> Result<ResendResult, ExecError>;
    /// `sync.node.notify_contact`.
    async fn node_notify_contact(&self, args: NodeArgs) -> Result<NotifyContactResult, ExecError>;
    /// `sync.condition.list`.
    async fn condition_list(&self) -> Result<ConditionListResult, ExecError>;
    /// `sync.quarantine.list`.
    async fn quarantine_list(&self) -> Result<QuarantineListResult, ExecError>;
    /// `sync.quarantine.retry`.
    async fn quarantine_retry(
        &self,
        args: QuarantineRetryArgs,
    ) -> Result<QuarantineRetryResult, ExecError>;
    /// `sync.quarantine.discard`: the dispatcher has checked that `confirm` equals the item.
    async fn quarantine_discard(
        &self,
        args: QuarantineDiscardArgs,
    ) -> Result<QuarantineDiscardResult, ExecError>;
    /// `sync.policy.show`.
    async fn policy_show(&self, args: PolicyShowArgs) -> Result<PolicyShowResult, ExecError>;
    /// `sync.policy.lint`.
    async fn policy_lint(&self) -> Result<PolicyLintResult, ExecError>;
    /// `sync.role.change`: the dispatcher has checked that `confirm` equals the new role.
    async fn role_change(&self, args: RoleChangeArgs) -> Result<RoleChangeResult, ExecError>;
    /// `sync.bootstrap.status`.
    async fn bootstrap_status(&self) -> Result<BootstrapStatusResult, ExecError>;
}
