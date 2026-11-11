#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Composition: the deep-access (customer-signed, hardware-bound) grant and the
//! support-session (scoped, time-boxed) grant are composed, never re-implemented.

mod common;

use common::*;
use spot_break_glass::{
    BreakGlassError, BreakGlassSession, CustomerConsent, Grant, Principal, State,
};
use spot_support_session::{Approver, SessionGrant, SessionPolicy};

fn two_approvals(session: &mut BreakGlassSession) {
    session.approve(&vendor("vendor-a"), 1000).unwrap();
    session.approve(&vendor("vendor-b"), 1000).unwrap();
}

#[test]
fn the_request_composes_into_the_deep_access_and_support_session_models() {
    let req = request();
    let access = req.access_request();
    assert_eq!(access.requester, req.subject());
    assert_eq!(access.spec.subject, req.subject());
    assert_eq!(access.reason, req.reason);
    assert_eq!(access.spec.reason, req.reason);
    assert_eq!(access.ticket_ref, req.ticket_id);
    assert_eq!(access.spec.tenant, req.tenant);
    assert_eq!(access.spec.starts_at, req.starts_at);
    assert_eq!(access.spec.expires_at, req.expires_at);
}

#[test]
fn a_customer_signed_hardware_bound_credential_activates_the_session() {
    let req = request();
    let mut deep = DeepFixture::new(req.access_request());
    let receipt_hash = deep.receipt.hash().unwrap();

    let (mut session, _) = BreakGlassSession::new(req, policy(), 1000).unwrap();
    session
        .record_consent(
            &CustomerConsent {
                approver: Principal::customer("customer-owner"),
                at: 1000,
                receipt_hash: Some(receipt_hash),
            },
            1000,
        )
        .unwrap();
    two_approvals(&mut session);

    let credential = deep.issue(1000).unwrap();
    assert_eq!(credential.receipt_hash(), receipt_hash);
    assert_eq!(credential.tenant(), TENANT);
    assert_eq!(credential.not_before(), 1000);
    assert!(credential.not_after() <= 3000);

    session
        .activate(Grant::Deep(Box::new(credential)), 1000)
        .unwrap();
    assert_eq!(session.state(), State::Active);
    let grant = session.grant().unwrap();
    assert!(grant.is_deep());
    assert!(grant.deep().is_some());
    assert!(grant.scoped().is_none());
}

#[test]
fn a_scoped_grant_activates_the_session() {
    let mut p = policy();
    p.require_customer_consent = false;
    let (mut session, _) = BreakGlassSession::new(request(), p, 1000).unwrap();
    two_approvals(&mut session);

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
    assert_eq!(session.state(), State::Active);
    let grant = session.grant().unwrap();
    assert!(grant.scoped().is_some());
    assert!(grant.deep().is_none());
}

#[test]
fn a_grant_for_another_tenant_is_refused() {
    let mut p = policy();
    p.require_customer_consent = false;
    let (mut session, _) = BreakGlassSession::new(request(), p, 1000).unwrap();
    two_approvals(&mut session);

    let mut spec = session.request().session_spec();
    spec.tenant = "tenant-b".into();
    let pending = SessionGrant::request(&spec, &SessionPolicy::default(), 1000).unwrap();
    let (grant, _) = pending
        .approve(
            &Approver::new("customer-owner", true),
            1000,
            &SessionPolicy::default(),
        )
        .unwrap();
    assert_eq!(
        session
            .activate(Grant::Scoped(Box::new(grant)), 1000)
            .unwrap_err(),
        BreakGlassError::GrantMismatch
    );
}

#[test]
fn a_grant_with_a_wider_window_is_refused() {
    let mut p = policy();
    p.require_customer_consent = false;
    let (mut session, _) = BreakGlassSession::new(request(), p, 1000).unwrap();
    two_approvals(&mut session);

    let mut spec = session.request().session_spec();
    spec.expires_at = 9000;
    let pending = SessionGrant::request(&spec, &SessionPolicy::default(), 1000).unwrap();
    let (grant, _) = pending
        .approve(
            &Approver::new("customer-owner", true),
            1000,
            &SessionPolicy::default(),
        )
        .unwrap();
    assert_eq!(
        session
            .activate(Grant::Scoped(Box::new(grant)), 1000)
            .unwrap_err(),
        BreakGlassError::GrantMismatch
    );
}
