//! The permission-change audit: every change emits a payload that validates against the real
//! `identity` topic, the actor keeps both identities, and the between-two-dates diff answers
//! the question.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::assignment;
use spot_events_contract::identity;
use spot_iam_governance::{ChangeActor, ChangeEvent, ChangeOp, GrantScope, Snapshot, diff_access};
use spot_iam_roles::Source;

fn validate(event: &ChangeEvent) {
    let data = event.to_identity_data().expect("data");
    identity::validate(event.topic(), &data)
        .unwrap_or_else(|errors| panic!("{}: {errors:#?}", event.topic()));
}

/// A canonical UUIDv7 record id, the shape the events contract's `Id` fields need.
const ASSIGNMENT: &str = "0191a4c2-1111-7000-8000-000000000001";
/// A second canonical UUIDv7 record id.
const USER: &str = "0191a4c2-2222-7000-8000-000000000002";

#[test]
fn a_role_change_validates_against_the_identity_topic() {
    let event = ChangeEvent::role(
        "Finance Approver",
        ChangeOp::Copied,
        "custom",
        ChangeActor::user("ana"),
        10,
    )
    .with_version("2")
    .with_copied_from("Finance Approver")
    .with_before(Snapshot::from_value(serde_json::json!({"duties": ["a"]})))
    .with_after(Snapshot::from_value(
        serde_json::json!({"duties": ["a", "b"]}),
    ));
    assert_eq!(event.topic(), "identity_role_changed");
    validate(&event);
}

#[test]
fn a_duty_change_validates_against_the_identity_topic() {
    let event = ChangeEvent::duty(
        "Approve payment run",
        ChangeOp::Updated,
        "reference",
        ChangeActor::system("pack_loader"),
        11,
    )
    .with_before(Snapshot::from_value(serde_json::json!({"version": 1})))
    .with_after(Snapshot::from_value(serde_json::json!({"version": 2})));
    assert_eq!(event.topic(), "identity_duty_changed");
    validate(&event);
}

#[test]
fn a_grant_created_validates_against_the_identity_topic() {
    let assignment = assignment(
        "jane",
        "Finance Approver",
        Some(GrantScope::value("Company", "Company A")),
    );
    let event = ChangeEvent::grant(
        ASSIGNMENT,
        USER,
        "Finance Approver",
        Source::Manual,
        ChangeOp::Created,
        ChangeActor::user("ana"),
        12,
    )
    .with_scope(Snapshot::of(&assignment).expect("scope"))
    .with_reason("moved team");
    assert_eq!(event.topic(), "identity_assignment_created");
    validate(&event);
}

#[test]
fn a_grant_changed_validates_against_the_identity_topic() {
    let before = assignment("jane", "Accounts Payable Clerk", None);
    let after = assignment("jane", "Finance Approver", None);
    let event = ChangeEvent::grant(
        ASSIGNMENT,
        USER,
        "Finance Approver",
        Source::Manual,
        ChangeOp::Updated,
        ChangeActor::user("ana"),
        13,
    )
    .with_before(Snapshot::of(&before).expect("before"))
    .with_after(Snapshot::of(&after).expect("after"));
    assert_eq!(event.topic(), "identity_assignment_changed");
    validate(&event);
}

#[test]
fn a_grant_ended_validates_against_the_identity_topic() {
    let event = ChangeEvent::grant(
        ASSIGNMENT,
        USER,
        "Finance Approver",
        Source::Rule,
        ChangeOp::Ended,
        ChangeActor::system("assignment_expiry"),
        14,
    )
    .with_cause("expired");
    assert_eq!(event.topic(), "identity_assignment_ended");
    validate(&event);
}

#[test]
fn an_impersonated_actor_keeps_both_identities_in_the_contract_shape() {
    let event = ChangeEvent::grant(
        ASSIGNMENT,
        USER,
        "Finance Approver",
        Source::Manual,
        ChangeOp::Created,
        ChangeActor::impersonated("support-1", "ana"),
        15,
    );
    let actor = event.actor.actor_json();
    assert_eq!(actor["kind"], "user");
    assert_eq!(actor["id"], "support-1");
    assert_eq!(actor["via"], "ana");
}

#[test]
fn the_diff_answers_what_changed_between_two_dates() {
    let events = vec![
        ChangeEvent::grant(
            "grant:1",
            "jane",
            "Finance Approver",
            Source::Manual,
            ChangeOp::Created,
            ChangeActor::user("ana"),
            100,
        ),
        ChangeEvent::grant(
            "grant:1",
            "jane",
            "Finance Approver",
            Source::Manual,
            ChangeOp::Ended,
            ChangeActor::system("assignment_expiry"),
            200,
        )
        .with_cause("expired"),
        ChangeEvent::grant(
            "grant:2",
            "jane",
            "Accounts Payable Clerk",
            Source::Manual,
            ChangeOp::Created,
            ChangeActor::user("ana"),
            300,
        ),
    ];
    let diff = diff_access(&events, "jane", 50, 250);
    assert_eq!(diff.granted.len(), 1);
    assert_eq!(diff.removed.len(), 1);
    assert_eq!(diff.granted[0].subject, "grant:1");
    assert!(diff.changed.is_empty());
    // Outside the window nothing changed.
    assert!(diff_access(&events, "jane", 0, 50).is_empty());
    // A different user is unaffected.
    assert!(diff_access(&events, "cara", 0, 1000).is_empty());
}
