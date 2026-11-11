#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Receive, park, retry, bounds, replays, skew, source check.
mod common;

use common::*;
use serde_json::json;
use spot_events_contract::SourceId;
use spot_sync_closure::{BundleRecord, RecordKey, Role};
use spot_sync_node::{BundleApply, NodeError, ParkingLimits, ParkingStore, Receipt, RepairCause};
use spot_sync_skew::HoldReason;

fn applied(r: &Receipt) -> &spot_sync_node::Applied {
    match r {
        Receipt::Applied(a) => a,
        other => panic!("expected Applied, got {other:?}"),
    }
}

#[test]
fn complete_bundle_applies() {
    let mut n = intake();
    let r = n
        .receive(
            deliver(bundle(vec![rec("a", 1, &[]), rec("b", 2, &["a"])])),
            0,
        )
        .expect("ok");
    assert_eq!(applied(&r).applied.records.len(), 2);
    assert!(n.applier().has(TENANT, &key("b"), 2));
    assert_eq!(n.applied_prefix(TENANT), Some(2));
}

#[test]
fn missing_parent_parks_whole_bundle_then_applies_when_parent_arrives() {
    let mut n = intake();
    let r = n
        .receive(
            deliver(bundle(vec![rec("x", 5, &[]), rec("child", 6, &["parent"])])),
            0,
        )
        .expect("ok");
    assert_eq!(
        r,
        Receipt::Parked {
            missing: vec![key("parent")]
        }
    );
    // The whole bundle is parked: not even the record with complete links was applied.
    assert!(!n.applier().has(TENANT, &key("x"), 0));
    assert_eq!(n.applier().calls, 0);

    let r = n
        .receive(deliver(bundle(vec![rec("parent", 3, &[])])), 1)
        .expect("ok");
    let a = applied(&r);
    assert_eq!(a.released.len(), 1);
    assert!(n.applier().has(TENANT, &key("child"), 6));
    assert!(n.applier().has(TENANT, &key("x"), 5));
    // The parent was applied before the child.
    let order = &n.applier().order;
    let p = order.iter().position(|k| k.1 == key("parent"));
    let c = order.iter().position(|k| k.1 == key("child"));
    assert!(p < c);
    assert!(n.parking().is_empty(TENANT));
}

#[test]
fn cycle_applies_together() {
    let mut n = intake();
    let r = n
        .receive(
            deliver(bundle(vec![rec("a", 1, &["b"]), rec("b", 2, &["a"])])),
            0,
        )
        .expect("ok");
    assert_eq!(applied(&r).applied.records.len(), 2);
}

#[test]
fn chain_of_parked_bundles_releases_in_arrival_order() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("c", 30, &["b"])])), 0)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("b", 20, &["a"])])), 1)
        .expect("ok");
    assert_eq!(n.applier().calls, 0);
    let r = n
        .receive(deliver(bundle(vec![rec("a", 10, &[])])), 2)
        .expect("ok");
    assert_eq!(applied(&r).released.len(), 2);
    assert_eq!(n.applied_prefix(TENANT), Some(30));
}

#[test]
fn park_bound_by_count_moves_oldest_to_repair() {
    let limits = ParkingLimits {
        max_bundles: 2,
        ..ParkingLimits::default()
    };
    let mut n = intake_with(limits);
    for (i, id) in ["p1", "p2", "p3"].iter().enumerate() {
        let pos = 10 * (i as u64 + 1);
        n.receive(deliver(bundle(vec![rec(id, pos, &["gone"])])), i as u64)
            .expect("ok");
    }
    assert_eq!(n.parking().len(TENANT), 2);
    assert_eq!(n.counters().dropped_by_bound, 1);
    let repair = n.take_repair();
    assert_eq!(repair.len(), 1);
    assert_eq!(repair[0].first_position, 10);
    assert_eq!(repair[0].cause, RepairCause::Bound);
    assert_eq!(repair[0].missing, vec![key("gone")]);
}

#[test]
fn park_bound_by_bytes_and_oversize() {
    let one = bundle(vec![rec("p1", 10, &["gone"])]).bytes;
    let limits = ParkingLimits {
        max_bytes: one + one / 2,
        ..ParkingLimits::default()
    };
    let mut n = intake_with(limits);
    n.receive(deliver(bundle(vec![rec("p1", 10, &["gone"])])), 0)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("p2", 20, &["gone"])])), 1)
        .expect("ok");
    assert_eq!(n.parking().len(TENANT), 1);
    assert!(n.parking().bytes(TENANT) <= limits.max_bytes);
    assert_eq!(n.counters().dropped_by_bound, 1);

    let mut huge = rec("p3", 30, &["gone"]);
    huge.image = json!({"refs": [["Doc", "gone"]], "pad": "x".repeat(10_000)});
    let r = n.receive(deliver(bundle(vec![huge])), 2).expect("ok");
    assert!(matches!(r, Receipt::Overflow { .. }));
    assert_eq!(n.parking().len(TENANT), 1);
    assert_eq!(n.applied_prefix(TENANT), None);
}

#[test]
fn ack_is_gated_by_a_parked_early_bundle_and_moves_when_it_applies() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("early", 5, &["parent"])])), 0)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("late", 9, &[])])), 1)
        .expect("ok");
    // `late` is applied but the parked bundle at position 5 holds the ack back.
    assert!(n.applier().has(TENANT, &key("late"), 9));
    assert_eq!(n.applied_prefix(TENANT), None);

    n.receive(deliver(bundle(vec![rec("parent", 2, &[])])), 2)
        .expect("ok");
    // parent (2) applied, parked bundle released, so the prefix reaches 9.
    assert_eq!(n.applied_prefix(TENANT), Some(9));
}

#[test]
fn ack_stops_just_before_the_parked_bundle() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("before", 3, &[])])), 0)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("early", 5, &["parent"])])), 1)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("late", 9, &[])])), 2)
        .expect("ok");
    assert_eq!(n.applied_prefix(TENANT), Some(3));
}

#[test]
fn replay_is_idempotent_and_older_version_is_ignored() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 5, &[])])), 0)
        .expect("ok");
    let calls = n.applier().calls;

    let r = n
        .receive(deliver(bundle(vec![rec("a", 5, &[])])), 1)
        .expect("ok");
    assert_eq!(applied(&r).duplicates, 1);
    assert!(applied(&r).applied.records.is_empty());

    let r = n
        .receive(deliver(bundle(vec![rec("a", 3, &[])])), 2)
        .expect("ok");
    assert_eq!(applied(&r).older_ignored, 1);

    assert_eq!(n.applier().calls, calls);
    assert_eq!(
        n.applier().stored.get(&(TENANT.to_owned(), key("a"))),
        Some(&5)
    );
    assert_eq!(n.counters().duplicates, 1);
    assert_eq!(n.counters().older_ignored, 1);
    assert_eq!(n.applied_prefix(TENANT), Some(5));
}

#[test]
fn partly_replayed_bundle_applies_only_the_new_records() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 5, &[])])), 0)
        .expect("ok");
    let r = n
        .receive(
            deliver(bundle(vec![rec("a", 5, &[]), rec("b", 6, &["a"])])),
            1,
        )
        .expect("ok");
    assert_eq!(applied(&r).applied.records.len(), 1);
    assert_eq!(applied(&r).duplicates, 1);
}

#[test]
fn undecodable_record_is_held_never_parked_or_applied() {
    let mut n = intake();
    let mut bad = rec("v2", 7, &[]);
    bad.image = json!({"refs": [], "major": 2});
    let r = n
        .receive(deliver(bundle(vec![rec("ok", 6, &[]), bad])), 0)
        .expect("ok");
    assert!(matches!(
        r,
        Receipt::Held(HoldReason::UnsupportedMajor { major: 2, .. })
    ));
    assert_eq!(n.applier().calls, 0);
    assert!(n.parking().is_empty(TENANT));
    assert_eq!(n.holds(TENANT).expect("tenant").len(), 1);
    // A hold never blocks the prefix.
    assert_eq!(n.applied_prefix(TENANT), Some(7));
}

#[test]
fn foreign_source_is_refused_before_parking() {
    let mut n = intake();
    let mut d = deliver(bundle(vec![rec("a", 1, &["gone"])]));
    d.source = SourceId::new("rogue.example.app").expect("valid");
    let e = n.receive(d, 0).expect_err("refused");
    assert_eq!(e.code(), "sync_node.foreign_source");
    assert!(matches!(e, NodeError::ForeignSource { .. }));
    assert!(n.parking().is_empty(TENANT));
    assert_eq!(n.applier().calls, 0);
}

#[test]
fn tenants_never_share_a_queue() {
    let mut n = intake();
    n.receive(
        deliver_to("acme", bundle(vec![rec("c", 5, &["parent"])])),
        0,
    )
    .expect("ok");
    // The same parent arrives for another tenant: it must not release acme's bundle.
    n.receive(deliver_to("other", bundle(vec![rec("parent", 1, &[])])), 1)
        .expect("ok");
    assert_eq!(n.parking().len("acme"), 1);
    assert_eq!(n.parking().len("other"), 0);
}

#[test]
fn refused_bundle_holds_the_prefix_until_it_applies() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("ok", 1, &[])])), 0)
        .expect("ok");
    n.applier_mut().refuse = true;
    let r = n
        .receive(deliver(bundle(vec![rec("bad", 4, &[])])), 1)
        .expect("ok");
    assert!(matches!(r, Receipt::Refused(_)));
    n.applier_mut().refuse = false;
    n.receive(deliver(bundle(vec![rec("later", 8, &[])])), 2)
        .expect("ok");
    assert_eq!(n.applied_prefix(TENANT), Some(1));
    n.receive(deliver(bundle(vec![rec("bad", 4, &[])])), 3)
        .expect("ok");
    assert_eq!(n.applied_prefix(TENANT), Some(8));
}

#[test]
fn expired_parked_bundle_goes_to_repair_and_keeps_holding_the_prefix() {
    let limits = ParkingLimits {
        max_age_ms: 1_000,
        ..ParkingLimits::default()
    };
    let mut n = intake_with(limits);
    n.receive(deliver(bundle(vec![rec("p", 5, &["gone"])])), 0)
        .expect("ok");
    n.receive(deliver(bundle(vec![rec("q", 9, &[])])), 10)
        .expect("ok");
    n.expire(500);
    assert_eq!(n.parking().len(TENANT), 1);
    n.expire(2_000);
    assert!(n.parking().is_empty(TENANT));
    let repair = n.take_repair();
    assert_eq!(repair[0].cause, RepairCause::Expired);
    assert_eq!(n.applied_prefix(TENANT), None);
    // A redelivery whose closure is now complete clears the hold.
    n.receive(
        deliver(bundle(vec![rec("gone", 2, &[]), rec("p", 5, &["gone"])])),
        3_000,
    )
    .expect("ok");
    assert_eq!(n.applied_prefix(TENANT), Some(9));
}

#[test]
fn zero_limit_is_rejected() {
    let limits = ParkingLimits {
        max_bundles: 0,
        ..ParkingLimits::default()
    };
    assert_eq!(
        limits.validate().expect_err("zero").code(),
        "sync_node.invalid_limits"
    );
}

#[test]
fn unused_role_import_guard() {
    let r: BundleRecord = rec("a", 1, &[]);
    assert_eq!(r.role, Role::Root);
    assert_eq!(r.key, RecordKey::new("Doc", "a"));
}
