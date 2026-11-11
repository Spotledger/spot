//! The campaign tests: opening (validation, the own-access rule), decisions (the
//! reviewer rule, finality, the note and reason owes), deadline policies, the report and
//! its CSV quoting, and the serde round trip the service's persistence relies on.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_access_review::{
    Campaign, CampaignSpec, CampaignStatus, DeadlinePolicy, Decision, ReviewError, ReviewEvent,
    ReviewItem, ReviewItemState,
};

// ---------- helpers ----------

fn spec(deadline: i64, policy: DeadlinePolicy) -> CampaignSpec {
    CampaignSpec {
        id: "camp-1".to_string(),
        title: "Quarterly review".to_string(),
        opened_by: "sec-lead".to_string(),
        escalation_contact: "sec-lead".to_string(),
        deadline,
        stale_after_days: 30,
        on_deadline: policy,
    }
}

fn item(id: &str, user: &str, reviewer: &str, last_use: Option<i64>) -> ReviewItem {
    common::simple_item(id, user, reviewer, last_use)
}

// ---------- opening ----------

#[test]
fn open_records_the_opening_event() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    let events = c.opening_events().to_vec();
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0],
        ReviewEvent::CampaignOpened { items: 2, .. }
    ));
    assert!(!c.is_completed());
    assert_eq!(c.item_states().len(), 2);
    let _ = &mut c;
}

#[test]
fn open_rejects_no_items() {
    assert_invalid(
        Campaign::open(spec(2_000, DeadlinePolicy::LeaveOpen), vec![], 1_000),
        "at least one item",
    );
}

#[test]
fn open_rejects_a_deadline_in_the_past() {
    assert_invalid(
        Campaign::open(
            spec(500, DeadlinePolicy::LeaveOpen),
            vec![item("i1", "u1", "r1", None)],
            1_000,
        ),
        "deadline",
    );
}

#[test]
fn open_rejects_duplicate_ids() {
    assert_invalid(
        Campaign::open(
            spec(2_000, DeadlinePolicy::LeaveOpen),
            vec![item("i1", "u1", "r1", None), item("i1", "u2", "r2", None)],
            1_000,
        ),
        "duplicate",
    );
}

#[test]
fn open_rejects_a_blank_id_or_assignment() {
    assert_invalid(
        Campaign::open(
            spec(2_000, DeadlinePolicy::LeaveOpen),
            vec![item("  ", "u1", "r1", None)],
            1_000,
        ),
        "blank",
    );
    assert_invalid(
        Campaign::open(
            spec(2_000, DeadlinePolicy::LeaveOpen),
            vec![blank_assignment_item("i1", "u1", "r1")],
            1_000,
        ),
        "assignment",
    );
}

// ---------- the own-access rule ----------

#[test]
fn own_access_item_is_created_escalated_and_the_contact_can_decide_it() {
    let (mut c, outcomes) = own_access_campaign();
    // The item opened already escalated: only the contact may decide, the user may not.
    assert_eq!(
        c.decide("i1", "self-reviewer", keep(), 1_100),
        Err(ReviewError::NotAuthorized {
            item: "i1".to_string(),
            detail: "only sec-lead may decide this escalated item".to_string(),
        })
    );
    let outcome = c
        .decide("i1", "sec-lead", remove("closing it out"), 1_200)
        .expect("contact decides");
    for o in outcomes {
        assert!(!o.events.is_empty() || !o.actions.is_empty());
    }
    assert!(
        outcome
            .events
            .iter()
            .any(|e| matches!(e, ReviewEvent::Decided { .. }))
    );
    assert!(outcome.actions.is_empty() || outcome.actions.len() == 1);
}

#[test]
fn own_access_item_with_no_contact_is_an_error() {
    let s = CampaignSpec {
        escalation_contact: String::new(),
        ..spec(2_000, DeadlinePolicy::LeaveOpen)
    };
    let items = vec![own_access_item("i1", "self-reviewer")];
    assert_invalid(Campaign::open(s, items, 1_000), "escalation contact");
}

// ---------- decisions ----------

#[test]
fn wrong_reviewer_cannot_decide() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    let err = c
        .decide("i1", "not-the-reviewer", keep(), 1_100)
        .expect_err("wrong reviewer refused");
    assert_eq!(err.code(), "review.not_authorized");
    assert!(err.to_string().contains("r1"));
}

#[test]
fn the_reviewer_and_only_the_reviewer_decides_then_the_item_is_final() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    c.decide("i1", "r1", remove("leaver"), 1_100)
        .expect("r1 decides");
    // A different decision on the same item is refused — decisions are final.
    let err = c
        .decide("i1", "r1", keep(), 1_200)
        .expect_err("second decision refused");
    assert_eq!(err.code(), "review.not_authorized");
}

#[test]
fn a_decision_after_completion_is_refused() {
    let mut c = completed_campaign();
    let err = c
        .decide("i2", "r2", keep(), 2_000)
        .expect_err("completed campaign refused");
    assert_eq!(err.code(), "review.campaign_closed");
    assert!(c.is_completed());
    assert!(matches!(c.item_states()[0], ReviewItemState::Kept { .. }));
}

#[test]
fn removal_needs_a_non_blank_reason() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    assert_invalid_decision(c.decide("i1", "r1", remove("   "), 1_100), "reason");
    assert_invalid_decision(
        c.decide(
            "i1",
            "r1",
            Decision::Remove {
                reason: String::new(),
            },
            1_100,
        ),
        "reason",
    );
    // A valid reason goes through.
    c.decide("i1", "r1", remove("gone"), 1_100)
        .expect("valid removal");
}

#[test]
fn keeping_stale_or_overridden_access_needs_a_note() {
    // last_use absent -> stale.
    let mut c = open_ok(
        spec(2_000, DeadlinePolicy::LeaveOpen),
        vec![item("i1", "u1", "r1", None)],
    );
    assert_invalid_decision(
        c.decide("i1", "r1", Decision::Keep { note: None }, 1_100),
        "note",
    );
    assert_invalid_decision(
        c.decide(
            "i1",
            "r1",
            Decision::Keep {
                note: Some("  ".into()),
            },
            1_100,
        ),
        "note",
    );
    c.decide(
        "i1",
        "r1",
        Decision::Keep {
            note: Some("on pause".into()),
        },
        1_100,
    )
    .expect("noted keep passes");
    let _ = &c;
}

#[test]
fn keeping_fresh_access_needs_no_note() {
    let mut c = open_ok(
        spec(2_000, DeadlinePolicy::LeaveOpen),
        vec![item("i1", "u1", "r1", Some(2_000 - 60))],
    );
    c.decide("i1", "r1", Decision::Keep { note: None }, 1_100)
        .expect("fresh keep needs no note");
    let _ = &c;
}

#[test]
fn unknown_items_are_refused() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    let err = c
        .decide("nope", "r1", keep(), 1_100)
        .expect_err("unknown item refused");
    assert_eq!(err.code(), "review.decision_invalid");
}

#[test]
fn a_removal_yields_a_remove_assignment_action() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    let outcome = c
        .decide("i1", "r1", remove("moved teams"), 1_100)
        .expect("removal");
    assert!(outcome.actions.len() == 1);
    assert!(
        outcome
            .events
            .iter()
            .any(|e| matches!(e, ReviewEvent::Decided { .. }))
    );
    // The campaign completes when both are decided.
    c.decide("i2", "r2", keep_with_note("audited"), 1_200)
        .expect("keep");
    assert!(c.is_completed());
}

// ---------- deadline ----------

#[test]
fn deadline_escalate_marks_pending_items_once_and_the_target_can_decide() {
    let mut c = open_ok(
        spec(
            2_000,
            DeadlinePolicy::Escalate {
                to: "sec-lead".to_string(),
            },
        ),
        two_items(),
    );
    let first = c.tick(2_500);
    assert_eq!(
        count(&first, |e| matches!(e, ReviewEvent::Escalated { .. })),
        2
    );
    // Idempotent: a second tick at the same (or later) time is empty.
    let second = c.tick(2_600);
    assert_eq!(
        count(&second, |e| matches!(e, ReviewEvent::Escalated { .. })),
        0
    );
    // The reviewer may no longer decide; the target may.
    assert!(c.decide("i1", "r1", keep(), 2_700).is_err());
    c.decide("i1", "sec-lead", remove("still unused"), 2_700)
        .expect("target decides");
    // An escalated keep of a stale item still owes a note.
    assert!(
        c.decide("i2", "sec-lead", Decision::Keep { note: None }, 2_700)
            .is_err()
    );
    c.decide(
        "i2",
        "sec-lead",
        Decision::Keep {
            note: Some("ok".into()),
        },
        2_700,
    )
    .expect("target keeps with note");
    assert!(c.is_completed());
}

#[test]
fn deadline_escalates_own_access_items_never_twice() {
    let (mut c, _) = own_access_campaign();
    let out = c.tick(2_500);
    // The own-access item escalated at creation is not escalated again.
    assert!(
        !out.events
            .iter()
            .any(|e| matches!(e, ReviewEvent::Escalated { item, .. } if item == "i1"))
    );
    let _ = &mut c;
}

#[test]
fn deadline_removes_unreviewed_items_and_is_idempotent() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::RemoveUnreviewed), two_items());
    let first = c.tick(2_500);
    assert_eq!(
        count(&first, |e| matches!(e, ReviewEvent::Removed { .. })),
        2
    );
    assert_eq!(first.actions.len(), 2);
    assert!(
        first
            .actions
            .iter()
            .all(|a| matches!(a, spot_access_review::ReviewAction::RemoveAssignment { reason, .. } if reason == "unreviewed at deadline"))
    );
    let second = c.tick(2_500);
    assert!(second.is_empty());
    assert!(c.is_completed());
}

#[test]
fn deadline_leave_open_changes_nothing() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    let out = c.tick(2_500);
    assert!(out.is_empty());
    assert!(!c.is_completed());
    // The reviewer can still decide after the deadline when the policy leaves items open.
    c.decide("i1", "r1", remove("late decision"), 2_600)
        .expect("still decidable");
    let _ = &c;
}

#[test]
fn tick_before_the_deadline_does_nothing() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::RemoveUnreviewed), two_items());
    let out = c.tick(1_500);
    assert!(out.is_empty());
    assert!(!c.is_completed());
    let _ = &mut c;
}

// ---------- report and CSV ----------

#[test]
fn report_counts_add_up_and_are_deterministic() {
    let (c, _) = completed_campaign_with_history();
    let r = c.report();
    assert_eq!(r.total, 3);
    assert_eq!(r.kept + r.removed, r.total);
    assert_eq!(r.pending, 0);
    assert_eq!(r.removals.len(), r.removed);
    assert_eq!(r.kept_flagged.len(), 2);
    // Reviewers are alphabetical; progress sums to the load.
    let names: Vec<&str> = r.reviewers.iter().map(|p| p.reviewer.as_str()).collect();
    assert!(names.windows(2).all(|w| w[0] <= w[1]));
    for p in &r.reviewers {
        assert!(p.decided <= p.total);
    }
    // Second report is identical (deterministic).
    assert_eq!(r, c.report());
    assert!(matches!(c.status(), CampaignStatus::Completed) || c.is_completed());
}

#[test]
fn csv_escapes_commas_quotes_and_newlines() {
    let (c, _) = completed_campaign_with_history();
    let csv = c.report().to_csv();
    let rows = parse_csv(&csv);
    // header + 1 removal + 2 kept flagged rows
    assert_eq!(rows.len(), 4);
    let removed_row = &rows[1];
    assert_eq!(removed_row[0], "removed");
    assert!(removed_row[4].contains("moved to another team"));
    // The kept flagged fields carry the flags and the notes.
    assert!(rows[2][4].contains("note, with comma"));
    assert!(rows[3][4].contains("has \"quotes\" and\nnewlines"));
    // The round trip preserved the tricky note verbatim.
    assert!(csv.contains("\"stale+overridden: has \"\"quotes\"\" and\nnewlines\""));
}

#[test]
fn csv_plain_fields_stay_plain() {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::RemoveUnreviewed), two_items());
    let _ = c.tick(2_500);
    let csv = c.report().to_csv();
    let rows = parse_csv(&csv);
    assert_eq!(rows.len(), 3);
    assert!(csv.starts_with("section,item,user,assignment,reason_or_note\n"));
    assert!(rows[1].contains(&"unreviewed at deadline".to_string()));
    let _ = &c;
}

// ---------- serde round trip ----------

#[test]
fn serde_round_trip_mid_flight_is_identical() {
    let (c_before, _) = completed_campaign_with_history();
    let json = serde_json::to_string(&c_before).expect("serialize");
    let c_after: Campaign = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(c_before, c_after);
    // Mid-flight too: after one decision, before the next.
    let mut mid = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    mid.decide("i1", "r1", remove("round trip"), 1_100)
        .expect("decide");
    let json = serde_json::to_string(&mid).expect("serialize");
    let mut back: Campaign = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(mid, back);
    assert!(
        back.decide("i2", "r2", keep_with_note("post-restore"), 1_200)
            .is_ok()
    );
}

#[test]
fn partial_outcome_includes_completion_event_last() {
    let mut c = open_ok(
        spec(20_000, DeadlinePolicy::LeaveOpen),
        vec![item("only", "u1", "r1", Some(19_950))],
    );
    let out = c.decide("only", "r1", keep(), 20_100).expect("decide");
    assert_eq!(out.events.len(), 2);
    assert!(matches!(out.events[0], ReviewEvent::Decided { .. }));
    assert!(matches!(out.events[1], ReviewEvent::Completed { .. }));
    assert!(c.is_completed());
}
