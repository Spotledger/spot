//! Allocation over leased ranges: blocks are consecutive and all-or-nothing, `set_next` is
//! refused, running-low and remaining track the lease, renewal continues past the old range,
//! and threads never share a number.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use spot_app_api::prelude::Month;
use spot_leased_sequences::{LeasedSequences, LeasedSequencesError};
use spot_partition::{MemoryRangeAllocator, RangeAllocator};
use spot_storage_contract::{ErrorKind, Sequences};

#[test]
fn a_block_of_three_is_consecutive_and_advances_by_three() {
    let key = series("INV-2026-");
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(
            &key,
            &node("edge-khi#branch_07"),
            10,
            at(2026, Month::June, 1, 12, 0),
        )
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    assert_eq!(seq.allocate_block(&key, 3).unwrap(), 1);
    assert_eq!(seq.remaining(&key), 7);
    assert_eq!(seq.allocate_block(&key, 3).unwrap(), 4);
    assert_eq!(seq.remaining(&key), 4);
    assert_eq!(seq.allocate_block(&key, 4).unwrap(), 7);
    assert_eq!(seq.remaining(&key), 0);
}

#[test]
fn a_zero_count_is_an_invalid_value_and_consumes_nothing() {
    let key = series("INV-2026-");
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(
            &key,
            &node("edge-khi#branch_07"),
            2,
            at(2026, Month::June, 1, 12, 0),
        )
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    let error = seq.allocate_block(&key, 0).unwrap_err();
    assert!(matches!(
        error,
        LeasedSequencesError::InvalidCount { count: 0 }
    ));
    assert_eq!(error.kind(), ErrorKind::InvalidValue);
    assert_eq!(seq.remaining(&key), 2, "nothing was consumed");
    assert_eq!(seq.allocate_block(&key, 1).unwrap(), 1);
}

#[test]
fn a_block_larger_than_the_lease_consumes_nothing() {
    let key = series("INV-2026-");
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(
            &key,
            &node("edge-khi#branch_07"),
            5,
            at(2026, Month::June, 1, 12, 0),
        )
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    assert_eq!(seq.allocate_block(&key, 3).unwrap(), 1);
    let error = seq.allocate_block(&key, 3).unwrap_err();
    assert!(matches!(
        error,
        LeasedSequencesError::LeaseExhausted {
            requested: 3,
            remaining: 2,
            ..
        }
    ));
    assert_eq!(error.code(), "leased_sequences.lease_exhausted");
    assert_eq!(
        seq.remaining(&key),
        2,
        "the refused block was not part-taken"
    );

    // The two numbers the caller can still have are handed out normally.
    assert_eq!(seq.allocate_block(&key, 2).unwrap(), 4);
    assert_eq!(seq.remaining(&key), 0);
}

#[test]
fn set_next_is_refused_and_leaves_the_counter_alone() {
    let key = series("INV-2026-");
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(
            &key,
            &node("edge-khi#branch_07"),
            5,
            at(2026, Month::June, 1, 12, 0),
        )
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 1);

    let error = block_on(seq.set_next(&key, 10)).unwrap_err();
    assert_eq!(error.kind, ErrorKind::Unsupported);
    assert!(
        error.explanation.contains("INV-2026-"),
        "the error names the series: {}",
        error.explanation
    );
    assert_eq!(seq.remaining(&key), 4, "the refusal consumed nothing");
    assert_eq!(
        block_on(seq.allocate(&key, 1)).unwrap(),
        2,
        "the branch still hands out its next leased number"
    );
}

#[test]
fn running_low_and_remaining_track_the_lease() {
    let key = series("INV-2026-");
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(
            &key,
            &node("edge-khi#branch_07"),
            10,
            at(2026, Month::June, 1, 12, 0),
        )
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(lease)).unwrap();

    assert_eq!(seq.len(), 1);
    assert!(!seq.is_empty());
    assert_eq!(seq.remaining(&key), 10);
    assert!(!seq.running_low(&key), "the whole range is still available");

    for expected in 1..=5 {
        assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), expected);
    }
    assert_eq!(seq.remaining(&key), 5);
    assert!(
        seq.running_low(&key),
        "half of the range is left, the signal to ask the hub for another"
    );

    let unleased = series("INV-2027-");
    assert_eq!(seq.remaining(&unleased), 0);
    assert!(seq.running_low(&unleased));
    assert!(seq.branch(&unleased).is_none());
    assert_eq!(seq.series_keys().count(), 1);
}

#[test]
fn installing_a_renewed_lease_continues_after_the_old_one() {
    let key = series("INV-2026-");
    let now = at(2026, Month::June, 1, 12, 0);
    let mut hub = MemoryRangeAllocator::new();
    let first = hub
        .lease(&key, &node("edge-khi#branch_07"), 2, now)
        .unwrap();
    let mut seq = LeasedSequences::new().with_lease(branch(first)).unwrap();

    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 1);
    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 2);
    assert!(block_on(seq.allocate(&key, 1)).is_err());

    let renewed = hub
        .lease(&key, &node("edge-khi#branch_07"), 3, now)
        .unwrap();
    assert_eq!((renewed.from, renewed.to), (3, 5));
    let replaced = seq.install(branch(renewed)).unwrap();
    assert!(replaced.is_some(), "the exhausted lease is replaced");
    assert_eq!(seq.remaining(&key), 3);
    assert_eq!(block_on(seq.allocate(&key, 1)).unwrap(), 3);

    let removed = seq.remove(&key);
    assert!(removed.is_some());
    assert!(seq.is_empty());
    assert!(block_on(seq.allocate(&key, 1)).is_err());
}

#[test]
fn a_series_with_no_lease_is_lease_exhausted_not_a_local_counter() {
    let key = series("INV-2026-");
    let mut seq = LeasedSequences::new();

    let error = seq.allocate_block(&key, 1).unwrap_err();
    assert!(matches!(
        error,
        LeasedSequencesError::LeaseExhausted {
            requested: 1,
            remaining: 0,
            ..
        }
    ));
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(
        error.to_string().contains("INV-2026-"),
        "the error names the series key: {error}"
    );
    assert!(
        !error.to_string().contains("branch_07"),
        "the error carries no node identity: {error}"
    );
}

#[test]
fn concurrent_threads_never_share_a_number() {
    const THREADS: usize = 4;
    const PER_THREAD: u32 = 50;

    let key = series("INV-2026-");
    let mut hub = MemoryRangeAllocator::new();
    let lease = hub
        .lease(
            &key,
            &node("edge-khi#branch_07"),
            THREADS as u64 * u64::from(PER_THREAD),
            at(2026, Month::June, 1, 12, 0),
        )
        .unwrap();
    let seq = Arc::new(Mutex::new(
        LeasedSequences::new().with_lease(branch(lease)).unwrap(),
    ));

    let numbers: Vec<u64> = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let seq = Arc::clone(&seq);
            let key = key.clone();
            handles.push(scope.spawn(move || {
                let mut mine = Vec::with_capacity(PER_THREAD as usize);
                for _ in 0..PER_THREAD {
                    let number = seq.lock().unwrap().allocate_block(&key, 1).unwrap();
                    mine.push(number);
                }
                mine
            }));
        }
        handles
            .into_iter()
            .flat_map(|handle| handle.join().unwrap())
            .collect()
    });

    let total = numbers.len();
    assert_eq!(total, THREADS * PER_THREAD as usize);
    let mut sorted = numbers;
    sorted.sort_unstable();
    let mut unique = sorted.clone();
    unique.dedup();
    assert_eq!(unique.len(), total, "no number was handed out twice");
    assert_eq!(
        unique,
        (1..=total as u64).collect::<Vec<_>>(),
        "the whole lease was used, exactly once each"
    );
}
