//! Revocation: by the tenant side and by the subject, at any time, final and immediate;
//! a revoked grant refuses everything and cannot be extended.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::check::ActionRequest;
use spot_support_session::model::SessionPolicy;
use spot_support_session::{DerivedStatus, SessionError, SessionGrant, SessionStatus, Tier};

fn read() -> ActionRequest {
    ActionRequest {
        action: "read".to_owned(),
        tier: Tier::T0,
        doctype: Some("Sales Invoice".to_owned()),
        tenant: TENANT.to_owned(),
    }
}

fn repair() -> ActionRequest {
    ActionRequest {
        action: "repost".to_owned(),
        tier: Tier::T2,
        doctype: Some("Sales Invoice".to_owned()),
        tenant: TENANT.to_owned(),
    }
}

fn policy_with_two_tiers() -> SessionPolicy {
    // A T2 ceiling so `repair` is under the ceiling and the ONLY failing reason of a
    // revoked check is the revocation itself.
    SessionPolicy {
        max_tier_without_owner_approval: Tier::T2,
        ..SessionPolicy::default()
    }
}

fn t2_grant() -> SessionGrant {
    let mut s = spec();
    s.tier_ceiling = Tier::T2;
    SessionGrant::request(&s, &policy_with_two_tiers(), START)
        .unwrap()
        .approve(&owner(), START, &policy_with_two_tiers())
        .unwrap()
        .0
}

#[test]
fn the_tenant_side_revokes_inside_the_window() {
    let mut g = t2_grant();
    let event = g
        .revoke("tenant-admin", "the ticket is resolved", START + 60)
        .unwrap();
    assert_eq!(event.kind(), "revoked");
    assert_eq!(event.actor(), "tenant-admin");
    assert_eq!(event.at(), START + 60);
    assert_eq!(event.reason(), "the ticket is resolved");
    assert_eq!(g.status, SessionStatus::Revoked);
    assert!(g.is_revoked());
    let r = g.revocation().unwrap();
    assert_eq!(r.by, "tenant-admin");
    assert_eq!(r.at, START + 60);
}

#[test]
fn the_subject_revokes_its_own_session() {
    let mut g = grant();
    let event = g
        .revoke("agent-support-7", "work finished early", START + 30)
        .unwrap();
    assert_eq!(event.kind(), "revoked");
    assert_eq!(event.actor(), "agent-support-7");
    assert_eq!(g.revocation().unwrap().by, "agent-support-7");
}

#[test]
fn revocation_works_before_the_start_too() {
    let mut g = grant();
    g.revoke("tenant-admin", "the request was withdrawn", START - 10)
        .unwrap();
    assert_eq!(g.status, SessionStatus::Revoked);
    // A revoked grant refuses everything, even in a window it never reached.
    let refusal = g.check(&read(), START + 1).unwrap_err();
    assert_eq!(refusal.code(), "session.revoked");
}

#[test]
fn revocation_works_after_expiry_too() {
    let mut g = grant();
    g.revoke("tenant-admin", "bookkeeping", END + 100).unwrap();
    let refusal = g.check(&read(), END + 101).unwrap_err();
    // Time is checked before revocation (the documented order): an expired grant that
    // was also revoked reports `expired`.
    assert_eq!(refusal.code(), "session.expired");
    assert_eq!(g.status, SessionStatus::Revoked);
}

#[test]
fn a_revoked_grant_refuses_everything_inside_its_window() {
    let mut g = t2_grant();
    g.revoke("tenant-admin", "suspect activity", START + 60)
        .unwrap();
    let refusal = g.check(&read(), START + 61).unwrap_err();
    assert_eq!(refusal.code(), "session.revoked");
    let refusal = g.check(&repair(), START + 61).unwrap_err();
    assert_eq!(refusal.code(), "session.revoked");
    // The refusal names the revocation even though the window is fine: `tier` is not
    // carried on a revocation refusal, the revocation decides alone.
    assert!(refusal.tier().is_none());
}

#[test]
fn a_revoked_grant_cannot_be_extended() {
    let mut g = grant();
    g.revoke("tenant-admin", "revoked for the test", START + 60)
        .unwrap();
    let err = g
        .extend(
            END + 1800,
            &approver(),
            START + 61,
            &SessionPolicy::default(),
        )
        .unwrap_err();
    assert!(matches!(err, SessionError::NotExtendable(_)));
    assert!(err.to_string().contains("revoked"));
}

#[test]
fn revocation_is_final_a_second_revocation_is_refused() {
    let mut g = grant();
    assert!(g.revoke("tenant-admin", "first", START + 60).is_ok());
    let err = g.revoke("subject", "second", START + 61).unwrap_err();
    assert!(matches!(err, SessionError::WrongState(_)));
    // The first revocation stands untouched.
    let r = g.revocation().unwrap();
    assert_eq!(r.by, "tenant-admin");
    assert_eq!(r.at, START + 60);
    assert_eq!(r.reason, "first");
}

#[test]
fn a_revoked_grant_outside_the_window_reports_the_time_first() {
    let mut g = grant();
    g.revoke("tenant-admin", "reason", START + 60).unwrap();
    // Before start: `not_yet_active` wins (the order places the window over revocation).
    let refusal = g.check(&read(), START - 1).unwrap_err();
    assert_eq!(refusal.code(), "session.not_yet_active");
}

#[test]
fn revocation_does_not_change_the_derived_liveness() {
    let mut g = grant();
    g.revoke("tenant-admin", "reason", START + 60).unwrap();
    // `status_at` derives from the times only; revocation is read separately, which is
    // how the audit trail can distinguish "the window closed" from "someone closed it".
    assert_eq!(g.status_at(START + 61), DerivedStatus::Active);
    assert!(g.is_revoked());
}
