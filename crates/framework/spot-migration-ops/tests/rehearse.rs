//! `rehearse` over a fake engine, the in-memory stores and a fake host: what is stored, when the
//! plan becomes `rehearsed`, the disk guard, the budget and the leftovers (no database).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::SiteId;
use spot_migration_apply::{BoxFuture, InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApplyTicket, ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore,
    MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_ops::rehearse::{
    RehearsalHost, RehearsalRunner, RunResult, VERDICT_ERROR, VERDICT_FAIL, VERDICT_PASS,
    VERDICT_TIMEOUT, clean, rehearse,
};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanHash,
    PlanState, PlanStore, RehearsalLedger, RehearsalRecord, SharedAuditSink,
};
use spot_migration_plan::{
    ReleasePlan, SchemaPlan, SchemaRunKind, SchemaStep, SchemaStepKind, SystemDescription,
};
use spot_permissions::Principal;

#[derive(Default)]
struct MemoryLedger(Mutex<Vec<RehearsalRecord>>);

impl RehearsalLedger for MemoryLedger {
    fn record(&self, record: RehearsalRecord) -> Result<(), LifecycleError> {
        self.0.lock().unwrap().push(record);
        Ok(())
    }
    fn latest(&self, hash: &PlanHash) -> Result<Option<RehearsalRecord>, LifecycleError> {
        Ok(self.history(hash)?.pop())
    }
    fn history(&self, hash: &PlanHash) -> Result<Vec<RehearsalRecord>, LifecycleError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|r| &r.plan_hash == hash)
            .cloned()
            .collect())
    }
}

enum Behaviour {
    Finish(RunResult),
    Error,
    Hang,
}

struct FakeRunner {
    behaviour: Behaviour,
    runs: Arc<Mutex<u32>>,
}

impl RehearsalRunner for FakeRunner {
    fn run<'a>(
        &'a self,
        _plan: &'a ReleasePlan,
        _ticket: &'a ApplyTicket,
        _site: &'a SiteId,
    ) -> BoxFuture<'a, Result<RunResult, LifecycleError>> {
        *self.runs.lock().unwrap() += 1;
        Box::pin(async move {
            match &self.behaviour {
                Behaviour::Finish(run) => Ok(run.clone()),
                Behaviour::Error => Err(LifecycleError::RehearsalFailed),
                Behaviour::Hang => {
                    std::future::pending::<()>().await;
                    Err(LifecycleError::RehearsalFailed)
                }
            }
        })
    }
}

struct FakeHost {
    free: u64,
    size: u64,
    budget: Duration,
    make: Box<dyn Fn() -> Behaviour + Send + Sync>,
    runs: Arc<Mutex<u32>>,
    ledger: Arc<MemoryLedger>,
    leftovers: Vec<String>,
}

impl RehearsalHost for FakeHost {
    fn runner(
        &self,
        _plan: &ReleasePlan,
        _hash: &PlanHash,
        _now: i64,
    ) -> Result<Arc<dyn RehearsalRunner>, LifecycleError> {
        Ok(Arc::new(FakeRunner {
            behaviour: (self.make)(),
            runs: Arc::clone(&self.runs),
        }))
    }
    fn free_bytes(&self) -> BoxFuture<'_, Result<u64, LifecycleError>> {
        Box::pin(async move { Ok(self.free) })
    }
    fn snapshot_bytes(&self) -> BoxFuture<'_, Result<u64, LifecycleError>> {
        Box::pin(async move { Ok(self.size) })
    }
    fn budget(&self) -> Duration {
        self.budget
    }
    fn ledger(&self) -> Arc<dyn RehearsalLedger> {
        self.ledger.clone()
    }
    fn clean_leftovers(&self) -> BoxFuture<'_, Result<Vec<String>, LifecycleError>> {
        Box::pin(async move { Ok(self.leftovers.clone()) })
    }
}

fn run_result(passed: bool) -> RunResult {
    RunResult {
        passed,
        outcome: if passed { "clean" } else { "invariants_failed" }.to_owned(),
        report_json: "{\"counts\":1}".to_owned(),
        text: "rehearsal report".to_owned(),
        cleaned: true,
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

fn host(
    behaviour: impl Fn() -> Behaviour + Send + Sync + 'static,
) -> (Arc<dyn RehearsalHost>, Arc<MemoryLedger>, Arc<Mutex<u32>>) {
    let ledger = Arc::new(MemoryLedger::default());
    let runs = Arc::new(Mutex::new(0));
    let host: Arc<dyn RehearsalHost> = Arc::new(FakeHost {
        free: 1_000,
        size: 100,
        budget: Duration::from_millis(200),
        make: Box::new(behaviour),
        runs: Arc::clone(&runs),
        ledger: Arc::clone(&ledger),
        leftovers: vec!["spot_rehearsal_abc".to_owned()],
    });
    (host, ledger, runs)
}

fn state_of(world: &World, hash: &str) -> PlanState {
    world
        .plans
        .load(&PlanHash::parse(hash).unwrap())
        .unwrap()
        .unwrap()
        .state
}

#[tokio::test]
async fn a_passing_rehearsal_marks_the_plan_rehearsed_and_stores_the_report() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, ledger, runs) = host(|| Behaviour::Finish(run_result(true)));
    let report = rehearse(&ctx_for(&w, manager("alice")), &host, &hash)
        .await
        .unwrap();
    assert!(report.passed);
    assert!(report.cleaned);
    assert_eq!(report.state, PlanState::Rehearsed);
    assert_eq!(state_of(&w, &hash), PlanState::Rehearsed);
    assert_eq!(*runs.lock().unwrap(), 1);
    let stored = ledger
        .latest(&PlanHash::parse(&hash).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.verdict, VERDICT_PASS);
    assert_eq!(stored.at, 5_000);
    assert_eq!(stored.report_json, "{\"counts\":1}");
}

#[tokio::test]
async fn a_failing_rehearsal_leaves_the_plan_approved_and_stores_the_report() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, ledger, _) = host(|| Behaviour::Finish(run_result(false)));
    let report = rehearse(&ctx_for(&w, manager("alice")), &host, &hash)
        .await
        .unwrap();
    assert!(!report.passed);
    assert_eq!(report.outcome, "invariants_failed");
    assert_eq!(report.state, PlanState::Approved);
    assert_eq!(state_of(&w, &hash), PlanState::Approved);
    let stored = ledger
        .latest(&PlanHash::parse(&hash).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.verdict, VERDICT_FAIL);
}

#[tokio::test]
async fn an_engine_error_is_recorded_and_refused() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, ledger, _) = host(|| Behaviour::Error);
    let refused = rehearse(&ctx_for(&w, manager("alice")), &host, &hash).await;
    assert_eq!(refused, Err(LifecycleError::RehearsalFailed));
    assert_eq!(state_of(&w, &hash), PlanState::Approved);
    let stored = ledger
        .latest(&PlanHash::parse(&hash).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.verdict, VERDICT_ERROR);
}

#[tokio::test]
async fn a_rehearsal_that_overruns_its_budget_is_aborted_and_recorded() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, ledger, _) = host(|| Behaviour::Hang);
    let refused = rehearse(&ctx_for(&w, manager("alice")), &host, &hash).await;
    assert_eq!(refused, Err(LifecycleError::RehearsalFailed));
    assert_eq!(state_of(&w, &hash), PlanState::Approved);
    let stored = ledger
        .latest(&PlanHash::parse(&hash).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.verdict, VERDICT_TIMEOUT);
}

#[tokio::test]
async fn insufficient_space_is_refused_before_anything_starts() {
    let w = world();
    let hash = approved_plan(&w);
    let ledger = Arc::new(MemoryLedger::default());
    let runs = Arc::new(Mutex::new(0));
    // 2.5 times 100 is 250; 249 is not enough, 250 is.
    for (free, ok) in [(249u64, false), (250, true)] {
        let host: Arc<dyn RehearsalHost> = Arc::new(FakeHost {
            free,
            size: 100,
            budget: Duration::from_millis(200),
            make: Box::new(|| Behaviour::Finish(run_result(true))),
            runs: Arc::clone(&runs),
            ledger: Arc::clone(&ledger),
            leftovers: Vec::new(),
        });
        let result = rehearse(&ctx_for(&w, manager("alice")), &host, &hash).await;
        if ok {
            assert!(result.unwrap().passed);
        } else {
            assert_eq!(result, Err(LifecycleError::InsufficientSpace));
            assert_eq!(*runs.lock().unwrap(), 0, "nothing was started");
            assert!(ledger.0.lock().unwrap().is_empty());
        }
    }
}

#[tokio::test]
async fn an_unknown_unapproved_or_unprivileged_request_is_refused() {
    let w = world();
    let hash = approved_plan(&w);
    let (host, _, runs) = host(|| Behaviour::Finish(run_result(true)));
    let alice = ctx_for(&w, manager("alice"));
    assert_eq!(
        rehearse(&alice, &host, &"f".repeat(64)).await,
        Err(LifecycleError::UnknownPlan)
    );
    assert_eq!(
        rehearse(
            &ctx_for(&w, Principal::with_roles("eve", &["Clerk"])),
            &host,
            &hash
        )
        .await,
        Err(LifecycleError::NotPermitted)
    );
    // A proposed plan (moved back by hand here) is not approved.
    let key = PlanHash::parse(&hash).unwrap();
    w.plans
        .mark_state(&key, PlanState::Approved, 2, PlanState::Superseded)
        .ok();
    let version = w.plans.load(&key).unwrap().unwrap().version;
    assert_eq!(
        w.plans
            .mark_state(&key, PlanState::Superseded, version, PlanState::Proposed),
        Ok(version + 1)
    );
    assert_eq!(
        rehearse(&alice, &host, &hash).await,
        Err(LifecycleError::NotApproved)
    );
    assert_eq!(*runs.lock().unwrap(), 0);
}

#[tokio::test]
async fn clean_reports_the_leftovers_and_needs_the_permission() {
    let w = world();
    let (host, _, _) = host(|| Behaviour::Finish(run_result(true)));
    let cleaned = clean(&ctx_for(&w, manager("alice")), host.as_ref())
        .await
        .unwrap();
    assert_eq!(cleaned, ["spot_rehearsal_abc"]);
    assert_eq!(
        clean(
            &ctx_for(&w, Principal::with_roles("eve", &["Clerk"])),
            host.as_ref()
        )
        .await,
        Err(LifecycleError::NotPermitted)
    );
}
