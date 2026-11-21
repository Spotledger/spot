//! Shared test configuration for the L8 integration tests.
#![allow(dead_code)]

use spot_l7_invariants::schedule::Profile;
use spot_l8_isolation::history::Workload;
use spot_l8_isolation::workload::WorkloadConfig;

/// A small but conflict-heavy configuration: very few keys, no cold keys, and a heavy bias to
/// read-modify-write, so the weakened backends reliably produce their anomaly.
pub fn conflict_heavy(workload: Workload, seed: u64) -> WorkloadConfig {
    let mut c = WorkloadConfig::for_profile(Profile::Smoke, workload, seed);
    c.transactions = 400;
    c.workers = 8;
    c.keys = 3;
    c.hot_keys = 3;
    c.hot_percent = 100;
    c.write_percent = 50;
    c.double_percent = 90;
    c
}

/// A very small configuration, for tests that only need the machinery to run.
pub fn tiny(workload: Workload, seed: u64) -> WorkloadConfig {
    let mut c = WorkloadConfig::for_profile(Profile::Smoke, workload, seed);
    c.transactions = 60;
    c.workers = 4;
    c.keys = 6;
    c
}
