//! The executors get real defaults (WP-RTB-01.T13): a port a service provides wins, the site
//! state defaults to "this process is the site, so it serves", and a site that cannot build a
//! missing permission or audit port refuses to start (`mgmt_exec.unavailable`) instead of
//! leaving the journal and sync operations unregistered. No database: the permission and audit
//! defaults need the site's backend and are covered by the host's integration run.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, unused_imports)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use spot_cli::services::SiteCtx;
use spot_cli::services::mgmt_exec::{
    ADMIN_ROLE, BoxFut, Caller, ExecAudit, ExecRecord, ExecRegistry, Permissions, SiteState,
    register,
};
use spot_core::ManualClock;
use spot_mgmt_journal::args::{
    PolicySetting, Position, ReaderGrantArgs, ReaderRevokeArgs, RebuildMode, RebuildPlanArgs,
    RebuildRunArgs, RebuildTarget, SnapshotListArgs, SubjectEraseArgs,
};
use spot_mgmt_journal::results::*;
use spot_mgmt_journal::{ExecError as JErr, ExecKind as JKind, JournalOps};
use spot_mgmt_sync::args::{
    EnrolArgs, NodeArgs, NodeRole, PolicyShowArgs, QuarantineDiscardArgs, QuarantineRetryArgs,
    ResendArgs, RoleChangeArgs,
};
use spot_mgmt_sync::results::{
    BootstrapStatusResult, ConditionListResult, EnrolResult, NodeLifecycle, NodeListResult,
    NodeShowResult, NodeStateResult, NotifyContactResult, OneTimeCode, PolicyLintResult,
    PolicyShowResult, QuarantineDiscardResult, QuarantineListResult, QuarantineRetryResult,
    ResendResult, RoleChangeResult, RotateCertificateResult,
};
use spot_mgmt_sync::{ExecError as SErr, ExecKind as SKind, SyncOps};
use spot_sync_health::HealthView;
use time::macros::datetime;
use tokio::sync::Notify;

type Calls = Arc<Mutex<Vec<String>>>;

fn unsupported_j<T>() -> Result<T, JErr> {
    Err(JErr::new(JKind::Failed, "unsupported_in_this_test"))
}
fn unsupported_s<T>() -> Result<T, SErr> {
    Err(SErr::new(SKind::Failed, "unsupported_in_this_test"))
}

struct FakeJournal {
    site: &'static str,
    calls: Calls,
    plan_hash: &'static str,
}
#[async_trait]
impl JournalOps for FakeJournal {
    async fn status(&self) -> Result<StatusResult, JErr> {
        unsupported_j()
    }
    async fn recovery_report(&self) -> Result<RecoveryReportResult, JErr> {
        unsupported_j()
    }
    async fn snapshot_list(&self, _a: SnapshotListArgs) -> Result<SnapshotListResult, JErr> {
        unsupported_j()
    }
    async fn snapshot_take(&self) -> Result<SnapshotTakeResult, JErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:snapshot_take", self.site));
        Ok(SnapshotTakeResult {
            id: "snap-1".to_owned(),
            position: Position {
                generation: 1,
                seq: 10,
            },
        })
    }
    async fn verify(&self) -> Result<VerifyResult, JErr> {
        unsupported_j()
    }
    async fn audit_last(&self) -> Result<AuditResult, JErr> {
        unsupported_j()
    }
    async fn audit_run(&self) -> Result<AuditResult, JErr> {
        unsupported_j()
    }
    async fn policy_get(&self) -> Result<PolicyResult, JErr> {
        unsupported_j()
    }
    async fn policy_set(&self, _p: PolicySetting) -> Result<PolicyResult, JErr> {
        unsupported_j()
    }
    async fn reship(&self) -> Result<ReshipResult, JErr> {
        unsupported_j()
    }
    async fn rebuild_plan(&self, _a: RebuildPlanArgs) -> Result<RebuildPlanResult, JErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:rebuild_plan", self.site));
        Ok(RebuildPlanResult {
            plan_hash: self.plan_hash.to_owned(),
            mode: RebuildMode::Full,
            end: Position {
                generation: 1,
                seq: 100,
            },
            snapshot: None,
            segments: 3,
            records: 100,
        })
    }
    async fn rebuild_run(&self, a: RebuildRunArgs) -> Result<RebuildRunResult, JErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:rebuild_run", self.site));
        Ok(RebuildRunResult {
            plan_hash: a.confirm_plan_hash,
            records_applied: 100,
            end: Position {
                generation: 1,
                seq: 100,
            },
            verified: true,
        })
    }
    async fn reader_list(&self) -> Result<ReaderListResult, JErr> {
        unsupported_j()
    }
    async fn reader_grant(&self, a: ReaderGrantArgs) -> Result<ReaderGrantResult, JErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:reader_grant", self.site));
        Ok(ReaderGrantResult {
            grantee: a.grantee,
            expires_at: "2026-12-01T00:00:00Z".to_owned(),
        })
    }
    async fn reader_revoke(&self, _a: ReaderRevokeArgs) -> Result<ReaderRevokeResult, JErr> {
        unsupported_j()
    }
    async fn subject_erase(&self, _a: SubjectEraseArgs) -> Result<SubjectEraseResult, JErr> {
        unsupported_j()
    }
}

struct FakeSync {
    site: &'static str,
    calls: Calls,
    gate: Option<Arc<Notify>>,
}
#[async_trait]
impl SyncOps for FakeSync {
    async fn health(&self) -> Result<HealthView, SErr> {
        unsupported_s()
    }
    async fn node_list(&self) -> Result<NodeListResult, SErr> {
        unsupported_s()
    }
    async fn node_show(&self, _a: NodeArgs) -> Result<NodeShowResult, SErr> {
        unsupported_s()
    }
    async fn node_enrol(&self, a: EnrolArgs) -> Result<EnrolResult, SErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:node_enrol", self.site));
        Ok(EnrolResult {
            enrolment_id: "enrol-1".to_owned(),
            node: a.node,
            expires_at: "2026-11-14T00:00:00Z".to_owned(),
            code: Some(OneTimeCode::new("S3CRET-CODE".to_owned())),
        })
    }
    async fn node_suspend(&self, a: NodeArgs) -> Result<NodeStateResult, SErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:node_suspend:{}", self.site, a.node));
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }
        Ok(NodeStateResult {
            node: a.node,
            state: NodeLifecycle::Suspended,
        })
    }
    async fn node_resume(&self, _a: NodeArgs) -> Result<NodeStateResult, SErr> {
        unsupported_s()
    }
    async fn node_revoke(&self, a: NodeArgs) -> Result<NodeStateResult, SErr> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("{}:node_revoke:{}", self.site, a.node));
        Ok(NodeStateResult {
            node: a.node,
            state: NodeLifecycle::Revoked,
        })
    }
    async fn node_rotate_certificate(&self, _a: NodeArgs) -> Result<RotateCertificateResult, SErr> {
        unsupported_s()
    }
    async fn node_resend(&self, _a: ResendArgs) -> Result<ResendResult, SErr> {
        unsupported_s()
    }
    async fn node_notify_contact(&self, _a: NodeArgs) -> Result<NotifyContactResult, SErr> {
        unsupported_s()
    }
    async fn condition_list(&self) -> Result<ConditionListResult, SErr> {
        unsupported_s()
    }
    async fn quarantine_list(&self) -> Result<QuarantineListResult, SErr> {
        unsupported_s()
    }
    async fn quarantine_retry(
        &self,
        _a: QuarantineRetryArgs,
    ) -> Result<QuarantineRetryResult, SErr> {
        unsupported_s()
    }
    async fn quarantine_discard(
        &self,
        _a: QuarantineDiscardArgs,
    ) -> Result<QuarantineDiscardResult, SErr> {
        unsupported_s()
    }
    async fn policy_show(&self, _a: PolicyShowArgs) -> Result<PolicyShowResult, SErr> {
        unsupported_s()
    }
    async fn policy_lint(&self) -> Result<PolicyLintResult, SErr> {
        unsupported_s()
    }
    async fn role_change(&self, _a: RoleChangeArgs) -> Result<RoleChangeResult, SErr> {
        unsupported_s()
    }
    async fn bootstrap_status(&self) -> Result<BootstrapStatusResult, SErr> {
        unsupported_s()
    }
}

#[derive(Default)]
struct Perms(Mutex<HashMap<String, HashSet<String>>>);
impl Perms {
    fn give(&self, who: &str, what: &[&str]) {
        self.0.lock().unwrap().insert(
            who.to_owned(),
            what.iter().map(|s| (*s).to_owned()).collect(),
        );
    }
}
impl Permissions for Perms {
    fn holds<'a>(&'a self, principal: &'a str, permission: &'a str) -> BoxFut<'a, Option<bool>> {
        Box::pin(async move {
            Some(
                self.0
                    .lock()
                    .unwrap()
                    .get(principal)
                    .is_some_and(|set| set.contains(permission)),
            )
        })
    }
}

struct Running(AtomicBool);
impl SiteState for Running {
    fn running(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[derive(Default)]
struct Audit {
    records: Mutex<Vec<ExecRecord>>,
    broken: AtomicBool,
}
impl ExecAudit for Audit {
    fn record<'a>(&'a self, record: &'a ExecRecord) -> BoxFut<'a, Result<(), ()>> {
        Box::pin(async move {
            if self.broken.load(Ordering::SeqCst) {
                return Err(());
            }
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        })
    }
}

// ---- registration (no server, no database) ----------------------------------------------

fn ctx(settings: Option<&str>) -> SiteCtx<'static> {
    let mut table = BTreeMap::new();
    if let Some(text) = settings {
        table.insert("mgmt_exec".to_owned(), toml::from_str(text).unwrap());
    }
    let mut ctx = SiteCtx::new(
        "acme",
        Duration::from_secs(1),
        Arc::new(ManualClock::new(datetime!(2026-11-14 08:00:00 UTC))),
        table,
        true,
    );
    ctx.as_module("fake_services", 1000);
    ctx
}

fn hosted(ctx: &mut SiteCtx<'_>, calls: &Calls) {
    ctx.provide::<dyn JournalOps>(Arc::new(FakeJournal {
        site: "acme",
        calls: calls.clone(),
        plan_hash: "abc123",
    }))
    .unwrap();
    ctx.provide::<dyn SyncOps>(Arc::new(FakeSync {
        site: "acme",
        calls: calls.clone(),
        gate: None,
    }))
    .unwrap();
}

fn caller(principal: &str) -> Caller {
    Caller {
        principal: principal.to_owned(),
        command_id: "c1".to_owned(),
    }
}

fn run_args() -> RebuildRunArgs {
    RebuildRunArgs {
        plan: RebuildPlanArgs {
            target: RebuildTarget::Latest,
            mode: RebuildMode::Full,
        },
        confirm_plan_hash: "abc123".into(),
    }
}

#[tokio::test]
async fn provided_ports_win_over_the_defaults() {
    let calls: Calls = Arc::default();
    let perms = Arc::new(Perms::default());
    perms.give("alice", &["journal.admin"]);
    let running = Arc::new(Running(AtomicBool::new(false)));
    let audit = Arc::new(Audit::default());
    let mut ctx = ctx(None);
    hosted(&mut ctx, &calls);
    ctx.provide::<dyn Permissions>(perms).unwrap();
    ctx.provide::<dyn SiteState>(running).unwrap();
    ctx.provide::<dyn ExecAudit>(audit.clone()).unwrap();
    register(&mut ctx).unwrap();

    let registry = ctx.port::<ExecRegistry>().expect("the executors register");
    let (journal, _) = registry.executors("acme", caller("alice")).unwrap();
    // The stopped site of the overriding port lets the offline operation through; the default
    // site state ("running") would have refused it.
    let result = journal.rebuild_run(run_args()).await.unwrap();
    assert!(result.verified);
    assert_eq!(calls.lock().unwrap().last().unwrap(), "acme:rebuild_run");
    assert_eq!(audit.records.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn the_site_state_defaults_to_running_without_a_site_environment() {
    let calls: Calls = Arc::default();
    let perms = Arc::new(Perms::default());
    perms.give("alice", &["journal.admin", "sync.admin"]);
    let audit = Arc::new(Audit::default());
    let mut ctx = ctx(None);
    hosted(&mut ctx, &calls);
    ctx.provide::<dyn Permissions>(perms).unwrap();
    ctx.provide::<dyn ExecAudit>(audit.clone()).unwrap();
    register(&mut ctx).unwrap();

    let registry = ctx.port::<ExecRegistry>().expect("the executors register");
    let (journal, _) = registry.executors("acme", caller("alice")).unwrap();
    let error = journal.rebuild_run(run_args()).await.unwrap_err();
    assert_eq!(
        (error.kind, error.reason.as_str()),
        (JKind::Conflict, "site_running")
    );
    assert!(calls.lock().unwrap().is_empty());
    let record = audit.records.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (record.outcome, record.code.as_deref()),
        ("refused", Some("mgmt_exec.site_running"))
    );
}

#[tokio::test]
async fn a_refused_permission_writes_an_audit_record() {
    let calls: Calls = Arc::default();
    let audit = Arc::new(Audit::default());
    let mut ctx = ctx(None);
    hosted(&mut ctx, &calls);
    ctx.provide::<dyn Permissions>(Arc::new(Perms::default()))
        .unwrap();
    ctx.provide::<dyn ExecAudit>(audit.clone()).unwrap();
    register(&mut ctx).unwrap();

    let registry = ctx.port::<ExecRegistry>().unwrap();
    let (journal, _) = registry.executors("acme", caller("mallory")).unwrap();
    assert_eq!(
        journal.snapshot_take().await.unwrap_err().kind,
        JKind::Denied
    );
    assert!(calls.lock().unwrap().is_empty());
    let records = audit.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(
        (records[0].outcome, records[0].actor.as_str()),
        ("refused", "mallory")
    );
}

#[test]
fn a_site_that_cannot_build_a_missing_port_refuses_to_start() {
    // Nothing provided and no site environment: neither the permission nor the audit default can
    // be built.
    let calls: Calls = Arc::default();
    let mut bare = ctx(None);
    hosted(&mut bare, &calls);
    let error = register(&mut bare).unwrap_err();
    assert!(error.0.starts_with("mgmt_exec.unavailable"), "{error}");
    assert!(bare.port::<ExecRegistry>().is_none());

    // Permissions given, audit missing: still refused, and the message names the port.
    let mut half = ctx(None);
    hosted(&mut half, &calls);
    half.provide::<dyn Permissions>(Arc::new(Perms::default()))
        .unwrap();
    let error = register(&mut half).unwrap_err();
    assert!(error.0.contains("audit"), "{error}");
    assert!(half.port::<ExecRegistry>().is_none());
}

#[test]
fn nothing_registers_when_the_operations_are_not_hosted_or_the_module_is_disabled() {
    let mut empty = ctx(None);
    register(&mut empty).unwrap();
    assert!(empty.port::<ExecRegistry>().is_none());

    let calls: Calls = Arc::default();
    let mut off = ctx(Some("enabled = false"));
    hosted(&mut off, &calls);
    register(&mut off).unwrap();
    assert!(off.port::<ExecRegistry>().is_none());
}
