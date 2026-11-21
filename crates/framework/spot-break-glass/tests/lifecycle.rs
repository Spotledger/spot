#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The lifecycle matrix, the two-person rule and the customer-consent rule.

mod common;

use common::*;
use spot_break_glass::{BreakGlassError, BreakGlassSession, CustomerConsent, Grant, Policy, State};
use spot_support_session::{Approver, SessionGrant, SessionPolicy};

fn scoped_grant(now: i64) -> Grant {
    let spec = session_spec();
    let pending = SessionGrant::request(&spec, &SessionPolicy::default(), now).unwrap();
    let (grant, _event) = pending
        .approve(
            &Approver::new("customer-owner", true),
            now,
            &SessionPolicy::default(),
        )
        .unwrap();
    Grant::Scoped(Box::new(grant))
}

#[test]
fn the_full_lifecycle_moves_request_approved_active_ended() {
    let (mut session, requested) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    assert_eq!(requested.kind, spot_break_glass::audit::REQUESTED);
    assert_eq!(session.state(), State::Requested);

    session
        .record_consent(
            &CustomerConsent {
                approver: spot_break_glass::Principal::customer("customer-owner"),
                at: 1000,
                receipt_hash: Some([9; 32]),
            },
            1000,
        )
        .unwrap();
    assert_eq!(session.state(), State::Requested);

    session.approve(&vendor("vendor-a"), 1001).unwrap();
    assert_eq!(
        session.state(),
        State::Requested,
        "one of two is not enough"
    );

    session.approve(&vendor("vendor-b"), 1002).unwrap();
    assert_eq!(session.state(), State::Approved);

    let active = session.activate(scoped_grant(1000), 1003).unwrap();
    assert_eq!(active.kind, spot_break_glass::audit::ACTIVE);
    assert_eq!(session.state(), State::Active);
    assert!(session.is_live_at(1500));

    let command = session.command(1500).unwrap();
    assert_eq!(command.commands, 1);
    assert_eq!(session.commands(), 1);

    let ended = session.end("work finished", 2000).unwrap();
    assert_eq!(ended.kind, spot_break_glass::audit::ENDED);
    assert_eq!(session.state(), State::Ended);
    assert_eq!(session.ended_at(), Some(2000));
    assert!(session.state().is_terminal());
}

#[test]
fn activation_before_two_approvals_is_refused_as_not_enough_approvers() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    let error = session.activate(scoped_grant(1000), 1000).unwrap_err();
    assert_eq!(
        error,
        BreakGlassError::NotEnoughApprovers { have: 1, need: 2 }
    );
    assert_eq!(session.state(), State::Requested);
}

#[test]
fn the_requester_cannot_approve_and_a_second_approval_by_one_person_does_not_count() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    assert_eq!(
        session.approve(&vendor("support-agent"), 1000).unwrap_err(),
        BreakGlassError::RequesterCannotApprove
    );
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    assert_eq!(
        session.approve(&vendor("vendor-a"), 1000).unwrap_err(),
        BreakGlassError::DuplicateApprover
    );
    assert_eq!(session.approvals().len(), 1);
    assert_eq!(session.state(), State::Requested);
}

#[test]
fn identities_differing_only_in_case_or_padding_are_one_person() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    assert_eq!(
        session
            .approve(&vendor(" Support-Agent "), 1000)
            .unwrap_err(),
        BreakGlassError::RequesterCannotApprove
    );
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    assert_eq!(
        session.approve(&vendor("Vendor-A "), 1000).unwrap_err(),
        BreakGlassError::DuplicateApprover
    );
}

#[test]
fn a_policy_cannot_lower_the_approver_count_below_two() {
    let mut weak = policy();
    weak.required_approvers = 1;
    assert!(matches!(
        BreakGlassSession::new(request(), weak, 1000).unwrap_err(),
        BreakGlassError::NotEnoughApprovers { .. }
    ));
}

#[test]
fn a_customer_principal_cannot_be_a_vendor_approver() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    assert_eq!(
        session
            .approve(
                &spot_break_glass::Principal::customer("customer-owner"),
                1000
            )
            .unwrap_err(),
        BreakGlassError::ApproverNotVendor
    );
}

#[test]
fn a_tenant_that_requires_consent_stays_requested_until_the_customer_consents() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    session.approve(&vendor("vendor-b"), 1001).unwrap();
    assert_eq!(
        session.state(),
        State::Requested,
        "two approvals but no consent"
    );
    session
        .record_consent(
            &CustomerConsent {
                approver: spot_break_glass::Principal::customer("customer-owner"),
                at: 1002,
                receipt_hash: Some([1; 32]),
            },
            1002,
        )
        .unwrap();
    assert_eq!(session.state(), State::Approved);
}

#[test]
fn a_tenant_that_does_not_require_consent_approves_on_two_approvals() {
    let mut p = policy();
    p.require_customer_consent = false;
    let (mut session, _) = BreakGlassSession::new(request(), p, 1000).unwrap();
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    session.approve(&vendor("vendor-b"), 1000).unwrap();
    assert_eq!(session.state(), State::Approved);
    assert!(session.consent().is_none());
}

#[test]
fn revoke_is_final_from_every_live_state_and_ends_the_session() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    session
        .revoke(&vendor("vendor-a"), "customer asked to stop", 1100)
        .unwrap();
    assert_eq!(session.state(), State::Revoked);
    assert_eq!(session.ended_at(), Some(1100));
    assert_eq!(
        session.revocation(),
        Some(("vendor-a", "customer asked to stop", 1100))
    );
    assert_eq!(
        session
            .revoke(&vendor("vendor-b"), "again", 1200)
            .unwrap_err(),
        BreakGlassError::WrongState("the session is already over; revocation is final")
    );
}

#[test]
fn ending_a_session_that_is_not_active_is_refused() {
    let (mut session, _) = BreakGlassSession::new(request(), policy(), 1000).unwrap();
    assert!(matches!(
        session.end("too early", 1000).unwrap_err(),
        BreakGlassError::WrongState(_)
    ));
}

#[test]
fn a_tenant_that_has_not_opted_in_cannot_open_a_session() {
    let p = Policy {
        opted_in: false,
        ..Policy::default()
    };
    let error = BreakGlassSession::new(request(), p, 1000).unwrap_err();
    assert_eq!(error, BreakGlassError::NotOptedIn);
}

#[test]
fn a_window_over_the_settings_cap_is_refused() {
    let mut p = policy();
    p.max_duration_secs = 4 * 3600;
    p.settings_max_duration_secs = 600;
    assert_eq!(p.max_duration(), 600);
    let error = BreakGlassSession::new(request(), p, 1000).unwrap_err();
    assert_eq!(
        error,
        BreakGlassError::DurationOverPolicy {
            asked: 2000,
            max: 600
        }
    );
}

#[test]
fn blank_reason_and_ticket_are_refused() {
    let mut r = request();
    r.reason = "  ".into();
    assert_eq!(
        BreakGlassSession::new(r, policy(), 1000).unwrap_err(),
        BreakGlassError::ReasonRequired
    );
    let mut r = request();
    r.ticket_id = String::new();
    assert_eq!(
        BreakGlassSession::new(r, policy(), 1000).unwrap_err(),
        BreakGlassError::TicketRequired
    );
}

#[test]
fn a_command_outside_the_window_is_refused() {
    let mut r = request();
    r.starts_at = 2000;
    r.expires_at = 3000;
    let mut p = policy();
    p.require_customer_consent = false;
    let (mut session, _) = BreakGlassSession::new(r, p, 1000).unwrap();
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    session.approve(&vendor("vendor-b"), 1000).unwrap();
    let spec = session.request().session_spec();
    let pending = SessionGrant::request(&spec, &SessionPolicy::default(), 1000).unwrap();
    let (grant, _) = pending
        .approve(
            &Approver::new("customer-owner", true),
            1000,
            &SessionPolicy::default(),
        )
        .unwrap();
    session
        .activate(Grant::Scoped(Box::new(grant)), 1000)
        .unwrap();
    assert_eq!(
        session.command(1500).unwrap_err(),
        BreakGlassError::NotStarted
    );
    assert_eq!(session.command(3000).unwrap_err(), BreakGlassError::Expired);
}
