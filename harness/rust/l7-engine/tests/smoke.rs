//! The engine workload and every engine invariant, on the in-memory fake.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_l7_engine::driver::DriverConfig;
use spot_l7_engine::runner::run_engine;
use spot_l7_engine::world::World;
use spot_storage_fake::FakeBackend;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_engine_workload_is_green_on_the_fake() {
    let world = Arc::new(World::new());
    let mut config = DriverConfig::for_profile(
        spot_l7_invariants::schedule::Profile::Smoke,
        20_261_012,
        "fake",
    );
    config.workers = 4;
    config.operations = 400;
    let (run, report) = run_engine(world, config, FakeBackend::new()).await.unwrap();
    println!("{}", report.to_text());
    assert!(
        !run.oracle.denials.is_empty(),
        "the workload must exercise at least one refusal"
    );
    assert!(report.ok, "{}", report.to_text());
}
