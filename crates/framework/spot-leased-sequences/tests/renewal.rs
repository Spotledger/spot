//! Renewal fails closed: a stale, overlapping, lower, repeated or foreign lease is refused, the
//! held lease and the saved high-water mark are left alone, and a strictly higher lease of the
//! same node and series is accepted.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_leased_sequences::{LeaseRefusal, LeasedSequences, LeasedSequencesError};
use spot_partition::{
    BranchSeries, HighWaterStore, LeaseId, LowWater, MemorySeriesStore, RangeLease,
};
use spot_storage_contract::SeriesKey;

fn lease(key: &SeriesKey, from: u64, to: u64, who: &str, id: u64) -> RangeLease {
    RangeLease::new(key.clone(), from, to, node(who), LeaseId::new(id)).unwrap()
}

const BRANCH: &str = "edge-khi#branch_07";

/// A series holding 10..=19 with 10..=12 already handed out.
fn held(store: &Arc<MemorySeriesStore>) -> (SeriesKey, LeasedSequences) {
    let key = series("INV-2026-");
    let mut seq = LeasedSequences::new();
    seq.renew(
        lease(&key, 10, 19, BRANCH, 1),
        LowWater::half(),
        store.clone(),
    )
    .unwrap();
    assert_eq!(seq.allocate_block(&key, 3).unwrap(), 10);
    (key, seq)
}

fn assert_refused(
    result: Result<Option<BranchSeries>, LeasedSequencesError>,
    expected: LeaseRefusal,
) {
    match result {
        Err(LeasedSequencesError::LeaseRefused { reason, .. }) => assert_eq!(reason, expected),
        other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }
}

fn untouched(key: &SeriesKey, seq: &mut LeasedSequences, store: &MemorySeriesStore) {
    assert_eq!(seq.branch(key).unwrap().lease().from, 10);
    assert_eq!(seq.branch(key).unwrap().lease().to, 19);
    assert_eq!(seq.remaining(key), 7, "13..=19 are still to come");
    let stored = store.load(key, &node(BRANCH)).unwrap();
    assert_eq!(stored.next, 13, "the high-water mark was not lowered");
    assert_eq!(seq.allocate_block(key, 1).unwrap(), 13);
}

#[test]
fn a_stale_lower_overlapping_or_repeated_range_is_refused() {
    for (from, to) in [
        (1, 5),
        (5, 12),
        (5, 25),
        (13, 30),
        (10, 19),
        (19, 30),
        (12, 12),
    ] {
        let store = Arc::new(MemorySeriesStore::new());
        let (key, mut seq) = held(&store);
        let offered = lease(&key, from, to, BRANCH, 9);
        assert_refused(
            seq.renew(offered.clone(), LowWater::half(), store.clone()),
            LeaseRefusal::NotAbove,
        );
        untouched(&key, &mut seq, &store);
        // The pre-built form is refused too, and the held series is still not replaced.
        let other = MemorySeriesStore::new();
        let rebuilt = BranchSeries::install(offered, LowWater::half(), Arc::new(other)).unwrap();
        assert_refused(seq.install(rebuilt), LeaseRefusal::NotAbove);
        assert_eq!(seq.branch(&key).unwrap().lease().from, 10);
    }
}

#[test]
fn another_node_is_refused() {
    let store = Arc::new(MemorySeriesStore::new());
    let (key, mut seq) = held(&store);

    let foreign = lease(&key, 100, 110, "edge-lhe#branch_02", 9);
    let rebuilt = BranchSeries::install(
        foreign,
        LowWater::half(),
        Arc::new(MemorySeriesStore::new()),
    )
    .unwrap();
    assert_refused(seq.install(rebuilt), LeaseRefusal::OtherNode);
    untouched(&key, &mut seq, &store);
}

#[test]
fn the_message_never_names_the_node() {
    let store = Arc::new(MemorySeriesStore::new());
    let (key, mut seq) = held(&store);
    let error = seq
        .renew(lease(&key, 1, 5, BRANCH, 9), LowWater::half(), store)
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("INV-2026-"));
    assert!(!text.contains("branch_07") && !text.contains("edge-khi"));
    assert_eq!(error.code(), "leased_sequences.lease_refused");
}

#[test]
fn renew_checks_the_store_before_it_writes_anything() {
    // A restart that has not reopened the series yet: nothing is held here, but the store
    // remembers 13 as the next number, so a stale lease must not reset it.
    let store = Arc::new(MemorySeriesStore::new());
    let (key, seq) = held(&store);
    drop(seq);

    let mut fresh = LeasedSequences::new();
    assert_refused(
        fresh.renew(
            lease(&key, 10, 19, BRANCH, 1),
            LowWater::half(),
            store.clone(),
        ),
        LeaseRefusal::NotAbove,
    );
    assert!(fresh.is_empty());
    assert_eq!(store.load(&key, &node(BRANCH)).unwrap().next, 13);
}

#[test]
fn a_strictly_higher_renewal_succeeds_and_the_mark_never_drops() {
    let store = Arc::new(MemorySeriesStore::new());
    let (key, mut seq) = held(&store);

    let replaced = seq
        .renew(
            lease(&key, 20, 29, BRANCH, 2),
            LowWater::half(),
            store.clone(),
        )
        .unwrap();
    assert_eq!(replaced.unwrap().lease().to, 19);
    let stored = store.load(&key, &node(BRANCH)).unwrap();
    assert_eq!((stored.lease.from, stored.next), (20, 20));
    assert_eq!(seq.allocate_block(&key, 1).unwrap(), 20);

    // A gap between the ranges is fine: the hub may have leased 30..=39 elsewhere.
    seq.renew(
        lease(&key, 40, 49, BRANCH, 3),
        LowWater::half(),
        store.clone(),
    )
    .unwrap();
    assert_eq!(seq.allocate_block(&key, 1).unwrap(), 40);
    // Going back is refused.
    assert_refused(
        seq.renew(
            lease(&key, 20, 29, BRANCH, 2),
            LowWater::half(),
            store.clone(),
        ),
        LeaseRefusal::NotAbove,
    );
    assert_eq!(store.load(&key, &node(BRANCH)).unwrap().next, 41);
}
