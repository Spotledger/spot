//! Provisioning (happy path, timing, failure at every step, resume and rollback) and
//! deprovisioning (suspend, archive, retention, two-person deletion).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{DAY, Fixture, request};
use proptest::prelude::*;
use spot_tenancy::ports::Outcome;
use spot_tenancy::{ControlStore, JournalKind, Step, TenantFlags, TenantId, TenantState, codes};

fn provisioned(f: &Fixture, name: &str, seed: u8) -> TenantId {
    let plane = f.plane();
    let plan = plane
        .provision(&request(name, "eu-west", seed), "ops", 1_000)
        .unwrap();
    plane.run(&plan.tenant, "ops", 1_001).unwrap();
    plan.tenant
}

#[test]
fn a_tenant_is_provisioned_in_order_and_inside_five_minutes() {
    let f = Fixture::new();
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    assert_eq!(
        plan.steps.iter().map(|(s, _)| *s).collect::<Vec<_>>(),
        Step::ALL
    );
    assert!(
        plan.estimated_ms < 5 * 60 * 1000,
        "estimate {} ms",
        plan.estimated_ms
    );
    assert_eq!(f.sim.owned_by(&plan.tenant), 0, "planning creates nothing");

    let t = plane.run(&plan.tenant, "ops", 1_001).unwrap();
    assert_eq!(t.state, TenantState::Active);
    assert_eq!(
        *f.sim.applied.borrow(),
        vec!["schema", "seed", "users", "licence", "telemetry"]
    );
    assert_eq!(f.sim.owned_by(&plan.tenant), 5, "one of each resource");
}

#[test]
fn run_twice_changes_nothing() {
    let f = Fixture::new();
    let id = provisioned(&f, "acme", 1);
    let before = f.sim.apply_calls.get();
    assert_eq!(
        f.plane().run(&id, "ops", 2_000).unwrap_err().code(),
        codes::WRONG_STATE
    );
    assert_eq!(f.sim.apply_calls.get(), before);
}

#[test]
fn every_attempt_is_audited_including_failures() {
    let f = Fixture::new();
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1_000)
        .unwrap();
    f.sim.fail_apply.set(Some((2, false)));
    let err = plane.run(&plan.tenant, "ops", 1_001).unwrap_err();
    assert_eq!(err.code(), codes::STEP_FAILED);
    let audit = f.sim.audit.borrow();
    assert!(audit.iter().any(|e| e.action == "provision_step"
        && e.step == Some("create_users")
        && e.outcome == Outcome::Failure(codes::STEP_FAILED)));
    assert!(
        audit
            .iter()
            .any(|e| e.action == "provision_run"
                && e.outcome == Outcome::Failure(codes::STEP_FAILED))
    );
    assert!(audit.iter().all(|e| !e.actor.is_empty()));
}

/// Fails the apply with index `k` (before or after its effect), then either resumes or rolls back.
fn fail_then(k: usize, after_effect: bool, roll_back: bool, undo_fail: Option<usize>) {
    let f = Fixture::new();
    let plane = f.plane();
    let other = provisioned(&f, "bystander", 9);
    let owned_before = f.sim.owned_by(&other);
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 5_000)
        .unwrap();
    let id = plan.tenant;
    f.sim
        .fail_apply
        .set(Some((f.sim.apply_calls.get() + k, after_effect)));
    let failed = plane.run(&id, "ops", 5_001);
    if k >= 5 {
        // Only five port calls exist; the injected failure never fires.
        assert!(failed.is_ok());
        return;
    }
    assert_eq!(failed.unwrap_err().code(), codes::STEP_FAILED);
    assert_eq!(f.store.get(&id).unwrap().state, TenantState::Provisioning);
    if roll_back {
        f.sim
            .fail_undo
            .set(undo_fail.map(|u| f.sim.undo_calls.get() + u));
        let mut result = plane.rollback(&id, "ops", 5_002);
        if undo_fail.is_some() && result.is_err() {
            assert_eq!(result.as_ref().unwrap_err().code(), codes::STEP_FAILED);
            assert_eq!(
                f.store.get(&id).unwrap().state,
                TenantState::Provisioning,
                "a failed rollback keeps the tenant visible"
            );
            result = plane.rollback(&id, "ops", 5_003);
        }
        let t = result.unwrap();
        assert_eq!(t.state, TenantState::Deleted);
        assert_eq!(
            f.sim.owned_by(&id),
            0,
            "rollback leaves no orphan (fail at {k}, after_effect {after_effect})"
        );
        // The name is free again at once.
        assert!(
            plane
                .provision(&request("acme", "eu-west", 3), "ops", 5_010)
                .is_ok()
        );
    } else {
        let t = plane.run(&id, "ops", 5_002).unwrap();
        assert_eq!(t.state, TenantState::Active);
        assert_eq!(
            f.sim.owned_by(&id),
            5,
            "resume completes with each resource exactly once (fail at {k})"
        );
        let journal = f.store.journal(&id).unwrap();
        for step in Step::ALL {
            assert!(
                journal
                    .iter()
                    .any(|e| e.step == step && e.kind == JournalKind::Done),
                "{step:?} done"
            );
        }
    }
    assert_eq!(
        f.sim.owned_by(&other),
        owned_before,
        "another tenant is never touched"
    );
}

proptest! {
    #[test]
    fn failure_at_any_step_resumes_or_rolls_back_without_orphans(
        k in 0usize..6,
        after in any::<bool>(),
        roll_back in any::<bool>(),
        undo_fail in proptest::option::of(0usize..5),
    ) {
        fail_then(k, after, roll_back, undo_fail);
    }
}

#[test]
fn rollback_is_refused_for_an_active_tenant() {
    let f = Fixture::new();
    let id = provisioned(&f, "acme", 1);
    assert_eq!(
        f.plane().rollback(&id, "ops", 9).unwrap_err().code(),
        codes::WRONG_STATE
    );
    assert_eq!(f.sim.owned_by(&id), 5);
}

#[test]
fn resume_after_a_partial_rollback_is_refused() {
    let f = Fixture::new();
    let plane = f.plane();
    let plan = plane
        .provision(&request("acme", "eu-west", 1), "ops", 1)
        .unwrap();
    f.sim.fail_apply.set(Some((3, false)));
    assert!(plane.run(&plan.tenant, "ops", 2).is_err());
    f.sim.fail_undo.set(Some(f.sim.undo_calls.get() + 1));
    assert!(plane.rollback(&plan.tenant, "ops", 3).is_err());
    assert_eq!(
        plane.run(&plan.tenant, "ops", 4).unwrap_err().code(),
        codes::WRONG_STATE,
        "no half-rolled-back resume"
    );
    assert!(plane.rollback(&plan.tenant, "ops", 5).is_ok());
}

fn deprovisioned(f: &Fixture, name: &str, seed: u8) -> TenantId {
    let plane = f.plane();
    let id = provisioned(f, name, seed);
    plane.suspend(&id, "ops", 2_000).unwrap();
    plane.archive(&id, "ops", 3_000, 30 * DAY).unwrap();
    id
}

#[test]
fn archive_needs_a_suspended_tenant_a_long_enough_delay_and_a_verified_backup() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = provisioned(&f, "acme", 1);
    assert_eq!(
        plane
            .archive(&id, "ops", 3_000, 30 * DAY)
            .unwrap_err()
            .code(),
        codes::WRONG_STATE,
        "users are locked out first"
    );
    plane.suspend(&id, "ops", 2_000).unwrap();
    assert_eq!(
        plane.archive(&id, "ops", 3_000, DAY).unwrap_err().code(),
        codes::INVALID_REQUEST
    );
    assert_eq!(
        plane
            .archive(&id, "ops", 3_000, u64::MAX)
            .unwrap_err()
            .code(),
        codes::INVALID_REQUEST,
        "overflow is refused"
    );
    f.sim.verify_ok.set(false);
    assert_eq!(
        plane
            .archive(&id, "ops", 3_000, 30 * DAY)
            .unwrap_err()
            .code(),
        codes::ARCHIVE_UNVERIFIED
    );
    assert_eq!(f.store.get(&id).unwrap().state, TenantState::Suspended);
    f.sim.verify_ok.set(true);
    let t = plane.archive(&id, "ops", 3_000, 30 * DAY).unwrap();
    assert_eq!(t.state, TenantState::Deprovisioning);
    assert_eq!(t.retention.unwrap().delete_after, 3_000 + 30 * DAY);
    assert_eq!(f.sim.owned_by(&id), 5, "archiving destroys nothing");
}

#[test]
fn deletion_is_refused_until_every_condition_holds() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = deprovisioned(&f, "acme", 1);
    let due = 3_000 + 30 * DAY;
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];

    assert_eq!(
        plane.delete(&id, "carol", due - 1).unwrap_err().code(),
        codes::RETENTION_PENDING
    );
    for approvers in [
        vec![],
        vec!["alice"],
        vec!["alice", "alice"],
        vec!["Alice", " alice "],
        vec!["alice", "carol"],
        vec!["carol", "CAROL", "alice"],
        vec!["alice", "  "],
    ] {
        *f.sim.approvers.borrow_mut() = approvers.iter().map(|s| (*s).to_owned()).collect();
        assert_eq!(
            plane.delete(&id, "carol", due).unwrap_err().code(),
            codes::APPROVAL_REQUIRED,
            "{approvers:?}"
        );
    }
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];
    f.sim.verify_ok.set(false);
    assert_eq!(
        plane.delete(&id, "carol", due).unwrap_err().code(),
        codes::ARCHIVE_UNVERIFIED,
        "the backup is re-verified at deletion"
    );
    f.sim.verify_ok.set(true);
    assert_eq!(
        plane.delete(&id, "  ", due).unwrap_err().code(),
        codes::INVALID_REQUEST
    );
    assert_eq!(f.sim.owned_by(&id), 5, "every refusal left the data alone");
    assert_eq!(f.sim.undo_calls.get(), 0);
    let audit = f.sim.audit.borrow();
    assert!(
        audit
            .iter()
            .filter(|e| e.action == "delete" && matches!(e.outcome, Outcome::Failure(_)))
            .count()
            >= 8,
        "refusals are audited"
    );
}

#[test]
fn a_legal_hold_blocks_archive_and_delete() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = deprovisioned(&f, "acme", 1);
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];
    let mut t = f.store.get(&id).unwrap();
    t.flags = TenantFlags::LEGAL_HOLD;
    f.store.update(t.clone(), t.version).unwrap();
    assert_eq!(
        plane
            .delete(&id, "carol", 3_000 + 30 * DAY)
            .unwrap_err()
            .code(),
        codes::LEGAL_HOLD
    );
    assert_eq!(f.sim.owned_by(&id), 5);
}

#[test]
fn an_approved_deletion_destroys_in_reverse_and_reserves_the_name() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = deprovisioned(&f, "acme", 1);
    let due = 3_000 + 30 * DAY;
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];
    let t = plane.delete(&id, "carol", due).unwrap();
    assert_eq!(t.state, TenantState::Deleted);
    assert_eq!(f.sim.owned_by(&id), 0);
    assert_eq!(
        *f.sim.undone.borrow(),
        vec!["telemetry", "licence", "users", "seed", "schema"]
    );
    assert!(
        f.sim
            .audit
            .borrow()
            .iter()
            .any(|e| e.action == "delete_begin")
    );
    // The name is reserved for the configured period, then free.
    let until = due + f.config.name_reservation_ms;
    assert_eq!(
        plane
            .provision(&request("acme", "eu-west", 7), "ops", until - 1)
            .unwrap_err()
            .code(),
        codes::NAME_RESERVED
    );
    assert!(
        plane
            .provision(&request("acme", "eu-west", 8), "ops", until)
            .is_ok()
    );
    assert_eq!(
        plane.delete(&id, "carol", until).unwrap_err().code(),
        codes::WRONG_STATE,
        "deleting twice is refused"
    );
}

#[test]
fn if_the_intent_cannot_be_audited_nothing_is_destroyed() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = deprovisioned(&f, "acme", 1);
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];
    *f.sim.audit_fails_on.borrow_mut() = Some("delete_begin");
    assert_eq!(
        plane
            .delete(&id, "carol", 3_000 + 30 * DAY)
            .unwrap_err()
            .code(),
        codes::AUDIT_FAILED
    );
    assert_eq!(f.sim.owned_by(&id), 5);
    assert_eq!(f.store.get(&id).unwrap().state, TenantState::Deprovisioning);
}

#[test]
fn a_failed_destruction_can_be_repeated_but_not_cancelled() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = deprovisioned(&f, "acme", 1);
    let due = 3_000 + 30 * DAY;
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];
    f.sim.fail_undo.set(Some(f.sim.undo_calls.get() + 2));
    assert_eq!(
        plane.delete(&id, "carol", due).unwrap_err().code(),
        codes::STEP_FAILED
    );
    assert_eq!(f.store.get(&id).unwrap().state, TenantState::Deprovisioning);
    assert_eq!(
        plane
            .cancel_deprovision(&id, "ops", due)
            .unwrap_err()
            .code(),
        codes::WRONG_STATE,
        "resources are already gone"
    );
    assert_eq!(
        plane.delete(&id, "carol", due).unwrap().state,
        TenantState::Deleted
    );
    assert_eq!(f.sim.owned_by(&id), 0);
}

#[test]
fn deprovisioning_can_be_cancelled_before_destruction() {
    let f = Fixture::new();
    let plane = f.plane();
    let id = deprovisioned(&f, "acme", 1);
    let t = plane.cancel_deprovision(&id, "ops", 4_000).unwrap();
    assert_eq!(t.state, TenantState::Suspended);
    assert!(t.retention.is_none());
    assert_eq!(
        plane.reactivate(&id, "ops", 4_001).unwrap().state,
        TenantState::Active
    );
}

#[test]
fn deleting_one_tenant_never_touches_another() {
    let f = Fixture::new();
    let plane = f.plane();
    let a = deprovisioned(&f, "alpha", 1);
    let b = provisioned(&f, "bravo", 2);
    *f.sim.approvers.borrow_mut() = vec!["alice".into(), "bob".into()];
    plane.delete(&a, "carol", 3_000 + 30 * DAY).unwrap();
    assert_eq!(f.sim.owned_by(&b), 5);
    assert_eq!(f.store.get(&b).unwrap().state, TenantState::Active);
    // Operating on the wrong tenant's id is a NotFound, not a cross-tenant hit.
    let ghost = TenantId::new_v7(42, [4; 10]);
    assert_eq!(
        plane.suspend(&ghost, "ops", 5).unwrap_err().code(),
        codes::NOT_FOUND
    );
}
