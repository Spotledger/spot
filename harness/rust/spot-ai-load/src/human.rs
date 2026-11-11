//! The virtual human users: a mix of document reads, filtered lists and one report-shaped
//! aggregate, with a pause between operations. Latency is what a person feels: the wait for a
//! pool connection plus the statement.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use deadpool_postgres::Pool;

use crate::calls::{Kind, Rng, execute};
use crate::clock::RunClock;
use crate::load::HumanLoad;
use crate::stats::Samples;

/// What the human users share.
pub struct HumanEnv {
    /// The human pool.
    pub pool: Pool,
    /// The run clock.
    pub clock: RunClock,
    /// The schema-qualified table.
    pub table: String,
    /// Rows in the table.
    pub rows: u64,
    /// The mean pause between operations.
    pub think: Duration,
    /// Out of 100 operations, how many are the aggregate.
    pub report_percent: u32,
    /// Where each finished operation is reported for the load signal.
    pub load: Arc<HumanLoad>,
    /// The seed of the operation plan.
    pub seed: u64,
    /// Set to end the users before their end time.
    pub stop: AtomicBool,
}

/// What one user measured.
#[derive(Default)]
pub struct HumanOutput {
    /// Every operation.
    pub all: Samples,
    /// Per operation name.
    pub by_op: BTreeMap<&'static str, Samples>,
    /// The wait for a pool connection.
    pub pool_wait: Samples,
    /// Operations that failed.
    pub errors: u64,
}

impl HumanOutput {
    /// Adds another user's measurements.
    pub fn merge(&mut self, other: &HumanOutput) {
        self.all.merge(&other.all);
        for (name, samples) in &other.by_op {
            self.by_op.entry(name).or_default().merge(samples);
        }
        self.pool_wait.merge(&other.pool_wait);
        self.errors += other.errors;
    }
}

/// Runs one user until `end` (a run-clock time) or until `env.stop` is set.
pub async fn run_user(env: Arc<HumanEnv>, index: u32, end: Duration) -> HumanOutput {
    let mut rng = Rng::new(env.seed ^ (u64::from(index) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut out = HumanOutput::default();
    let think_us = u64::try_from(env.think.as_micros())
        .unwrap_or(u64::MAX)
        .max(2);
    while !env.stop.load(Ordering::Relaxed) && env.clock.now() < end {
        let roll = rng.below(100);
        let kind = if roll < u64::from(env.report_percent) {
            Kind::Aggregate
        } else if roll < u64::from(env.report_percent) + 30 {
            Kind::Filter
        } else {
            Kind::Lookup
        };
        let started = env.clock.now();
        let got = env.pool.get().await;
        let waited = env.clock.now().saturating_sub(started);
        out.pool_wait.push(waited);
        let ok = match got {
            Ok(client) => execute(&client, kind, &env.table, env.rows, rng.next_u64(), 0)
                .await
                .is_ok(),
            Err(_) => false,
        };
        let latency = env.clock.now().saturating_sub(started);
        if ok {
            out.all.push(latency);
            out.by_op.entry(kind.name()).or_default().push(latency);
            env.load.record(latency, waited);
        } else {
            out.errors += 1;
        }
        // Between half and one and a half times the mean pause, so users drift apart.
        let pause = think_us / 2 + rng.below(think_us);
        tokio::time::sleep(Duration::from_micros(pause)).await;
    }
    out
}
