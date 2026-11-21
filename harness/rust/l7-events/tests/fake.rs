//! The engine workload on the in-memory fake, judged by the six invariants.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_l7_engine::driver::DriverConfig;
use spot_l7_engine::runner::run_engine;
use spot_l7_engine::world::World;
use spot_l7_events::table::TopicTable;
use spot_l7_invariants::invariant::StoreAccess;
use spot_l7_invariants::schedule::Profile;
use spot_storage_fake::FakeBackend;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_event_of_the_engine_workload_conforms_on_the_fake() {
    let world = Arc::new(World::new());
    let mut config = DriverConfig::for_profile(Profile::Smoke, 20_261_113, "fake");
    config.workers = 4;
    config.operations = 600;
    let (run, base) = run_engine(world.clone(), config, FakeBackend::new())
        .await
        .unwrap();
    assert!(
        base.ok,
        "the L7 engine invariants hold first: {}",
        base.to_text()
    );
    let report = support::check_run(&world, &run).await;
    let access =
        spot_l7_engine::runner::EngineAccess::new(run.engine.clone(), world.catalog.clone());
    println!(
        "{}",
        TopicTable::of(&access.outbox().await.unwrap()).to_text()
    );
    assert!(report.ok, "{}", report.to_text());
}
