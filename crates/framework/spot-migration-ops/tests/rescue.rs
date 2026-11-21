//! `rescue` over a fake host and the in-memory stores: the two-step confirmation bound to the
//! counted loss, the second person, the gate and hold, and the plan ending `superseded`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_core::SiteId;
use spot_migration_apply::{BoxFuture, InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_ops::rescue::{
    HostRun, Mode, RescueHost, SnapshotFacts, confirm_token, preview, rescue,
};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanHash,
    PlanState, PlanStore, SharedAuditSink,
};
use spot_migration_plan::{
    ReleasePlan, SchemaPlan, SchemaRunKind, SchemaStep, SchemaStepKind, SystemDescription,
};
use spot_permissions::Principal;

struct FakeHost {
    snapshot: Option<SnapshotFacts>,
    loss: Arc<Mutex<BTreeMap<String, u64>>>,
    quiesced: bool,
    runs: Arc<Mutex<u32>>,
}

impl RescueHost for FakeHost {
    fn snapshot<'a>(
        &'a self,
        _plan: &'a ReleasePlan,
        _hash: &'a PlanHash,
    ) -> BoxFuture<'a, Result<Option<SnapshotFacts>, LifecycleError>> {
        Box::pin(async move { Ok(self.snapshot.clone()) })
    }
    fn loss_window(
        &self,
        _snapshot_at: i64,
    ) -> BoxFuture<'_, Result<BTreeMap<String, u64>, LifecycleError>> {
        Box::pin(async move { Ok(self.loss.lock().unwrap().clone()) })
    }
    fn quiesced(&self) -> BoxFuture<'_, Result<bool, LifecycleError>> {
        Box::pin(async move { Ok(self.quiesced) })
    }
    fn run<'a>(
        &'a self,
        _mode: Mode,
        _plan: &'a ReleasePlan,
        _snapshot: &'a SnapshotFacts,
        _now: i64,
    ) -> BoxFuture<'a, Result<HostRun, LifecycleError>> {
        *self.runs.lock().unwrap() += 1;
        Box::pin(async move {
            Ok(HostRun {
                text: "restored the pre-apply snapshot".to_owned(),
                unreplayed: BTreeMap::new(),
            })
        })
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

fn loss(invoices: u64) -> Arc<Mutex<BTreeMap<String, u64>>> {
    Arc::new(Mutex::new(BTreeMap::from([(
        "invoice".to_owned(),
        invoices,
    )])))
}

fn snapshot(verified: bool) -> Option<SnapshotFacts> {
    Some(SnapshotFacts {
        id: "snap-1".to_owned(),
        at: 1_000,
        verified,
    })
}

fn host(
    snapshot: Option<SnapshotFacts>,
    quiesced: bool,
    loss: Arc<Mutex<BTreeMap<String, u64>>>,
) -> (Arc<dyn RescueHost>, Arc<Mutex<u32>>) {
    let runs = Arc::new(Mutex::new(0));
    let host: Arc<dyn RescueHost> = Arc::new(FakeHost {
        snapshot,
        loss,
        quiesced,
        runs: Arc::clone(&runs),
    });
    (host, runs)
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

fn failed_plan(world: &World) -> String {
    let hash = approved_plan(world);
    force(world, &hash, PlanState::Applying);
    force(world, &hash, PlanState::Failed);
    hash
}

#[tokio::test]
async fn a_confirmed_two_person_restore_supersedes_the_plan() {
    let w = world();
    let hash = failed_plan(&w);
    let (host, runs) = host(snapshot(true), true, loss(3));
    let ctx = ctx_for(&w, manager("alice"));
    let shown = preview(&ctx, &host, &hash, Mode::Restore).await.unwrap();
    assert_eq!(shown.loss.get("invoice"), Some(&3));
    assert_eq!(shown.snapshot.id, "snap-1");
    assert_eq!(*runs.lock().unwrap(), 0, "a preview changes nothing");
    let report = rescue(&ctx, &host, &hash, Mode::Restore, &shown.token, "bob")
        .await
        .unwrap();
    assert_eq!(report.state, PlanState::Superseded);
    assert_eq!(state_of(&w, &hash), PlanState::Superseded);
    assert_eq!(*runs.lock().unwrap(), 1);
}

#[tokio::test]
async fn the_token_is_bound_to_the_counted_loss_and_the_mode() {
    let w = world();
    let hash = failed_plan(&w);
    let counts = loss(3);
    let (host, runs) = host(snapshot(true), true, Arc::clone(&counts));
    let ctx = ctx_for(&w, manager("alice"));
    let shown = preview(&ctx, &host, &hash, Mode::Restore).await.unwrap();
    // More writes arrive after the preview: the acknowledgement no longer matches.
    *counts.lock().unwrap() = BTreeMap::from([("invoice".to_owned(), 4)]);
    let refused = rescue(&ctx, &host, &hash, Mode::Restore, &shown.token, "bob").await;
    assert_eq!(refused.err(), Some(LifecycleError::RescueRefused));
    // The same token for another mode is another token.
    *counts.lock().unwrap() = BTreeMap::from([("invoice".to_owned(), 3)]);
    let other = rescue(&ctx, &host, &hash, Mode::Down, &shown.token, "bob").await;
    assert_eq!(other.err(), Some(LifecycleError::RescueRefused));
    assert_eq!(*runs.lock().unwrap(), 0);
    assert_eq!(state_of(&w, &hash), PlanState::Failed, "nothing changed");
    let hash_parsed = PlanHash::parse(&hash).unwrap();
    assert_ne!(
        confirm_token(&hash_parsed, Mode::Restore, 1_000, &BTreeMap::new()),
        confirm_token(&hash_parsed, Mode::Down, 1_000, &BTreeMap::new())
    );
}

#[tokio::test]
async fn a_second_person_is_required_and_must_be_someone_else() {
    let w = world();
    let hash = failed_plan(&w);
    let (host, runs) = host(snapshot(true), true, loss(1));
    let ctx = ctx_for(&w, manager("alice"));
    let shown = preview(&ctx, &host, &hash, Mode::Restore).await.unwrap();
    let alone = rescue(&ctx, &host, &hash, Mode::Restore, &shown.token, "").await;
    assert_eq!(alone.err(), Some(LifecycleError::NotPermitted));
    let same = rescue(&ctx, &host, &hash, Mode::Restore, &shown.token, "alice").await;
    assert_eq!(same.err(), Some(LifecycleError::SelfApproval));
    assert_eq!(*runs.lock().unwrap(), 0);
}

#[tokio::test]
async fn the_state_the_snapshot_the_gate_and_the_right_are_all_checked() {
    let w = world();
    let ctx = ctx_for(&w, manager("alice"));
    // An approved plan (never applied) cannot be rescued.
    let approved = approved_plan(&w);
    let (ok_host, _) = host(snapshot(true), true, loss(1));
    assert_eq!(
        preview(&ctx, &ok_host, &approved, Mode::Restore)
            .await
            .err(),
        Some(LifecycleError::RescueRefused)
    );
    // No verified snapshot.
    let hash = failed_plan(&w);
    for missing in [None, snapshot(false)] {
        let (bad, _) = host(missing, true, loss(1));
        assert_eq!(
            preview(&ctx, &bad, &hash, Mode::Restore).await.err(),
            Some(LifecycleError::RescueRefused)
        );
    }
    // The gate is not closed / the hold is not active: nothing runs.
    let (open, runs) = host(snapshot(true), false, loss(1));
    let shown = preview(&ctx, &open, &hash, Mode::Restore).await.unwrap();
    let refused = rescue(&ctx, &open, &hash, Mode::Restore, &shown.token, "bob").await;
    assert_eq!(refused.err(), Some(LifecycleError::RescueRefused));
    assert_eq!(*runs.lock().unwrap(), 0);
    // A stranger is refused.
    let stranger = ctx_for(&w, Principal::with_roles("mallory", &["viewer"]));
    assert_eq!(
        preview(&stranger, &ok_host, &hash, Mode::Restore)
            .await
            .err(),
        Some(LifecycleError::NotPermitted)
    );
    // An unknown plan.
    assert_eq!(
        preview(&ctx, &ok_host, &"a".repeat(64), Mode::Restore)
            .await
            .err(),
        Some(LifecycleError::UnknownPlan)
    );
}

#[tokio::test]
async fn an_applied_plan_can_be_rescued_too() {
    let w = world();
    let hash = approved_plan(&w);
    force(&w, &hash, PlanState::Applying);
    force(&w, &hash, PlanState::Applied);
    let (host, runs) = host(snapshot(true), true, loss(0));
    let ctx = ctx_for(&w, manager("alice"));
    let shown = preview(&ctx, &host, &hash, Mode::Down).await.unwrap();
    rescue(&ctx, &host, &hash, Mode::Down, &shown.token, "bob")
        .await
        .unwrap();
    assert_eq!(*runs.lock().unwrap(), 1);
}
