//! A node can never be recorded as having applied what the hub had not written (WP-SYN-05.T3).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_conditions::MemorySink;
use spot_sync_acks::{AckError, AckLedger, MemoryStore, NodeRecord, NodeStateStore, StreamHead};
use spot_sync_obligations::{MandatoryEvent, ObligationSettings};

const T: &str = "t1";
const H: i64 = 3_600_000;

type Ledger = AckLedger<MemoryStore, MemorySink>;

fn event(position: u64) -> MandatoryEvent {
    MandatoryEvent {
        position,
        published_at_millis: 0,
        effective_from_millis: None,
        record: format!("tax_rate/{position}"),
    }
}

fn ledger() -> Ledger {
    AckLedger::new(
        MemoryStore::new(),
        MemorySink::new(),
        defs(),
        ObligationSettings::default(),
    )
    .unwrap()
}

fn snapshot(l: &Ledger) -> (Vec<NodeRecord>, Vec<MandatoryEvent>) {
    let store = l.store();
    let nodes = store
        .nodes()
        .iter()
        .filter_map(|(n, t)| store.load(n, t))
        .collect();
    (nodes, store.events(T))
}

#[test]
fn u64_max_is_refused_and_nothing_changes() {
    let mut l = ledger();
    l.register_node("n1", T, "1", 0).unwrap();
    l.register_mandatory(T, event(5));
    l.on_ack("n1", T, 3, StreamHead::new(10), H).unwrap();
    let before = snapshot(&l);
    let err = l
        .on_ack("n1", T, u64::MAX, StreamHead::new(10), 2 * H)
        .unwrap_err();
    assert_eq!(
        err,
        AckError::PrefixBeyondHead {
            node: "n1".to_owned(),
            tenant: T.to_owned(),
            claimed: u64::MAX,
            head: 10
        }
    );
    assert_eq!(err.code(), "sync_acks.prefix_beyond_head");
    assert_eq!(
        snapshot(&l),
        before,
        "record, crossings and contact unchanged"
    );
    assert!(l.conditions().sink().is_empty());
    assert!(l.conditions().open().is_empty());
}

#[test]
fn a_prefix_equal_to_the_head_is_accepted() {
    let mut l = ledger();
    l.register_node("n1", T, "1", 0).unwrap();
    l.on_ack("n1", T, 10, StreamHead::new(10), H).unwrap();
    assert_eq!(l.store().load("n1", T).unwrap().applied_prefix, 10);
}

#[test]
fn the_same_ack_value_is_accepted_once_the_head_advances() {
    let mut l = ledger();
    l.register_node("n1", T, "1", 0).unwrap();
    assert!(l.on_ack("n1", T, 12, StreamHead::new(10), H).is_err());
    l.on_ack("n1", T, 12, StreamHead::new(12), 2 * H).unwrap();
    assert_eq!(l.store().load("n1", T).unwrap().applied_prefix, 12);
}

#[test]
fn regression_is_checked_before_the_head() {
    let mut l = ledger();
    l.register_node("n1", T, "1", 0).unwrap();
    l.on_ack("n1", T, 8, StreamHead::new(10), H).unwrap();
    // Below the recorded prefix and above the head: the regression is reported.
    let err = l.on_ack("n1", T, 5, StreamHead::new(3), 2 * H).unwrap_err();
    assert_eq!(err.code(), "sync_acks.prefix_regressed");
}

#[test]
fn an_ack_never_covers_a_position_the_hub_had_not_written() {
    let mut l = ledger();
    l.register_node("n1", T, "1", 0).unwrap();
    // Hostile ack first: refused, so the later event stays outstanding.
    assert!(l.on_ack("n1", T, u64::MAX, StreamHead::new(10), H).is_err());
    l.on_ack("n1", T, 10, StreamHead::new(10), H).unwrap();
    assert!(l.register_mandatory(T, event(12)));
    let missing = l.who_is_missing(T, 12).unwrap();
    assert_eq!(missing.len(), 1);
    assert_eq!(missing[0].node, "n1");
    // An event the hub had already written (position 8) is covered by that ack.
    assert!(l.register_mandatory(T, event(8)));
    assert!(l.who_is_missing(T, 8).unwrap().is_empty());
}

#[test]
fn a_hold_above_the_head_is_refused_and_nothing_changes() {
    let mut l = ledger();
    l.register_node("n1", T, "1", 0).unwrap();
    l.register_mandatory(T, event(5));
    l.register_mandatory(T, event(50));
    let before = snapshot(&l);
    let err = l
        .set_holds("n1", T, vec![5, 50], StreamHead::new(10), H)
        .unwrap_err();
    assert_eq!(
        err,
        AckError::HoldBeyondHead {
            node: "n1".to_owned(),
            tenant: T.to_owned(),
            claimed: 50,
            head: 10
        }
    );
    assert_eq!(err.code(), "sync_acks.hold_beyond_head");
    assert_eq!(snapshot(&l), before);
    // Within the head it is accepted.
    l.set_holds("n1", T, vec![5], StreamHead::new(10), H)
        .unwrap();
    assert_eq!(l.store().load("n1", T).unwrap().holds, vec![5]);
}

/// The three obligation definitions, taken from the catalogue.
fn defs() -> Vec<spot_conditions::ConditionDef> {
    [
        "OBLIGATION_DUE_SOON",
        "OBLIGATION_OVERDUE",
        "OBLIGATION_RED",
    ]
    .into_iter()
    .map(|id| spot_sync_conditions::entry(id).unwrap().def)
    .collect()
}
