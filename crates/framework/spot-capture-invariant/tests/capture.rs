//! The capture invariant: one seeded case per finding kind, exclusions, ordering, bounded memory
//! and the shuffle property (WP-JRN-02.T1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use proptest::prelude::*;
use spot_capture_invariant::{
    CaptureInvariantError, Checker, Exclusions, FeedEntry, FindingKind, Op, Position, StateEvent,
};

fn pos(seq: u64) -> Position {
    Position { generation: 1, seq }
}

fn feed(seq: u64, entity: &str, id: &str, version: u64, op: Op) -> FeedEntry {
    FeedEntry {
        position: pos(seq),
        entity: entity.into(),
        id: id.into(),
        version,
        op,
    }
}

fn event(seq: u64, entity: &str, id: &str, version: u64, op: Op) -> StateEvent {
    StateEvent {
        position: pos(seq),
        entity: entity.into(),
        id: id.into(),
        version,
        op,
    }
}

fn checker() -> Checker {
    Checker::new(Exclusions::new(), 16).unwrap()
}

#[test]
fn clean_streams_are_clean() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_event(event(2, "Invoice", "I-2", 1, Op::Insert))
        .unwrap();
    c.push_feed(feed(2, "Invoice", "I-2", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    assert!(r.is_clean());
    assert_eq!(r.matched, 2);
    assert_eq!(r.count(FindingKind::MissingStateEvent), 0);
}

#[test]
fn a_write_that_bypassed_capture_is_missing_state_event() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_feed(feed(2, "Invoice", "I-2", 1, Op::Insert))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    assert!(!r.is_clean());
    assert_eq!(r.findings.len(), 1);
    let f = &r.findings[0];
    assert_eq!(f.kind, FindingKind::MissingStateEvent);
    assert_eq!(
        (f.entity.as_str(), f.id.as_str(), f.version),
        ("Invoice", "I-2", 1)
    );
    assert_eq!(f.feed_position, Some(pos(2)));
    assert_eq!(f.event_position, None);
}

#[test]
fn an_event_without_a_feed_entry_is_an_orphan() {
    let mut c = checker();
    c.push_event(event(1, "Invoice", "I-9", 3, Op::Update))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.count(FindingKind::OrphanStateEvent), 1);
    assert_eq!(r.findings[0].event_position, Some(pos(1)));
}

#[test]
fn a_second_event_is_a_duplicate_before_and_after_the_match() {
    // Both events before the feed entry.
    let mut c = checker();
    c.push_event(event(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_event(event(2, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_feed(feed(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].kind, FindingKind::DuplicateStateEvent);
    assert_eq!(r.findings[0].first_event_position, Some(pos(1)));
    assert_eq!(r.findings[0].event_position, Some(pos(2)));

    // The duplicate arrives after the pair matched.
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    c.push_event(event(2, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.count(FindingKind::DuplicateStateEvent), 1);
    assert_eq!(r.findings[0].feed_position, Some(pos(1)));
    assert_eq!(r.count(FindingKind::MissingStateEvent), 0);
}

#[test]
fn a_different_operation_is_a_miss_plus_an_orphan() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 2, Op::Delete))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-1", 2, Op::Update))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.findings.len(), 2);
    assert_eq!(r.count(FindingKind::MissingStateEvent), 1);
    assert_eq!(r.count(FindingKind::OrphanStateEvent), 1);
    assert_eq!(r.count(FindingKind::OpMismatch), 0);
}

fn history() -> Vec<(Op, u64)> {
    // Insert v1, update v2, update v3, delete named v3 (the last version before deletion).
    vec![
        (Op::Insert, 1),
        (Op::Update, 2),
        (Op::Update, 3),
        (Op::Delete, 3),
    ]
}

#[test]
fn a_write_then_delete_is_clean_in_every_arrival_order() {
    for feed_first in [true, false] {
        let mut c = checker();
        let h = history();
        let feeds = |c: &mut Checker| {
            for (i, (op, v)) in h.iter().enumerate() {
                c.push_feed(feed(i as u64 + 1, "Invoice", "I-1", *v, *op))
                    .unwrap();
            }
        };
        let events = |c: &mut Checker| {
            for (i, (op, v)) in h.iter().enumerate() {
                c.push_event(event(i as u64 + 1, "Invoice", "I-1", *v, *op))
                    .unwrap();
            }
        };
        if feed_first {
            feeds(&mut c);
            events(&mut c);
        } else {
            events(&mut c);
            feeds(&mut c);
        }
        let r = c.finish();
        assert!(r.findings.is_empty(), "{:?}", r.findings);
        assert_eq!(r.matched, 4);
    }
}

#[test]
fn a_delete_event_without_a_delete_feed_entry_is_an_orphan() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 3, Op::Update))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-1", 3, Op::Update))
        .unwrap();
    c.push_event(event(2, "Invoice", "I-1", 3, Op::Delete))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].kind, FindingKind::OrphanStateEvent);
    assert_eq!(r.findings[0].event_op, Some(Op::Delete));
}

#[test]
fn two_identical_deletes_are_still_duplicates() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 3, Op::Delete))
        .unwrap();
    let err = c
        .push_feed(feed(2, "Invoice", "I-1", 3, Op::Delete))
        .unwrap_err();
    assert_eq!(err, CaptureInvariantError::FeedDuplicate);

    let mut c = checker();
    c.push_event(event(1, "Invoice", "I-1", 3, Op::Delete))
        .unwrap();
    c.push_event(event(2, "Invoice", "I-1", 3, Op::Delete))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.count(FindingKind::DuplicateStateEvent), 1);
}

#[test]
fn a_different_version_is_a_version_mismatch_not_a_miss_and_an_orphan() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 4, Op::Update))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-1", 3, Op::Update))
        .unwrap();
    let r = c.finish();
    assert_eq!(r.findings.len(), 1);
    let f = &r.findings[0];
    assert_eq!(f.kind, FindingKind::VersionMismatch);
    assert_eq!((f.version, f.event_version), (4, Some(3)));
}

#[test]
fn a_derived_entity_without_a_projector_is_a_catalog_error() {
    let exclusions = Exclusions::new()
        .derived("Bin", "stock", "rebuilt from ledger entries", None)
        .unwrap()
        .derived(
            "Cache",
            "platform",
            "rebuilt on demand",
            Some("cache-projector"),
        )
        .unwrap();
    let r = Checker::new(exclusions, 16).unwrap().finish();
    assert_eq!(r.count(FindingKind::DerivedWithoutProjector), 1);
    assert_eq!(r.findings[0].entity, "Bin");
}

#[test]
fn entries_leaving_the_window_are_reported_not_dropped() {
    let mut c = Checker::new(Exclusions::new(), 2).unwrap();
    for i in 1..=4 {
        c.push_feed(feed(i, "Invoice", &format!("I-{i}"), 1, Op::Insert))
            .unwrap();
    }
    let r = c.finish();
    assert_eq!(r.count(FindingKind::OutOfWindow), 2);
    assert_eq!(r.count(FindingKind::MissingStateEvent), 2);
    assert!(r.peak_tracked <= 2);
}

#[test]
fn exclusions_are_skipped_on_both_sides_and_listed() {
    let exclusions = Exclusions::new()
        .derived(
            "Bin",
            "stock",
            "rebuilt from ledger entries",
            Some("bin-projector"),
        )
        .unwrap()
        .local("Session", "platform", "not part of the recoverable record")
        .unwrap();
    let mut c = Checker::new(exclusions, 16).unwrap();
    c.push_feed(feed(1, "Bin", "B-1", 1, Op::Insert)).unwrap();
    c.push_feed(feed(2, "Session", "S-1", 1, Op::Insert))
        .unwrap();
    c.push_event(event(1, "Session", "S-2", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    assert!(r.is_clean());
    assert_eq!(r.exclusions.len(), 2);
    assert_eq!(r.exclusions[0].feed_entries_skipped, 1);
    assert_eq!(r.exclusions[1].feed_entries_skipped, 1);
    assert_eq!(r.exclusions[1].events_skipped, 1);
    assert_eq!(r.exclusions[1].owner, "platform");
}

#[test]
fn incomplete_exclusions_are_refused() {
    assert!(Exclusions::new().local("", "o", "r").is_err());
    assert!(Exclusions::new().local("E", " ", "r").is_err());
    assert!(Exclusions::new().local("E", "o", "").is_err());
    let twice = Exclusions::new()
        .local("E", "o", "r")
        .unwrap()
        .local("E", "o", "r");
    assert_eq!(
        twice.unwrap_err().code(),
        "capture_invariant.exclusion_invalid"
    );
    assert_eq!(
        Checker::new(Exclusions::new(), 0).err(),
        Some(CaptureInvariantError::WindowInvalid)
    );
}

#[test]
fn created_and_deleted_in_one_transaction_leaves_nothing_on_either_side() {
    // The feed and the journal are both silent: the streams carry only the other document.
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-keep", 1, Op::Insert))
        .unwrap();
    c.push_event(event(1, "Invoice", "I-keep", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    assert!(r.is_clean());
    assert!(r.findings.iter().all(|f| f.id != "I-temp"));
    // An event for the transient document would be an orphan.
    let mut c = checker();
    c.push_event(event(1, "Invoice", "I-temp", 1, Op::Delete))
        .unwrap();
    assert_eq!(c.finish().count(FindingKind::OrphanStateEvent), 1);
}

#[test]
fn streams_must_be_in_position_order() {
    let mut c = checker();
    c.push_feed(feed(5, "A", "1", 1, Op::Insert)).unwrap();
    assert_eq!(
        c.push_feed(feed(5, "A", "2", 1, Op::Insert))
            .unwrap_err()
            .code(),
        "capture_invariant.stream_out_of_order"
    );
    c.push_event(event(5, "A", "1", 1, Op::Insert)).unwrap();
    assert!(c.push_event(event(4, "A", "2", 1, Op::Insert)).is_err());
    // A new generation sorts after the old one whatever the sequence.
    let later = Position {
        generation: 2,
        seq: 0,
    };
    let mut e = event(0, "A", "3", 1, Op::Insert);
    e.position = later;
    c.push_event(e).unwrap();
}

#[test]
fn a_repeated_feed_entry_is_refused() {
    let mut c = checker();
    c.push_feed(feed(1, "A", "1", 1, Op::Insert)).unwrap();
    let err = c.push_feed(feed(2, "A", "1", 1, Op::Insert)).unwrap_err();
    assert_eq!(err, CaptureInvariantError::FeedDuplicate);
}

#[test]
fn the_json_form_is_stable_and_lists_every_kind() {
    let mut c = checker();
    c.push_feed(feed(1, "Invoice", "I-1", 1, Op::Insert))
        .unwrap();
    let r = c.finish();
    let first = r.to_json().unwrap();
    assert_eq!(first, r.clone().to_json().unwrap());
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["counts"]["capture_invariant.missing_state_event"], 1);
    assert_eq!(v["counts"]["capture_invariant.out_of_window"], 0);
    assert_eq!(v["findings"][0]["kind"], "missing_state_event");
    assert_eq!(v["findings"][0]["feed_op"], "insert");
}

#[test]
fn a_million_clean_entries_pass_in_bounded_memory() {
    const WINDOW: usize = 64;
    const LAG: u64 = 8;
    let mut c = Checker::new(Exclusions::new(), WINDOW).unwrap();
    let total: u64 = 1_000_000;
    for i in 1..=total {
        c.push_feed(feed(i, "Invoice", &i.to_string(), 1, Op::Insert))
            .unwrap();
        if i > LAG {
            let j = i - LAG;
            c.push_event(event(j, "Invoice", &j.to_string(), 1, Op::Insert))
                .unwrap();
        }
    }
    for j in (total - LAG + 1)..=total {
        c.push_event(event(j, "Invoice", &j.to_string(), 1, Op::Insert))
            .unwrap();
    }
    let r = c.finish();
    assert!(r.is_clean(), "{:?}", r.findings.first());
    assert_eq!(r.matched, total);
    assert!(r.peak_tracked <= WINDOW);
    assert!(r.peak_pending <= LAG as usize + 1);
}

/// Run one scenario: `txs` transactions of `size` documents. Events of a transaction take the
/// transaction's positions in the order `keys` sorts them. Document `skip` (if any) has no event.
fn run(
    txs: usize,
    size: usize,
    keys: &[u32],
    skip: Option<usize>,
) -> Vec<(FindingKind, String, String, u64)> {
    let mut c = Checker::new(Exclusions::new(), 4096).unwrap();
    let mut seq = 0u64;
    let mut event_seq = 0u64;
    for t in 0..txs {
        let docs: Vec<usize> = (0..size).map(|d| t * size + d).collect();
        for &d in &docs {
            seq += 1;
            c.push_feed(feed(seq, "Doc", &format!("D-{d}"), 1, Op::Insert))
                .unwrap();
        }
        let mut order = docs.clone();
        order.sort_by_key(|d| (keys[d % keys.len()], *d));
        for d in order {
            if skip == Some(d) {
                continue;
            }
            event_seq += 1;
            c.push_event(event(event_seq, "Doc", &format!("D-{d}"), 1, Op::Insert))
                .unwrap();
        }
    }
    let r = c.finish();
    r.findings
        .iter()
        .map(|f| (f.kind, f.entity.clone(), f.id.clone(), f.version))
        .collect()
}

proptest! {
    #[test]
    fn shuffling_events_within_a_transaction_does_not_change_the_verdict(
        txs in 1usize..12,
        size in 1usize..6,
        keys in proptest::collection::vec(any::<u32>(), 1..64),
        skip in proptest::option::of(0usize..60),
    ) {
        let skip = skip.filter(|d| *d < txs * size);
        let in_order = run(txs, size, &[0], skip);
        let shuffled = run(txs, size, &keys, skip);
        prop_assert_eq!(in_order, shuffled);
    }
}
