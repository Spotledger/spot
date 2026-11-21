//! Every lifecycle verb under a run record (WP-MIG-11.T3): the permit is checked first, a refusal is
//! itself recorded and the body never runs, a failure is recorded with its reason code, start and
//! finish are separate records, the chain verifies, nothing but names reaches a record, and the audit
//! store failing refuses the verb (fail closed) while a rescue's finish is kept for a retry. In-memory
//! stores, no database.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_audit_chain::{
    AuditChain, CheckpointPolicy, InMemoryStore, KeyRing, LocalSigner, Verifier,
};
use spot_core::SiteId;
use spot_migration_apply::{BoxFuture, InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::{
    ApplyEvent, ApprovalEvent, ChainRunRecorder, ChainRunStore, FinishEvent, MigrationAuditError,
    Outcome, RehearsalEvent, RoleAccess, RunFilter, RunHandle, RunId, RunRecorder, RunSpec,
    RunStore,
};
use spot_migration_ops::approve::{approve, reject};
use spot_migration_ops::audit::{explain, list, show};
use spot_migration_ops::context::{AuditSpec, PlanStore, no_digest, no_plan};
use spot_migration_ops::hold::{ReleaseMode, hold, release};
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, SharedAuditSink,
};
use spot_migration_plan::{SchemaPlan, SchemaRefusal, SystemDescription};
use spot_permissions::Principal;

const SITE: &str = "alpha";
const CANARY: &str = "CANARY-5f3a-never-in-a-record";

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

/// One site's recorder, its run store and the key ring that verifies its chain.
struct Audit {
    recorder: Arc<ChainRunRecorder<InMemoryStore>>,
    store: Arc<dyn RunStore>,
    ring: KeyRing,
}

fn audit(site: &str, seed: u8) -> Audit {
    let signer = Arc::new(LocalSigner::from_seed([seed; 32]));
    let mut ring = KeyRing::new();
    ring.add(&signer.public_key()).unwrap();
    let recorder = Arc::new(ChainRunRecorder::new(AuditChain::new(
        InMemoryStore::new(),
        site,
        signer,
        CheckpointPolicy { every: 0 },
    )));
    // The run store borrows the chain's store; one `Arc` is leaked (a test process).
    let leaked: &'static Arc<ChainRunRecorder<InMemoryStore>> =
        Box::leak(Box::new(Arc::clone(&recorder)));
    let store: Arc<dyn RunStore> = Arc::new(ChainRunStore::new(leaked.store(), site));
    Audit {
        recorder,
        store,
        ring,
    }
}

fn ctx_with(
    w: &World,
    site: &str,
    actor: Principal,
    audit: &Audit,
    recorder: Arc<dyn RunRecorder>,
) -> Arc<LifecycleContext> {
    let ports = LifecyclePorts {
        clock: Arc::new(FixedClock::new(5_000)),
        approvals: Arc::clone(&w.approvals),
        runs: Some(Arc::clone(&audit.store)),
        snapshots: None,
        gate: Arc::new(InMemoryAdmissionGate::new()),
        hold: Arc::new(RecordingHold::new()),
        faults: Arc::new(NoFaults),
        plans: w.plans.clone(),
        access: Arc::new(RoleAccess::new().granting("release_manager")),
    };
    Arc::new(
        LifecycleContext::new(
            SiteId::new(site).unwrap(),
            Actor::new(actor, Channel::Cli).unwrap(),
            ports,
        )
        .with_recorder(recorder),
    )
}

fn manager(name: &str) -> Principal {
    Principal::with_roles(name, &["release_manager"])
}

fn ctx(w: &World, a: &Audit, name: &str) -> Arc<LifecycleContext> {
    ctx_with(w, SITE, manager(name), a, a.recorder.clone())
}

fn inputs() -> PlanInputs {
    let mut inputs = PlanInputs::new(
        SystemDescription::default(),
        SystemDescription::default(),
        SchemaPlan {
            steps: Vec::new(),
            refusals: vec![SchemaRefusal {
                entity: "party".to_owned(),
                field: Some("note".to_owned()),
                code: "migrate.refused".to_owned(),
                reason: "it drops a column".to_owned(),
            }],
        },
    );
    inputs.expand_contract = Some(Arc::new(|_| spot_expand_contract::Report::default()));
    inputs
}

async fn runs(ctx: &LifecycleContext) -> Vec<spot_migration_audit::MigrationRun> {
    list(ctx, RunFilter::all()).await.unwrap()
}

// ---- plan, approve, reject -------------------------------------------------------------------

#[tokio::test]
async fn a_permitted_plan_leaves_a_start_and_a_finish_with_the_digest_and_explain_renders_it() {
    let w = world();
    let a = audit(SITE, 1);
    let alice = ctx(&w, &a, "alice");
    let report = plan(&alice, &inputs()).unwrap();
    let all = runs(&alice).await;
    assert_eq!(all.len(), 1);
    let run = &all[0];
    assert_eq!(run.plan_digest, report.hash.as_str());
    assert_eq!(run.actor, "user:alice");
    assert_eq!(run.outcome, Outcome::Verified);
    assert!(run.ended.is_some());
    let evidence = show(&alice, run.id.as_str()).await.unwrap();
    assert_eq!(evidence.reason(), None);
    assert_eq!(
        evidence.entries.len(),
        2,
        "one start record and one finish record"
    );
    let text = explain(&alice, run.id.as_str()).await.unwrap();
    assert!(text.contains(run.id.as_str()), "{text}");
}

#[tokio::test]
async fn an_unpermitted_caller_is_refused_recorded_as_refused_and_the_body_never_ran() {
    let w = world();
    let a = audit(SITE, 2);
    let reader = ctx(&w, &a, "alice");
    let mallory = ctx_with(
        &w,
        SITE,
        Principal::with_roles("mallory", &["viewer"]),
        &a,
        a.recorder.clone(),
    );
    assert_eq!(
        plan(&mallory, &inputs()).err().map(|e| e.error),
        Some(LifecycleError::NotPermitted)
    );
    assert!(w.plans.list().unwrap().is_empty(), "the body did not run");
    let all = runs(&reader).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].outcome, Outcome::Refused);
    assert_eq!(all[0].actor, "user:mallory");
    assert_eq!(
        all[0].plan.summary.changes, 0,
        "a refusal records no plan content"
    );
    let evidence = show(&reader, all[0].id.as_str()).await.unwrap();
    assert_eq!(
        evidence.reason().as_deref(),
        Some("migration.not_permitted")
    );
}

#[tokio::test]
async fn a_failing_verb_is_recorded_with_its_reason_code() {
    let w = world();
    let a = audit(SITE, 3);
    let alice = ctx(&w, &a, "alice");
    let unknown = "ab".repeat(32);
    assert_eq!(
        approve(&alice, &unknown, "no"),
        Err(LifecycleError::UnknownPlan)
    );
    let all = runs(&alice).await;
    assert_eq!(all[0].outcome, Outcome::Refused);
    let evidence = show(&alice, all[0].id.as_str()).await.unwrap();
    assert_eq!(evidence.reason().as_deref(), Some("migration.unknown_plan"));
    assert!(
        explain_text(&alice, all[0].id.as_str())
            .await
            .contains("Reason migration.unknown_plan")
    );
}

async fn explain_text(ctx: &LifecycleContext, id: &str) -> String {
    let evidence = show(ctx, id).await.unwrap();
    spot_migration_audit::explain_evidence(&evidence).render()
}

#[tokio::test]
async fn approve_and_reject_record_the_decision_and_the_chain_verifies() {
    let w = world();
    let a = audit(SITE, 4);
    let alice = ctx(&w, &a, "alice");
    let hash = plan(&alice, &inputs()).unwrap().hash.as_str().to_owned();
    let bob = ctx(&w, &a, "bob");
    approve(&bob, &hash, CANARY).unwrap();
    let carol = ctx(&w, &a, "carol");
    reject(&carol, &hash, CANARY).ok();
    let all = runs(&alice).await;
    assert!(all.len() >= 3, "plan, approve and reject each leave a run");
    let approved = all.iter().find(|r| r.actor == "user:bob").unwrap();
    assert_eq!(approved.plan_digest, hash);
    assert_eq!(approved.approvals.len(), 1);
    assert_eq!(approved.approvals[0].approver, "user:bob");
    let verifier = Verifier::new(&a.ring);
    let chain = ChainRunStore::new(a.recorder.store(), SITE);
    assert!(chain.verify(&verifier).await.unwrap().is_ok());
}

// ---- canary -------------------------------------------------------------------------------------

#[tokio::test]
async fn nothing_but_names_reaches_a_record() {
    let w = world();
    let a = audit(SITE, 5);
    let alice = ctx(&w, &a, "alice");
    let hash = plan(&alice, &inputs()).unwrap().hash.as_str().to_owned();
    approve(&ctx(&w, &a, "bob"), &hash, CANARY).unwrap();
    let _ = hold(&alice, "mig-1", CANARY).await;
    for run in runs(&alice).await {
        let evidence = show(&alice, run.id.as_str()).await.unwrap();
        let text = format!("{evidence:?}");
        assert!(
            !text.contains(CANARY),
            "the canary reached a record: {text}"
        );
        assert!(
            !text.contains("it drops a column"),
            "plan prose reached a record"
        );
    }
}

// ---- hold and release -----------------------------------------------------------------------------

#[tokio::test]
async fn hold_and_release_are_recorded_and_an_unpermitted_hold_does_not_begin() {
    let w = world();
    let a = audit(SITE, 6);
    let alice = ctx(&w, &a, "alice");
    hold(&alice, "mig-1", "maintenance").await.unwrap();
    release(&alice, "mig-1", ReleaseMode::Replay).await.unwrap();
    let mallory = ctx_with(
        &w,
        SITE,
        Principal::with_roles("mallory", &["viewer"]),
        &a,
        a.recorder.clone(),
    );
    assert_eq!(
        hold(&mallory, "mig-2", "x").await.err(),
        Some(LifecycleError::NotPermitted)
    );
    let all = runs(&alice).await;
    assert_eq!(all.len(), 3);
    assert_eq!(
        all.iter().filter(|r| r.outcome == Outcome::Refused).count(),
        1
    );
    assert_eq!(
        all.iter()
            .filter(|r| r.outcome == Outcome::Verified)
            .count(),
        2
    );
}

#[tokio::test]
async fn concurrent_verbs_interleave_their_records_without_breaking_the_chain() {
    let w = world();
    let a = audit(SITE, 7);
    let alice = ctx(&w, &a, "alice");
    let (one, two) = (Arc::clone(&alice), Arc::clone(&alice));
    let (r1, r2) = tokio::join!(async move { hold(&one, "mig-a", "a").await }, async move {
        hold(&two, "mig-b", "b").await
    });
    assert!(r1.is_ok() || r2.is_ok());
    let all = runs(&alice).await;
    let ids: std::collections::BTreeSet<_> = all.iter().map(|r| r.id.clone()).collect();
    assert_eq!(ids.len(), all.len(), "run ids are distinct");
    let chain = ChainRunStore::new(a.recorder.store(), SITE);
    assert!(chain.verify(&Verifier::new(&a.ring)).await.unwrap().is_ok());
}

// ---- open runs ---------------------------------------------------------------------------------------

#[tokio::test]
async fn a_start_without_a_finish_shows_as_open() {
    let w = world();
    let a = audit(SITE, 8);
    let alice = ctx(&w, &a, "alice");
    let handle = a
        .recorder
        .begin(RunSpec {
            id: RunId::new("crashed-run").unwrap(),
            plan_digest: no_digest(),
            plan: no_plan(),
            actor: "user:alice".to_owned(),
            at: 1,
        })
        .await
        .unwrap();
    drop(handle);
    let all = runs(&alice).await;
    let open = all.iter().find(|r| r.id.as_str() == "crashed-run").unwrap();
    assert!(!open.outcome.is_terminal());
    assert!(open.ended.is_none());
}

// ---- fail closed ---------------------------------------------------------------------------------------

/// A recorder that can be told to fail its start, or its finish.
struct Flaky {
    inner: Arc<ChainRunRecorder<InMemoryStore>>,
    fail_begin: AtomicBool,
    fail_finish: AtomicBool,
}

impl Flaky {
    fn new(inner: Arc<ChainRunRecorder<InMemoryStore>>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            fail_begin: AtomicBool::new(false),
            fail_finish: AtomicBool::new(false),
        })
    }
}

fn down() -> MigrationAuditError {
    MigrationAuditError::Chain {
        message: "the audit store is down".to_owned(),
    }
}

impl RunRecorder for Flaky {
    fn begin<'a>(&'a self, spec: RunSpec) -> BoxFuture<'a, Result<RunHandle, MigrationAuditError>> {
        if self.fail_begin.load(Ordering::SeqCst) {
            return Box::pin(async { Err(down()) });
        }
        self.inner.begin(spec)
    }
    fn record_approval<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: ApprovalEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        self.inner.record_approval(handle, event)
    }
    fn record_rehearsal<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: RehearsalEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        self.inner.record_rehearsal(handle, event)
    }
    fn record_apply<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: ApplyEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        self.inner.record_apply(handle, event)
    }
    fn finish<'a>(
        &'a self,
        handle: &'a RunHandle,
        event: FinishEvent,
    ) -> BoxFuture<'a, Result<(), MigrationAuditError>> {
        if self.fail_finish.load(Ordering::SeqCst) {
            return Box::pin(async { Err(down()) });
        }
        self.inner.finish(handle, event)
    }
}

#[tokio::test]
async fn an_audit_store_that_cannot_begin_a_run_refuses_the_verb() {
    let w = world();
    let a = audit(SITE, 9);
    let flaky = Flaky::new(Arc::clone(&a.recorder));
    flaky.fail_begin.store(true, Ordering::SeqCst);
    let alice = ctx_with(&w, SITE, manager("alice"), &a, flaky);
    assert_eq!(
        plan(&alice, &inputs()).err().map(|e| e.error),
        Some(LifecycleError::Store)
    );
    assert!(w.plans.list().unwrap().is_empty(), "the body did not run");
    assert_eq!(
        hold(&alice, "mig-1", "x").await.err(),
        Some(LifecycleError::Store)
    );
}

#[tokio::test]
async fn a_failing_finish_fails_a_verb_but_a_rescue_keeps_it_for_a_retry() {
    let w = world();
    let a = audit(SITE, 10);
    let flaky = Flaky::new(Arc::clone(&a.recorder));
    let alice = ctx_with(&w, SITE, manager("alice"), &a, flaky.clone());
    flaky.fail_finish.store(true, Ordering::SeqCst);

    // An ordinary verb: its success is not reported as one when the evidence cannot be written.
    let ordinary: Result<u32, LifecycleError> = alice
        .audited(
            AuditSpec::new("ordinary", no_digest(), no_plan()),
            |_| async { Ok(1) },
        )
        .await;
    assert_eq!(ordinary, Err(LifecycleError::Store));

    // A rescue (keep the finish): the result is returned, the finish waits.
    let mut spec = AuditSpec::new("rescue", no_digest(), no_plan());
    spec.keep_finish_on_failure = true;
    let kept: Result<u32, LifecycleError> = alice.audited(spec, |_| async { Ok(7) }).await;
    assert_eq!(kept, Ok(7));
    assert_eq!(alice.pending_finishes(), 1);
    assert_eq!(alice.retry_pending_finishes().await, 0, "still down");
    assert_eq!(alice.pending_finishes(), 1);

    flaky.fail_finish.store(false, Ordering::SeqCst);
    assert_eq!(alice.retry_pending_finishes().await, 1);
    assert_eq!(alice.pending_finishes(), 0);
    let all = runs(&alice).await;
    let rescue = all
        .iter()
        .find(|r| r.id.as_str().starts_with("rescue-"))
        .unwrap();
    assert_eq!(rescue.outcome, Outcome::Verified);
}

// ---- sites ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn one_sites_verbs_never_write_to_another_sites_chain() {
    let w = world();
    let alpha = audit("alpha", 11);
    let beta = audit("beta", 12);
    let on_alpha = ctx_with(
        &w,
        "alpha",
        manager("alice"),
        &alpha,
        alpha.recorder.clone(),
    );
    let on_beta = ctx_with(&w, "beta", manager("alice"), &beta, beta.recorder.clone());
    hold(&on_alpha, "mig-1", "x").await.unwrap();
    assert_eq!(runs(&on_alpha).await.len(), 1);
    assert!(runs(&on_beta).await.is_empty());
}

#[tokio::test]
async fn without_a_recorder_a_verb_runs_unrecorded_as_before() {
    let w = world();
    let a = audit(SITE, 13);
    let ports = LifecyclePorts {
        clock: Arc::new(FixedClock::new(5_000)),
        approvals: Arc::clone(&w.approvals),
        runs: Some(Arc::clone(&a.store)),
        snapshots: None,
        gate: Arc::new(InMemoryAdmissionGate::new()),
        hold: Arc::new(RecordingHold::new()),
        faults: Arc::new(NoFaults),
        plans: w.plans.clone(),
        access: Arc::new(RoleAccess::new().granting("release_manager")),
    };
    let bare = LifecycleContext::new(
        SiteId::new(SITE).unwrap(),
        Actor::new(manager("alice"), Channel::Cli).unwrap(),
        ports,
    );
    hold(&bare, "mig-1", "x").await.unwrap();
    assert!(runs(&bare).await.is_empty());
}
