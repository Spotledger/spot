#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Coalescing rules, one test per event class (ADR-066 section 2).

mod common;

use common::{audit, domain, progress, rid, state, topics};
use spot_sync_catchup::coalesce;

const NODE: &str = "branch-1";

#[test]
fn state_events_collapse_to_the_latest_image_per_record() {
    let events = vec![state(101, "a", 1), state(102, "a", 2), state(103, "a", 3)];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 1);
    assert_eq!(coalesced.events[0].position, 103);
    assert_eq!(coalesced.events[0].subject_version, Some(3));
    assert_eq!(coalesced.superseded, 2);
    assert_eq!(coalesced.suppressed_audit, 0);
}

#[test]
fn the_latest_image_is_the_highest_version_even_when_it_is_older_in_position() {
    // Coalescing reorders: the version 3 image was written at 105, the version 2 image at 109.
    // The node needs the version 3 image, as it needs the state, not the history.
    let events = vec![state(105, "a", 3), state(109, "a", 2)];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 1);
    assert_eq!(coalesced.events[0].position, 105);
}

#[test]
fn different_records_do_not_collapse_into_each_other() {
    let events = vec![state(101, "a", 1), state(102, "b", 1)];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 2);
    assert_eq!(coalesced.superseded, 0);
}

#[test]
fn domain_events_are_kept_in_full() {
    let events = vec![
        domain(101, "a", 1),
        domain(102, "a", 2),
        domain(103, "a", 3),
    ];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 3);
    assert_eq!(coalesced.superseded, 0);
}

#[test]
fn a_domain_topic_that_declares_latest_per_subject_collapses_per_subject() {
    let events = vec![
        progress(101, "job-1", 20),
        progress(102, "job-1", 80),
        progress(103, "job-2", 10),
    ];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 2);
    assert_eq!(coalesced.events[0].position, 102);
    assert_eq!(coalesced.events[1].position, 103);
    assert_eq!(coalesced.superseded, 1);
}

#[test]
fn audit_events_reach_only_the_node_they_are_addressed_to() {
    let events = vec![
        audit(101, Some(NODE)),
        audit(102, Some("branch-2")),
        audit(103, None),
    ];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 1);
    assert_eq!(coalesced.events[0].position, 101);
    assert_eq!(coalesced.suppressed_audit, 2);
}

#[test]
fn audit_events_are_never_collapsed_even_addressed_to_the_same_node() {
    let events = vec![audit(101, Some(NODE)), audit(102, Some(NODE))];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 2);
    assert_eq!(coalesced.superseded, 0);
}

#[test]
fn a_mandatory_event_is_never_coalesced_away_or_suppressed() {
    let events = vec![
        state(101, "a", 1).mandatory(),
        state(105, "a", 4),
        audit(106, Some("branch-2")).mandatory(),
    ];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 3);
    assert_eq!(coalesced.superseded, 0);
    assert_eq!(coalesced.suppressed_audit, 0);
    assert_eq!(coalesced.events[0].position, 101);
    assert_eq!(coalesced.events[1].position, 105);
    assert_eq!(coalesced.events[2].position, 106);
}

#[test]
fn the_output_is_in_position_order_whatever_the_input_order() {
    let events = vec![state(103, "c", 1), state(101, "a", 1), state(102, "b", 1)];
    let coalesced = coalesce(&events, &topics(), NODE).unwrap();
    let positions: Vec<u64> = coalesced.events.iter().map(|e| e.position).collect();
    assert_eq!(positions, vec![101, 102, 103]);
}

#[test]
fn an_unregistered_topic_is_refused() {
    let mut unknown = state(101, "a", 1);
    unknown.topic = "made_up_topic".to_owned();
    assert_eq!(
        coalesce(&[unknown], &topics(), NODE).unwrap_err().code(),
        "sync_catchup.unknown_topic"
    );
}

#[test]
fn a_state_event_with_no_record_is_refused() {
    let mut subjectless = state(101, "a", 1);
    subjectless.subject = None;
    assert_eq!(
        coalesce(&[subjectless], &topics(), NODE)
            .unwrap_err()
            .code(),
        "sync_catchup.subject_missing"
    );
}

#[test]
fn a_domain_event_with_no_subject_is_kept() {
    let mut progress_without_subject = progress(101, "job-1", 1);
    progress_without_subject.subject = None;
    let coalesced = coalesce(&[progress_without_subject], &topics(), NODE).unwrap();
    assert_eq!(coalesced.events.len(), 1);
    assert_eq!(coalesced.events[0].subject, None);
    assert_eq!(rid("job-1").id, "job-1");
}
