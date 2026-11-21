//! Extension: a new approval that moves the end later, refused over the extension count,
//! over the total duration from the original start, after expiry and after revocation;
//! the history stays in the grant.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::model::{SessionPolicy, SessionStatus};
use spot_support_session::{SessionError, SessionGrant, Tier};

fn policy_two_extensions() -> SessionPolicy {
    SessionPolicy::default()
}

#[test]
fn an_extension_within_the_limits_moves_the_end_and_records_the_history() {
    let mut g = grant();
    let new_end = END + 1800;
    let event = g
        .extend(new_end, &approver(), START + 60, &policy_two_extensions())
        .unwrap();
    assert_eq!(event.kind(), "extended");
    assert_eq!(event.actor(), "customer-admin");
    assert_eq!(event.at(), START + 60);
    assert_eq!(g.expires_at, new_end);
    assert_eq!(g.extensions().len(), 1);
    let e = &g.extensions()[0];
    assert_eq!(e.approver, "customer-admin");
    assert_eq!(e.at, START + 60);
    assert_eq!(e.extended_to, new_end);
    assert_eq!(g.status, SessionStatus::Active);
}

#[test]
fn an_extension_needs_a_distinct_approver_too() {
    let mut g = grant();
    // The requester approves its own extension: refused, exactly as at approval time.
    let p = SessionPolicy::default();
    let requester = spot_support_session::Approver::new(&g.requested_by.clone(), false);
    let err = g
        .extend(END + 1800, &requester, START + 60, &p)
        .unwrap_err();
    assert!(matches!(err, SessionError::ApprovalPolicy(_)));
    assert!(err.to_string().contains("differ from the requester"));
}

#[test]
fn an_extension_above_the_owner_rule_needs_a_tenant_owner() {
    let mut g = t2_grant();
    // The extension is its own approval under the policy passed here: the default one,
    // where a T2 ceiling needs a tenant owner.
    let p = SessionPolicy::default();
    let err = g
        .extend(END + 1800, &approver(), START + 60, &p)
        .unwrap_err();
    assert!(matches!(err, SessionError::ApprovalPolicy(_)));
    assert!(err.to_string().contains("tenant owner"));
    // The owner can approve the same extension.
    let ok = g.extend(END + 1800, &owner(), START + 60, &p).unwrap();
    assert_eq!(ok.kind(), "extended");
}

#[test]
fn an_extension_over_max_extensions_is_refused() {
    let mut g = grant();
    let p = policy_two_extensions();
    assert!(g.extend(END + 600, &approver(), START + 10, &p).is_ok());
    assert!(g.extend(END + 1200, &approver(), START + 20, &p).is_ok());
    let err = g
        .extend(END + 1800, &approver(), START + 30, &p)
        .unwrap_err();
    assert!(matches!(err, SessionError::ExtensionPolicy(_)));
    assert!(err.to_string().contains("2"));
    // The refused extension left no history and no new end.
    assert_eq!(g.extensions().len(), 2);
    assert_eq!(g.expires_at, END + 1200);
}

#[test]
fn an_extension_beyond_the_total_duration_is_refused() {
    let mut g = grant();
    let p = SessionPolicy::default();
    // The original start is `START`; the policy allows 4 h in total, the current end is
    // 1 h in, so at most 3 h more. Ask for 3 h + 1 s.
    let err = g
        .extend(START + 4 * 3600 + 1, &approver(), START + 60, &p)
        .unwrap_err();
    assert!(matches!(err, SessionError::ExtensionPolicy(_)));
    assert!(err.to_string().contains("original start"));
    // Exactly at 4 h total is fine.
    assert!(
        g.extend(START + 4 * 3600, &approver(), START + 60, &p)
            .is_ok()
    );
}

#[test]
fn an_extension_not_later_than_the_current_end_is_refused() {
    let mut g = grant();
    let err = g
        .extend(END, &approver(), START + 60, &SessionPolicy::default())
        .unwrap_err();
    assert!(matches!(err, SessionError::ExtensionNotLater { .. }));
    let err = g
        .extend(END - 1, &approver(), START + 60, &SessionPolicy::default())
        .unwrap_err();
    assert!(matches!(err, SessionError::ExtensionNotLater { .. }));
}

#[test]
fn an_extension_after_expiry_is_refused() {
    let mut g = grant();
    let err = g
        .extend(END + 1800, &approver(), END, &SessionPolicy::default())
        .unwrap_err();
    assert!(matches!(err, SessionError::NotExtendable(_)));
    assert!(err.to_string().contains("expired"));
    // One second before the boundary it would have worked; the boundary is exclusive.
}

#[test]
fn an_extension_after_revocation_is_refused_even_inside_the_window() {
    let mut g = grant();
    g.revoke("tenant-admin", "reason", START + 30).unwrap();
    let err = g
        .extend(
            END + 1800,
            &approver(),
            START + 60,
            &SessionPolicy::default(),
        )
        .unwrap_err();
    assert!(matches!(err, SessionError::NotExtendable(_)));
    assert!(err.to_string().contains("revoked"));
}

#[test]
fn a_second_extension_counts_and_the_total_is_measured_from_the_original_start() {
    let mut g = grant();
    let p = SessionPolicy::default();
    // Two extensions of 90 min each: 1 h + 1.5 h + 1.5 h = 4 h in total, exactly the
    // maximum, so both are accepted; a third is over the count.
    assert!(
        g.extend(START + 2 * 3600 + 1800, &approver(), START + 10, &p)
            .is_ok()
    );
    assert!(
        g.extend(START + 4 * 3600, &approver(), START + 20, &p)
            .is_ok()
    );
    let err = g
        .extend(START + 4 * 3600 + 1, &approver(), START + 30, &p)
        .unwrap_err();
    // The count refuses first (2 extensions already approved).
    assert!(matches!(err, SessionError::ExtensionPolicy(_)));
    assert_eq!(g.extensions().len(), 2);
}

#[test]
fn the_extension_history_survives_a_tier_rule_refusal_untouched() {
    let mut g = t2_grant();
    let p = policy_two_tiers();
    assert!(g.extend(END + 1800, &owner(), START + 60, &p).is_ok());
    let before = g.extensions().len();
    // A refused extension changes nothing.
    let _ = g.extend(END + 1800, &owner(), START + 61, &p);
    assert_eq!(g.extensions().len(), before);
    assert_eq!(g.tier_ceiling, Tier::T2);
}

fn policy_two_tiers() -> SessionPolicy {
    SessionPolicy {
        max_tier_without_owner_approval: Tier::T2,
        ..SessionPolicy::default()
    }
}

fn t2_grant() -> SessionGrant {
    let mut s = spec();
    s.tier_ceiling = Tier::T2;
    SessionGrant::request(&s, &policy_two_tiers(), START)
        .unwrap()
        .approve(&owner(), START, &policy_two_tiers())
        .unwrap()
        .0
}
