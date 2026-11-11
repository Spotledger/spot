//! Fixtures shared by the campaign tests: spec and item builders, assertion helpers and
//! a tiny CSV reader for the quoting round trip.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_access_review::{
    AssignmentRef, Campaign, CampaignSpec, DeadlinePolicy, Decision, Outcome, OverrideNote,
    ReviewError, ReviewEvent, ReviewItem, ReviewItemState,
};

/// A spec with the fixed ids the tests use.
pub fn spec(deadline: i64, policy: DeadlinePolicy) -> CampaignSpec {
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

/// A plain item: assignment `a-<id>`, direct source, no override.
pub fn simple_item(id: &str, user: &str, reviewer: &str, last_use: Option<i64>) -> ReviewItem {
    ReviewItem {
        id: id.to_string(),
        user: user.to_string(),
        assignment: AssignmentRef::new(format!("assign-{id}")),
        source: "direct".to_string(),
        last_use,
        sod_override: None,
        reviewer: reviewer.to_string(),
    }
}

/// An item whose reviewer is its user — the own-access case.
pub fn own_access_item(id: &str, who: &str) -> ReviewItem {
    ReviewItem {
        id: id.to_string(),
        user: who.to_string(),
        assignment: AssignmentRef::new(format!("assign-{id}")),
        source: "direct".to_string(),
        last_use: Some(900),
        sod_override: Some(OverrideNote::new("mitigating control M1".to_string())),
        reviewer: who.to_string(),
    }
}

/// An item whose assignment reference is blank (the validation case).
pub fn blank_assignment_item(id: &str, user: &str, reviewer: &str) -> ReviewItem {
    ReviewItem {
        assignment: AssignmentRef::new("   "),
        ..simple_item(id, user, reviewer, None)
    }
}

/// The note on i1 in the history fixture (a comma, for the CSV quoting).
pub const COMMA_NOTE: &str = "note, with comma";
/// The note on i3 in the history fixture (a quote and a newline).
pub const HARD_NOTE: &str = "has \"quotes\" and\nnewlines";

/// Two plain items, one per reviewer r1/r2, both stale (never used).
pub fn two_items() -> Vec<ReviewItem> {
    vec![
        simple_item("i1", "u1", "r1", None),
        simple_item("i2", "u2", "r2", None),
    ]
}

/// A keep without a note (fresh) — the ordinary keep.
pub fn keep() -> Decision {
    Decision::Keep { note: None }
}

/// A keep with the note a stale or overridden item owes.
pub fn keep_with_note(note: &str) -> Decision {
    Decision::Keep {
        note: Some(note.to_string()),
    }
}

/// A removal with a concrete reason.
pub fn remove(reason: &str) -> Decision {
    Decision::Remove {
        reason: reason.to_string(),
    }
}

/// Opens a campaign the tests need open; panics with the error otherwise (a fixture
/// failure, not a behaviour under test).
pub fn open_ok(s: CampaignSpec, items: Vec<ReviewItem>) -> Campaign {
    Campaign::open(s, items, 1_000).expect("fixture: valid campaign")
}

/// Asserts an open error names the expected rule.
pub fn assert_invalid(result: Result<Campaign, ReviewError>, needle: &str) {
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected an error naming `{needle}`"),
    };
    assert!(
        err.to_string().contains(needle),
        "error `{err}` should mention `{needle}`"
    );
    assert!(
        err.code().starts_with("review."),
        "error code must be in the review.* family"
    );
}

/// Asserts a decide error names the expected rule.
pub fn assert_invalid_decision(result: Result<Outcome, ReviewError>, needle: &str) {
    let err = match result {
        Err(e) => e,
        Ok(_) => panic!("expected an error naming `{needle}`"),
    };
    assert!(
        err.to_string().contains(needle),
        "error `{err}` should mention `{needle}`"
    );
}

/// How many events in the outcome match the predicate.
pub fn count(outcome: &Outcome, pred: impl Fn(&ReviewEvent) -> bool) -> usize {
    outcome.events.iter().filter(|e| pred(e)).count()
}

/// An own-access campaign: one item whose reviewer is its user, plus one plain item.
/// Returns the campaign and the opening outcomes' placeholder (the opening events live on
/// the campaign now — [`Campaign::opening_events`]).
pub fn own_access_campaign() -> (Campaign, Vec<Outcome>) {
    let s = spec(2_000, DeadlinePolicy::LeaveOpen);
    let items = vec![
        own_access_item("i1", "self-reviewer"),
        simple_item("i2", "u2", "r2", None),
    ];
    let c = open_ok(s, items);
    (c, Vec::new())
}

/// A two-item campaign fully decided by hand: kept flagged, removed, kept flagged.
/// i1: stale keep with a comma'd note. i2: removed ("moved to another team").
/// i3 kept with the hardest note (quote and newline).
pub fn completed_campaign_with_history() -> (Campaign, Vec<Outcome>) {
    let s = spec(2_000, DeadlinePolicy::LeaveOpen);
    // i3 is stale (never used) and carries a SoD override: the hardest report/CSV row.
    let mut i3 = simple_item("i3", "u3", "r1", None);
    i3.sod_override = Some(OverrideNote::new("mitigating control M2".to_string()));
    let mut c = open_ok(
        s,
        vec![
            simple_item("i1", "u1", "r1", None),
            simple_item("i2", "u2", "r2", None),
            i3,
        ],
    );
    let outcomes = vec![
        c.decide(
            "i1",
            "r1",
            Decision::Keep {
                note: Some(COMMA_NOTE.to_string()),
            },
            1_100,
        )
        .expect("keep i1"),
        c.decide("i2", "r2", remove("moved to another team"), 1_150)
            .expect("remove i2"),
        c.decide(
            "i3",
            "r1",
            Decision::Keep {
                note: Some(HARD_NOTE.to_string()),
            },
            1_200,
        )
        .expect("keep i3"),
    ];
    for o in &outcomes {
        assert!(
            !o.events.is_empty() || !o.actions.is_empty(),
            "fixture step produced nothing"
        );
    }
    let _ = ReviewItemState::Pending { escalated_to: None };
    (c, outcomes)
}

/// A simple two-item campaign fully decided: i1 kept (with its note — it is stale), i2
/// removed; then completed.
pub fn completed_campaign() -> Campaign {
    let mut c = open_ok(spec(2_000, DeadlinePolicy::LeaveOpen), two_items());
    c.decide("i1", "r1", keep_with_note("still needed"), 1_100)
        .expect("keep");
    c.decide("i2", "r2", remove("done"), 1_150).expect("remove");
    assert!(c.is_completed());
    c
}

/// A minimal CSV reader for the quoting tests: fields are comma-separated, a field wrapped
/// in doubles quotes may contain commas, doubled quotes and newlines (RFC 4180).
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut field = String::new();
    let mut row: Vec<String> = Vec::new();
    let mut in_quotes = false;
    let bytes: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if in_quotes {
            if c == '"' {
                if i + 1 < bytes.len() && bytes[i + 1] == '"' {
                    field.push('"');
                    i += 2;
                    continue;
                }
                in_quotes = false;
                i += 1;
                continue;
            }
            field.push(c);
            i += 1;
        } else {
            match c {
                '"' => {
                    in_quotes = true;
                    i += 1;
                }
                ',' => {
                    row.push(std::mem::take(&mut field));
                    i += 1;
                }
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    if !(row.len() == 1 && row[0].is_empty()) {
                        rows.push(std::mem::take(&mut row));
                    }
                    i += 1;
                }
                '\r' => {
                    i += 1;
                }
                _ => {
                    field.push(c);
                    i += 1;
                }
            }
        }
    }
    row.push(std::mem::take(&mut field));
    if !(row.len() == 1 && row[0].is_empty()) {
        rows.push(row);
    }
    rows
}
