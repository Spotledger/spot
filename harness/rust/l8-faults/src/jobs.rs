//! The job-engine scenario: jobs survive a killed worker, leases expire and are re-taken,
//! idempotency keys take effect exactly once and retries stay bounded.
//!
//! It uses the real job engine and its job invariants from WP-TST-07.T2
//! ([`spot_l7_engine::jobs`]). A "killed worker" is a claim that is abandoned without completing:
//! the lease expires, `reap_expired` returns the job to `Retrying`, and another worker takes it.
//! Each idempotency key is enqueued twice, exactly as an at-least-once document event stream
//! would deliver it.
//!
//! # Honest limit
//!
//! The job engine's PostgreSQL store is a later task; this scenario runs over the in-memory store
//! the engine ships today, so it proves the **engine's** crash behaviour (lost worker, expired
//! lease, dedup, bounded retries), not a PostgreSQL job table surviving a server kill. That part
//! arrives with the PostgreSQL job store.

use spot_l7_engine::jobs::{JobConfig, check_jobs, run_jobs};
use spot_l7_engine::world::World;
use spot_l7_invariants::invariant::{Report, RunMeta};

/// How the job scenario is sized.
#[derive(Clone, Debug)]
pub struct JobScenarioConfig {
    /// The seed.
    pub seed: u64,
    /// Jobs to enqueue (each twice).
    pub jobs: u64,
    /// One in this many handler runs fails and retries.
    pub fail_one_in: u64,
    /// One in this many handler runs is killed (the claim is abandoned).
    pub kill_one_in: u64,
    /// The profile name.
    pub profile: String,
}

impl JobScenarioConfig {
    /// A configuration for a profile.
    pub fn for_profile(
        profile: spot_l7_invariants::schedule::Profile,
        seed: u64,
    ) -> JobScenarioConfig {
        let jobs = match profile {
            spot_l7_invariants::schedule::Profile::Smoke => 200,
            spot_l7_invariants::schedule::Profile::Standard => 2_000,
            spot_l7_invariants::schedule::Profile::Nightly => 20_000,
        };
        JobScenarioConfig {
            seed,
            jobs,
            fail_one_in: 3,
            kill_one_in: 4,
            profile: profile.name().to_owned(),
        }
    }
}

/// What the job scenario found.
pub struct JobScenarioOutcome {
    /// The job invariants' report.
    pub report: Report,
    /// Jobs the store inserted.
    pub inserted: usize,
    /// Enqueues the store deduplicated.
    pub duplicates: u64,
    /// Notes for the report.
    pub notes: Vec<String>,
}

/// Runs the job workload and checks the job invariants.
pub async fn run_job_scenario(config: JobScenarioConfig) -> JobScenarioOutcome {
    let run = run_jobs(JobConfig {
        seed: config.seed,
        jobs: config.jobs,
        fail_one_in: config.fail_one_in,
        kill_one_in: config.kill_one_in,
        profile: config.profile.clone(),
    })
    .await;
    let inserted = run.inserted.len();
    let duplicates = run.duplicates;
    let effects = run.effects.len();
    let run = std::sync::Arc::new(run);
    let world = World::new();
    let meta = RunMeta {
        seed: config.seed,
        backend: "jobs".to_owned(),
        profile: config.profile.clone(),
        workers: 1,
    };
    let report = check_jobs(&run, world.catalog.clone(), meta).await;
    let notes = vec![
        format!("jobs inserted={inserted} deduplicated={duplicates} keys_with_effect={effects}"),
        "the PostgreSQL job store is a later task; this proves the engine's lease, dedup and \
         retry behaviour over the in-memory store"
            .to_owned(),
    ];
    JobScenarioOutcome {
        report,
        inserted,
        duplicates,
        notes,
    }
}
