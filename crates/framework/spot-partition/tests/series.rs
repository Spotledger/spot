//! Per-branch naming ranges: disjoint leases, running-low, offline exhaustion, no reuse after a
//! restart, installs that are not above the stored high-water mark refused, and 100 simulated
//! nodes allocating concurrently with zero collisions.
//!
//! # `BranchSeries` has exactly one owner
//!
//! `BranchSeries` is deliberately **not** `Clone`: two copies would each hand out the
//! same next number from their own cursor and both write the store. The property is enforced by
//! the compiler — this file cannot clone a series — and the behaviour is proved by
//! [`one_owner_holds_the_range`]: two installs of one range cannot both succeed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use spot_partition::{
    BranchSeries, HighWaterStore, LeaseId, LowWater, MemoryRangeAllocator, MemorySeriesStore,
    PartitionError, RangeAllocator, RangeLease, StoredSeries,
};
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

fn series(key: &str) -> SeriesKey {
    SeriesKey::new(key).unwrap()
}

#[test]
fn the_hub_grants_disjoint_ranges() {
    let mut hub = MemoryRangeAllocator::new();
    let key = series("SINV-2026-");
    let b7 = node("edge-khi#branch_07");
    let b9 = node("edge-khi#branch_09");

    let first = hub.lease(&key, &b7, 10, at(1)).unwrap();
    let second = hub.lease(&key, &b9, 5, at(2)).unwrap();
    assert_eq!((first.from, first.to), (1, 10));
    assert_eq!((second.from, second.to), (11, 15));
    assert!(first.lease_id != second.lease_id);
    assert_eq!(hub.grants().len(), 2);
}

#[test]
fn a_branch_allocates_in_order_and_reports_running_low() {
    let mut hub = MemoryRangeAllocator::new();
    let key = series("SINV-2026-");
    let b7 = node("edge-khi#branch_07");
    let lease = hub.lease(&key, &b7, 10, at(1)).unwrap();
    let store = Arc::new(MemorySeriesStore::new());
    let mut alloc = BranchSeries::install(lease, LowWater::half(), store).unwrap();

    assert_eq!(alloc.remaining(), 10);
    assert!(!alloc.running_low());
    for expected in 1..=5 {
        assert_eq!(alloc.allocate().unwrap(), expected);
    }
    assert_eq!(alloc.remaining(), 5);
    assert!(alloc.running_low(), "half of the range is left");
}

#[test]
fn offline_exhaustion_refuses_rather_than_reusing_a_number() {
    let mut hub = MemoryRangeAllocator::new();
    let key = series("SINV-2026-");
    let b7 = node("edge-khi#branch_07");
    let lease = hub.lease(&key, &b7, 3, at(1)).unwrap();
    let store = Arc::new(MemorySeriesStore::new());
    let mut alloc = BranchSeries::install(lease, LowWater::half(), store).unwrap();

    for expected in 1..=3 {
        assert_eq!(alloc.allocate().unwrap(), expected);
    }
    let exhausted = alloc.allocate().unwrap_err();
    assert!(matches!(exhausted, PartitionError::RangeExhausted { .. }));
    assert_eq!(exhausted.code(), "partition.range_exhausted");
    assert_eq!(alloc.remaining(), 0);
    assert!(alloc.running_low());
}

#[test]
fn a_restart_never_reuses_a_number() {
    let mut hub = MemoryRangeAllocator::new();
    let key = series("SINV-2026-");
    let b7 = node("edge-khi#branch_07");
    let store = Arc::new(MemorySeriesStore::new());
    let lease = hub.lease(&key, &b7, 100, at(1)).unwrap();

    let mut alloc = BranchSeries::install(lease, LowWater::half(), store.clone()).unwrap();
    assert_eq!(alloc.allocate().unwrap(), 1);
    assert_eq!(alloc.allocate().unwrap(), 2);
    assert_eq!(alloc.allocate().unwrap(), 3);
    drop(alloc);

    let mut reopened = BranchSeries::reopen(&key, &b7, LowWater::half(), store).unwrap();
    assert_eq!(
        reopened.allocate().unwrap(),
        4,
        "the high-water mark was kept"
    );

    // A node with no stored state has no lease to allocate from.
    let fresh = Arc::new(MemorySeriesStore::new());
    let missing = BranchSeries::reopen(&key, &b7, LowWater::half(), fresh).unwrap_err();
    assert!(matches!(missing, PartitionError::NoLease { .. }));
}

#[test]
fn an_allocated_number_becomes_a_record_name() {
    let mut hub = MemoryRangeAllocator::new();
    let key = series("SINV-2026-");
    let b7 = node("edge-khi#branch_07");
    let lease = hub.lease(&key, &b7, 5, at(1)).unwrap();
    let store = Arc::new(MemorySeriesStore::new());
    let mut alloc = BranchSeries::install(lease, LowWater::half(), store).unwrap();

    let name = alloc.allocate_name("INV-").unwrap();
    assert_eq!(name.as_str(), "INV-1");
}

/// 100 nodes each lease ranges, allocate 40 numbers, and restart between allocations; the
/// union of allocated numbers is exactly 4000 distinct values.
#[test]
fn a_hundred_nodes_allocating_and_restarting_never_collide() {
    const NODES: usize = 100;
    const PER_NODE: usize = 40;

    let hub = Arc::new(Mutex::new(MemoryRangeAllocator::new()));
    let key = series("SINV-2026-");
    let low = LowWater::new(1, 4).unwrap();
    let now = at(1);

    let per_node: Vec<Vec<u64>> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(NODES);
        for index in 0..NODES {
            let hub = Arc::clone(&hub);
            let key = key.clone();
            handles.push(scope.spawn(move || -> Vec<u64> {
                let node = node(&format!("edge-khi#branch_{index:03}"));
                let store = Arc::new(MemorySeriesStore::new());
                // A per-node deterministic sequence of lease sizes: no randomness source.
                let mut seed = (index as u64).wrapping_mul(2_654_435_761).wrapping_add(1);
                let mut next_size = move || {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    1 + (seed >> 33) % 6
                };
                let first = hub
                    .lock()
                    .unwrap()
                    .lease(&key, &node, next_size(), now)
                    .unwrap();
                let mut alloc = BranchSeries::install(first, low, store.clone()).unwrap();

                let mut mine = Vec::with_capacity(PER_NODE);
                while mine.len() < PER_NODE {
                    if alloc.running_low() {
                        let lease = hub
                            .lock()
                            .unwrap()
                            .lease(&key, &node, next_size(), now)
                            .unwrap();
                        alloc = BranchSeries::install(lease, low, store.clone()).unwrap();
                    }
                    match alloc.allocate() {
                        Ok(number) => mine.push(number),
                        Err(_) => {
                            let lease = hub
                                .lock()
                                .unwrap()
                                .lease(&key, &node, next_size(), now)
                                .unwrap();
                            alloc = BranchSeries::install(lease, low, store.clone()).unwrap();
                        }
                    }
                    if mine.len() % 7 == 0 {
                        alloc = BranchSeries::reopen(&key, &node, low, store.clone()).unwrap();
                    }
                }
                mine
            }));
        }
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for mine in &per_node {
        assert_eq!(mine.len(), PER_NODE);
        assert!(
            mine.windows(2).all(|w| w[0] < w[1]),
            "a node's numbers increase, so no restart reused one"
        );
    }

    let mut all: Vec<u64> = per_node.iter().flatten().copied().collect();
    let total = all.len();
    assert_eq!(total, NODES * PER_NODE);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total, "no number was handed out twice");

    // Every granted range lies inside the series and no two granted ranges overlap.
    let mut grants = hub.lock().unwrap().grants();
    grants.sort_by_key(|grant| grant.lease.from);
    for pair in grants.windows(2) {
        assert!(
            pair[0].lease.to < pair[1].lease.from,
            "granted ranges are disjoint"
        );
    }
}

fn lease(from: u64, to: u64, id: u64, key: &SeriesKey, who: &NodeId) -> RangeLease {
    RangeLease::new(key.clone(), from, to, who.clone(), LeaseId::new(id)).unwrap()
}

#[test]
fn an_install_that_is_not_above_the_stored_range_is_refused_and_keeps_the_mark() {
    let key = series("INV-");
    let b7 = node("edge-khi#branch_07");
    let store: Arc<dyn HighWaterStore> = Arc::new(MemorySeriesStore::new());
    let mut held =
        BranchSeries::install(lease(10, 19, 1, &key, &b7), LowWater::half(), store.clone())
            .unwrap();
    for expected in 10..=12 {
        assert_eq!(held.allocate().unwrap(), expected);
    }
    assert_eq!(
        store.load(&key, &b7).unwrap().next,
        13,
        "the high-water mark is 13"
    );

    // Lower, overlapping, equal and touching ranges: each is refused, with the documented code.
    for (from, to, id) in [
        (1, 5, 2),
        (5, 12, 3),
        (5, 25, 4),
        (10, 19, 5),
        (19, 30, 6),
        (12, 12, 7),
    ] {
        let refused = BranchSeries::install(
            lease(from, to, id, &key, &b7),
            LowWater::half(),
            store.clone(),
        )
        .unwrap_err();
        assert!(
            matches!(refused, PartitionError::LeaseNotAboveStored { .. }),
            "{from}..={to} is refused"
        );
        assert_eq!(refused.code(), "partition.lease_not_above_stored");
        let text = refused.to_string();
        assert!(
            !text.contains("19") && !text.contains("12") && !text.contains("13"),
            "the message names no number of the stored range: {text}"
        );
        assert_eq!(
            store.load(&key, &b7).unwrap().next,
            13,
            "a refused install left the stored mark alone"
        );
    }

    // The series that held the range still allocates after the refusals.
    assert_eq!(held.allocate().unwrap(), 13);

    // A range above the stored one is accepted; the stored mark moves to its first number.
    let above = BranchSeries::install(lease(20, 29, 8, &key, &b7), LowWater::half(), store.clone())
        .unwrap();
    assert_eq!(above.lease().from, 20);
    assert_eq!(store.load(&key, &b7).unwrap().next, 20);
}

#[test]
fn a_first_install_succeeds_and_a_restart_resumes_at_the_stored_mark() {
    let key = series("INV-");
    let b7 = node("edge-khi#branch_07");
    let store: Arc<dyn HighWaterStore> = Arc::new(MemorySeriesStore::new());
    let granted = lease(5, 14, 3, &key, &b7);

    let mut series =
        BranchSeries::install(granted.clone(), LowWater::half(), store.clone()).unwrap();
    assert_eq!(
        series.remaining(),
        10,
        "a first install starts at the lease's from"
    );
    for expected in 5..=7 {
        assert_eq!(series.allocate().unwrap(), expected);
    }
    drop(series);

    // The same lease id installed again is a resume: it continues at the stored mark, never at
    // the lease's first number, so numbers 5 to 7 are not handed out twice.
    let mut resumed = BranchSeries::install(granted, LowWater::half(), store.clone()).unwrap();
    assert_eq!(resumed.remaining(), 7, "the mark resumes at 8");
    assert_eq!(resumed.allocate().unwrap(), 8);
    drop(resumed);

    let mut reopened = BranchSeries::reopen(&key, &b7, LowWater::half(), store.clone()).unwrap();
    assert_eq!(reopened.allocate().unwrap(), 9);

    // The same range under another lease id is not a resume: it would restart at 5.
    let duplicate = lease(5, 14, 4, &key, &b7);
    let refused = BranchSeries::install(duplicate, LowWater::half(), store.clone()).unwrap_err();
    assert!(matches!(
        refused,
        PartitionError::LeaseNotAboveStored { .. }
    ));
    assert_eq!(store.load(&key, &b7).unwrap().next, 10);
}

#[test]
fn one_owner_holds_the_range() {
    let key = series("INV-");
    let b7 = node("edge-khi#branch_07");
    let store: Arc<dyn HighWaterStore> = Arc::new(MemorySeriesStore::new());

    let mut first =
        BranchSeries::install(lease(1, 10, 1, &key, &b7), LowWater::half(), store.clone()).unwrap();
    assert_eq!(first.allocate().unwrap(), 1);

    // A second owner would need its own lease id, and that lease over the same range is
    // refused. `BranchSeries` is not `Clone`, so this install is the only way to try for a
    // second owner at all.
    let second = BranchSeries::install(lease(1, 10, 2, &key, &b7), LowWater::half(), store.clone());
    assert!(matches!(
        second,
        Err(PartitionError::LeaseNotAboveStored { .. })
    ));
    assert_eq!(
        first.allocate().unwrap(),
        2,
        "the one owner still hands out"
    );
}

#[test]
fn two_threads_install_disjoint_ranges_then_one_of_them_wins_an_overlap() {
    let key = series("INV-");
    let b7 = node("edge-khi#branch_07");
    let low = LowWater::half();
    let store: Arc<dyn HighWaterStore> = Arc::new(MemorySeriesStore::new());

    // Disjoint ranges: the lower one first, then the one above it, each on its own thread.
    std::thread::scope(|scope| {
        let (ready, wait) = std::sync::mpsc::channel();
        let lower_store = Arc::clone(&store);
        let lower_key = key.clone();
        let lower_b7 = b7.clone();
        let lower = scope.spawn(move || {
            let mut series =
                BranchSeries::install(lease(1, 10, 1, &lower_key, &lower_b7), low, lower_store)
                    .unwrap();
            let first = series.allocate().unwrap();
            ready.send(first).unwrap();
            first
        });
        assert_eq!(wait.recv().unwrap(), 1, "the lower range handed out 1");
        let above =
            BranchSeries::install(lease(11, 20, 2, &key, &b7), low, Arc::clone(&store)).unwrap();
        assert_eq!(above.lease().from, 11);
        assert_eq!(lower.join().unwrap(), 1);
    });
    assert_eq!(store.load(&key, &b7).unwrap().next, 11);

    // Now the same range from eight threads at once: exactly one install wins.
    let outcomes: Vec<Result<u64, PartitionError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8_u64)
            .map(|index| {
                let store = Arc::clone(&store);
                let key = key.clone();
                let b7 = b7.clone();
                scope.spawn(move || {
                    BranchSeries::install(
                        lease(21, 30, 100 + index, &key, &b7),
                        LowWater::half(),
                        store,
                    )
                    .map(|series| series.lease().from)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(
        winners, 1,
        "exactly one install of an overlapping range wins"
    );
    for outcome in &outcomes {
        if let Err(error) = outcome {
            assert_eq!(error.code(), "partition.lease_not_above_stored");
        }
    }
    assert_eq!(
        store.load(&key, &b7).unwrap().next,
        21,
        "the stored mark is the winner's first number"
    );
}

/// A store whose writes can be armed to fail: nothing is persisted and no number is handed out.
#[derive(Debug, Default)]
struct FailingStore {
    inner: MemorySeriesStore,
    fail: AtomicBool,
}

impl FailingStore {
    fn new() -> Self {
        Self::default()
    }

    fn arm(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }
}

impl HighWaterStore for FailingStore {
    fn load(&self, series: &SeriesKey, node: &NodeId) -> Option<StoredSeries> {
        self.inner.load(series, node)
    }

    fn save(
        &self,
        series: &SeriesKey,
        node: &NodeId,
        state: StoredSeries,
    ) -> Result<(), PartitionError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(PartitionError::Store {
                reason: "the disk is full".to_owned(),
            });
        }
        self.inner.save(series, node, state)
    }
}

#[test]
fn a_failing_store_fails_closed_and_never_reuses_a_number() {
    let key = series("INV-");
    let b7 = node("edge-khi#branch_07");
    let low = LowWater::half();
    let store = Arc::new(FailingStore::new());
    let dyn_store: Arc<dyn HighWaterStore> = store.clone();
    let granted = lease(1, 5, 1, &key, &b7);

    // The install cannot write its starting mark, so no series exists: nothing to allocate from.
    store.arm();
    let refused = BranchSeries::install(granted.clone(), low, dyn_store.clone()).unwrap_err();
    assert!(matches!(refused, PartitionError::Store { .. }));
    assert_eq!(refused.code(), "partition.store");
    assert!(store.load(&key, &b7).is_none(), "nothing was persisted");
    assert!(matches!(
        BranchSeries::reopen(&key, &b7, low, dyn_store.clone()).unwrap_err(),
        PartitionError::NoLease { .. }
    ));

    // With the store working the same lease installs and hands out numbers.
    store.disarm();
    let mut series = BranchSeries::install(granted, low, dyn_store.clone()).unwrap();
    assert_eq!(series.allocate().unwrap(), 1);
    assert_eq!(series.allocate().unwrap(), 2);

    // A failed write hands out nothing, so the number it would have used is still free.
    store.arm();
    let failed = series.allocate().unwrap_err();
    assert!(matches!(failed, PartitionError::Store { .. }));
    store.disarm();
    assert_eq!(
        series.allocate().unwrap(),
        3,
        "the failed write did not spend a number"
    );
}
