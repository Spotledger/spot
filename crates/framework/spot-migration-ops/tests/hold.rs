//! The hold and release operations over the recording hold (no database).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_core::SiteId;
use spot_mgmt_ops::{CatalogueBuilder, Invocation, OpId, OperationModule, ParamValue};
use spot_migration_apply::{InMemoryAdmissionGate, NoFaults, RecordingHold, SideEffectHold};
use spot_migration_approval::{
    ApprovalPolicy, ApprovalService, ApprovalStore, InMemoryApprovalStore, MemoryAuditSink,
};
use spot_migration_audit::RoleAccess;
use spot_migration_ops::hold::{
    HoldInspector, HoldView, MANUAL_HOLD_ID, ReleaseMode, hold, release, status,
};
use spot_migration_ops::testing::{FixedClock, InMemoryPlanStore};
use spot_migration_ops::{
    Actor, Channel, LifecycleContext, LifecycleError, LifecyclePorts, SharedAuditSink, handlers,
};
use spot_permissions::Principal;

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

const ADMIN: &str = "Tenant Administrator";

fn context(site: &str, actor: Principal, hold: Arc<RecordingHold>) -> Arc<LifecycleContext> {
    let store: Arc<dyn ApprovalStore> = Arc::new(InMemoryApprovalStore::new());
    let approvals = ApprovalService::new(
        store,
        SharedAuditSink(Arc::new(MemoryAuditSink::new())),
        ApprovalPolicy::default(),
    )
    .unwrap();
    let ports = LifecyclePorts {
        clock: Arc::new(FixedClock::new(1_000)),
        approvals: Arc::new(approvals),
        runs: None,
        snapshots: None,
        gate: Arc::new(InMemoryAdmissionGate::new()),
        hold,
        faults: Arc::new(NoFaults),
        plans: Arc::new(InMemoryPlanStore::new()),
        access: Arc::new(RoleAccess::new()),
    };
    Arc::new(LifecycleContext::new(
        SiteId::new(site).unwrap(),
        Actor::new(actor, Channel::Cli).unwrap(),
        ports,
    ))
}

fn admin(name: &str) -> Principal {
    Principal::with_roles(name, &[ADMIN])
}

#[test]
fn hold_then_replay_release() {
    let recording = Arc::new(RecordingHold::new());
    let ctx = context("alpha", admin("alice"), recording.clone());
    let out = block_on(hold(&ctx, MANUAL_HOLD_ID, "verify restore")).unwrap();
    assert_eq!(out.migration_id, MANUAL_HOLD_ID);
    assert_eq!(out.at, 1_000);
    assert_eq!(recording.held().as_deref(), Some(MANUAL_HOLD_ID));
    block_on(release(&ctx, MANUAL_HOLD_ID, ReleaseMode::Replay)).unwrap();
    assert_eq!(recording.held(), None);
    assert_eq!(recording.log(), ["begin:manual", "release:manual"]);
}

#[test]
fn an_actor_without_the_permission_is_refused_and_nothing_happens() {
    let recording = Arc::new(RecordingHold::new());
    let ctx = context(
        "alpha",
        Principal::with_roles("eve", &["Clerk"]),
        recording.clone(),
    );
    assert_eq!(
        block_on(hold(&ctx, MANUAL_HOLD_ID, "x")),
        Err(LifecycleError::NotPermitted)
    );
    assert!(recording.log().is_empty());
}

#[test]
fn a_hold_taken_by_a_migration_is_not_released_here() {
    let recording = Arc::new(RecordingHold::new());
    let ctx = context("alpha", admin("alice"), recording.clone());
    block_on(recording.begin("mig-7", "apply", 5)).unwrap();
    assert_eq!(
        block_on(release(&ctx, MANUAL_HOLD_ID, ReleaseMode::Replay)),
        Err(LifecycleError::HoldActive)
    );
    assert_eq!(
        block_on(hold(&ctx, MANUAL_HOLD_ID, "again")),
        Err(LifecycleError::HoldActive)
    );
    assert_eq!(recording.held().as_deref(), Some("mig-7"));
}

#[test]
fn discard_needs_a_second_approver_who_holds_the_permission() {
    let recording = Arc::new(RecordingHold::new());
    let ctx = context("alpha", admin("alice"), recording.clone());
    block_on(hold(&ctx, MANUAL_HOLD_ID, "x")).unwrap();
    let discard = |approver: Principal| ReleaseMode::Discard {
        approver,
        reason: "the copy was wrong".to_owned(),
    };
    // Nobody approves their own discard.
    assert_eq!(
        block_on(release(&ctx, MANUAL_HOLD_ID, discard(admin("alice")))),
        Err(LifecycleError::NotPermitted)
    );
    // An approver without the permission, an anonymous one and the system do not count.
    for approver in [
        Principal::with_roles("bob", &["Clerk"]),
        Principal::default(),
        Principal::system(),
    ] {
        assert_eq!(
            block_on(release(&ctx, MANUAL_HOLD_ID, discard(approver))),
            Err(LifecycleError::NotPermitted)
        );
    }
    assert_eq!(recording.held().as_deref(), Some(MANUAL_HOLD_ID));
    block_on(release(&ctx, MANUAL_HOLD_ID, discard(admin("carol")))).unwrap();
    assert_eq!(recording.held(), None);
    assert_eq!(recording.log(), ["begin:manual", "discard:manual"]);
}

#[test]
fn selective_release_is_refused_and_the_hold_stays() {
    let recording = Arc::new(RecordingHold::new());
    let ctx = context("alpha", admin("alice"), recording.clone());
    block_on(hold(&ctx, MANUAL_HOLD_ID, "x")).unwrap();
    let result = block_on(release(
        &ctx,
        MANUAL_HOLD_ID,
        ReleaseMode::Selective {
            classes: vec!["mail".to_owned()],
        },
    ));
    assert!(matches!(result, Err(LifecycleError::NotImplemented { .. })));
    assert_eq!(recording.held().as_deref(), Some(MANUAL_HOLD_ID));
}

#[test]
fn a_hold_in_one_site_does_not_hold_another() {
    let (a, b) = (
        Arc::new(RecordingHold::new()),
        Arc::new(RecordingHold::new()),
    );
    let ctx_a = context("alpha", admin("alice"), a.clone());
    let _ctx_b = context("bravo", admin("alice"), b.clone());
    block_on(hold(&ctx_a, MANUAL_HOLD_ID, "x")).unwrap();
    assert!(a.held().is_some());
    assert!(b.held().is_none());
}

struct Fixed(HoldView);

impl HoldInspector for Fixed {
    fn state(&self) -> spot_migration_apply::BoxFuture<'_, Result<HoldView, LifecycleError>> {
        let view = self.0.clone();
        Box::pin(async move { Ok(view) })
    }
}

#[test]
fn status_reports_through_the_inspector_and_needs_the_permission() {
    let view = HoldView {
        held: true,
        migration_id: Some("manual".to_owned()),
        reason: Some("x".to_owned()),
        since: Some(3),
    };
    let ctx = context("alpha", admin("alice"), Arc::new(RecordingHold::new()));
    assert_eq!(block_on(status(&ctx, &Fixed(view.clone()))).unwrap(), view);
    let denied = context(
        "alpha",
        Principal::with_roles("eve", &["Clerk"]),
        Arc::new(RecordingHold::new()),
    );
    assert_eq!(
        block_on(status(&denied, &Fixed(view))),
        Err(LifecycleError::NotPermitted)
    );
}

fn validated(op: &str, site: &str, reason: Option<&str>) -> spot_mgmt_ops::Validated {
    let mut builder = CatalogueBuilder::new();
    spot_migration_ops::MigrationOpsModule.register(&mut builder);
    let catalogue = builder.build().unwrap();
    let mut invocation =
        Invocation::new(OpId::new(op).unwrap()).with("site", ParamValue::Text(site.to_owned()));
    if let Some(reason) = reason {
        invocation = invocation.with("reason", ParamValue::Text(reason.to_owned()));
    }
    catalogue.validate(&invocation).unwrap()
}

#[test]
fn the_management_handlers_hold_and_release_this_site_only() {
    let recording = Arc::new(RecordingHold::new());
    let ctx = context("alpha", admin("alice"), recording.clone());
    let registry = handlers(&ctx);
    let hold_op = OpId::new("site.migrate.hold").unwrap();
    let release_op = OpId::new("site.migrate.release").unwrap();
    // Another site's name is refused before anything runs.
    let other = validated("site.migrate.hold", "bravo", Some("x"));
    let refused = block_on(registry.get(&hold_op).unwrap().handle(&other));
    assert_eq!(refused, Err(LifecycleError::NotPermitted));
    assert!(recording.log().is_empty());
    let mine = validated("site.migrate.hold", "alpha", Some("maintenance"));
    block_on(registry.get(&hold_op).unwrap().handle(&mine)).unwrap();
    assert!(recording.held().is_some());
    let release_call = validated("site.migrate.release", "alpha", None);
    block_on(registry.get(&release_op).unwrap().handle(&release_call)).unwrap();
    assert!(recording.held().is_none());
    // The other operations stay fail-closed stubs.
    let plan_call = validated("site.migrate.plan", "alpha", None);
    let plan_op = OpId::new("site.migrate.plan").unwrap();
    assert!(matches!(
        block_on(registry.get(&plan_op).unwrap().handle(&plan_call)),
        Err(LifecycleError::NotImplemented { .. })
    ));
}
