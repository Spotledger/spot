//! The same run on PostgreSQL. Without `SPOT_TEST_PG_URL` it prints `SKIPPED`; under
//! `SPOT_REQUIRE_PG=1` a missing server is a failure (`TestDb::create` decides).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use spot_l7_engine::driver::DriverConfig;
use spot_l7_engine::runner::run_engine;
use spot_l7_engine::world::World;
use spot_l7_invariants::schedule::Profile;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_event_of_the_engine_workload_conforms_on_postgres() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    };
    let mut pg: PgConfig = db.config();
    pg.pool_max_size = 12;
    pg.pool_wait_timeout = Duration::from_secs(300);
    let backend = PgBackend::connect(pg).await.unwrap();
    let world = Arc::new(World::new());
    let mut config = DriverConfig::for_profile(Profile::Smoke, 20_261_113, "pg");
    config.workers = 4;
    config.operations = 400;
    let (run, base) = run_engine(world.clone(), config, backend).await.unwrap();
    assert!(base.ok, "{}", base.to_text());
    let report = support::check_run(&world, &run).await;
    assert!(report.ok, "{}", report.to_text());
}
