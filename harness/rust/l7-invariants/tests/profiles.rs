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

async fn on_fake(profile: Profile) {
    let config = DriverConfig::for_profile(profile, SEED);
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
#[ignore = "standard profile: 32 workers, 50,000 operations"]
async fn standard_on_the_fake() {
    on_fake(Profile::Standard).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "standard profile: 32 workers, 50,000 operations; needs PostgreSQL"]
async fn standard_on_postgres() {
    on_pg(Profile::Standard).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "nightly profile: 200 workers, 1,000,000 operations; long"]
async fn nightly_on_the_fake() {
    on_fake(Profile::Nightly).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "nightly profile: 200 workers, 1,000,000 operations; needs PostgreSQL; long"]
async fn nightly_on_postgres() {
    on_pg(Profile::Nightly).await;
}
