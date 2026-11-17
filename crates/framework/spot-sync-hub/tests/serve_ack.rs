//! Acknowledgement by serve sequence (WP-SYN-11.T2, option A): the queue numbers what it is given,
//! the hub trims by the number a node acknowledges and derives the journal prefix itself, so an
//! acknowledgement can never claim more than was applied.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::*;
use spot_conditions::MemorySink;
use spot_sync_acks::{AckLedger, MemoryStore};
use spot_sync_closure::{Bundle, BundleKind};
use spot_sync_hub::{
    HubRelay, LedgerBook, MemoryQueue, MemoryStream, PushOutcome, QueuedBundle, SendQueue,
    SharedBook, SharedLedgerBook, StreamEntry,
};
use spot_sync_obligations::{Lane, ObligationSettings, Pending};

type Relay = HubRelay<SharedQueue, MemoryStream, Src>;

const NOW: i64 = 1_000;

/// Positions 1 and 2 are bulk `item` records; position 3 is a mandatory `tax` record, so the
/// queue serves 3 first, ahead of the lower bulk positions (the second trigger of PR 2269).
fn entries() -> Vec<StreamEntry> {
    let mut tax = entry(3, "c", "tax", "T1", 1);
    tax.lane = Lane::Mandatory;
    vec![
        entry(1, "a", "item", "I1", 1),
        entry(2, "b", "item", "I2", 1),
        tax,
    ]
}

fn relay(q: &SharedQueue) -> Relay {
    let mut src = Src::default();
    src.put("item", "I1", plain(), 1);
    src.put("item", "I2", plain(), 2);
    src.put("tax", "T1", plain(), 3);
    let mut stream = MemoryStream::new();
    for e in entries() {
        stream.append(e);
    }
    let mut r = HubRelay::new(cfg(), stream, q.clone(), src, Box::new(Refs)).unwrap();
    r.add_destination(dest("d", &["item", "tax"])).unwrap();
    r.tick(NOW).unwrap();
    r
}

fn seqs(q: &SharedQueue) -> Vec<(u64, u64)> {
    q.0.borrow()
        .all("d")
        .iter()
        .map(|p| (p.position, p.item.serve_seq))
        .collect()
}

#[test]
fn the_queue_numbers_items_without_gaps_and_peek_names_the_item_below() {
    let q = SharedQueue::default();
    let _r = relay(&q);
    // Serving order: the mandatory item first, then the bulk items by position.
    assert_eq!(seqs(&q), vec![(3, 3), (1, 1), (2, 2)]);
    let all = q.0.borrow().all("d");
    let after: Vec<u64> = all.iter().map(|p| p.item.serve_after).collect();
    assert_eq!(after, vec![2, 0, 1]);
    assert_eq!(q.0.borrow().last_serve_seq("d").unwrap(), 3);
}

#[test]
fn a_claim_below_the_serve_order_trims_only_what_it_names() {
    let q = SharedQueue::default();
    let mut r = relay(&q);
    // The node applied the mandatory item (sequence 3) but not 1 and 2: its contiguous prefix is
    // 0, and nothing is dropped, however high the journal position of what it did apply.
    let ack = r.on_ack_served_at("d", 0, NOW + 1).unwrap();
    assert_eq!(ack.removed, 0);
    assert_eq!(ack.applied_prefix, 0);
    assert_eq!(q.0.borrow().all("d").len(), 3);

    // Sequence 1 applied: item at position 1 is trimmed; the oldest item left is position 2.
    let ack = r.on_ack_served_at("d", 1, NOW + 2).unwrap();
    assert_eq!(ack.removed, 1);
    assert_eq!(ack.applied_prefix, 1);

    // Everything applied: the queue is empty and the prefix is the cursor.
    let ack = r.on_ack_served_at("d", 3, NOW + 3).unwrap();
    assert_eq!(ack.removed, 2);
    assert_eq!(ack.applied_prefix, 3);
    assert!(q.0.borrow().all("d").is_empty());
}

#[test]
fn a_prefix_that_does_not_reach_the_lowest_queued_position_is_derived_from_the_queue() {
    let q = SharedQueue::default();
    let mut r = relay(&q);
    // Sequences 1 and 2 applied (positions 1 and 2); the mandatory item (position 3) is not.
    let ack = r.on_ack_served_at("d", 2, NOW + 1).unwrap();
    assert_eq!(ack.removed, 2);
    assert_eq!(ack.applied_prefix, 2);
    assert_eq!(seqs(&q), vec![(3, 3)]);
}

#[test]
fn a_repeat_or_lower_claim_is_harmless_and_never_moves_the_prefix_back() {
    let q = SharedQueue::default();
    let mut r = relay(&q);
    assert_eq!(
        r.on_ack_served_at("d", 2, NOW + 1).unwrap().applied_prefix,
        2
    );
    let again = r.on_ack_served_at("d", 1, NOW + 2).unwrap();
    assert_eq!(again.removed, 0);
    assert_eq!(again.applied_prefix, 2);
    let same = r.on_ack_served_at("d", 2, NOW + 3).unwrap();
    assert_eq!(same.removed, 0);
    assert_eq!(same.applied_prefix, 2);
}

#[test]
fn a_claim_above_the_newest_assigned_sequence_is_refused_and_marks_the_node_suspect() {
    let q = SharedQueue::default();
    let mut r = relay(&q);
    let err = r.on_ack_served_at("d", 4, NOW + 1).unwrap_err();
    assert_eq!(err.code(), "sync_hub.serve_ack_ahead");
    assert_eq!(q.0.borrow().all("d").len(), 3);
    assert_eq!(r.tick(NOW + 2).unwrap().suspect, vec!["d".to_owned()]);
    assert_eq!(
        r.on_ack_served_at("nobody", 1, NOW).unwrap_err().code(),
        "sync_hub.unknown_destination"
    );
}

#[test]
fn the_ledger_sees_the_derived_prefix_so_a_mandatory_event_is_not_satisfied_early() {
    let q = SharedQueue::default();
    let mut r = relay(&q);
    let book: SharedLedgerBook<MemoryStore, MemorySink> = SharedBook::new(LedgerBook::new(
        AckLedger::with_catalogue(
            MemoryStore::new(),
            MemorySink::new(),
            ObligationSettings::default(),
        )
        .unwrap(),
    ));
    r.set_ledger(Box::new(book));
    assert!(r.register_node("d", "1", 0).unwrap());
    let mandatory = entries().pop().unwrap();
    assert!(r.register_mandatory(&mandatory, NOW, None).unwrap());
    assert_eq!(r.who_is_missing(3).unwrap().len(), 1);

    // The two bulk items are applied; the mandatory event at position 3 is not.
    r.on_ack_served_at("d", 2, NOW + 1).unwrap();
    assert_eq!(r.who_is_missing(3).unwrap().len(), 1);

    r.on_ack_served_at("d", 3, NOW + 2).unwrap();
    assert!(r.who_is_missing(3).unwrap().is_empty());
}

fn pending(position: u64, index: u32, redelivery: bool) -> Pending<QueuedBundle> {
    Pending {
        lane: Lane::Bulk,
        position,
        item: QueuedBundle {
            index,
            redelivery,
            bundle: Bundle {
                kind: BundleKind::Root,
                records: Vec::new(),
                bytes: 0,
            },
            serve_seq: 0,
            serve_after: 0,
            floor: 0,
        },
    }
}

#[test]
fn a_duplicate_push_keeps_its_number_and_the_counter_never_falls() {
    let mut q = MemoryQueue::new();
    assert_eq!(
        q.push("d", pending(5, 0, false)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(
        q.push("d", pending(6, 0, false)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(
        q.push("d", pending(5, 0, false)).unwrap(),
        PushOutcome::Duplicate
    );
    // A redelivery of position 5 is a new identity and takes the next number.
    assert_eq!(
        q.push("d", pending(5, 0, true)).unwrap(),
        PushOutcome::Queued
    );
    let numbers: Vec<(u64, bool, u64)> = q
        .all("d")
        .iter()
        .map(|p| (p.position, p.item.redelivery, p.item.serve_seq))
        .collect();
    assert_eq!(numbers, vec![(5, false, 1), (5, true, 3), (6, false, 2)]);
    assert_eq!(q.last_serve_seq("d").unwrap(), 3);

    // Trimming by sequence takes the redelivery too; the counter stays where it was.
    assert_eq!(q.ack_served("d", 3).unwrap(), 3);
    assert_eq!(q.last_serve_seq("d").unwrap(), 3);
    assert_eq!(
        q.push("d", pending(7, 0, false)).unwrap(),
        PushOutcome::Queued
    );
    assert_eq!(q.all("d")[0].item.serve_seq, 4);
    // Another destination numbers from 1.
    assert_eq!(q.last_serve_seq("other").unwrap(), 0);
    assert_eq!(q.lowest_position_after("d", 3).unwrap(), Some(7));
    assert_eq!(q.lowest_position_after("d", 4).unwrap(), None);
}

#[test]
fn removing_an_item_from_the_middle_leaves_the_chain_unbroken() {
    let mut q = MemoryQueue::new();
    q.push("d", pending(1, 0, false)).unwrap(); // sequence 1
    q.push("d", pending(2, 0, true)).unwrap(); // sequence 2
    q.push("d", pending(3, 0, false)).unwrap(); // sequence 3
    assert_eq!(q.remove_redelivered("d", 2).unwrap(), 1);
    let chain: Vec<(u64, u64)> = q
        .all("d")
        .iter()
        .map(|p| (p.item.serve_seq, p.item.serve_after))
        .collect();
    // Sequence 3 names 1 as the item below it: a node that applied 1 may pass 3.
    assert_eq!(chain, vec![(1, 0), (3, 1)]);
}
