//! `plan` and `show` over the in-memory stores (no database).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_core::SiteId;
use spot_expand_contract::usage::UsageManifest;
use spot_expand_contract::{Release, ReleaseId, check};
use spot_migration_apply::{InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::plan::{PlanInputs, plan, show};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanState,
    PlanStore, SharedAuditSink,
};
use spot_migration_plan::{
    ReleasePlan, SchemaPlan, SchemaRefusal, SchemaRunKind, SchemaStep, SchemaStepKind,
    SystemDescription,
};
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

fn ctx_for(world: &World, actor: Principal) -> LifecycleContext {
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
    LifecycleContext::new(
        SiteId::new("alpha").unwrap(),
        Actor::new(actor, Channel::Cli).unwrap(),
        ports,
    )
}

fn alice() -> Principal {
    Principal::with_roles("alice", &["release_manager"])
}

fn additive() -> SchemaPlan {
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
    }
}

fn destructive() -> SchemaPlan {
    SchemaPlan {
        steps: Vec::new(),
        refusals: vec![SchemaRefusal {
            entity: "party".to_owned(),
            field: Some("note".to_owned()),
            code: "migrate.refused".to_owned(),
            reason: "it drops a column".to_owned(),
        }],
    }
}

fn inputs(schema: SchemaPlan) -> PlanInputs {
    PlanInputs::new(
        SystemDescription::default(),
        SystemDescription::default(),
        schema,
    )
}

/// A gate whose previous release still uses `party.note`, so dropping it now is too early.
fn early_contract_gate() -> spot_migration_ops::plan::ExpandContractFn {
    let id = ReleaseId::new("2.0.0").unwrap();
    let next = ReleaseId::new("2.1.0").unwrap();
    let previous =
        UsageManifest::from_toml("release = \"1.0.0\"\nobjects = [\"party.note\"]\n").unwrap();
    let current = UsageManifest::from_toml("release = \"2.0.0\"\nobjects = []\n").unwrap();
    Arc::new(move |plan: &ReleasePlan| {
        check(
            &Release {
                id: &id,
                next: Some(&next),
                plan,
                usage: &current,
            },
            &previous,
        )
    })
}

#[test]
fn an_additive_plan_auto_approves_under_the_default_policy() {
    let w = world();
    let ctx = ctx_for(&w, alice());
    let report = plan(&ctx, &inputs(additive())).unwrap();
    assert!(report.created);
    assert_eq!(report.risk, "additive");
    assert_eq!(report.state, PlanState::Approved);
    assert_eq!(report.hash.as_str().len(), 64);
    assert_eq!(report.author, "alice");
    let text = report.render("alpha");
    assert!(text.contains(report.hash.as_str()));
    assert!(text.contains("next:"));
    assert_eq!(w.plans.list().unwrap().len(), 1);
}

#[test]
fn the_same_inputs_give_the_same_hash_and_one_row() {
    let w = world();
    let first = plan(&ctx_for(&w, alice()), &inputs(additive())).unwrap();
    let again = plan(&ctx_for(&w, alice()), &inputs(additive())).unwrap();
    assert_eq!(first.hash, again.hash);
    assert!(!again.created);
    // Even another person converges on the stored plan.
    let other = plan(
        &ctx_for(&w, Principal::with_roles("bob", &["release_manager"])),
        &inputs(additive()),
    )
    .unwrap();
    assert!(!other.created);
    assert_eq!(other.author, "alice");
    assert_eq!(w.plans.list().unwrap().len(), 1);
}

#[test]
fn two_concurrent_plans_converge_to_one_hash() {
    let w = world();
    let handles: Vec<_> = ["alice", "bob"]
        .into_iter()
        .map(|name| {
            let ctx = ctx_for(&w, Principal::with_roles(name, &["release_manager"]));
            std::thread::spawn(move || plan(&ctx, &inputs(additive())).unwrap().hash)
        })
        .collect();
    let hashes: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(hashes[0], hashes[1]);
    assert_eq!(w.plans.list().unwrap().len(), 1);
}

#[test]
fn a_destructive_plan_needs_review_and_is_stored_as_proposed() {
    let w = world();
    let mut destructive_inputs = inputs(destructive());
    destructive_inputs.expand_contract =
        Some(Arc::new(|_| spot_expand_contract::Report::default()));
    let report = plan(&ctx_for(&w, alice()), &destructive_inputs).unwrap();
    assert_eq!(report.risk, "destructive");
    assert_eq!(report.state, PlanState::Proposed);
}

#[test]
fn an_expand_contract_violation_is_refused_with_the_finding_named() {
    let w = world();
    let mut gated = inputs(destructive());
    gated.expand_contract = Some(early_contract_gate());
    let refused = plan(&ctx_for(&w, alice()), &gated).unwrap_err();
    assert_eq!(refused.error, LifecycleError::RiskRequiresReview);
    assert!(!refused.findings.is_empty());
    assert!(
        refused.findings[0].target.contains("party"),
        "{:?}",
        refused.findings
    );
    assert!(
        w.plans.list().unwrap().is_empty(),
        "a refused plan is not stored"
    );
}

#[test]
fn a_destructive_plan_with_no_gate_is_refused() {
    let w = world();
    let refused = plan(&ctx_for(&w, alice()), &inputs(destructive())).unwrap_err();
    assert_eq!(refused.error, LifecycleError::RiskRequiresReview);
    assert_eq!(refused.findings[0].code, "expand_contract.not_configured");
    assert!(w.plans.list().unwrap().is_empty());
}

#[test]
fn an_unprivileged_actor_is_refused_and_nothing_is_stored() {
    let w = world();
    let eve = Principal::with_roles("eve", &["Clerk"]);
    let refused = plan(&ctx_for(&w, eve.clone()), &inputs(additive())).unwrap_err();
    assert_eq!(refused.error, LifecycleError::NotPermitted);
    assert_eq!(refused.error.exit_code(), 2);
    assert!(w.plans.list().unwrap().is_empty());
    assert_eq!(
        show(&ctx_for(&w, eve), &"0".repeat(64), None),
        Err(LifecycleError::NotPermitted)
    );
}

#[test]
fn show_prints_the_plan_its_state_and_approvals() {
    let w = world();
    let planned = plan(&ctx_for(&w, alice()), &inputs(additive())).unwrap();
    let shown = show(&ctx_for(&w, alice()), planned.hash.as_str(), None).unwrap();
    assert_eq!(shown.hash, planned.hash);
    assert_eq!(shown.state, PlanState::Approved);
    assert_eq!(shown.risk, "additive");
    assert_eq!(shown.author, "alice");
    assert!(shown.approvals.ready);
    assert!(!shown.text.is_empty());
    assert_eq!(
        show(&ctx_for(&w, alice()), &"f".repeat(64), None),
        Err(LifecycleError::UnknownPlan)
    );
    assert_eq!(
        show(&ctx_for(&w, alice()), "not-a-hash", None),
        Err(LifecycleError::UnknownPlan)
    );
}

#[test]
fn changed_inputs_after_planning_make_show_report_superseded() {
    let w = world();
    let planned = plan(&ctx_for(&w, alice()), &inputs(additive())).unwrap();
    let mut changed = additive();
    changed.steps[0].object = Some("other".to_owned());
    changed.steps[0].key = "add:party.other".to_owned();
    let shown = show(
        &ctx_for(&w, alice()),
        planned.hash.as_str(),
        Some(&inputs(changed)),
    )
    .unwrap();
    assert_eq!(shown.state, PlanState::Superseded);
    let stored = w.plans.load(&planned.hash).unwrap().unwrap();
    assert_eq!(stored.state, PlanState::Superseded);
    // Unchanged inputs keep the state.
    let planned2 = plan(&ctx_for(&w, alice()), &destructive_with_gate()).unwrap();
    let again = show(
        &ctx_for(&w, alice()),
        planned2.hash.as_str(),
        Some(&inputs_with_gate()),
    )
    .unwrap();
    assert_eq!(again.state, PlanState::Proposed);
}

fn destructive_with_gate() -> PlanInputs {
    inputs_with_gate()
}

fn inputs_with_gate() -> PlanInputs {
    let mut i = inputs(destructive());
    i.expand_contract = Some(Arc::new(|_| spot_expand_contract::Report::default()));
    i
}
