//! The migration audit operations over an in-memory chain: every verb leaves a run record (also on
//! refusal and error), `list`/`show`/`explain` need the permission and stay inside the site, and the
//! chain check reports the first broken link (no database).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_audit_chain::{AuditChain, CheckpointPolicy, InMemoryStore, LocalSigner};
use spot_core::SiteId;
use spot_migration_apply::{BoxFuture, InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::{
    ChainRunRecorder, ChainRunStore, Outcome, RoleAccess, RunFilter, RunRecorder, RunStore,
};
use spot_migration_ops::audit::{
    ChainStatus, ChainVerifier, explain, list, outcome_of, plan_ref, record_verb, show, verify,
};
use spot_migration_ops::plan::PlanInputs;
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, SharedAuditSink,
};
use spot_migration_plan::{ReleasePlan, SchemaPlan, SystemDescription};
use spot_permissions::Principal;

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

fn ctx_for(
    world: &World,
    actor: Principal,
    runs: Option<Arc<dyn RunStore>>,
) -> Arc<LifecycleContext> {
    let ports = LifecyclePorts {
        clock: Arc::new(FixedClock::new(5_000)),
        approvals: Arc::clone(&world.approvals),
        runs,
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

fn release_plan() -> ReleasePlan {
    PlanInputs::new(
        SystemDescription::default(),
        SystemDescription::default(),
        SchemaPlan::default(),
    )
    .release_plan()
}

/// A recorder and a run store over one in-memory chain for `site`. The run store borrows the
/// chain's store, so one `Arc` of the recorder is leaked (a test process, never production).
fn chain(site: &str, seed: u8) -> (Arc<dyn RunRecorder>, Arc<dyn RunStore>) {
    let signer = Arc::new(LocalSigner::from_seed([seed; 32]));
    let recorder = Arc::new(ChainRunRecorder::new(AuditChain::new(
        InMemoryStore::new(),
        site,
        signer,
        CheckpointPolicy { every: 0 },
    )));
    let leaked: &'static Arc<ChainRunRecorder<InMemoryStore>> =
        Box::leak(Box::new(Arc::clone(&recorder)));
    let store: Arc<dyn RunStore> = Arc::new(ChainRunStore::new(leaked.store(), site));
    (recorder, store)
}

async fn verb(
    ctx: &LifecycleContext,
    recorder: &Arc<dyn RunRecorder>,
    run: &str,
    result: Result<(), LifecycleError>,
) -> Result<(), LifecycleError> {
    let plan = release_plan();
    let reference = plan_ref(&plan, "{}", "plans/x.json");
    record_verb(
        ctx,
        recorder,
        run,
        "digest",
        reference,
        Outcome::Verified,
        async move { result },
    )
    .await
}

#[tokio::test]
async fn every_verb_leaves_a_run_record_also_on_refusal_and_error() {
    let w = world();
    let (recorder, store) = chain("alpha", 3);
    let ctx = ctx_for(&w, manager("alice"), Some(store));
    verb(&ctx, &recorder, "run-ok", Ok(())).await.unwrap();
    let refused = verb(
        &ctx,
        &recorder,
        "run-refused",
        Err(LifecycleError::NotApproved),
    )
    .await;
    assert_eq!(
        refused,
        Err(LifecycleError::NotApproved),
        "the error is unchanged"
    );
    let failed = verb(
        &ctx,
        &recorder,
        "run-failed",
        Err(LifecycleError::ApplyFailed),
    )
    .await;
    assert_eq!(failed, Err(LifecycleError::ApplyFailed));

    let runs = list(&ctx, RunFilter::all()).await.unwrap();
    let outcome = |id: &str| runs.iter().find(|r| r.id.as_str() == id).unwrap().outcome;
    assert_eq!(runs.len(), 3);
    assert_eq!(outcome("run-ok"), Outcome::Verified);
    assert_eq!(outcome("run-refused"), Outcome::Refused);
    assert_eq!(outcome("run-failed"), Outcome::Failed);
    assert!(runs.iter().all(|r| r.actor == "user:alice"));
    assert_eq!(outcome_of(&LifecycleError::NotPermitted), Outcome::Refused);
    assert_eq!(outcome_of(&LifecycleError::Store), Outcome::Failed);
}

#[tokio::test]
async fn show_and_explain_read_one_run() {
    let w = world();
    let (recorder, store) = chain("alpha", 4);
    let ctx = ctx_for(&w, manager("alice"), Some(store));
    verb(&ctx, &recorder, "run-one", Ok(())).await.unwrap();
    let evidence = show(&ctx, "run-one").await.unwrap();
    assert_eq!(evidence.run.id.as_str(), "run-one");
    assert!(!evidence.entries.is_empty());
    let text = explain(&ctx, "run-one").await.unwrap();
    assert!(text.contains("run-one"), "{text}");
    assert_eq!(
        show(&ctx, "run-none").await.err(),
        Some(LifecycleError::UnknownPlan)
    );
}

#[tokio::test]
async fn a_user_without_the_right_sees_nothing_and_a_missing_store_refuses() {
    let w = world();
    let (recorder, store) = chain("alpha", 5);
    let alice = ctx_for(&w, manager("alice"), Some(Arc::clone(&store)));
    verb(&alice, &recorder, "run-x", Ok(())).await.unwrap();
    let mallory = ctx_for(
        &w,
        Principal::with_roles("mallory", &["viewer"]),
        Some(store),
    );
    assert_eq!(
        list(&mallory, RunFilter::all()).await.err(),
        Some(LifecycleError::NotPermitted)
    );
    assert_eq!(
        show(&mallory, "run-x").await.err(),
        Some(LifecycleError::NotPermitted)
    );
    let unwired = ctx_for(&w, manager("alice"), None);
    assert!(matches!(
        list(&unwired, RunFilter::all()).await,
        Err(LifecycleError::PortUnavailable { .. })
    ));
}

#[tokio::test]
async fn the_runs_of_another_site_are_invisible() {
    let w = world();
    let (recorder_a, _store_a) = chain("alpha", 6);
    let (_recorder_b, store_b) = chain("beta", 7);
    let ctx_a = ctx_a_with(&w, &_store_a);
    verb(&ctx_a, &recorder_a, "run-a", Ok(())).await.unwrap();
    // The beta context reads beta's chain only.
    let ctx_b = ctx_for(&w, manager("alice"), Some(store_b));
    assert!(list(&ctx_b, RunFilter::all()).await.unwrap().is_empty());
    assert_eq!(
        show(&ctx_b, "run-a").await.err(),
        Some(LifecycleError::UnknownPlan)
    );
}

fn ctx_a_with(w: &World, store: &Arc<dyn RunStore>) -> Arc<LifecycleContext> {
    ctx_for(w, manager("alice"), Some(Arc::clone(store)))
}

struct Verifier(ChainStatus);

impl ChainVerifier for Verifier {
    fn verify(&self) -> BoxFuture<'_, Result<ChainStatus, LifecycleError>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

#[tokio::test]
async fn the_chain_check_reports_the_first_broken_link_and_needs_the_right() {
    let w = world();
    let ctx = ctx_for(&w, manager("alice"), None);
    let broken = Verifier(ChainStatus {
        checked: 9,
        head: 9,
        breaks: vec![(7, "bad_hash".to_owned()), (4, "bad_link".to_owned())],
    });
    let status = verify(&ctx, &broken).await.unwrap();
    assert!(!status.is_ok());
    assert_eq!(status.first_break(), Some(4));
    let intact = Verifier(ChainStatus {
        checked: 3,
        head: 3,
        breaks: Vec::new(),
    });
    assert!(verify(&ctx, &intact).await.unwrap().is_ok());
    let mallory = ctx_for(&w, Principal::with_roles("mallory", &["viewer"]), None);
    assert_eq!(
        verify(&mallory, &intact).await.err(),
        Some(LifecycleError::NotPermitted)
    );
}
