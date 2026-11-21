//! Invitations: single-use and time-limited, with the clock supplied by the test.
#![allow(missing_docs)]

mod common;

use std::time::Duration;

use common::id;
use spot_core::{Clock, ManualClock};
use spot_iam_directory::{Invitation, InvitationState, UserType, codes};
use time::OffsetDateTime;

fn clock() -> ManualClock {
    ManualClock::new(common::at(0))
}

fn invite(clock: &ManualClock, valid_for: Duration) -> Invitation {
    Invitation::new(
        id(),
        "new.hire@example.com",
        UserType::Internal,
        clock.now_utc() + valid_for,
    )
}

fn code(r: spot_iam_directory::Result<Invitation>) -> Vec<&'static str> {
    r.err().map(|e| e.codes()).unwrap_or_default()
}

#[test]
fn a_pending_invitation_is_accepted_once() {
    let clock = clock();
    let inv = invite(&clock, Duration::from_secs(3600));
    assert!(inv.validate().is_ok());
    clock.advance(Duration::from_secs(60));
    let accepted = inv
        .accept(clock.now_utc())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(accepted.state, InvitationState::Accepted);
    assert_eq!(
        code(accepted.accept(clock.now_utc())),
        [codes::INVITATION_USED]
    );
}

#[test]
fn an_invitation_cannot_be_accepted_at_or_after_its_expiry() {
    let clock = clock();
    let inv = invite(&clock, Duration::from_secs(3600));
    clock.advance(Duration::from_secs(3599));
    assert!(inv.accept(clock.now_utc()).is_ok());
    clock.advance(Duration::from_secs(1));
    assert_eq!(
        code(inv.accept(clock.now_utc())),
        [codes::INVITATION_EXPIRED]
    );
    assert_eq!(inv.state_at(clock.now_utc()), InvitationState::Expired);
    // Storing the expiry changes the state, and an expired one stays expired.
    let stored = inv.expire(clock.now_utc());
    assert_eq!(stored.state, InvitationState::Expired);
    assert_eq!(stored.expire(clock.now_utc()), stored);
}

#[test]
fn a_cancelled_invitation_cannot_be_accepted_and_an_accepted_one_cannot_be_cancelled() {
    let clock = clock();
    let inv = invite(&clock, Duration::from_secs(3600));
    let cancelled = inv
        .cancel(clock.now_utc())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(cancelled.state, InvitationState::Cancelled);
    assert_eq!(
        code(cancelled.accept(clock.now_utc())),
        [codes::INVITATION_CANCELLED]
    );
    // Cancelling again is harmless.
    assert_eq!(
        cancelled
            .cancel(clock.now_utc())
            .unwrap_or_else(|e| panic!("{e}")),
        cancelled
    );
    let accepted = inv
        .accept(clock.now_utc())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(
        code(accepted.cancel(clock.now_utc())),
        [codes::INVITATION_USED]
    );
}

#[test]
fn only_people_are_invited_and_the_address_must_be_one() {
    let at: OffsetDateTime = common::at(10);
    for ty in [UserType::Service, UserType::Agent] {
        let e = Invitation::new(id(), "a@b.example", ty, at)
            .validate()
            .err()
            .unwrap_or_else(|| panic!("must fail"));
        assert_eq!(e.codes(), [codes::INVITATION_TYPE]);
    }
    for bad in [
        "",
        "no-at.example.com",
        "a@b",
        "a b@c.example",
        "@c.example",
        "a@@c.example",
        "a@.example",
    ] {
        let e = Invitation::new(id(), bad, UserType::Portal, at)
            .validate()
            .err()
            .unwrap_or_else(|| panic!("`{bad}` must fail"));
        assert_eq!(e.codes(), [codes::INVALID_EMAIL], "`{bad}`");
    }
    // Both problems at once.
    let e = Invitation::new(id(), "x", UserType::Agent, at)
        .validate()
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::INVALID_EMAIL, codes::INVITATION_TYPE]);
}
