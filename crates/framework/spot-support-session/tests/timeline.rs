//! The timeline: explicit instants before the start, at it, inside, at `expires_at`
//! (expired exactly at the boundary, the half-open window `[starts_at, expires_at)`)
//! and after it; and the status derivation that needs no background job.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_support_session::check::ActionRequest;
use spot_support_session::model::DerivedStatus;
use spot_support_session::{DerivedStatus as DS, Tier};

fn read() -> ActionRequest {
    ActionRequest {
        action: "read".to_owned(),
        tier: Tier::T0,
        doctype: Some("Sales Invoice".to_owned()),
        tenant: TENANT.to_owned(),
    }
}

#[test]
fn before_the_start_the_session_is_not_yet_active_and_the_check_refuses() {
    let g = grant();
    let now = START - 1;
    assert_eq!(g.status_at(now), DerivedStatus::NotYetActive);
    let refusal = g.check(&read(), now).unwrap_err();
    assert_eq!(refusal.code(), "session.not_yet_active");
}

#[test]
fn at_the_start_instant_the_session_may_act() {
    let g = grant();
    // `starts_at` itself is inside the half-open window.
    assert_eq!(g.status_at(START), DerivedStatus::Active);
    assert!(g.check(&read(), START).is_ok());
}

#[test]
fn inside_the_window_the_session_is_active() {
    let g = grant();
    let now = START + 1800;
    assert_eq!(g.status_at(now), DerivedStatus::Active);
    assert!(g.check(&read(), now).is_ok());
}

#[test]
fn at_expires_at_the_session_is_already_expired() {
    let g = grant();
    // The boundary belongs to the expired side: the window is `[starts_at, expires_at)`.
    // Documented in the crate docs' time convention; this test pins it.
    assert_eq!(g.status_at(END), DerivedStatus::Expired);
    let refusal = g.check(&read(), END).unwrap_err();
    assert_eq!(refusal.code(), "session.expired");
}

#[test]
fn one_second_before_expires_at_the_session_still_runs() {
    let g = grant();
    assert_eq!(g.status_at(END - 1), DerivedStatus::Active);
    assert!(g.check(&read(), END - 1).is_ok());
}

#[test]
fn after_the_window_the_session_is_expired() {
    let g = grant();
    let now = END + 1;
    assert_eq!(g.status_at(now), DerivedStatus::Expired);
    let refusal = g.check(&read(), now).unwrap_err();
    assert_eq!(refusal.code(), "session.expired");
}

#[test]
fn the_derived_status_serialises_for_a_caller_that_persists_it() {
    let text_active = serde_json::to_string(&DS::Active).unwrap();
    assert_eq!(text_active, "\"active\"");
    let back: DS = serde_json::from_str(&text_active).unwrap();
    assert_eq!(back, DerivedStatus::Active);
}

#[test]
fn no_background_job_is_needed_the_status_moves_by_itself() {
    let g = grant();
    // The same grant, read at three instants, three answers, no mutation anywhere.
    assert_eq!(g.status_at(START - 1), DerivedStatus::NotYetActive);
    assert_eq!(g.status_at(START + 1), DerivedStatus::Active);
    assert_eq!(g.status_at(END + 1), DerivedStatus::Expired);
}
