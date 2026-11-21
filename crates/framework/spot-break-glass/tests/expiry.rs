#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Expiry is enforced by the monotonic agent clock: the session is killed at the exact
//! instant the window closes, never a second later or earlier.

mod common;

use common::*;
use spot_break_glass::{
    BreakGlassError, BreakGlassSession, ExpiryOutcome, Grant, RecordingKiller, State,
};
use spot_support_session::{Approver, SessionGrant, SessionPolicy};

fn live_session() -> BreakGlassSession {
    let mut p = policy();
    p.require_customer_consent = false;
    let (mut session, _) = BreakGlassSession::new(request(), p, 1000).unwrap();
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
    assert_eq!(session.state(), State::Active);
    session
}

#[test]
fn expiry_kills_the_session_at_the_exact_instant_and_not_before() {
    let mut session = live_session();
    let mut killer = RecordingKiller::default();

    assert!(matches!(
        session.enforce_expiry(&mut killer, 2999).unwrap(),
        ExpiryOutcome::Live
    ));
    assert!(killer.kills().is_empty(), "not yet");
    assert_eq!(session.state(), State::Active);

    let outcome = session.enforce_expiry(&mut killer, 3000).unwrap();
    match outcome {
        ExpiryOutcome::Killed(event) => {
            assert_eq!(event.kind, spot_break_glass::audit::EXPIRED);
            assert_eq!(event.at, 3000);
        }
        other => panic!("expected a kill, got {other:?}"),
    }
    assert_eq!(killer.kills(), &[(SESSION.to_owned(), 3000)]);
    assert_eq!(session.state(), State::Expired);
    assert_eq!(session.ended_at(), Some(3000));
    assert!(!session.is_live_at(3000), "the boundary belongs to expired");

    assert!(matches!(
        session.enforce_expiry(&mut killer, 3001).unwrap(),
        ExpiryOutcome::AlreadyOver
    ));
    assert_eq!(killer.kills().len(), 1, "killed exactly once");
}

#[test]
fn a_clock_that_goes_backwards_fails_closed() {
    let mut session = live_session();
    let mut killer = RecordingKiller::default();
    session.enforce_expiry(&mut killer, 2500).unwrap();
    assert_eq!(
        session.enforce_expiry(&mut killer, 2000).unwrap_err(),
        BreakGlassError::ClockWentBackwards
    );
    assert!(killer.kills().is_empty());
    assert_eq!(session.state(), State::Active);
}

#[test]
fn a_command_at_the_expiry_instant_is_refused() {
    let mut session = live_session();
    assert_eq!(session.command(2999).unwrap().commands, 1);
    assert_eq!(session.command(3000).unwrap_err(), BreakGlassError::Expired);
}
