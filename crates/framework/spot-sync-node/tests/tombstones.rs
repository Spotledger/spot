#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Deletions travel: a tombstone is applied as a delete, counts for the prefix, replays without
//! a second effect and never brings a deleted record back (WP-SYN-02.T6).
mod common;

use common::*;
use spot_sync_node::{BundleApply, ParkingStore, Receipt};

fn applied(r: &Receipt) -> &spot_sync_node::Applied {
    match r {
        Receipt::Applied(a) => a,
        other => panic!("expected Applied, got {other:?}"),
    }
}

#[test]
fn create_update_delete_in_order_leave_no_record() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 1, &[])])), 0)
        .unwrap();
    n.receive(deliver(bundle(vec![rec("a", 2, &[])])), 1)
        .unwrap();
    let r = n.receive(deliver(bundle(vec![tomb("a", 3)])), 2).unwrap();
    assert_eq!(applied(&r).applied.records.len(), 1);
    assert!(!n.applier().has(TENANT, &key("a"), 0));
    assert!(n.applier().has_tombstone(TENANT, &key("a"), 3));
    assert_eq!(n.applier().delete_effects, 1);
    assert_eq!(n.applied_prefix(TENANT), Some(3));
}

#[test]
fn a_tombstone_counts_as_applied_for_the_prefix() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![tomb("never-seen", 1)])), 0)
        .unwrap();
    // Nothing existed, nothing was deleted, and the prefix still moves.
    assert_eq!(n.applier().delete_effects, 0);
    assert_eq!(n.applied_prefix(TENANT), Some(1));
}

#[test]
fn delete_then_create_with_the_same_id_applies_both() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 1, &[])])), 0)
        .unwrap();
    n.receive(deliver(bundle(vec![tomb("a", 2)])), 1).unwrap();
    assert!(!n.applier().has(TENANT, &key("a"), 0));
    let r = n
        .receive(deliver(bundle(vec![rec("a", 3, &[])])), 2)
        .unwrap();
    assert_eq!(applied(&r).applied.records.len(), 1);
    assert!(n.applier().has(TENANT, &key("a"), 3));
    assert_eq!(n.applied_prefix(TENANT), Some(3));
}

#[test]
fn a_replayed_tombstone_has_no_second_effect() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 1, &[])])), 0)
        .unwrap();
    n.receive(deliver(bundle(vec![tomb("a", 2)])), 1).unwrap();
    let calls = n.applier().calls;
    let r = n.receive(deliver(bundle(vec![tomb("a", 2)])), 2).unwrap();
    let a = applied(&r);
    assert_eq!(a.duplicates, 1);
    assert!(a.applied.records.is_empty());
    assert_eq!(n.applier().calls, calls, "the sink was not called again");
    assert_eq!(n.applier().delete_effects, 1);
    assert_eq!(n.counters().duplicates, 1);
}

#[test]
fn an_old_image_does_not_bring_a_deleted_record_back() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![tomb("a", 5)])), 0).unwrap();
    let r = n
        .receive(deliver(bundle(vec![rec("a", 4, &[])])), 1)
        .unwrap();
    assert_eq!(applied(&r).older_ignored, 1);
    assert!(!n.applier().has(TENANT, &key("a"), 0));
}

#[test]
fn an_old_tombstone_does_not_delete_a_record_created_later() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("a", 9, &[])])), 0)
        .unwrap();
    let r = n.receive(deliver(bundle(vec![tomb("a", 7)])), 1).unwrap();
    assert_eq!(applied(&r).older_ignored, 1);
    assert!(n.applier().has(TENANT, &key("a"), 9));
    assert_eq!(n.applier().delete_effects, 0);
}

#[test]
fn a_tombstone_replays_after_a_restart_without_a_second_delete() {
    // The sink (the database) survives; the intake is rebuilt and the hub re-sends the bundle.
    let mut first = intake();
    first
        .receive(deliver(bundle(vec![rec("a", 1, &[])])), 0)
        .unwrap();
    first
        .receive(deliver(bundle(vec![tomb("a", 2)])), 1)
        .unwrap();
    let sink = std::mem::take(first.applier_mut());
    let mut second = intake();
    *second.applier_mut() = sink;
    let r = second
        .receive(deliver(bundle(vec![tomb("a", 2)])), 2)
        .unwrap();
    assert_eq!(applied(&r).duplicates, 1);
    assert_eq!(second.applier().delete_effects, 1);
    assert_eq!(second.applied_prefix(TENANT), Some(2));
}

#[test]
fn a_tombstone_never_satisfies_a_reference_and_the_bundle_parks() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("p", 1, &[])])), 0)
        .unwrap();
    n.receive(deliver(bundle(vec![tomb("p", 2)])), 1).unwrap();
    let r = n
        .receive(deliver(bundle(vec![rec("child", 3, &["p"])])), 2)
        .unwrap();
    assert_eq!(
        r,
        Receipt::Parked {
            missing: vec![key("p")]
        }
    );
    assert!(!n.applier().has(TENANT, &key("child"), 0));
    // The prefix stays behind the parked bundle.
    assert_eq!(n.applied_prefix(TENANT), Some(2));
}

#[test]
fn a_gone_answer_releases_the_parked_bundle() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![tomb("p", 2)])), 0).unwrap();
    n.receive(deliver(bundle(vec![rec("child", 3, &["p"])])), 1)
        .unwrap();
    let outcome = n.resolve_gone(TENANT, &[key("p")]).unwrap();
    assert_eq!(outcome.released.len(), 1);
    assert!(outcome.not_recorded.is_empty());
    assert!(n.applier().has(TENANT, &key("child"), 3));
    assert!(n.parking().is_empty(TENANT));
    assert_eq!(n.applied_prefix(TENANT), Some(3));
}

#[test]
fn a_gone_answer_for_an_unknown_tenant_creates_nothing() {
    let mut n = intake();
    let outcome = n.resolve_gone("other", &[key("p")]).unwrap();
    assert!(outcome.released.is_empty());
    assert_eq!(n.tenant_count(), 0);
}

#[test]
fn a_gone_answer_for_one_tenant_does_not_release_another() {
    let mut n = intake();
    n.receive(deliver_to("t1", bundle(vec![rec("child", 3, &["p"])])), 0)
        .unwrap();
    n.receive(deliver_to("t2", bundle(vec![rec("child", 3, &["p"])])), 1)
        .unwrap();
    n.resolve_gone("t1", &[key("p")]).unwrap();
    assert!(n.applier().has("t1", &key("child"), 3));
    assert!(!n.applier().has("t2", &key("child"), 0));
}

#[test]
fn a_tombstone_in_a_mixed_bundle_applies_with_the_records_in_one_transaction() {
    let mut n = intake();
    n.receive(deliver(bundle(vec![rec("old", 1, &[])])), 0)
        .unwrap();
    let r = n
        .receive(deliver(bundle(vec![tomb("old", 4), rec("new", 5, &[])])), 1)
        .unwrap();
    assert_eq!(applied(&r).applied.records.len(), 2);
    assert!(!n.applier().has(TENANT, &key("old"), 0));
    assert!(n.applier().has(TENANT, &key("new"), 5));
}
