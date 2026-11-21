#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]
//! The long deterministic run (WP-SYN-11.T5, ADR-062 s4, ADR-066 s5 and s7).
//!
//! One seed drives thirty simulated days of mixed disturbances over a hub and 100 nodes: node
//! outages of a minute to 36 hours, link flaps, nodes restored from an older backup, clock
//! jumps, and a rolling release upgrade (N and N-1 nodes side by side). After every day the
//! checker of the suite runs, plus the chain-wide invariants of `soak/invariants.rs`; at the end
//! of day 30 a final convergence phase runs until quiet and every node's state is compared with
//! what the hub's stream and the node's scope say it must hold.
//!
//! * `soak_smoke`: 3 days, 10 nodes, 5 seeds; part of the default test run.
//! * `soak_30_days` (`--ignored`): 30 days, 100 nodes, 20 seeds (`SPOT_SOAK_SEEDS=1000` for the
//!   nightly harness).
//! * `soak_replay` (`--ignored`): `SEED=<n>` (and `SPOT_SOAK_SHAPE=smoke|full`) replays one
//!   seed, printing the plan, the trace of the failing day and the first violated invariant.
//!
//! What the run does **not** do, because the World of `spot-sync-chaos` does not offer it and
//! this task may not change that crate's source (see the report's open questions): create
//! documents offline at nodes, a second hub tenant with its own nodes (a foreign tenant's
//! records are written into the stream and must never arrive), restart the hub, run the
//! obligation ledger or the repair loops, or serve a node by release. The release upgrade is
//! therefore run next to the World with the real `spot-sync-skew` functions
//! (`soak/release.rs`), and the hub restart is planned and traced but not simulated.

#[path = "soak/invariants.rs"]
mod invariants;
#[path = "soak/plan.rs"]
mod plan;
#[path = "soak/release.rs"]
mod release;
#[path = "soak/run.rs"]
mod run;

use std::time::Instant;

use plan::{Shape, SoakPlan, SoakSettings};
use run::{SoakFailure, SoakReport};

/// The seeds of a run: `0..count`, where `count` is the default unless `variable` says otherwise.
fn seed_count(variable: &str, default: u64) -> u64 {
    std::env::var(variable)
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .filter(|count| *count > 0)
        .unwrap_or(default)
}

fn run_seed(shape: Shape, seed: u64) -> Result<SoakReport, SoakFailure> {
    run::run(&SoakSettings::of(shape, seed))
}

fn describe(report: &SoakReport) -> String {
    format!(
        "SEED={} digest {:016x} rounds {} (settled in {}) outages {} flaps {} jumps {} restores {} \
         (skipped {}) hub restarts planned {} held at end {} upgrade offers {}",
        report.seed,
        report.digest,
        report.rounds,
        report.settle_rounds,
        report.outages,
        report.flaps,
        report.jumps,
        report.restores,
        report.restores_skipped,
        report.hub_restarts,
        report.held_at_end,
        report.upgrade_offers
    )
}

#[test]
fn soak_smoke() {
    let seeds = seed_count("SPOT_SOAK_SMOKE_SEEDS", 5);
    for seed in 0..seeds {
        match run_seed(Shape::Smoke, seed) {
            Ok(report) => {
                assert!(report.rounds > 0);
                assert!(
                    report.upgrade_offers > 0,
                    "SEED={seed}: the too-old node was never offered the upgrade"
                );
            }
            Err(failure) => panic!("{}", failure.report()),
        }
    }
}

#[test]
fn the_same_seed_gives_a_byte_identical_trace() {
    for seed in [3, 11] {
        let first = run_seed(Shape::Smoke, seed).unwrap_or_else(|f| panic!("{}", f.report()));
        let second = run_seed(Shape::Smoke, seed).unwrap_or_else(|f| panic!("{}", f.report()));
        assert_eq!(
            first.digest, second.digest,
            "SEED={seed}: two runs of one seed left different traces"
        );
        assert_eq!(first, second, "SEED={seed}");
    }
    let a = run_seed(Shape::Smoke, 1).unwrap_or_else(|f| panic!("{}", f.report()));
    let b = run_seed(Shape::Smoke, 2).unwrap_or_else(|f| panic!("{}", f.report()));
    assert_ne!(a.digest, b.digest, "two seeds played the same game");
}

#[test]
fn a_failing_seed_prints_a_one_line_replay_command() {
    let failure = SoakFailure {
        seed: 77,
        shape: "full",
        day: 9,
        invariant: "guarantee",
        detail: "boom".to_owned(),
        trace_tail: vec!["a line".to_owned()],
    };
    let line = failure.replay_line();
    assert!(line.starts_with("SEED=77 "));
    assert!(!line.contains('\n'));
    assert!(line.contains("SPOT_SOAK_SHAPE=full"));
    assert!(line.contains("soak_replay"));
    let report = failure.report();
    assert!(report.contains("guarantee") && report.contains("boom") && report.contains("a line"));
}

/// The acceptance run. Slow: run it in release.
///
/// `cargo test -p spot-sync-chaos --release --test soak -- --ignored soak_30_days`
#[test]
#[ignore = "soak"]
fn soak_30_days() {
    let seeds = seed_count("SPOT_SOAK_SEEDS", 20);
    let mut digests: Vec<(u64, u64)> = Vec::new();
    for seed in 0..seeds {
        let started = Instant::now();
        let report = match run_seed(Shape::Full, seed) {
            Ok(report) => report,
            Err(failure) => panic!("{}", failure.report()),
        };
        // The wall clock is read here, in the test, only to say how slow the simulator was; it
        // decides nothing and never enters the world.
        let seconds = started.elapsed().as_secs();
        println!("{} in {seconds}s", describe(&report));
        if seconds > 90 {
            println!(
                "NOTE SEED={seed}: the 30 day, 100 node run took {seconds}s (reference 90s); a slow \
                 world is a finding about the simulator, not a protocol defect"
            );
        }
        assert!(
            report.upgrade_offers > 0,
            "SEED={seed}: the too-old node was never offered the upgrade"
        );
        digests.push((seed, report.digest));
    }
    // Determinism at full size: one seed again, same trace.
    if let Some((seed, digest)) = digests.first().copied() {
        let again = run_seed(Shape::Full, seed).unwrap_or_else(|f| panic!("{}", f.report()));
        assert_eq!(
            digest, again.digest,
            "SEED={seed}: the full run is not deterministic"
        );
    }
}

/// Replays one seed from the environment: prints the plan, runs it, and on failure prints the
/// trace of the failing day and the first violated invariant.
///
/// `SEED=<n> SPOT_SOAK_SHAPE=full cargo test -p spot-sync-chaos --test soak soak_replay -- --ignored --nocapture`
#[test]
#[ignore = "replay"]
fn soak_replay() {
    let Some(seed) = std::env::var("SEED")
        .ok()
        .and_then(|text| text.trim().parse::<u64>().ok())
    else {
        println!("soak_replay: set SEED=<n> (and SPOT_SOAK_SHAPE=smoke|full) to replay a seed");
        return;
    };
    let shape = std::env::var("SPOT_SOAK_SHAPE")
        .ok()
        .and_then(|text| Shape::parse(&text))
        .unwrap_or(Shape::Smoke);
    let settings = SoakSettings::of(shape, seed);
    println!("{}", SoakPlan::generate(&settings).describe());
    match run::run(&settings) {
        Ok(report) => println!("{}", describe(&report)),
        Err(failure) => panic!("{}", failure.report()),
    }
}
