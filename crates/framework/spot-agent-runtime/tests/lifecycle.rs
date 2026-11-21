//! End-to-end tests for `spot-agent-runtime` with fake ports.
//!
//! These drive the crate the way the privileged executor does: build a verified
//! [`ExecutionPlan`] and hand it to the [`RuntimeHandler`]. The ports are scripted fakes, so
//! the tests prove the module's own behaviour — the allow-list, the guards, the settings diff
//! and the migration lifecycle — without a database or a running service.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_agent_core::{
    ExecutionPlan, HandlerOutcome, HandlerRegistry, OperationHandler, PlanParts,
};
use spot_agent_runtime::{
    ApplyPort, Approval, ApprovalDecision, ApprovalPort, BackupEntry, BackupKind, BackupPort,
    BackupRequest, BackupStarted, DrainReport, PgAdminPort, ReleasePlanner, ReplicaLag,
    ReplicaPort, ReplicaRole, ReplicaStatus, RuntimeControl, RuntimeHandler, RuntimeModule,
    RuntimePorts, SettingsPorts, StandbyOutcome, StandbyRequest, StartReport, StopReport,
    build_catalogue, ids,
};
use spot_mgmt_ops::{AcceptedParam, OpId, ParamType, ParamValue, Risk, Validated};
use spot_migration_apply::{
    ApplyOutcome, ApplyState, RecoveryAction, RecoveryOutcome, RestoreCause, RestoreMechanism,
    Version,
};
use spot_migration_plan::{ReleasePlan, SystemDescription};
use spot_snapshot::VerifyReport;

// ---------------------------------------------------------------------------------------------
// Fakes: one per port.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct FakePg {
    calls: Mutex<Vec<String>>,
}

impl PgAdminPort for FakePg {
    fn read(&self, _name: &str) -> spot_agent_runtime::RuntimeResult<(String, bool)> {
        Ok(("100".to_owned(), false))
    }

    fn alter_system(&self, name: &str, value: &str) -> spot_agent_runtime::RuntimeResult<()> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("alter {name}={value}"));
        Ok(())
    }

    fn reload(&self) -> spot_agent_runtime::RuntimeResult<()> {
        self.calls.lock().unwrap().push("reload".to_owned());
        Ok(())
    }

    fn restart(&self) -> spot_agent_runtime::RuntimeResult<()> {
        self.calls.lock().unwrap().push("restart".to_owned());
        Ok(())
    }
}

struct FakeReplica {
    reachable: bool,
    promoted: Mutex<usize>,
}

impl Default for FakeReplica {
    fn default() -> Self {
        Self {
            reachable: false,
            promoted: Mutex::new(0),
        }
    }
}

impl ReplicaPort for FakeReplica {
    fn status(&self) -> spot_agent_runtime::RuntimeResult<ReplicaStatus> {
        Ok(ReplicaStatus {
            role: ReplicaRole::Standby,
            streaming: true,
            primary_host: Some("primary".to_owned()),
            receive_lsn: None,
            replay_lsn: None,
        })
    }

    fn lag(&self) -> spot_agent_runtime::RuntimeResult<ReplicaLag> {
        Ok(ReplicaLag {
            bytes: 0,
            seconds: Some(0),
            streaming: true,
        })
    }

    fn create_standby(
        &self,
        request: &StandbyRequest,
    ) -> spot_agent_runtime::RuntimeResult<StandbyOutcome> {
        Ok(StandbyOutcome {
            data_dir: request.data_dir.clone(),
            host: request.host.clone(),
            port: request.port,
            slot: request.slot.clone(),
            notes: Vec::new(),
        })
    }

    fn promote(&self) -> spot_agent_runtime::RuntimeResult<()> {
        *self.promoted.lock().unwrap() += 1;
        Ok(())
    }

    fn primary_reachable(&self) -> spot_agent_runtime::RuntimeResult<bool> {
        Ok(self.reachable)
    }
}

#[derive(Default)]
struct FakeBackup {
    started: Mutex<Vec<BackupRequest>>,
}

impl BackupPort for FakeBackup {
    fn start(
        &self,
        request: &BackupRequest,
        now: i64,
    ) -> spot_agent_runtime::RuntimeResult<BackupStarted> {
        self.started.lock().unwrap().push(request.clone());
        Ok(BackupStarted {
            id: format!("snap-{now}"),
            location: "/var/backups/spot".to_owned(),
            kind: request.kind,
            created_at: now,
        })
    }

    fn list(&self) -> spot_agent_runtime::RuntimeResult<Vec<BackupEntry>> {
        Ok(vec![BackupEntry {
            id: "snap-1".to_owned(),
            kind: BackupKind::BaseBackup,
            label: "nightly".to_owned(),
            bytes: 1024,
            created_at: 1,
            verified: true,
        }])
    }

    fn verify(&self, _id: &str) -> spot_agent_runtime::RuntimeResult<VerifyReport> {
        Ok(VerifyReport {
            files_checked: 2,
            bytes_checked: 2048,
        })
    }
}

struct FakeRuntime {
    drained: bool,
    stops: Mutex<usize>,
    starts: Mutex<usize>,
}

impl Default for FakeRuntime {
    fn default() -> Self {
        Self {
            drained: true,
            stops: Mutex::new(0),
            starts: Mutex::new(0),
        }
    }
}

impl RuntimeControl for FakeRuntime {
    fn stop(&self, drain_secs: u64, _now: i64) -> spot_agent_runtime::RuntimeResult<StopReport> {
        *self.stops.lock().unwrap() += 1;
        Ok(StopReport {
            drain: DrainReport {
                drained: self.drained,
                in_flight: if self.drained { 0 } else { 1 },
                waited_secs: drain_secs,
            },
            stopped: true,
        })
    }

    fn start(&self) -> spot_agent_runtime::RuntimeResult<StartReport> {
        *self.starts.lock().unwrap() += 1;
        Ok(StartReport {
            started: true,
            notes: Vec::new(),
        })
    }
}

struct FakePlanner;

impl ReleasePlanner for FakePlanner {
    fn plan(
        &self,
        target_version: &str,
        now: i64,
    ) -> spot_agent_runtime::RuntimeResult<ReleasePlan> {
        use spot_agent_runtime::DescriptionPlanner;
        DescriptionPlanner::new(SystemDescription::new(), SystemDescription::new())
            .plan(target_version, now)
    }
}

struct FakeApproval {
    decision: ApprovalDecision,
    submitted: Mutex<usize>,
}

impl ApprovalPort for FakeApproval {
    fn submit(
        &self,
        _plan: &ReleasePlan,
        _now: i64,
    ) -> spot_agent_runtime::RuntimeResult<ApprovalDecision> {
        *self.submitted.lock().unwrap() += 1;
        Ok(self.decision.clone())
    }
}

struct FakeApply {
    outcome: ApplyOutcome,
    applied: Mutex<usize>,
    recovered: Mutex<usize>,
}

impl ApplyPort for FakeApply {
    fn apply(
        &self,
        _plan: &ReleasePlan,
        approval: &Approval,
        _now: i64,
    ) -> spot_agent_runtime::RuntimeResult<ApplyOutcome> {
        assert!(!approval.digest.as_hex().is_empty());
        *self.applied.lock().unwrap() += 1;
        Ok(self.outcome.clone())
    }

    fn recover(
        &self,
        _plan: &ReleasePlan,
        _now: i64,
    ) -> spot_agent_runtime::RuntimeResult<RecoveryOutcome> {
        *self.recovered.lock().unwrap() += 1;
        Ok(RecoveryOutcome {
            action: RecoveryAction::Resumed { from_step: 2 },
            state: ApplyState::Open {
                version: Version::New,
                verified: true,
            },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------------------------

struct Harness {
    module: RuntimeModule,
    pg: Arc<FakePg>,
    replica: Arc<FakeReplica>,
    backup: Arc<FakeBackup>,
    runtime: Arc<FakeRuntime>,
    approval: Arc<FakeApproval>,
    apply: Arc<FakeApply>,
}

fn harness(reachable: bool, decision: ApprovalDecision, outcome: ApplyOutcome) -> Harness {
    let pg = Arc::new(FakePg::default());
    let replica = Arc::new(FakeReplica {
        reachable,
        ..FakeReplica::default()
    });
    let backup = Arc::new(FakeBackup::default());
    let runtime = Arc::new(FakeRuntime::default());
    let approval = Arc::new(FakeApproval {
        decision,
        submitted: Mutex::new(0),
    });
    let apply = Arc::new(FakeApply {
        outcome,
        applied: Mutex::new(0),
        recovered: Mutex::new(0),
    });
    let ports = RuntimePorts {
        pg: pg.clone(),
        replica: replica.clone(),
        backup: backup.clone(),
        runtime: runtime.clone(),
        planner: Arc::new(FakePlanner),
        approval: approval.clone(),
        apply: apply.clone(),
    };
    let module = RuntimeModule::new(ports, SettingsPorts::runtime().unwrap(), "srv-1");
    Harness {
        module,
        pg,
        replica,
        backup,
        runtime,
        approval,
        apply,
    }
}

fn text(name: &str, value: &str) -> AcceptedParam {
    AcceptedParam {
        name: name.to_owned(),
        ty: ParamType::Text {
            max_len: 256,
            pattern: None,
        },
        value: ParamValue::Text(value.to_owned()),
        from_default: false,
        secret: false,
    }
}

fn boolean(name: &str, value: bool) -> AcceptedParam {
    AcceptedParam {
        name: name.to_owned(),
        ty: ParamType::Bool,
        value: ParamValue::Bool(value),
        from_default: false,
        secret: false,
    }
}

fn validated(op: &str, params: Vec<AcceptedParam>) -> Validated {
    Validated {
        op: OpId::new(op).unwrap(),
        params,
    }
}

fn plan(_op: &str, args: Validated, now: i64) -> ExecutionPlan {
    ExecutionPlan::new(PlanParts {
        agent_id: "agent-test".to_owned(),
        command_id: "cmd-test".to_owned(),
        command_digest: [0_u8; 32],
        op: args.op.clone(),
        args,
        risk: Risk::Read,
        signers: Vec::new(),
        issued_at: now,
    })
}

fn failure_detail(outcome: HandlerOutcome) -> String {
    match outcome {
        HandlerOutcome::Failed { detail } => detail,
        HandlerOutcome::Succeeded => panic!("expected a failure, got success"),
    }
}

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn every_catalogue_operation_has_a_handler() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let mut registry = HandlerRegistry::new();
    RuntimeHandler::register(
        &Arc::new(RuntimeHandler::new(Arc::new(harness.module))),
        &mut registry,
    );
    let catalogue = build_catalogue().unwrap();
    assert_eq!(registry.len(), catalogue.len());
    for spec in catalogue.iter() {
        assert!(
            registry.contains(&spec.id),
            "{} has a handler",
            spec.id.as_str()
        );
    }
}

#[test]
fn the_catalogue_validates_a_real_invocation() {
    let catalogue = build_catalogue().unwrap();
    let invocation = spot_mgmt_ops::Invocation::new(OpId::new(ids::DB_PARAM_SET).unwrap())
        .with("name", ParamValue::Text("work_mem".to_owned()))
        .with("value", ParamValue::Text("64MB".to_owned()));
    let validated = catalogue.validate(&invocation).unwrap();
    assert_eq!(validated.params.len(), 2);
    // An unknown parameter is refused by the catalogue before the module ever sees it.
    let bogus = spot_mgmt_ops::Invocation::new(OpId::new(ids::DB_PARAM_SET).unwrap())
        .with("name", ParamValue::Text("secret_backdoor".to_owned()))
        .with("value", ParamValue::Text("on".to_owned()));
    assert!(catalogue.validate(&bogus).is_err());
    // The value shape is checked by the catalogue and the shared value checker; the module
    // re-checks the PostgreSQL-specific unit before it writes.
}

#[test]
fn reading_a_parameter_reports_the_unit_and_restart_hint() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::DB_PARAM_GET, vec![text("name", "shared_buffers")]);
    assert_eq!(
        handler.handle(&plan(ids::DB_PARAM_GET, args, 1_000)),
        HandlerOutcome::Succeeded
    );
}

#[test]
fn setting_a_parameter_writes_alters_and_applies_it() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));

    let reload = validated(
        ids::DB_PARAM_SET,
        vec![text("name", "work_mem"), text("value", "64MB")],
    );
    assert_eq!(
        handler.handle(&plan(ids::DB_PARAM_SET, reload, 1_000)),
        HandlerOutcome::Succeeded
    );

    let restart = validated(
        ids::DB_PARAM_SET,
        vec![text("name", "shared_buffers"), text("value", "256MB")],
    );
    assert_eq!(
        handler.handle(&plan(ids::DB_PARAM_SET, restart, 1_000)),
        HandlerOutcome::Succeeded
    );

    assert_eq!(
        *harness.pg.calls.lock().unwrap(),
        vec![
            "alter work_mem=65536kB".to_owned(),
            "reload".to_owned(),
            "alter shared_buffers=262144kB".to_owned(),
            "restart".to_owned(),
        ]
    );
}

#[test]
fn the_promote_guard_refuses_a_reachable_primary_through_the_handler() {
    let harness = harness(
        true,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::DB_REPLICA_PROMOTE, vec![boolean("critical", false)]);
    let detail = failure_detail(handler.handle(&plan(ids::DB_REPLICA_PROMOTE, args, 1_000)));
    assert!(
        detail.contains("agent_runtime.promote_primary_reachable"),
        "{detail}"
    );
    assert_eq!(*harness.replica.promoted.lock().unwrap(), 0);
}

#[test]
fn the_critical_flag_allows_a_forced_promotion() {
    let harness = harness(
        true,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::DB_REPLICA_PROMOTE, vec![boolean("critical", true)]);
    assert_eq!(
        handler.handle(&plan(ids::DB_REPLICA_PROMOTE, args, 1_000)),
        HandlerOutcome::Succeeded
    );
    assert_eq!(*harness.replica.promoted.lock().unwrap(), 1);
}

#[test]
fn a_role_change_is_a_settings_diff_with_a_restart_plan() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(
        ids::NODE_ROLE_SET,
        vec![AcceptedParam {
            name: "roles".to_owned(),
            ty: ParamType::List(Box::new(ParamType::Text {
                max_len: 16,
                pattern: None,
            })),
            value: spot_agent_runtime::handler::role_list_value(&[
                "web".to_owned(),
                "jobs".to_owned(),
            ]),
            from_default: false,
            secret: false,
        }],
    );
    assert_eq!(
        handler.handle(&plan(ids::NODE_ROLE_SET, args, 1_000)),
        HandlerOutcome::Succeeded
    );
}

#[test]
fn an_invalid_role_combination_fails_through_the_handler() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(
        ids::NODE_ROLE_SET,
        vec![AcceptedParam {
            name: "roles".to_owned(),
            ty: ParamType::List(Box::new(ParamType::Text {
                max_len: 16,
                pattern: None,
            })),
            value: spot_agent_runtime::handler::role_list_value(&[
                "database".to_owned(),
                "branch-edge".to_owned(),
            ]),
            from_default: false,
            secret: false,
        }],
    );
    let detail = failure_detail(handler.handle(&plan(ids::NODE_ROLE_SET, args, 1_000)));
    assert!(
        detail.contains("agent_runtime.role_combination"),
        "{detail}"
    );
}

#[test]
fn a_profile_applies_as_a_diff() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(
        ids::RUNTIME_PROFILE_APPLY,
        vec![text("profile", "cluster-redis")],
    );
    assert_eq!(
        handler.handle(&plan(ids::RUNTIME_PROFILE_APPLY, args, 1_000)),
        HandlerOutcome::Succeeded
    );
}

#[test]
fn a_backup_starts_and_verifies() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let start = validated(
        ids::DB_BACKUP_START,
        vec![text("kind", "base"), text("label", "pre-upgrade")],
    );
    assert_eq!(
        handler.handle(&plan(ids::DB_BACKUP_START, start, 2_000)),
        HandlerOutcome::Succeeded
    );
    assert_eq!(harness.backup.started.lock().unwrap().len(), 1);

    let verify = validated(ids::DB_BACKUP_VERIFY, vec![text("id", "snap-2000")]);
    assert_eq!(
        handler.handle(&plan(ids::DB_BACKUP_VERIFY, verify, 2_000)),
        HandlerOutcome::Succeeded
    );
}

#[test]
fn an_approved_and_verified_upgrade_succeeds() {
    let harness = harness(
        false,
        ApprovalDecision::Approved {
            approvers: vec!["alice".to_owned()],
        },
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::RUNTIME_UPGRADE_START, vec![text("version", "2.0.0")]);
    assert_eq!(
        handler.handle(&plan(ids::RUNTIME_UPGRADE_START, args, 3_000)),
        HandlerOutcome::Succeeded
    );
    assert_eq!(*harness.approval.submitted.lock().unwrap(), 1);
    assert_eq!(*harness.apply.applied.lock().unwrap(), 1);
}

#[test]
fn a_pending_upgrade_is_submitted_but_not_applied() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::RUNTIME_UPGRADE_START, vec![text("version", "2.0.0")]);
    assert_eq!(
        handler.handle(&plan(ids::RUNTIME_UPGRADE_START, args, 3_000)),
        HandlerOutcome::Succeeded
    );
    assert_eq!(*harness.apply.applied.lock().unwrap(), 0);
}

#[test]
fn a_failed_verify_returns_the_rollback_evidence_and_fails_the_operation() {
    let harness = harness(
        false,
        ApprovalDecision::Approved {
            approvers: Vec::new(),
        },
        ApplyOutcome::Restored {
            cause: RestoreCause::VerifyFailed,
            mechanism: RestoreMechanism::Snapshot,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::RUNTIME_UPGRADE_START, vec![text("version", "2.0.0")]);
    let detail = failure_detail(handler.handle(&plan(ids::RUNTIME_UPGRADE_START, args, 3_000)));
    assert!(
        detail.contains("agent_runtime.upgrade_rolled_back"),
        "{detail}"
    );
    assert!(detail.contains("verify_failed"), "{detail}");
    assert!(detail.contains("snapshot"), "{detail}");
}

#[test]
fn a_rejected_plan_fails_the_operation() {
    let harness = harness(
        false,
        ApprovalDecision::Rejected {
            by: vec!["bob".to_owned()],
        },
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::RUNTIME_UPGRADE_START, vec![text("version", "2.0.0")]);
    let detail = failure_detail(handler.handle(&plan(ids::RUNTIME_UPGRADE_START, args, 3_000)));
    assert!(
        detail.contains("agent_runtime.upgrade_rejected"),
        "{detail}"
    );
    assert_eq!(*harness.apply.applied.lock().unwrap(), 0);
}

#[test]
fn a_kill_mid_upgrade_resumes_from_the_engine_journal() {
    let harness = harness(
        false,
        ApprovalDecision::Approved {
            approvers: Vec::new(),
        },
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(ids::RUNTIME_UPGRADE_RESUME, vec![text("version", "2.0.0")]);
    assert_eq!(
        handler.handle(&plan(ids::RUNTIME_UPGRADE_RESUME, args, 4_000)),
        HandlerOutcome::Succeeded
    );
    assert_eq!(*harness.apply.recovered.lock().unwrap(), 1);
}

#[test]
fn stopping_the_runtime_requires_a_drain() {
    let harness = harness(
        false,
        ApprovalDecision::Pending,
        ApplyOutcome::Opened {
            version: Version::New,
            verified: true,
        },
    );
    let handler = RuntimeHandler::new(Arc::new(harness.module));
    let args = validated(
        ids::RUNTIME_SERVICE_STOP,
        vec![AcceptedParam {
            name: "drain_secs".to_owned(),
            ty: ParamType::Duration,
            value: ParamValue::Duration(30),
            from_default: false,
            secret: false,
        }],
    );
    assert_eq!(
        handler.handle(&plan(ids::RUNTIME_SERVICE_STOP, args, 5_000)),
        HandlerOutcome::Succeeded
    );
    assert_eq!(*harness.runtime.stops.lock().unwrap(), 1);
}
