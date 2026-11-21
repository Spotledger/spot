//! `human_approvers` applies the `is_person` guard itself (WP-BPM-05.T11): a direct caller never
//! gets a blank, invisible or system actor.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::now;
use spot_bpm_tasks::{Decision, TaskAction, TaskEvent, TaskId, human_approvers, is_person};

fn approval(actor: &str) -> TaskEvent {
    let task = TaskId::new("t1").expect("id");
    let mut event = TaskEvent::new(&task, TaskAction::Completed, actor, now());
    event.choice = Some(Decision::Approve);
    event
}

#[test]
fn blank_invisible_and_system_actors_are_never_approvers() {
    for actor in [
        "",
        " ",
        "\u{00A0}",
        "\u{2028}",
        "\u{200B}",
        "system:sla",
        "System:SLA",
        "system:x",
        " SYSTEM:x",
    ] {
        assert!(!is_person(actor), "{actor:?} must not be a person");
        assert!(
            human_approvers(&[approval(actor)]).is_empty(),
            "{actor:?} must not be an approver"
        );
    }
}

#[test]
fn mixed_history_returns_exactly_the_people() {
    let events = [
        approval(" "),
        approval("system:sla"),
        approval("ann"),
        approval("bob"),
    ];
    let got: Vec<String> = human_approvers(&events).into_iter().collect();
    assert_eq!(got, vec!["ann".to_owned(), "bob".to_owned()]);
}

#[test]
fn padded_names_are_returned_trimmed_and_merge() {
    let got: Vec<String> = human_approvers(&[approval(" ann"), approval("ann ")])
        .into_iter()
        .collect();
    assert_eq!(got, vec!["ann".to_owned()]);
    let got: Vec<String> = human_approvers(&[approval(" ann"), approval("ann"), approval("Ann")])
        .into_iter()
        .collect();
    assert_eq!(
        got,
        vec!["Ann".to_owned(), "ann".to_owned()],
        "case is not folded"
    );
}

#[test]
fn a_very_long_actor_is_returned_once() {
    let long = "a".repeat(10_000);
    let got = human_approvers(&[approval(&long), approval(&long)]);
    assert_eq!(got.len(), 1);
    assert!(got.contains(&long));
}
