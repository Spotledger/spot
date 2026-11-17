//! The journal and sync executors over fakes: permission per family, the site from the audience,
//! plan hashes, the site-stopped rule, audit for every outcome, audit-first for destructive
//! operations, one destructive operation at a time, idempotent retry, the one-time code and the
//! per-principal limiter. No database: the executors are wrappers; the services under them have
//! their own suites.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use spot_cli::services::mgmt_exec::{
    ADMIN_ROLE, BoxFut, Caller, ExecAudit, ExecRecord, ExecRegistry, Permissions, Settings, Shared,
    SiteExecutors, SiteState,
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

struct World {
    registry: ExecRegistry,
    perms: Arc<Perms>,
    running: Arc<Running>,
    audit: Arc<Audit>,
    calls: Calls,
    gate: Arc<Notify>,
    shared: Arc<Shared>,
}

fn world() -> World {
    let calls: Calls = Arc::default();
    let perms = Arc::new(Perms::default());
    let running = Arc::new(Running(AtomicBool::new(false)));
    let audit = Arc::new(Audit::default());
    let gate = Arc::new(Notify::new());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-13 08:00:00 UTC)));
    let shared = Shared::new(
        perms.clone(),
        running.clone(),
        audit.clone(),
        clock.clone(),
        Settings::default(),
    );
    let registry = ExecRegistry::new();
    registry.register(
        "acme",
        SiteExecutors {
            shared: Arc::clone(&shared),
            journal: Arc::new(FakeJournal {
                site: "acme",
                calls: calls.clone(),
                plan_hash: "abc123",
            }),
            sync: Arc::new(FakeSync {
                site: "acme",
                calls: calls.clone(),
                gate: Some(gate.clone()),
            }),
        },
    );
    registry.register(
        "beta",
        SiteExecutors {
            shared: Shared::new(
                perms.clone(),
                running.clone(),
                audit.clone(),
                clock,
                Settings::default(),
            ),
            journal: Arc::new(FakeJournal {
                site: "beta",
                calls: calls.clone(),
                plan_hash: "abc123",
            }),
            sync: Arc::new(FakeSync {
                site: "beta",
                calls: calls.clone(),
                gate: None,
            }),
        },
    );
    World {
        registry,
        perms,
        running,
        audit,
        calls,
        gate,
        shared,
    }
}

fn caller(principal: &str, command: &str) -> Caller {
    Caller {
        principal: principal.to_owned(),
        command_id: command.to_owned(),
    }
}

fn plan_args() -> RebuildPlanArgs {
    RebuildPlanArgs {
        target: RebuildTarget::Latest,
        mode: RebuildMode::Full,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_permitted_snapshot_reaches_the_service_and_is_audited() {
    let w = world();
    w.perms.give("alice", &["journal.admin"]);
    let (journal, _) = w.registry.executors("acme", caller("alice", "c1")).unwrap();
    let result = journal.snapshot_take().await.unwrap();
    assert_eq!(result.id, "snap-1");
    assert_eq!(w.calls.lock().unwrap().clone(), ["acme:snapshot_take"]);
    let records = w.audit.records.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(
        (
            records[0].action,
            records[0].outcome,
            records[0].actor.as_str()
        ),
        ("snapshot_taken", "ok", "alice")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_is_audited_and_the_service_is_not_called() {
    let w = world();
    w.perms.give("mallory", &["sync.admin"]); // the wrong family
    let (journal, sync) = w
        .registry
        .executors("acme", caller("mallory", "c1"))
        .unwrap();
    assert_eq!(
        journal.snapshot_take().await.unwrap_err().kind,
        JKind::Denied
    );
    // sync.admin cannot grant journal_reader (journal.admin is separate).
    assert_eq!(
        journal
            .reader_grant(ReaderGrantArgs {
                grantee: "bob".into(),
                days: 3,
                external: true
            })
            .await
            .unwrap_err()
            .kind,
        JKind::Denied
    );
    assert!(w.calls.lock().unwrap().is_empty());
    let records = w.audit.records.lock().unwrap().clone();
    assert_eq!(
        records.len(),
        1,
        "the snapshot refusal; the grant service audits its own grants"
    );
    assert_eq!(records[0].outcome, "refused");
    assert_eq!(records[0].code.as_deref(), Some("mgmt_exec.denied"));
    // sync.admin does reach the sync operations.
    sync.node_revoke(NodeArgs {
        node: "edge-1".into(),
    })
    .await
    .unwrap();
    assert_eq!(w.calls.lock().unwrap().clone(), ["acme:node_revoke:edge-1"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_site_is_the_audience_and_a_command_cannot_reach_another_site() {
    let w = world();
    w.perms.give("alice", &["journal.admin", "sync.admin"]);
    assert_eq!(
        w.registry
            .executors("gamma", caller("alice", "c1"))
            .err()
            .map(|e| e.kind),
        Some(JKind::NotFound)
    );
    let (_, sync) = w.registry.executors("acme", caller("alice", "c2")).unwrap();
    // Parameters naming beta's node ids reach acme's service only: the handle map is keyed by
    // the audience.
    sync.node_revoke(NodeArgs {
        node: "beta-node-7".into(),
    })
    .await
    .unwrap();
    assert_eq!(
        w.calls.lock().unwrap().clone(),
        ["acme:node_revoke:beta-node-7"]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_run_replans_and_refuses_a_plan_nobody_confirmed() {
    let w = world();
    w.perms.give("alice", &["journal.admin"]);
    let (journal, _) = w.registry.executors("acme", caller("alice", "c1")).unwrap();
    let wrong = RebuildRunArgs {
        plan: plan_args(),
        confirm_plan_hash: "deadbeef".into(),
    };
    assert_eq!(
        journal.rebuild_run(wrong).await.unwrap_err().kind,
        JKind::Conflict
    );
    assert_eq!(
        w.calls.lock().unwrap().clone(),
        ["acme:rebuild_plan"],
        "only the re-plan ran"
    );
    let (journal, _) = w.registry.executors("acme", caller("alice", "c2")).unwrap();
    let right = RebuildRunArgs {
        plan: plan_args(),
        confirm_plan_hash: "abc123".into(),
    };
    assert!(journal.rebuild_run(right).await.unwrap().verified);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_operations_are_refused_while_the_site_runs() {
    let w = world();
    w.perms.give("alice", &["journal.admin"]);
    w.running.0.store(true, Ordering::SeqCst);
    let (journal, _) = w.registry.executors("acme", caller("alice", "c1")).unwrap();
    let run = RebuildRunArgs {
        plan: plan_args(),
        confirm_plan_hash: "abc123".into(),
    };
    let error = journal.rebuild_run(run).await.unwrap_err();
    assert_eq!(
        (error.kind, error.reason.as_str()),
        (JKind::Conflict, "site_running")
    );
    assert!(w.calls.lock().unwrap().is_empty());
    let record = w.audit.records.lock().unwrap().last().cloned().unwrap();
    assert_eq!(
        (record.outcome, record.code.as_deref()),
        ("refused", Some("mgmt_exec.site_running"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_destructive_operation_does_nothing_when_its_audit_cannot_be_written() {
    let w = world();
    w.perms.give("alice", &["sync.admin", "journal.admin"]);
    w.audit.broken.store(true, Ordering::SeqCst);
    let (journal, sync) = w.registry.executors("acme", caller("alice", "c1")).unwrap();
    let error = sync
        .node_revoke(NodeArgs {
            node: "edge-1".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind, SKind::Unavailable);
    assert!(
        w.calls.lock().unwrap().is_empty(),
        "audit first: no record, no action"
    );
    // A non-destructive change is performed and then reported as unaudited, never as a quiet
    // success.
    let error = journal.snapshot_take().await.unwrap_err();
    assert_eq!(error.kind, JKind::Unavailable);
    assert_eq!(w.calls.lock().unwrap().clone(), ["acme:snapshot_take"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_same_command_id_answers_the_first_result_and_the_code_is_delivered_once() {
    let w = world();
    w.perms.give("alice", &["sync.node.enrol"]);
    let enrol = || EnrolArgs {
        node: "edge-9".into(),
        role: NodeRole::Edge,
        partitions: vec![],
        subscription: "all".into(),
        valid_hours: 24,
    };
    let (_, sync) = w
        .registry
        .executors("acme", caller("alice", "cmd-77"))
        .unwrap();
    let first = sync.node_enrol(enrol()).await.unwrap();
    assert!(first.code.is_some(), "the code leaves once");
    let (_, again) = w
        .registry
        .executors("acme", caller("alice", "cmd-77"))
        .unwrap();
    let retry = again.node_enrol(enrol()).await.unwrap();
    assert_eq!(retry.enrolment_id, "enrol-1");
    assert!(
        retry.code.is_none(),
        "the stored result never holds the code"
    );
    assert_eq!(w.calls.lock().unwrap().len(), 1, "the service ran once");
    // The audit records never carry it.
    let text = format!("{:?}", w.audit.records.lock().unwrap());
    assert!(!text.contains("S3CRET"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_destructive_operation_per_site_at_a_time() {
    let w = world();
    w.perms.give("alice", &["sync.admin"]);
    let (_, first) = w.registry.executors("acme", caller("alice", "c1")).unwrap();
    let (_, second) = w.registry.executors("acme", caller("alice", "c2")).unwrap();
    let gate = Arc::clone(&w.gate);
    let (a, b) = tokio::join!(
        first.node_suspend(NodeArgs {
            node: "edge-1".into()
        }),
        async {
            let refused = second
                .node_revoke(NodeArgs {
                    node: "edge-2".into(),
                })
                .await;
            gate.notify_one();
            refused
        }
    );
    assert!(a.is_ok());
    let refused = b.unwrap_err();
    assert_eq!(
        (refused.kind, refused.reason.as_str()),
        (SKind::Conflict, "busy")
    );
    // The lease is free again afterwards.
    let (_, third) = w.registry.executors("acme", caller("alice", "c3")).unwrap();
    assert!(
        third
            .node_revoke(NodeArgs {
                node: "edge-2".into()
            })
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_reader_grants_over_seven_days_need_the_administrator_role() {
    let w = world();
    w.perms.give("ops", &["journal.admin"]);
    w.perms.give("boss", &["journal.admin", ADMIN_ROLE]);
    let long = || ReaderGrantArgs {
        grantee: "auditor".into(),
        days: 30,
        external: true,
    };
    let (ops, _) = w.registry.executors("acme", caller("ops", "c1")).unwrap();
    assert_eq!(
        ops.reader_grant(long()).await.unwrap_err().kind,
        JKind::Denied
    );
    let (boss, _) = w.registry.executors("acme", caller("boss", "c2")).unwrap();
    assert!(boss.reader_grant(long()).await.is_ok());
    let (ops, _) = w.registry.executors("acme", caller("ops", "c3")).unwrap();
    let short = ReaderGrantArgs {
        grantee: "auditor".into(),
        days: 7,
        external: true,
    };
    assert!(ops.reader_grant(short).await.is_ok());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flood_of_commands_is_limited_per_principal_and_counted_not_audited() {
    let w = world();
    w.perms.give("noisy", &[]);
    let mut denied = 0;
    let mut limited = 0;
    for i in 0..40 {
        let (journal, _) = w
            .registry
            .executors("acme", caller("noisy", &format!("c{i}")))
            .unwrap();
        let error = journal.snapshot_take().await.unwrap_err();
        match error.kind {
            JKind::Denied => denied += 1,
            JKind::Conflict => limited += 1,
            other => panic!("{other:?}"),
        }
    }
    assert_eq!((denied, limited), (30, 10));
    assert_eq!(w.shared.rate_limited(), 10);
    // Thirty refusals were audited; the limiter's ten were counted only.
    assert_eq!(w.audit.records.lock().unwrap().len(), 30);
    // Another principal is not limited by it.
    w.perms.give("calm", &["journal.admin"]);
    let (journal, _) = w.registry.executors("acme", caller("calm", "x")).unwrap();
    assert!(journal.snapshot_take().await.is_ok());
}
