//! The timeline builder: order, tie-break, summary keys (WP-DOC-07.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_activity::assignment::{Assignment, AssignmentAction, AssignmentStatus};
use spot_activity::comment::Comment;
use spot_activity::share::{Share, ShareRight};
use spot_activity::timeline::{CrateEvent, EntryKind, Timeline, TimelineEntry};
use spot_activity::version::Version;
use spot_app_api::Actor;
use spot_app_api::prelude::{CommandId, Value};

fn actor_user() -> Actor {
    Actor::User {
        id: uuid7(4, 4),
        via: None,
    }
}

fn command(s: &str) -> CommandId {
    CommandId::new(s).unwrap()
}

fn version(
    at_secs: i64,
    doc_version: u64,
    changes: Vec<spot_activity::diff::FieldChange>,
) -> Version {
    Version {
        doc_version: spot_app_api::prelude::Version(doc_version),
        at: at(at_secs),
        actor: actor_user(),
        command_id: Some(command("cmd-1")),
        changes,
    }
}

fn plain_change(path: &str, b: &str, a: &str) -> spot_activity::diff::FieldChange {
    spot_activity::diff::FieldChange {
        path: path.to_owned(),
        before: Some(Value::Text(b.to_owned())),
        after: Some(Value::Text(a.to_owned())),
    }
}

fn comment(at_secs: i64, author: &str, body: &str) -> Comment {
    Comment::new(uuid7(5, 1), uuid7(5, 2), author, at(at_secs), body)
}

fn assignment(at_secs: i64, assignee: &str) -> Assignment {
    Assignment::new_open(
        uuid7(6, 1),
        uuid7(6, 2),
        assignee,
        "boss",
        at(at_secs),
        None,
        &[],
    )
    .unwrap()
}

fn share(at_secs: i64, grantee: &str) -> Share {
    Share::new(
        uuid7(7, 1),
        grantee,
        [ShareRight::Read],
        "owner",
        at(at_secs),
        None,
    )
    .unwrap()
}

fn event(at_secs: i64, name: &str) -> CrateEvent {
    CrateEvent {
        at: at(at_secs),
        event: name.to_owned(),
        params: serde_json::json!({}),
        actor: actor_user(),
    }
}

fn kinds(entries: &[TimelineEntry]) -> Vec<EntryKind> {
    entries.iter().map(|e| e.kind).collect()
}

#[test]
fn entries_are_ordered_by_time() {
    let entries = Timeline::build(
        &[version(100, 3, vec![])],
        &[comment(50, "amy", "hi")],
        &[assignment(10, "bob")],
        &[share(5, "carol")],
        &[event(1, "doc_posted")],
    );
    // All inputs in the reverse order; the timeline sorts them.
    let times: Vec<i64> = entries
        .iter()
        .map(|e| e.at.unix_timestamp() - NOW)
        .collect();
    assert_eq!(times, vec![1, 5, 10, 50, 100], "{entries:?}");
}

#[test]
fn the_tie_break_is_kind_then_id() {
    // Same instant: version first, then comment, assignment, share, event.
    let entries = Timeline::build(
        &[version(10, 3, vec![])],
        &[comment(10, "amy", "hi")],
        &[assignment(10, "bob")],
        &[share(10, "carol")],
        &[event(10, "doc_posted")],
    );
    assert_eq!(
        kinds(&entries),
        vec![
            EntryKind::Version,
            EntryKind::Comment,
            EntryKind::Assignment,
            EntryKind::Share,
            EntryKind::Event,
        ],
        "{entries:?}"
    );
}

#[test]
fn two_versions_at_one_instant_keep_version_order() {
    let v1 = version(10, 1, vec![]);
    let v2 = version(10, 2, vec![]);
    let entries = Timeline::build(&[v2.clone(), v1.clone()], &[], &[], &[], &[]);
    assert_eq!(
        entries
            .iter()
            .map(|e| e.ref_id.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "2"],
        "the saved order decides, not list order"
    );
}

#[test]
fn the_same_inputs_build_the_same_list() {
    let a = Timeline::build(
        &[version(1, 1, vec![])],
        &[comment(2, "amy", "hi")],
        &[assignment(3, "bob")],
        &[share(4, "carol")],
        &[event(5, "doc_posted")],
    );
    let b = Timeline::build(
        &[version(1, 1, vec![])],
        &[comment(2, "amy", "hi")],
        &[assignment(3, "bob")],
        &[share(4, "carol")],
        &[event(5, "doc_posted")],
    );
    assert_eq!(a, b);
}

#[test]
fn entries_carry_keys_and_parameters_never_text() {
    let v = version(10, 3, vec![plain_change("number", "SI-1", "SI-2")]);
    let entries = Timeline::build(&[v], &[], &[], &[], &[]);
    assert_eq!(entries[0].kind, EntryKind::Version);
    assert_eq!(entries[0].summary_key, "activity.version_changed");
    let changes = &entries[0].summary_params["changes"];
    assert_eq!(changes[0]["path"], "number");
    assert_eq!(changes[0]["before"], "SI-1");
    assert_eq!(changes[0]["after"], "SI-2");

    let c = comment(10, "amy", "hi @bob");
    let entries = Timeline::build(&[], &[c], &[], &[], &[]);
    assert_eq!(entries[0].summary_key, "activity.commented");
    assert_eq!(entries[0].summary_params["author"], "amy");

    let a = assignment(10, "bob");
    let entries = Timeline::build(&[], &[], &[a], &[], &[]);
    assert_eq!(entries[0].summary_key, "activity.assignment.assigned");

    let s = share(10, "carol");
    let entries = Timeline::build(&[], &[], &[], &[s], &[]);
    assert_eq!(entries[0].summary_key, "activity.shared");

    let e = event(10, "doc_posted");
    let entries = Timeline::build(&[], &[], &[], &[], &[e]);
    assert_eq!(entries[0].summary_key, "activity.event.doc_posted");

    // A comment's body is a parameter of its entry? No: the body renders where the comment
    // record itself renders; the timeline parameters carry the author and mentions.
    let c = comment(10, "amy", "hi");
    let entries = Timeline::build(&[], &[c], &[], &[], &[]);
    assert!(entries[0].summary_params.get("body").is_none());
}

#[test]
fn an_assignment_line_shows_the_record_state() {
    let mut a = assignment(10, "bob");
    a.apply(AssignmentAction::Complete).unwrap();
    assert_eq!(a.status, AssignmentStatus::Done);
    // The timeline carries the creation; the caller renders status changes from the record.
    let entries = Timeline::build(&[], &[], &[a], &[], &[]);
    assert_eq!(entries[0].ref_id, uuid7(6, 1).to_string());
}

#[test]
fn empty_inputs_build_an_empty_timeline() {
    assert!(Timeline::build(&[], &[], &[], &[], &[]).is_empty());
}

#[test]
fn ref_ids_map_back_to_records() {
    let c = comment(10, "amy", "hi");
    let id = c.id;
    let entries = Timeline::build(&[], &[c], &[], &[], &[]);
    assert_eq!(entries[0].ref_id, id.to_string());
}

#[test]
fn entry_serde_round_trip() {
    let entries = Timeline::build(
        &[version(10, 3, vec![])],
        &[comment(20, "amy", "hi")],
        &[],
        &[],
        &[],
    );
    let json = serde_json::to_string(&entries).unwrap();
    let back: Vec<TimelineEntry> = serde_json::from_str(&json).unwrap();
    assert_eq!(back, entries);
}

#[test]
fn comment_body_is_not_in_summary_params() {
    // A comment's body renders where the comment record itself renders; the timeline carries
    // the author and mentions, and the body stays out of the summary parameters (it can be
    // long and quoted; the record holds it).
    let c = comment(10, "amy", "an unquoted body");
    let entries = Timeline::build(&[], &[c], &[], &[], &[]);
    assert!(entries[0].summary_params.get("body").is_none());
}
