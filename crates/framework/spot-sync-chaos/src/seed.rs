//! Seeds: every failure names its seed and how to replay it.

use crate::model::ChaosError;

/// The environment variable that replaces a scenario's seeds with one: the replay hook.
pub const SEED_ENV: &str = "SPOT_CHAOS_SEED";

/// The one-line replay command for a failing seed, led by `SEED=<n>`.
#[must_use]
pub fn replay_line(seed: u64) -> String {
    format!(
        "SEED={seed} replay: {SEED_ENV}={seed} cargo test -p spot-sync-chaos -- --include-ignored"
    )
}

/// A failure report: the seed, the replay line and the reason.
#[must_use]
pub fn report_failure(error: &ChaosError) -> String {
    format!("{}\n  {error}", replay_line(error.seed))
}

/// The seed to replay, when the environment names one. A scenario that finds `Some` runs that
/// seed alone. This is the only ambient input of the crate and it only ever selects a seed.
#[must_use]
pub fn seed_override() -> Option<u64> {
    std::env::var(SEED_ENV)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}
