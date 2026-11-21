//! Approval: the distinct-approver rule, the tenant-owner rule, and the `Approved` event.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::model::SessionPolicy;
use spot_support_session::{SessionError, SessionGrant, Tier};

#[test]
fn approval_activates_the_stored_status_and_sets_the_approver() {
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let (grant, _) = pending
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap();
    assert_eq!(grant.status, spot_support_session::SessionStatus::Active);
    assert_eq!(grant.approved_by, "customer-admin");
    assert_eq!(grant.requested_by, "support-desk");
}

#[test]
fn the_approved_event_names_the_approver_and_time() {
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let (_, event) = pending
        .approve(&approver(), START + 10, &SessionPolicy::default())
        .unwrap();
    assert_eq!(event.kind(), "approved");
    assert_eq!(event.actor(), "customer-admin");
    assert_eq!(event.at(), START + 10);
}

#[test]
fn the_same_approver_is_rejected_when_the_policy_says_so() {
    let mut s = spec();
    s.requested_by = "customer-admin".to_owned();
    let pending = SessionGrant::request(&s, &SessionPolicy::default(), START).unwrap();
    let err = pending
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap_err();
    assert!(matches!(err, SessionError::ApprovalPolicy(_)));
    assert!(err.to_string().contains("differ from the requester"));
}

#[test]
fn the_same_approver_is_allowed_when_the_policy_says_it_may() {
    let mut s = spec();
    s.requested_by = "customer-admin".to_owned();
    let p = SessionPolicy {
        require_distinct_approver: false,
        ..SessionPolicy::default()
    };
    let pending = SessionGrant::request(&s, &p, START).unwrap();
    let (grant, _) = pending.approve(&approver(), START, &p).unwrap();
    assert_eq!(grant.approved_by, "customer-admin");
}

#[test]
fn a_t3_ceiling_needs_a_tenant_owner() {
    let mut s = spec();
    s.tier_ceiling = Tier::T3;
    let pending = SessionGrant::request(&s, &SessionPolicy::default(), START).unwrap();
    let err = pending
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap_err();
    assert!(matches!(err, SessionError::ApprovalPolicy(_)));
    assert!(err.to_string().contains("tenant owner"));
}

#[test]
fn a_t3_ceiling_with_a_tenant_owner_approving_is_accepted() {
    let mut s = spec();
    s.tier_ceiling = Tier::T3;
    let pending = SessionGrant::request(&s, &SessionPolicy::default(), START).unwrap();
    let (grant, _) = pending
        .approve(&owner(), START, &SessionPolicy::default())
        .unwrap();
    assert_eq!(grant.approved_by, "owner-ops");
    assert_eq!(grant.tier_ceiling, Tier::T3);
}

#[test]
fn a_t2_ceiling_also_needs_a_tenant_owner_by_default() {
    let mut s = spec();
    s.tier_ceiling = Tier::T2;
    let pending = SessionGrant::request(&s, &SessionPolicy::default(), START).unwrap();
    let err = pending
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap_err();
    assert!(matches!(err, SessionError::ApprovalPolicy(_)));
}

#[test]
fn a_policy_allowing_t2_without_owner_accepts_a_plain_approver_at_t2() {
    let mut s = spec();
    s.tier_ceiling = Tier::T2;
    let p = SessionPolicy {
        max_tier_without_owner_approval: Tier::T2,
        ..SessionPolicy::default()
    };
    let pending = SessionGrant::request(&s, &p, START).unwrap();
    assert!(pending.approve(&approver(), START, &p).is_ok());
}

#[test]
fn approving_twice_is_impossible_because_the_pending_grant_is_consumed() {
    // The type is the control: `approve(self, ...)`, so a second approval cannot even be
    // written without keeping a clone; the moved value proves one grant, one approval.
    let pending = SessionGrant::request(&spec(), &SessionPolicy::default(), START).unwrap();
    let first = pending
        .approve(&approver(), START, &SessionPolicy::default())
        .unwrap();
    assert_eq!(first.0.status, spot_support_session::SessionStatus::Active);
}
