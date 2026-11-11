//! Restart and chaos: a crash between allocate and use never leads to a reissued number, a
//! failing store hands out nothing, and a seeded random crash/restart loop never issues a
//! number twice.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use spot_app_api::prelude::Month;
use spot_leased_sequences::{LeasedSequences, LeasedSequencesError};
use spot_partition::{
    BranchSeries, HighWaterStore, LowWater, MemoryRangeAllocator, MemorySeriesStore,
    PartitionError, RangeAllocator, StoredSeries,
};
use spot_storage_contract::{ErrorKind, Sequences, SeriesKey};
use spot_sync_protocol::NodeId;

const BRANCH: &str = "edge-khi#branch_07";

/// A store whose writes can be made to fail, or to persist and then report failure (a lost
/// acknowledgement).
struct FlakyStore {
    inner: MemorySeriesStore,
    fail: AtomicBool,
    persist_then_fail: AtomicBool,
}

impl FlakyStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySeriesStore::new(),
            fail: AtomicBool::new(false),
            persist_then_fail: AtomicBool::new(false),
        })
    }
}

impl HighWaterStore for FlakyStore {
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
        self.inner.save(series, node, state)?;
        if self.persist_then_fail.load(Ordering::SeqCst) {
            return Err(PartitionError::Store {
                reason: "the acknowledgement was lost".to_owned(),
            });
        }
        Ok(())
    }
}

fn reopen(store: &Arc<impl HighWaterStore + 'static>, key: &SeriesKey) -> LeasedSequences {
    let store: Arc<dyn HighWaterStore> = store.clone();
    let branch = BranchSeries::reopen(key, &node(BRANCH), LowWater::half(), store).unwrap();
    LeasedSequences::new().with_lease(branch).unwrap()
}

fn leased(
    key: &SeriesKey,
    size: u64,
    store: &Arc<impl HighWaterStore + 'static>,
) -> LeasedSequences {
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(key, &node(BRANCH), size, at(2026, Month::June, 1, 12, 0))
        .unwrap();
    let store: Arc<dyn HighWaterStore> = store.clone();
    let mut seq = LeasedSequences::new();
    seq.renew(lease, LowWater::half(), store).unwrap();
    seq
}

#[test]
fn numbers_continue_after_a_restart_without_reissue() {
    let key = series("INV-2026-");
    let store = Arc::new(MemorySeriesStore::new());
    let mut seq = leased(&key, 10, &store);
    let first = [
        block_on(seq.allocate(&key, 1)).unwrap(),
        block_on(seq.allocate(&key, 2)).unwrap(),
    ];
    assert_eq!(first, [1, 2]);

    // Crash: the in-memory allocator is gone; the number 4 was handed out but never used.
    let handed_out_unused = block_on(seq.allocate(&key, 1)).unwrap();
    assert_eq!(handed_out_unused, 4);
    drop(seq);

    let mut seq = reopen(&store, &key);
    assert_eq!(seq.remaining(&key), 6);
    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 5);

    // A second restart with nothing allocated in between changes nothing.
    drop(seq);
    let mut seq = reopen(&store, &key);
    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 6);
}

#[test]
fn a_failing_store_hands_out_no_number_and_a_retry_never_reissues_one() {
    let key = series("INV-2026-");
    let store = FlakyStore::new();
    let mut seq = leased(&key, 10, &store);
    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 1);

    // The write fails outright: no number, remaining unchanged, then the retry works.
    store.fail.store(true, Ordering::SeqCst);
    let error = block_on(seq.allocate(&key, 1)).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unavailable);
    assert_eq!(seq.remaining(&key), 9);
    store.fail.store(false, Ordering::SeqCst);

    // The write is persisted but its acknowledgement is lost: no number was returned, so the
    // caller holds nothing; the store already counts it as spent.
    store.persist_then_fail.store(true, Ordering::SeqCst);
    assert!(block_on(seq.allocate(&key, 1)).is_err());
    store.persist_then_fail.store(false, Ordering::SeqCst);

    // A retry in the same process and a restart agree and never go back to an issued number.
    let retry = block_on(seq.allocate(&key, 1)).unwrap();
    assert!(retry > 1, "{retry} was issued before");
    drop(seq);
    let mut seq = reopen(&store, &key);
    let after_restart = block_on(seq.allocate(&key, 1)).unwrap();
    assert!(after_restart > retry, "{after_restart} follows {retry}");
}

#[test]
fn a_block_that_fails_halfway_loses_its_numbers_and_never_reuses_them() {
    let key = series("INV-2026-");
    let store = FlakyStore::new();
    let mut seq = leased(&key, 10, &store);
    // Fails on the very first write of the block: nothing issued.
    store.fail.store(true, Ordering::SeqCst);
    assert!(matches!(
        seq.allocate_block(&key, 3),
        Err(LeasedSequencesError::Store { .. })
    ));
    store.fail.store(false, Ordering::SeqCst);
    let next = seq.allocate_block(&key, 1).unwrap();
    assert_eq!(next, 1);
}

/// A tiny seeded generator (xorshift64*), so the loop is reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

#[test]
fn a_seeded_crash_restart_loop_never_issues_a_number_twice() {
    for seed in [1_u64, 7, 42, 2026, 0xDEAD_BEEF, 987_654_321] {
        let mut rng = Rng(seed);
        let key = series("INV-2026-");
        let store = FlakyStore::new();
        let mut hub = MemoryRangeAllocator::new();
        let now = at(2026, Month::June, 1, 12, 0);
        let dyn_store: Arc<dyn HighWaterStore> = store.clone();

        let mut seq = LeasedSequences::new();
        seq.renew(
            hub.lease(&key, &node(BRANCH), 8, now).unwrap(),
            LowWater::half(),
            dyn_store.clone(),
        )
        .unwrap();

        let mut issued: BTreeSet<u64> = BTreeSet::new();
        let mut highest = 0;
        for _ in 0..400 {
            match rng.below(10) {
                // Crash and restart: reopen from the store alone.
                0 | 1 => {
                    drop(seq);
                    seq = reopen(&store, &key);
                }
                // Renew with the next hub range (always strictly above).
                2 => {
                    let lease = hub
                        .lease(&key, &node(BRANCH), 1 + rng.below(8), now)
                        .unwrap();
                    let _ = seq.renew(lease, LowWater::half(), dyn_store.clone());
                }
                // Try to renew with a stale range from the past: must be refused.
                3 => {
                    let held = seq.branch(&key).unwrap().lease().clone();
                    if held.from > 1 {
                        let stale = spot_partition::RangeLease::new(
                            key.clone(),
                            1,
                            held.from - 1 + rng.below(3),
                            node(BRANCH),
                            spot_partition::LeaseId::new(999),
                        )
                        .unwrap();
                        assert!(
                            seq.renew(stale, LowWater::half(), dyn_store.clone())
                                .is_err()
                        );
                    }
                }
                // Allocate, sometimes with a failing or ack-losing store.
                _ => {
                    let mode = rng.below(6);
                    store.fail.store(mode == 0, Ordering::SeqCst);
                    store.persist_then_fail.store(mode == 1, Ordering::SeqCst);
                    let count = 1 + u32::try_from(rng.below(3)).unwrap();
                    if let Ok(first) = seq.allocate_block(&key, count) {
                        for number in first..first + u64::from(count) {
                            assert!(issued.insert(number), "seed {seed}: {number} issued twice");
                            highest = highest.max(number);
                        }
                    }
                    store.fail.store(false, Ordering::SeqCst);
                    store.persist_then_fail.store(false, Ordering::SeqCst);
                }
            }
            // The persisted mark is always past every number issued.
            let stored = store.load(&key, &node(BRANCH)).unwrap();
            assert!(
                stored.next > highest,
                "seed {seed}: mark behind an issued number"
            );
        }
        assert!(!issued.is_empty(), "seed {seed} issued something");
    }
}
