//! Assignments: the full state machine and the duplicate rule (WP-DOC-07.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_activity::assignment::{Assignment, AssignmentAction, AssignmentStatus};
use spot_activity::error::ActivityError;

fn open() -> Assignment {
    Assignment::new_open(uuid7(2, 1), uuid7(2, 2), "bob", "amy", at(0), None, &[]).unwrap()
}

#[test]
fn every_legal_transition_holds() {
    // Open → Done (Complete) and Open → Cancelled (Cancel); Done and Cancelled are final.
    assert_eq!(
        AssignmentStatus::Open
            .transition(AssignmentAction::Complete)
            .unwrap(),
        AssignmentStatus::Done
    );
    assert_eq!(
        AssignmentStatus::Open
            .transition(AssignmentAction::Cancel)
            .unwrap(),
        AssignmentStatus::Cancelled
    );
    assert!(
        AssignmentStatus::Done
            .transition(AssignmentAction::Complete)
            .is_err()
    );
    assert!(
        AssignmentStatus::Done
            .transition(AssignmentAction::Cancel)
            .is_err()
    );
    assert!(
        AssignmentStatus::Cancelled
            .transition(AssignmentAction::Complete)
            .is_err()
    );
    assert!(
        AssignmentStatus::Cancelled
            .transition(AssignmentAction::Cancel)
            .is_err()
    );
}

#[test]
fn every_illegal_transition_carries_the_stable_code() {
    let e = AssignmentStatus::Done
        .transition(AssignmentAction::Complete)
        .unwrap_err();
    assert_eq!(e.code(), "activity.assignment_transition_invalid");
    match &e {
        ActivityError::AssignmentTransitionInvalid(_, _) => {}
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn apply_mutates_through_the_machine() {
    let mut a = open();
    assert_eq!(a.status, AssignmentStatus::Open);
    a.apply(AssignmentAction::Complete).unwrap();
    assert_eq!(a.status, AssignmentStatus::Done);
    // A finished assignment refuses everything.
    assert!(a.apply(AssignmentAction::Cancel).is_err());
    assert_eq!(
        a.status,
        AssignmentStatus::Done,
        "a refused move mutates nothing"
    );
}

#[test]
fn a_second_open_assignment_for_the_same_pair_is_a_duplicate() {
    let first = open();
    let open_list = [first.clone()];
    let e = Assignment::new_open(
        uuid7(2, 9),
        first.doc,
        "bob",
        "amy",
        at(10),
        None,
        &open_list,
    )
    .unwrap_err();
    assert_eq!(e.code(), "activity.assignment_duplicate");
    match &e {
        ActivityError::AssignmentDuplicate(assignee, doc) => {
            assert_eq!(assignee, "bob");
            assert_eq!(doc, &first.doc.to_string());
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn open_assignments_to_other_users_or_docs_are_fine() {
    let first = open();
    let open_list = [first.clone()];
    // A different assignee.
    assert!(
        Assignment::new_open(
            uuid7(2, 3),
            first.doc,
            "carol",
            "amy",
            at(10),
            None,
            &open_list
        )
        .is_ok()
    );
    // A different document.
    assert!(
        Assignment::new_open(
            uuid7(2, 4),
            uuid7(9, 9),
            "bob",
            "amy",
            at(10),
            None,
            &open_list
        )
        .is_ok()
    );
}

#[test]
fn a_finished_or_cancelled_assignment_does_not_block_a_new_one() {
    let mut first = open();
    first.apply(AssignmentAction::Complete).unwrap();
    assert!(
        Assignment::new_open(
            uuid7(2, 5),
            first.doc,
            "bob",
            "amy",
            at(10),
            None,
            &[first.clone()]
        )
        .is_ok()
    );
    // (Already Done, so `first` blocks nothing: the duplicate rule counts open ones only.)
    let cancelled = Assignment {
        status: AssignmentStatus::Cancelled,
        ..open()
    };
    assert!(
        Assignment::new_open(
            uuid7(2, 6),
            cancelled.doc,
            "bob",
            "amy",
            at(10),
            None,
            &[cancelled]
        )
        .is_ok()
    );
}

#[test]
fn status_and_action_serde_are_stable_snake_case() {
    assert_eq!(
        serde_json::to_string(&AssignmentStatus::Open).unwrap(),
        "\"open\""
    );
    assert_eq!(
        serde_json::to_string(&AssignmentAction::Complete).unwrap(),
        "\"complete\""
    );
    let s: AssignmentStatus = serde_json::from_str("\"cancelled\"").unwrap();
    assert_eq!(s, AssignmentStatus::Cancelled);
}

#[test]
fn assignment_serde_round_trip_and_names() {
    let a = open();
    let json = serde_json::to_string(&a).unwrap();
    let back: Assignment = serde_json::from_str(&json).unwrap();
    assert_eq!(back, a);
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    for key in ["id", "doc", "assignee", "assigned_by", "at", "status"] {
        assert!(parsed.get(key).is_some(), "missing {key} in {json}");
    }
    // Optional `due` skips when absent...
    assert!(parsed.get("due").is_none());
    // ...and appears when set.
    let a = Assignment::new_open(
        uuid7(2, 1),
        uuid7(2, 2),
        "bob",
        "amy",
        at(0),
        Some(at(60)),
        &[],
    )
    .unwrap();
    let parsed: serde_json::Value = serde_json::to_value(&a).unwrap();
    assert_eq!(
        parsed["due"],
        at(60)
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    );
}
