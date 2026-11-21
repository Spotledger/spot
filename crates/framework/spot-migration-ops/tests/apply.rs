//! `apply` over a fake engine, the in-memory stores and a fake host: the plan-state lock, the
//! rehearsal rule, resume, failure and the maintenance gate (no database).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_migration_apply::{
    ApplyOutcome, ApplyRequest, ApplyService, ApplyState, BoxFuture, GateStatus,
    InMemoryAdmissionGate, NoFaults, RecordingHold, RecoveryAction, RecoveryOutcome, Version,
};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::apply::{ApplyHost, apply, maintenance_enter, maintenance_leave};
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanHash,
    PlanState, PlanStore, SharedAuditSink,
};
use spot_migration_plan::{
    ReleasePlan, SchemaPlan, SchemaRunKind, SchemaStep, SchemaStepKind, SystemDescription,
};
use spot_permissions::Principal;

#[derive(Clone, Copy)]
enum Behaviour {
    Open,
    Restore,
    Error,
}

struct FakeEngine {
    behaviour: Behaviour,
    applies: Arc<Mutex<u32>>,
    recovers: Arc<Mutex<u32>>,
}

impl ApplyService for FakeEngine {
    fn apply(
        &self,
        _request: ApplyRequest,
    ) -> BoxFuture<'_, spot_migration_apply::Result<ApplyOutcome>> {
        *self.applies.lock().unwrap() += 1;
        Box::pin(async move {
            match self.behaviour {
                Behaviour::Open => Ok(ApplyOutcome::Opened {
                    version: Version::New,
                    verified: true,
                }),
                Behaviour::Restore => Ok(ApplyOutcome::Restored {
                    cause: spot_migration_apply::RestoreCause::VerifyFailed,
                    mechanism: spot_migration_apply::RestoreMechanism::Snapshot,
                }),
                Behaviour::Error => Err(spot_migration_apply::Error::InProgress {
                    plan_digest: String::new(),
                }),
            }
        })
    }
    fn recover<'a>(
        &'a self,
        _plan: &'a ReleasePlan,
        _now: i64,
    ) -> BoxFuture<'a, spot_migration_apply::Result<RecoveryOutcome>> {
        *self.recovers.lock().unwrap() += 1;
        Box::pin(async move {
            Ok(RecoveryOutcome {
                action: RecoveryAction::Resumed { from_step: 2 },
                state: ApplyState::Open {
                    version: Version::New,
                    verified: true,
                },
            })
        })
    }
    fn state(&self) -> ApplyState {
        ApplyState::Idle
    }
}

struct FakeHost {
    behaviour: Behaviour,
    rehearsal: bool,
    applies: Arc<Mutex<u32>>,
    recovers: Arc<Mutex<u32>>,
}

impl ApplyHost for FakeHost {
    fn engine<'a>(
        &'a self,
        _plan: &'a ReleasePlan,
        _hash: &'a PlanHash,
        _now: i64,
    ) -> BoxFuture<'a, Result<Arc<dyn ApplyService>, LifecycleError>> {
        Box::pin(async move {
            Ok(Arc::new(FakeEngine {
                behaviour: self.behaviour,
                applies: Arc::clone(&self.applies),
                recovers: Arc::clone(&self.recovers),
            }) as Arc<dyn ApplyService>)
        })
    }
    fn rehearsal_required(&self) -> bool {
        self.rehearsal
    }
}

struct World {
    approvals: Arc<Approvals>,
    plans: Arc<InMemoryPlanStore>,
}

fn world() -> World {
    let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
    World {
        approvals: Arc::new(
            ApprovalService::new(
                store,
                SharedAuditSink(Arc::new(MemoryAuditSink::new())),
                ApprovalPolicy::default(),
            )
            .unwrap(),
        ),
        plans: Arc::new(InMemoryPlanStore::new()),
    }
}

fn ctx_for(world: &World, actor: Principal) -> Arc<LifecycleContext> {
    let ports = LifecyclePorts {
        clock: Arc::new(FixedClock::new(5_000)),
        approvals: Arc::clone(&world.approvals),
        runs: None,
        snapshots: None,
        gate: Arc::new(InMemoryAdmissionGate::new()),
        hold: Arc::new(RecordingHold::new()),
        faults: Arc::new(NoFaults),
        plans: world.plans.clone(),
        access: Arc::new(RoleAccess::new().granting("release_manager")),
    };
    Arc::new(LifecycleContext::new(
        SiteId::new("alpha").unwrap(),
        Actor::new(actor, Channel::Cli).unwrap(),
        ports,
    ))
}

fn manager(name: &str) -> Principal {
    Principal::with_roles(name, &["release_manager"])
}

/// An additive plan, auto-approved by the default policy.
fn approved_plan(world: &World) -> String {
    let inputs = PlanInputs::new(
        SystemDescription::default(),
        SystemDescription::default(),
        SchemaPlan {
            steps: vec![SchemaStep {
                key: "add:party.nickname".to_owned(),
                kind: SchemaStepKind::AddColumn,
                entity: "party".to_owned(),
                object: Some("nickname".to_owned()),
                needs_backfill: false,
                run: SchemaRunKind::Transaction,
                sentence: "add the nickname column".to_owned(),
            }],
            refusals: Vec::new(),
        },
    );
    let ctx = ctx_for(world, manager("alice"));
    let report = std::thread::spawn(move || plan(&ctx, &inputs).unwrap())
        .join()
        .unwrap();
    assert_eq!(report.state, PlanState::Approved);
    report.hash.as_str().to_owned()
}

type HostRig = (Arc<dyn ApplyHost>, Arc<Mutex<u32>>, Arc<Mutex<u32>>);

fn host(behaviour: Behaviour, rehearsal: bool) -> HostRig {
    let (applies, recovers) = (Arc::new(Mutex::new(0)), Arc::new(Mutex::new(0)));
    let host: Arc<dyn ApplyHost> = Arc::new(FakeHost {
        behaviour,
        rehearsal,
        applies: Arc::clone(&applies),
        recovers: Arc::clone(&recovers),
    });
    (host, applies, recovers)
}

fn state_of(world: &World, hash: &str) -> PlanState {
    world
        .plans
        .load(&PlanHash::parse(hash).unwrap())
        .unwrap()
        .unwrap()
        .state
}

fn force(world: &World, hash: &str, next: PlanState) {
    let hash = PlanHash::parse(hash).unwrap();
    let stored = world.plans.load(&hash).unwrap().unwrap();
    world
        .plans
        .mark_state(&hash, stored.state, stored.version, next)
        .unwrap();
}

#[tokio::test]
async fn an_approved_plan_is_applied_once_and_marked_applied() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, applies, _) = host(Behaviour::Open, false);
    let report = apply(&ctx_for(&w, manager("alice")), &host, &hash, false)
        .await
        .unwrap();
    assert_eq!(report.state, PlanState::Applied);
    assert_eq!(state_of(&w, &hash), PlanState::Applied);
    assert_eq!(*applies.lock().unwrap(), 1);
    // An applied plan is never applied again.
    let again = apply(&ctx_for(&w, manager("alice")), &host, &hash, false).await;
    assert_eq!(again, Err(LifecycleError::NotApproved));
    assert_eq!(*applies.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_restore_or_an_engine_error_marks_the_plan_failed() {
    for behaviour in [Behaviour::Restore, Behaviour::Error] {
        let w = world();
        let hash = approved_plan(&w);
        let (host, _, _) = host(behaviour, false);
        let refused = apply(&ctx_for(&w, manager("alice")), &host, &hash, false).await;
        assert_eq!(refused, Err(LifecycleError::ApplyFailed));
        assert_eq!(state_of(&w, &hash), PlanState::Failed);
    }
}

#[tokio::test]
async fn a_second_apply_while_one_runs_is_refused_as_busy() {
    let w = world();
    let hash = approved_plan(&w);
    force(&w, &hash, PlanState::Applying);
    let (host, applies, _) = host(Behaviour::Open, false);
    let refused = apply(&ctx_for(&w, manager("alice")), &host, &hash, false).await;
    assert_eq!(refused, Err(LifecycleError::SiteBusy));
    assert_eq!(*applies.lock().unwrap(), 0, "the engine is never built");
}

#[tokio::test]
async fn resume_continues_an_unfinished_apply_from_its_checkpoint() {
    let w = world();
    let hash = approved_plan(&w);
    force(&w, &hash, PlanState::Applying);
    let (host, applies, recovers) = host(Behaviour::Open, false);
    let report = apply(&ctx_for(&w, manager("alice")), &host, &hash, true)
        .await
        .unwrap();
    assert_eq!(report.state, PlanState::Applied);
    assert_eq!(
        (*applies.lock().unwrap(), *recovers.lock().unwrap()),
        (0, 1)
    );
    // A failed plan resumes too.
    let w = world();
    let hash = approved_plan(&w);
    force(&w, &hash, PlanState::Applying);
    force(&w, &hash, PlanState::Failed);
    let (second, _, recovers) = self::host(Behaviour::Open, false);
    apply(&ctx_for(&w, manager("alice")), &second, &hash, true)
        .await
        .unwrap();
    assert_eq!(*recovers.lock().unwrap(), 1);
}

#[tokio::test]
async fn a_plan_that_must_be_rehearsed_is_refused_until_it_is() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, applies, _) = host(Behaviour::Open, true);
    let refused = apply(&ctx_for(&w, manager("alice")), &host, &hash, false).await;
    assert_eq!(refused, Err(LifecycleError::NotRehearsed));
    assert_eq!(
        state_of(&w, &hash),
        PlanState::Approved,
        "nothing was locked"
    );
    force(&w, &hash, PlanState::Rehearsed);
    apply(&ctx_for(&w, manager("alice")), &host, &hash, false)
        .await
        .unwrap();
    assert_eq!(*applies.lock().unwrap(), 1);
}

#[tokio::test]
async fn an_unknown_or_undecided_plan_and_a_stranger_are_refused() {
    let w = world();
    let (host, applies, _) = host(Behaviour::Open, false);
    let unknown = apply(
        &ctx_for(&w, manager("alice")),
        &host,
        &"a".repeat(64),
        false,
    )
    .await;
    assert_eq!(unknown, Err(LifecycleError::UnknownPlan));
    let hash = approved_plan(&w);
    let stranger = apply(
        &ctx_for(&w, Principal::with_roles("mallory", &["viewer"])),
        &host,
        &hash,
        false,
    )
    .await;
    assert_eq!(stranger, Err(LifecycleError::NotPermitted));
    assert_eq!(*applies.lock().unwrap(), 0);
}

#[tokio::test]
async fn the_maintenance_gate_closes_and_opens() {
    let w = world();
    let ctx = ctx_for(&w, manager("alice"));
    maintenance_enter(&ctx, "upgrade").await.unwrap();
    assert!(matches!(ctx.gate().status(), GateStatus::Closed { .. }));
    maintenance_leave(&ctx).await.unwrap();
    assert_eq!(ctx.gate().status(), GateStatus::Open);
    let stranger = ctx_for(&w, Principal::with_roles("mallory", &["viewer"]));
    assert_eq!(
        maintenance_enter(&stranger, "x").await,
        Err(LifecycleError::NotPermitted)
    );
}
