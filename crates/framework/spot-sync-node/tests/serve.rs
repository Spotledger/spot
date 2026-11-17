#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The serve-sequence prefix (WP-SYN-11.T2, option A): the node acknowledges only what the hub's
//! queue sent it and it applied, in the queue's own numbering, so the acknowledgement can never
//! claim more than was applied.
mod common;

use common::*;
use serde_json::json;
use spot_sync_node::{Receipt, ServeStamp, ServeTracker};

fn stamp(seq: u64, after: u64) -> ServeStamp {
    ServeStamp { seq, after }
}

fn serve(n: &mut Intake, id: &str, position: u64, refs: &[&str], s: ServeStamp) -> Receipt {
    n.receive_served(deliver(bundle(vec![rec(id, position, refs)])), s, 0)
        .expect("ok")
}

#[test]
fn a_high_journal_position_served_first_acknowledges_nothing() {
    let mut n = intake();
    // The mandatory lane (or an image that ran ahead) hands the node position 14 first. The
    // journal prefix leaps to 14; the serve prefix does not move, because sequences 1 and 2 are
    // still in the hub's queue below it.
    assert!(matches!(
        serve(&mut n, "late", 14, &[], stamp(3, 2)),
        Receipt::Applied(_)
    ));
    assert_eq!(n.applied_prefix(TENANT), Some(14));
    assert_eq!(n.served_prefix(TENANT), None);

    serve(&mut n, "a", 5, &[], stamp(1, 0));
    assert_eq!(n.served_prefix(TENANT), Some(1));
    serve(&mut n, "b", 6, &[], stamp(2, 1));
    assert_eq!(n.served_prefix(TENANT), Some(3));
}

#[test]
fn a_bundle_the_hub_removed_from_its_queue_does_not_block_the_chain() {
    let mut n = intake();
    // Sequence 2 left the queue (a redelivery acknowledged): 3 names 1 as the item below it.
    serve(&mut n, "c", 9, &[], stamp(3, 1));
    assert_eq!(n.served_prefix(TENANT), None);
    serve(&mut n, "a", 5, &[], stamp(1, 0));
    assert_eq!(n.served_prefix(TENANT), Some(3));
}

#[test]
fn a_parked_bundle_holds_the_serve_prefix_until_it_is_applied() {
    let mut n = intake();
    assert!(matches!(
        serve(&mut n, "a", 10, &["m"], stamp(1, 0)),
        Receipt::Parked { .. }
    ));
    serve(&mut n, "b", 20, &[], stamp(2, 1));
    assert_eq!(n.served_prefix(TENANT), None);
    // The missing record arrives (sequence 3): the parked bundle is released and applied, and
    // the three sequences count together.
    let receipt = serve(&mut n, "m", 30, &[], stamp(3, 2));
    let Receipt::Applied(applied) = receipt else {
        panic!("expected the bundle to be applied");
    };
    assert_eq!(applied.released.len(), 1);
    assert_eq!(n.served_prefix(TENANT), Some(3));
}

#[test]
fn a_refused_bundle_holds_the_serve_prefix_until_the_hub_serves_it_again() {
    let mut n = intake();
    n.applier_mut().refuse = true;
    assert!(matches!(
        serve(&mut n, "a", 10, &[], stamp(1, 0)),
        Receipt::Refused(_)
    ));
    n.applier_mut().refuse = false;
    serve(&mut n, "b", 20, &[], stamp(2, 1));
    assert_eq!(n.served_prefix(TENANT), None);
    // The same item again, as the hub serves it until it is acknowledged.
    assert!(matches!(
        serve(&mut n, "a", 10, &[], stamp(1, 0)),
        Receipt::Applied(_)
    ));
    assert_eq!(n.served_prefix(TENANT), Some(2));
}

#[test]
fn a_bundle_on_the_repair_list_holds_the_prefix_until_every_record_is_applied() {
    let mut n = intake();
    serve(&mut n, "a", 10, &["gone"], stamp(1, 0));
    serve(&mut n, "b", 20, &[], stamp(2, 1));
    n.expire(10_000_000_000);
    assert_eq!(n.repair_len(), 1);
    assert_eq!(n.served_prefix(TENANT), None);
    // The record it needed arrives (sequence 3), but the repaired bundle itself is still not
    // applied, so it keeps holding the prefix.
    serve(&mut n, "gone", 5, &[], stamp(3, 2));
    assert_eq!(n.served_prefix(TENANT), None);
    // The hub serves it again, still queued under its own sequence, and now it applies.
    serve(&mut n, "a", 10, &["gone"], stamp(1, 0));
    assert_eq!(n.served_prefix(TENANT), Some(3));
}

#[test]
fn a_bundle_held_for_version_skew_counts_into_the_prefix() {
    let mut n = intake();
    let mut held = rec("h", 10, &[]);
    held.image = json!({"refs": [], "major": 2});
    let receipt = n
        .receive_served(deliver(bundle(vec![held])), stamp(1, 0), 0)
        .expect("ok");
    assert!(matches!(receipt, Receipt::Held(_)));
    assert_eq!(n.served_prefix(TENANT), Some(1));
}

#[test]
fn a_restart_rebuilds_the_prefix_from_the_stamps_the_hub_serves_again() {
    // The hub trimmed sequence 1 before the node restarted; the oldest bundle it still queues
    // names nothing below it.
    let mut n = intake();
    assert_eq!(n.served_prefix(TENANT), None);
    serve(&mut n, "b", 20, &[], stamp(2, 0));
    serve(&mut n, "c", 30, &[], stamp(3, 2));
    assert_eq!(n.served_prefix(TENANT), Some(3));
}

#[test]
fn a_repeat_of_a_settled_bundle_changes_nothing() {
    let mut n = intake();
    serve(&mut n, "a", 10, &[], stamp(1, 0));
    serve(&mut n, "a", 10, &[], stamp(1, 0));
    serve(&mut n, "b", 20, &[], stamp(2, 1));
    serve(&mut n, "a", 10, &[], stamp(1, 0));
    assert_eq!(n.served_prefix(TENANT), Some(2));
}

#[test]
fn a_bundle_received_without_a_stamp_never_moves_the_serve_prefix() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 10, &[])])), 0)
        .expect("ok");
    assert_eq!(n.applied_prefix(TENANT), Some(10));
    assert_eq!(n.served_prefix(TENANT), None);
}

#[test]
fn tenants_keep_their_own_serve_prefix() {
    let mut n = intake();
    serve(&mut n, "a", 10, &[], stamp(1, 0));
    n.receive_served(
        deliver_to("other", bundle(vec![rec("a", 10, &[])])),
        stamp(5, 4),
        0,
    )
    .expect("ok");
    assert_eq!(n.served_prefix(TENANT), Some(1));
    assert_eq!(n.served_prefix("other"), None);
    assert_eq!(n.served_prefix("nobody"), None);
}

#[test]
fn the_tracker_alone_follows_the_chain_in_any_arrival_order() {
    let chain = [stamp(1, 0), stamp(2, 1), stamp(4, 2), stamp(5, 4)];
    let orders: [[usize; 4]; 3] = [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1]];
    for order in orders {
        let mut t = ServeTracker::new();
        for i in order {
            t.settle(chain[i]);
        }
        assert_eq!(t.prefix(), Some(5), "order {order:?}");
        assert_eq!(t.waiting(), 0);
    }
    let mut t = ServeTracker::new();
    t.settle(stamp(5, 4));
    t.settle(stamp(2, 1));
    assert_eq!(t.prefix(), None);
    assert_eq!(t.waiting(), 2);
}
