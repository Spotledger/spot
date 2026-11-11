//! The `standard` and `nightly` profiles, run on demand (`cargo test -- --ignored` or
//! `ci/run.sh`), on the fake and on PostgreSQL.
//!
//! The profile comes from `SPOT_L7_PROFILE` (default `smoke`). The fake's cost grows with the
//! data it holds, so the fake run is bounded by `SPOT_L7_FAKE_OPERATIONS` (default 20,000);
//! `standard` and `nightly` are meant for PostgreSQL. A JSON report is written to
//! `SPOT_L7_REPORT_DIR` when it is set.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_l7_engine::driver::DriverConfig;
use spot_l7_engine::naming::NamingConfig;
use spot_l7_engine::runner::{run_engine, run_naming};
use spot_l7_engine::world::World;
use spot_l7_invariants::invariant::Report;
use spot_l7_invariants::schedule::Profile;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_fake::FakeBackend;

fn profile() -> Profile {
    Profile::from_env()
}

fn seed() -> u64 {
    std::env::var("SPOT_L7_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20_261_017)
}

fn fake_operations(profile: Profile) -> u64 {
    std::env::var("SPOT_L7_FAKE_OPERATIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| profile.operations().min(20_000))
}

fn write_report(name: &str, report: &Report) {
    if let Ok(dir) = std::env::var("SPOT_L7_REPORT_DIR") {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(
            std::path::Path::new(&dir).join(format!("{name}.json")),
            report.to_json(),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "long profile run: use --ignored or ci/run.sh"]
async fn the_profile_is_green_on_the_fake() {
    let profile = profile();
    let world = Arc::new(World::new());
    let mut config = DriverConfig::for_profile(profile, seed(), "fake");
    config.operations = fake_operations(profile);
    let (_run, report) = run_engine(world, config, FakeBackend::new()).await.unwrap();
    println!("{}", report.to_text());
    write_report(&format!("engine-fake-{}", profile.name()), &report);
    assert!(report.ok, "{}", report.to_text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[ignore = "long profile run: use --ignored or ci/run.sh"]
async fn the_profile_is_green_on_postgres() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    };
    let profile = profile();
    let mut config: PgConfig = db.config();
    config.pool_max_size = (profile.workers() as usize + 8).min(64);
    config.pool_wait_timeout = Duration::from_secs(300);
    config.connect_timeout = Duration::from_secs(60);

    let world = Arc::new(World::new());
    let driver_config = DriverConfig::for_profile(profile, seed(), "pg");
    let engine_backend = PgBackend::connect(config.clone()).await.expect("connect");
    let (_run, report) = run_engine(world.clone(), driver_config, engine_backend)
        .await
        .unwrap();
    println!("{}", report.to_text());
    write_report(&format!("engine-pg-{}", profile.name()), &report);
    assert!(report.ok, "{}", report.to_text());

    let naming_backend = Arc::new(PgBackend::connect(config).await.expect("connect"));
    let (_naming, naming_report) = run_naming(
        world,
        NamingConfig::for_profile(profile, seed()),
        naming_backend,
    )
    .await
    .unwrap();
    println!("{}", naming_report.to_text());
    write_report(&format!("naming-pg-{}", profile.name()), &naming_report);
    assert!(naming_report.ok, "{}", naming_report.to_text());
}
