//! `approve` and `reject`: separation of duties, the policy's approver count, the plan-hash
//! binding and the audit trail (in-memory stores, no database).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_core::SiteId;
use spot_mgmt_ops::{CatalogueBuilder, Invocation, OpId, OperationModule, ParamValue};
use spot_migration_apply::{InMemoryAdmissionGate, NoFaults, RecordingHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::approve::{approve, reject};
use spot_migration_ops::plan::{PlanInputs, plan};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Approvals, Channel, LifecycleContext, LifecycleError, LifecyclePorts, PlanHash,
    PlanState, PlanStore, SharedAuditSink, StoredPlan, handlers,
};
use spot_migration_plan::{SchemaPlan, SchemaRefusal, SystemDescription};
use spot_permissions::Principal;

struct World {
    approvals: Arc<Approvals>,
    plans: Arc<InMemoryPlanStore>,
    audit: Arc<MemoryAuditSink>,
}

fn world() -> World {
    let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
    let audit = Arc::new(MemoryAuditSink::new());
    World {
        approvals: Arc::new(
            ApprovalService::new(
                store,
                SharedAuditSink(audit.clone()),
                ApprovalPolicy::default(),
            )
            .unwrap(),
        ),
        plans: Arc::new(InMemoryPlanStore::new()),
        audit,
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

fn manager(name: &str) -> Principal {
    Principal::with_roles(name, &["release_manager"])
}

/// A destructive plan (two approvers by default) authored by alice.
fn destructive_plan(world: &World) -> String {
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
    plan(&ctx_for(world, manager("alice")), &inputs)
        .unwrap()
        .hash
        .as_str()
        .to_owned()
}

#[test]
fn the_author_cannot_approve_or_reject_their_own_plan() {
    let w = world();
    let hash = destructive_plan(&w);
    let alice = ctx_for(&w, manager("alice"));
    assert_eq!(
        approve(&alice, &hash, "mine"),
        Err(LifecycleError::SelfApproval)
    );
    assert_eq!(
        reject(&alice, &hash, "mine"),
        Err(LifecycleError::SelfApproval)
    );
    assert!(w.approvals.store().all().unwrap().is_empty());
}

#[test]
fn a_destructive_plan_needs_two_approvers_then_is_approved() {
    let w = world();
    let hash = destructive_plan(&w);
    let first = approve(&ctx_for(&w, manager("bob")), &hash, "looks right").unwrap();
    assert_eq!(first.state, PlanState::Proposed);
    assert_eq!(first.approvals.required, 2);
    assert_eq!(first.approvals.approved_by, ["bob"]);
    assert!(!first.approvals.ready);
    // The same person twice is still one approver.
    let again = approve(&ctx_for(&w, manager("bob")), &hash, "again").unwrap();
    assert_eq!(again.state, PlanState::Proposed);
    let second = approve(&ctx_for(&w, manager("carol")), &hash, "second look").unwrap();
    assert_eq!(second.state, PlanState::Approved);
    assert!(second.approvals.ready);
    let stored = w
        .plans
        .load(&PlanHash::parse(&hash).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(stored.state, PlanState::Approved);
}

#[test]
fn an_approver_without_the_policy_role_the_system_and_an_unprivileged_user_are_refused() {
    let w = world();
    let hash = destructive_plan(&w);
    // Holds the migration right but not the role the policy asks for.
    let admin_only = Principal::with_roles("dan", &["Tenant Administrator"]);
    assert_eq!(
        approve(&ctx_for(&w, admin_only), &hash, "x"),
        Err(LifecycleError::NotPermitted)
    );
    assert_eq!(
        approve(&ctx_for(&w, Principal::system()), &hash, "x"),
        Err(LifecycleError::NotPermitted)
    );
    assert_eq!(
        approve(
            &ctx_for(&w, Principal::with_roles("eve", &["Clerk"])),
            &hash,
            "x"
        ),
        Err(LifecycleError::NotPermitted)
    );
    assert!(w.approvals.store().all().unwrap().is_empty());
}

#[test]
fn a_rejection_closes_the_plan() {
    let w = world();
    let hash = destructive_plan(&w);
    let rejected = reject(&ctx_for(&w, manager("bob")), &hash, "not now").unwrap();
    assert_eq!(rejected.state, PlanState::Rejected);
    assert_eq!(rejected.approvals.rejected_by, ["bob"]);
    assert_eq!(
        approve(&ctx_for(&w, manager("carol")), &hash, "x"),
        Err(LifecycleError::PlanChanged)
    );
}

#[test]
fn a_plan_changed_in_the_store_cannot_be_approved() {
    let w = world();
    let hash = PlanHash::parse(&"a".repeat(64)).unwrap();
    w.plans
        .save(StoredPlan {
            hash: hash.clone(),
            json: "{}".to_owned(),
            author: "alice".to_owned(),
            created_at: 1,
            state: PlanState::Proposed,
            version: 1,
        })
        .unwrap();
    assert_eq!(
        approve(&ctx_for(&w, manager("bob")), hash.as_str(), "x"),
        Err(LifecycleError::PlanChanged)
    );
    assert_eq!(
        approve(&ctx_for(&w, manager("bob")), &"b".repeat(64), "x"),
        Err(LifecycleError::UnknownPlan)
    );
}

#[test]
fn decisions_and_the_plan_registration_are_audited() {
    let w = world();
    let hash = destructive_plan(&w);
    approve(&ctx_for(&w, manager("bob")), &hash, "ok").unwrap();
    reject(&ctx_for(&w, manager("carol")), &hash, "no").unwrap();
    let kinds: Vec<String> = w.audit.records().iter().map(|r| r.kind.clone()).collect();
    assert!(kinds.contains(&spot_migration_approval::service::kinds::PLAN_REGISTERED.to_owned()));
    assert!(kinds.contains(&spot_migration_approval::service::kinds::APPROVED.to_owned()));
    assert!(kinds.contains(&spot_migration_approval::service::kinds::REJECTED.to_owned()));
}

#[test]
fn the_management_handlers_approve_and_reject_this_site_only() {
    let w = world();
    let hash = destructive_plan(&w);
    let ctx = Arc::new(ctx_for(&w, manager("bob")));
    let registry = handlers(&ctx);
    let mut builder = CatalogueBuilder::new();
    spot_migration_ops::MigrationOpsModule.register(&mut builder);
    let catalogue = builder.build().unwrap();
    let call = |op: &str, site: &str| {
        let invocation = Invocation::new(OpId::new(op).unwrap())
            .with("site", ParamValue::Text(site.to_owned()))
            .with("plan_hash", ParamValue::Text(hash.clone()));
        catalogue.validate(&invocation).unwrap()
    };
    let op = OpId::new("site.migrate.approve").unwrap();
    let other_site = call("site.migrate.approve", "bravo");
    assert_eq!(
        futures_lite_block_on(registry.get(&op).unwrap().handle(&other_site)),
        Err(LifecycleError::NotPermitted)
    );
    let mine = call("site.migrate.approve", "alpha");
    let out = futures_lite_block_on(registry.get(&op).unwrap().handle(&mine)).unwrap();
    assert_eq!(out.fields["plan_hash"], hash);
    assert_eq!(out.fields["approved_by"], "bob");
}

fn futures_lite_block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}
