//! A deterministic, fault-injecting simulator (ADR-062 section 6, WP-SYN-11 in miniature).
//!
//! The simulator drives the real [`Sender`](crate::Sender) and [`Receiver`](crate::Receiver)
//! state machines over a seeded, discrete-event network that can **drop, duplicate, reorder and
//! partition** messages and can **crash** either side at any protocol step. It reads no clock and
//! no randomness outside its seed, so a failing seed replays exactly.
//!
//! Two topologies, matching the two session directions:
//!
//! * [`Direction::Pull`] (down): a hub with `sources` streams produces records; `edges` edges pull
//!   and apply all of them. The property: every edge's applied set equals the produced set
//!   exactly once, in per-source sequence order, for every seed.
//! * [`Direction::Push`] (up): `edges` edges each own `sources` streams and push them; the hub
//!   applies all of them. The same property holds at the hub.
//!
//! Both start with a version-negotiating [`Hello`](crate::Hello) handshake and carry bounded flow
//! control, per-session retransmission with jittered backoff, and the two-phase checkpoint rule.
//!
//! Time is a logical tick; the network's latency, drops and crashes are drawn from one seeded
//! generator in a fixed event order, so `run` is a pure function of its [`SimConfig`].

mod engine;

use std::fmt;

pub use engine::run_seeds;

/// Which way records flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Down: a hub produces, edges pull and apply (ADR-062 "pull").
    Pull,
    /// Up: edges produce, the hub pulls and applies (ADR-062 "push").
    Push,
}

/// How the simulated network misbehaves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetworkConfig {
    /// Chance in parts per thousand that a message is dropped.
    pub drop_permille: u64,
    /// Chance in parts per thousand that a message is delivered twice.
    pub duplicate_permille: u64,
    /// The shortest delivery latency, in ticks.
    pub latency: u64,
    /// The extra latency drawn uniformly in `0..jitter`, in ticks (reordering).
    pub jitter: u64,
    /// Chance in parts per thousand that a chaos step starts a partition.
    pub partition_permille: u64,
    /// How long a started partition lasts, in ticks.
    pub partition_ticks: u64,
    /// How often a chaos step runs, in ticks (only while records are still being produced).
    pub chaos_every: u64,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            drop_permille: 40,
            duplicate_permille: 30,
            latency: 1,
            jitter: 3,
            partition_permille: 30,
            partition_ticks: 25,
            chaos_every: 40,
        }
    }
}

/// One simulator run's parameters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimConfig {
    /// The seed; a run is a pure function of this.
    pub seed: u64,
    /// Which way records flow.
    pub direction: Direction,
    /// The site name used to build source ids.
    pub site: String,
    /// Streams: the hub's sources (pull) or each edge's sources (push).
    pub sources: usize,
    /// How many edges take part.
    pub edges: usize,
    /// How many records in total.
    pub records: usize,
    /// Distinct versions written per record name (exercises name-and-version idempotency).
    pub versions_per_name: u64,
    /// The most records in one batch.
    pub max_batch: usize,
    /// The most sequences outstanding ahead of the acknowledged high-water mark.
    pub window: u64,
    /// Ticks between two produced records.
    pub produce_every: u64,
    /// Ticks an edge waits after an empty pull response before asking again.
    pub poll_every: u64,
    /// Ticks before an unanswered request is retransmitted.
    pub retry_every: u64,
    /// Chance in parts per thousand of a crash at each protocol step.
    pub crash_permille: u64,
    /// Fail one checkpoint store write in every `store_fail_every` (0 or 1 disables it); this is
    /// the crash *between* a durable apply and its checkpoint.
    pub store_fail_every: u64,
    /// The network.
    pub network: NetworkConfig,
    /// Give up after this many ticks.
    pub max_ticks: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            seed: 0,
            direction: Direction::Pull,
            site: "localhost".to_owned(),
            sources: 4,
            edges: 3,
            records: 500,
            versions_per_name: 5,
            max_batch: 16,
            window: 32,
            produce_every: 1,
            poll_every: 30,
            retry_every: 20,
            crash_permille: 5,
            store_fail_every: 13,
            network: NetworkConfig::default(),
            max_ticks: 100_000,
        }
    }
}

/// What one run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SimReport {
    /// The seed.
    pub seed: u64,
    /// The direction.
    pub direction: Direction,
    /// How many records the producers made.
    pub produced: usize,
    /// How many records each consumer applied (one entry per edge for pull, one for the hub for
    /// push).
    pub applied: Vec<usize>,
    /// The logical ticks the run took.
    pub ticks: u64,
    /// How many messages entered the network.
    pub sent: u64,
    /// How many messages the network dropped or a partition swallowed.
    pub dropped: u64,
}

/// Why a run did not converge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimError {
    /// The seed, so the failure replays.
    pub seed: u64,
    /// A plain-language reason.
    pub reason: String,
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "simulation seed {} failed: {}", self.seed, self.reason)
    }
}

impl std::error::Error for SimError {}

/// A seeded generator for the simulator: xorshift64*, so a seed replays exactly.
#[derive(Clone, Debug)]
pub(crate) struct Rng(u64);

impl Rng {
    /// A generator from a seed (never zero).
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    /// The next 64 bits.
    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A value below `n` (0 when `n` is 0).
    pub(crate) fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// True with probability `per_mille` in a thousand.
    pub(crate) fn chance(&mut self, per_mille: u64) -> bool {
        per_mille > 0 && self.below(1_000) < per_mille.min(1_000)
    }
}

/// Runs one simulation and checks its invariant, returning what it did.
///
/// The invariant checked is the acceptance criterion: the consumer's final applied set equals the
/// produced set exactly once, in per-source sequence order. A violation or a failure to converge
/// within [`SimConfig::max_ticks`] returns [`SimError`] naming the seed.
pub fn run(config: &SimConfig) -> Result<SimReport, SimError> {
    engine::run(config)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn rng_is_seeded_and_replays() {
        let mut a = Rng::new(7);
        let mut b = Rng::new(7);
        for _ in 0..10 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert!(Rng::new(1).chance(1000));
        assert!(!Rng::new(1).chance(0));
    }

    #[test]
    fn a_clean_run_converges_in_both_directions() {
        for direction in [Direction::Pull, Direction::Push] {
            let config = SimConfig {
                direction,
                network: NetworkConfig {
                    drop_permille: 0,
                    duplicate_permille: 0,
                    jitter: 0,
                    partition_permille: 0,
                    ..NetworkConfig::default()
                },
                crash_permille: 0,
                store_fail_every: 0,
                ..SimConfig::default()
            };
            let report = run(&config).unwrap();
            assert_eq!(report.produced, config.records);
            assert!(report.applied.iter().all(|&n| n == config.records));
        }
    }
}
