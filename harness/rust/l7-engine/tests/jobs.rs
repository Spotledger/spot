//! The job engine workload: idempotent jobs survive handler failures and killed workers, take
//! effect once per key, and reach a terminal state.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_jobs::JobState;
use spot_l7_engine::jobs::{JobConfig, check_jobs, run_jobs};
use spot_l7_engine::world::World;
use spot_l7_invariants::invariant::RunMeta;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_job_workload_is_green_and_retries_are_exercised() {
    let world = Arc::new(World::new());
    let config = JobConfig {
        seed: 5_150_151,
        jobs: 300,
        fail_one_in: 3,
        kill_one_in: 5,
        profile: "smoke".to_owned(),
    };
    let run = run_jobs(config.clone()).await;
    assert_eq!(run.inserted.len(), config.jobs as usize);
    assert_eq!(run.duplicates, config.jobs, "every job was enqueued twice");

    let meta = RunMeta {
        seed: config.seed,
        backend: "jobs".to_owned(),
        profile: config.profile.clone(),
        workers: 1,
    };
    let report = check_jobs(&Arc::new(run.clone()), world.catalog.clone(), meta).await;
    println!("{}", report.to_text());
    assert!(report.ok, "{}", report.to_text());

    let succeeded = run
        .outcomes
        .iter()
        .filter(|o| matches!(o.state, JobState::Succeeded))
        .count();
    let dead = run
        .outcomes
        .iter()
        .filter(|o| matches!(o.state, JobState::Dead))
        .count();
    assert!(succeeded > 0, "some jobs must succeed");
    assert_eq!(run.effects.len(), succeeded, "one effect per succeeded job");
    // The kill/fail probabilities make retries (and some dead-letters) certain.
    assert!(
        run.outcomes.iter().any(|o| o.attempts > 0),
        "some jobs must have retried"
    );
    assert!(dead > 0, "some jobs must be dead-lettered");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_key_takes_effect_at_most_once_even_with_kills() {
    let config = JobConfig {
        seed: 8_181_818,
        jobs: 100,
        fail_one_in: 4,
        kill_one_in: 4,
        profile: "smoke".to_owned(),
    };
    let run = run_jobs(config).await;
    for (key, count) in &run.effects {
        assert_eq!(*count, 1, "key {key} took effect {count} times");
    }
}
