#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A seed replays byte for byte, and a failing seed says how to replay it.

use spot_sync_chaos::{ChaosError, World, WorldConfig, replay_line, report_failure};

/// Seeds kept from earlier runs of the suite; each is replayed twice and compared.
const STORED_SEEDS: [u64; 6] = [0, 1, 7, 42, 4_242, 9_999_999];

fn chaotic(seed: u64) -> WorldConfig {
    WorldConfig {
        nodes: 4,
        reorder_window: 6,
        duplicate: true,
        loss_permille: 120,
        ..WorldConfig::seeded(seed)
    }
}

fn run(seed: u64) -> String {
    let mut world = World::build(chaotic(seed)).unwrap();
    world.settle(600).unwrap();
    world.transcript_text()
}

#[test]
fn replaying_a_stored_seed_is_byte_identical() {
    for seed in STORED_SEEDS {
        let first = run(seed);
        let second = run(seed);
        assert!(!first.is_empty());
        assert_eq!(first.as_bytes(), second.as_bytes(), "SEED={seed}");
    }
}

#[test]
fn different_seeds_do_not_all_play_the_same_game() {
    let a = run(1);
    let b = run(2);
    assert_ne!(a, b);
}

#[test]
fn a_failure_prints_the_seed_and_a_one_line_replay_command() {
    let line = replay_line(77);
    assert!(line.starts_with("SEED=77 "));
    assert!(!line.contains('\n'));
    assert!(line.contains("SPOT_CHAOS_SEED=77"));
    assert!(line.contains("cargo test -p spot-sync-chaos"));
    let report = report_failure(&ChaosError::new(77, "boom"));
    assert!(report.starts_with("SEED=77 "));
    assert!(report.contains("boom"));
}

#[test]
fn a_world_that_cannot_converge_reports_its_seed() {
    let mut world = World::build(WorldConfig {
        nodes: 1,
        loss_permille: 1_000,
        ..WorldConfig::seeded(31)
    })
    .unwrap();
    let error = world.settle(5).unwrap_err();
    assert_eq!(error.seed, 31);
    assert!(error.to_string().starts_with("SEED=31 "));
}
