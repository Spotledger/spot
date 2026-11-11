#![allow(clippy::unwrap_used, clippy::expect_used)]
//! A hundred nodes return at once (ADR-066 section 7): admission control bounds how many
//! bootstrap together, the others are told to come back with a spread delay, and every node ends
//! up converged.

use spot_sync_bootstrap::AdmissionSettings;
use spot_sync_chaos::{World, WorldConfig, assert_guarantees, replay_line, seed_override};

fn scale_world(seed: u64, nodes: usize) -> World {
    World::build(WorldConfig {
        nodes,
        records: 9,
        versions: 2,
        batch: 3,
        ..WorldConfig::seeded(seed)
    })
    .unwrap()
}

fn all_return(seed: u64, nodes: usize, settings: AdmissionSettings) {
    let mut world = scale_world(seed, nodes);
    world.settle(400).unwrap();
    let everyone: Vec<usize> = (0..nodes).collect();
    let report = world
        .return_all(&everyone, settings)
        .unwrap_or_else(|error| panic!("{}\n  {error}", replay_line(seed)));
    assert_eq!(report.completed, nodes, "{}", replay_line(seed));
    assert!(
        report.max_running <= settings.max_concurrent_bootstraps,
        "{} {} bootstraps ran at once, the limit is {}",
        replay_line(seed),
        report.max_running,
        settings.max_concurrent_bootstraps
    );
    assert_eq!(report.admitted.len(), nodes);
    world.settle(400).unwrap();
    assert_guarantees(&world);
}

#[test]
fn a_hundred_nodes_return_at_once_within_the_admission_limit() {
    let settings = AdmissionSettings::default();
    let mut world = scale_world(1, 100);
    world.settle(400).unwrap();
    let everyone: Vec<usize> = (0..100).collect();
    let report = world.return_all(&everyone, settings).unwrap();
    assert_eq!(report.completed, 100);
    assert!(report.max_running <= settings.max_concurrent_bootstraps);
    assert!(
        report.max_running >= 2,
        "the admission controller was never exercised"
    );
    // Eight slots for a hundred nodes: most were told to come back, and with a spread.
    assert!(report.retries >= 90, "only {} retries", report.retries);
    assert!(
        report.elapsed_ms >= settings.retry_base_ms,
        "the herd was not spread over time"
    );
    world.settle(400).unwrap();
    assert_guarantees(&world);
}

#[test]
fn a_tighter_limit_still_completes_and_takes_longer() {
    let loose = AdmissionSettings::default();
    let tight = AdmissionSettings {
        max_concurrent_bootstraps: 2,
        ..loose
    };
    let mut a = scale_world(2, 40);
    a.settle(400).unwrap();
    let mut b = scale_world(2, 40);
    b.settle(400).unwrap();
    let nodes: Vec<usize> = (0..40).collect();
    let fast = a.return_all(&nodes, loose).unwrap();
    let slow = b.return_all(&nodes, tight).unwrap();
    assert_eq!(fast.completed, 40);
    assert_eq!(slow.completed, 40);
    assert!(slow.max_running <= 2);
    assert!(slow.elapsed_ms >= fast.elapsed_ms);
    b.settle(400).unwrap();
    assert_guarantees(&b);
}

#[test]
fn the_pacing_of_chunks_is_exercised_per_node() {
    let settings = AdmissionSettings {
        chunk_burst: 1,
        chunk_interval_ms: 200,
        ..AdmissionSettings::default()
    };
    let mut world = scale_world(3, 10);
    world.settle(400).unwrap();
    let nodes: Vec<usize> = (0..10).collect();
    let report = world.return_all(&nodes, settings).unwrap();
    assert_eq!(report.completed, 10);
    assert!(report.paced > 0, "no chunk was ever paced");
}

#[test]
fn the_same_seed_returns_the_same_admission_order() {
    let run = || {
        let mut world = scale_world(9, 30);
        world.settle(400).unwrap();
        let nodes: Vec<usize> = (0..30).collect();
        world
            .return_all(&nodes, AdmissionSettings::default())
            .unwrap()
    };
    assert_eq!(run(), run());
}

/// The long run the nightly harness uses: a thousand seeds. Replay one with
/// `SPOT_CHAOS_SEED=<n>`.
#[test]
#[ignore = "seeds"]
fn seeds() {
    let seeds: Vec<u64> = match seed_override() {
        Some(seed) => vec![seed],
        None => (0..1_000).collect(),
    };
    for seed in seeds {
        // Vary the shape with the seed so the thousand runs are not one run a thousand times.
        let nodes = 10 + usize::try_from(seed % 91).unwrap_or(0);
        let settings = AdmissionSettings {
            max_concurrent_bootstraps: 1 + usize::try_from(seed % 8).unwrap_or(0),
            ..AdmissionSettings::default()
        };
        all_return(seed, nodes, settings);
    }
}
