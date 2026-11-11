//! `smoke` runs: the whole workload and every framework invariant on the in-memory fake and on
//! PostgreSQL. A run that fails prints the report with its seed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_invariants::driver::DriverConfig;
use spot_l7_invariants::model::Model;
use spot_l7_invariants::pg::pg_env;
use spot_l7_invariants::runner::run_and_check;
use spot_l7_invariants::schedule::Profile;
use spot_storage_fake::FakeBackend;

const SEED: u64 = 20_261_012;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smoke_is_green_on_the_fake() {
    let model = Arc::new(Model::new().unwrap());
    let config = DriverConfig::for_profile(Profile::Smoke, SEED);
    let (done, report) = run_and_check(Arc::new(FakeBackend::new()), model, config, "fake", |_| {})
        .await
        .unwrap();
    println!("{}", report.to_text());
    assert!(report.ok, "{}", report.to_text());
    let counts = done.history.counts();
    assert_eq!(counts.total, Profile::Smoke.operations());
    assert!(counts.committed > 500, "{counts:?}");
    assert_eq!(
        counts.failed, counts.exhausted,
        "unexpected failures: {counts:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn smoke_is_green_on_postgres() {
    let Some(env) = pg_env(Profile::Smoke.workers()).await else {
        return;
    };
    let model = Arc::new(Model::new().unwrap());
    let config = DriverConfig::for_profile(Profile::Smoke, SEED);
    let (done, report) = run_and_check(env.backend.clone(), model, config, "pg", |_| {})
        .await
        .unwrap();
    println!("{}", report.to_text());
    assert!(report.ok, "{}", report.to_text());
    let counts = done.history.counts();
    assert_eq!(counts.total, Profile::Smoke.operations());
    assert_eq!(
        counts.failed, counts.exhausted,
        "unexpected failures: {counts:?}"
    );
}
