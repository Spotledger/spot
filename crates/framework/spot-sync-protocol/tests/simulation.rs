//! The acceptance property of WP-EDG-02.T1 (ADR-062 section 6): over a seeded, lossy,
//! duplicating, reordering, partitioning network with crashes at every protocol step, the
//! consumer's final applied set equals the produced set exactly once, in per-source order.
//!
//! The simulator checks that invariant itself and returns `Err` naming the seed when it fails, so
//! every seed below is a proof that the state machines resume after a cut at any point without a
//! duplicate or a gap.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(missing_docs)]

use spot_sync_protocol::sim::{self, Direction, NetworkConfig, SimConfig};
use spot_sync_protocol::{SimReport, run_seeds};

/// The 500-records-over-4-sources, 3-edges pull property, for 2,000 seeds.
#[test]
fn pull_holds_for_two_thousand_seeds() {
    let config = SimConfig {
        direction: Direction::Pull,
        records: 500,
        sources: 4,
        edges: 3,
        ..SimConfig::default()
    };
    run_seeds(&config, 0..2_000).unwrap();
}

/// The upward direction: edges produce, the hub applies, with the same faults.
#[test]
fn push_holds_for_two_hundred_seeds() {
    let config = SimConfig {
        direction: Direction::Push,
        records: 300,
        sources: 2,
        edges: 3,
        ..SimConfig::default()
    };
    run_seeds(&config, 0..200).unwrap();
}

/// A harsher network: heavier loss, duplication and partitions, and a crash more often than not,
/// over fewer seeds. This is the "cut at every protocol step" corner.
#[test]
fn a_hostile_network_still_converges() {
    let config = SimConfig {
        direction: Direction::Pull,
        records: 240,
        sources: 3,
        edges: 3,
        max_batch: 8,
        window: 16,
        crash_permille: 200,
        store_fail_every: 7,
        network: NetworkConfig {
            drop_permille: 150,
            duplicate_permille: 150,
            latency: 1,
            jitter: 6,
            partition_permille: 120,
            partition_ticks: 40,
            chaos_every: 20,
        },
        max_ticks: 200_000,
        ..SimConfig::default()
    };
    run_seeds(&config, 0..120).unwrap();
}

/// One seed, inspected: the run converged, dropped some messages, and reported sane numbers.
#[test]
fn a_single_faulty_run_is_observable() {
    let config = SimConfig {
        direction: Direction::Pull,
        seed: 99,
        records: 200,
        ..SimConfig::default()
    };
    let SimReport {
        produced,
        applied,
        dropped,
        ticks,
        ..
    } = sim::run(&config).unwrap();
    assert_eq!(produced, 200);
    assert_eq!(applied, vec![200, 200, 200]);
    assert!(dropped > 0, "the network dropped nothing");
    assert!(ticks > 0);
}

/// The same seed replays exactly, so a reported failure is reproducible.
#[test]
fn a_seed_replays_exactly() {
    let config = SimConfig {
        seed: 5,
        records: 120,
        ..SimConfig::default()
    };
    let first = sim::run(&config).unwrap();
    let second = sim::run(&config).unwrap();
    assert_eq!(first, second, "the same seed must replay exactly");
}
