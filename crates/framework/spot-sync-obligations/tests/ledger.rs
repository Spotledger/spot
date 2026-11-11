//! Outstanding, deadlines, lane order, evidence and the report.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_sync_obligations::{
    Crossing, Lane, MandatoryEvent, NodeState, Obligation, ObligationError, ObligationSettings,
    Pending, ReportStatus, due_by, evidence, lane_order, outstanding, report,
};

const H: i64 = 3_600_000;

fn ev(position: u64, published_h: i64, effective_h: Option<i64>) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at_millis: published_h * H,
        effective_from_millis: effective_h.map(|h| h * H),
        record: format!("rate/v{position}"),
    }
}

fn ob(e: MandatoryEvent) -> Obligation {
    Obligation {
        event: e,
        held: false,
    }
}

#[test]
fn outstanding_is_above_prefix_in_order() {
    let events = vec![ev(5, 0, None), ev(3, 0, None), ev(9, 0, None)];
    let got: Vec<u64> = outstanding(&events, 4, &[])
        .unwrap()
        .iter()
        .map(|o| o.event.position)
        .collect();
    assert_eq!(got, vec![5, 9]);
}

#[test]
fn held_records_count_as_outstanding_below_the_prefix() {
    let events = vec![ev(3, 0, None), ev(5, 0, None), ev(9, 0, None)];
    let got = outstanding(&events, 6, &[3]).unwrap();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].event.position, got[0].held), (3, true));
    assert_eq!((got[1].event.position, got[1].held), (9, false));
}

#[test]
fn hold_above_prefix_is_not_duplicated() {
    let events = vec![ev(5, 0, None)];
    let got = outstanding(&events, 1, &[5, 5]).unwrap();
    assert_eq!(got.len(), 1);
    assert!(!got[0].held);
}

#[test]
fn unknown_hold_is_an_error() {
    let err = outstanding(&[ev(1, 0, None)], 0, &[7]).unwrap_err();
    assert_eq!(err, ObligationError::HoldUnknown(7));
    assert_eq!(err.code(), "sync_obligations.hold_unknown");
}

#[test]
fn due_by_is_published_plus_window() {
    let s = ObligationSettings::default();
    assert_eq!(due_by(&ob(ev(1, 10, None)), &s), 34 * H);
}

#[test]
fn effective_date_earlier_than_window_wins() {
    let s = ObligationSettings::default();
    // published 0h, window ends 24h; effective 30h minus 12h margin = 18h, earlier.
    assert_eq!(due_by(&ob(ev(1, 0, Some(30))), &s), 18 * H);
}

#[test]
fn window_earlier_than_effective_date_wins() {
    let s = ObligationSettings::default();
    // effective 100h minus 12h = 88h, later than 24h.
    assert_eq!(due_by(&ob(ev(1, 0, Some(100))), &s), 24 * H);
}

#[test]
fn effective_date_already_passed_gives_a_past_deadline() {
    let s = ObligationSettings::default();
    // effective at 5h, published 10h: due = -7h relative... 5h - 12h = -7h.
    assert_eq!(due_by(&ob(ev(1, 10, Some(5))), &s), -7 * H);
}

#[test]
fn due_by_saturates() {
    let s = ObligationSettings::default();
    let mut e = ev(1, 0, None);
    e.published_at_millis = i64::MAX;
    assert_eq!(due_by(&ob(e), &s), i64::MAX);
}

#[test]
fn lanes_sort_mandatory_first_then_position_and_are_stable() {
    let p = |lane, position, item: &'static str| Pending {
        lane,
        position,
        item,
    };
    let sorted = lane_order(vec![
        p(Lane::Bulk, 1, "b1"),
        p(Lane::Mandatory, 9, "m9"),
        p(Lane::Mandatory, 2, "m2a"),
        p(Lane::Bulk, 0, "b0"),
        p(Lane::Mandatory, 2, "m2b"),
    ]);
    let names: Vec<&str> = sorted.iter().map(|x| x.item).collect();
    assert_eq!(names, vec!["m2a", "m2b", "m9", "b0", "b1"]);
}

#[test]
fn evidence_only_on_crossing() {
    let events = vec![ev(3, 0, None), ev(6, 0, None), ev(9, 0, None)];
    assert_eq!(evidence("n1", 6, 8, &events, 1, 2).unwrap(), None);
    assert_eq!(evidence("n1", 4, 4, &events, 1, 2).unwrap(), None);
    let e = evidence("n1", 2, 6, &events, 100, 200).unwrap().unwrap();
    assert_eq!((e.node.as_str(), e.through_position, e.count), ("n1", 6, 2));
    assert_eq!(
        (e.applied_at_reported_millis, e.received_at_observed_millis),
        (100, 200)
    );
}

#[test]
fn evidence_prefix_must_not_regress() {
    let err = evidence("n1", 5, 4, &[], 0, 0).unwrap_err();
    assert_eq!(err.code(), "sync_obligations.prefix_regressed");
}

fn node(name: &str, prefix: u64, holds: Vec<u64>, crossings: Vec<Crossing>) -> NodeState {
    NodeState {
        node: name.into(),
        applied_prefix: prefix,
        holds,
        last_seen_millis: 77,
        crossings,
    }
}

#[test]
fn report_distinguishes_applied_and_outstanding_nodes() {
    let events = vec![ev(4, 5, None), ev(8, 6, None)];
    let rows = report(
        &events,
        &[
            node("b2", 5, vec![], vec![]),
            node(
                "b1",
                8,
                vec![],
                vec![
                    Crossing {
                        through_position: 4,
                        applied_at_millis: 10,
                    },
                    Crossing {
                        through_position: 8,
                        applied_at_millis: 30,
                    },
                    Crossing {
                        through_position: 9,
                        applied_at_millis: 20,
                    },
                ],
            ),
            node("b3", 9, vec![4], vec![]),
            node("b4", 9, vec![], vec![]),
        ],
    );
    let by: Vec<(&str, ReportStatus)> = rows
        .iter()
        .map(|r| (r.node.as_str(), r.status.clone()))
        .collect();
    assert_eq!(by[0], ("b1", ReportStatus::AppliedAt(20)));
    assert_eq!(
        by[1],
        (
            "b2",
            ReportStatus::OutstandingSince {
                since_millis: 6 * H,
                last_seen_millis: 77
            }
        )
    );
    assert_eq!(
        by[2],
        (
            "b3",
            ReportStatus::OutstandingSince {
                since_millis: 5 * H,
                last_seen_millis: 77
            }
        )
    );
    assert_eq!(by[3], ("b4", ReportStatus::AppliedUnrecorded));
}

#[test]
fn report_with_no_events_is_empty() {
    assert!(report(&[], &[node("b1", 0, vec![], vec![])]).is_empty());
}
