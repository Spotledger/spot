//! The larger profiles. They run only when asked for (`cargo test -p spot-l7-invariants --
//! --ignored`), and through the `spot-l7` binary in CI.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_invariants::driver::DriverConfig;
use spot_l7_invariants::model::Model;
use spot_l7_invariants::pg::pg_env;
use spot_l7_invariants::runner::run_and_check;
use spot_l7_invariants::schedule::Profile;
use spot_storage_fake::FakeBackend;

const SEED: u64 = 424_242;

/// The fake's cost grows with the data it holds (the first public nightly: the 50,000-operation
/// standard profile took 28 minutes on it, against one minute on PostgreSQL, and the 1,000,000
/// operation nightly profile was still running after 50). The fake runs are therefore bounded by
/// `SPOT_L7_FAKE_OPERATIONS` (default 20,000, as in `spot-l7-engine`'s profiles); the full
/// profiles run on PostgreSQL.
fn fake_operations(profile: Profile) -> u64 {
    std::env::var("SPOT_L7_FAKE_OPERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| profile.operations().min(20_000))
}

async fn on_fake(profile: Profile) {
    let mut config = DriverConfig::for_profile(profile, SEED);
    config.operations = fake_operations(profile);
    let model = Arc::new(Model::new().unwrap());
    let (_, report) = run_and_check(Arc::new(FakeBackend::new()), model, config, "fake", |_| {})
        .await
        .unwrap();
    assert!(report.ok, "{}", report.to_text());
}

async fn on_pg(profile: Profile) {
    let Some(env) = pg_env(profile.workers()).await else {
        return;
    };
    let config = DriverConfig::for_profile(profile, SEED);
    let model = Arc::new(Model::new().unwrap());
    let (_, report) = run_and_check(env.backend.clone(), model, config, "pg", |_| {})
        .await
        .unwrap();
    assert!(report.ok, "{}", report.to_text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "standard profile: 32 workers, 20,000 operations on the fake (SPOT_L7_FAKE_OPERATIONS)"]
async fn standard_on_the_fake() {
    on_fake(Profile::Standard).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "standard profile: 32 workers, 50,000 operations; needs PostgreSQL"]
async fn standard_on_postgres() {
    on_pg(Profile::Standard).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "nightly profile: 200 workers, 20,000 operations on the fake (SPOT_L7_FAKE_OPERATIONS)"]
async fn nightly_on_the_fake() {
    on_fake(Profile::Nightly).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "nightly profile: 200 workers, 1,000,000 operations; needs PostgreSQL; long"]
async fn nightly_on_postgres() {
    on_pg(Profile::Nightly).await;
}
