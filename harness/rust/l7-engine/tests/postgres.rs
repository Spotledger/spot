//! The engine and naming workloads on PostgreSQL, with seeded store corruptions (WP-TST-07.T2).
//!
//! Without `SPOT_TEST_PG_URL` the tests print `SKIPPED`; with `SPOT_REQUIRE_PG=1` a missing
//! server is a failure. The CI script (`ci/run.sh`) sets both, so a release run cannot pass by
//! skipping.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use spot_l7_engine::driver::DriverConfig;
use spot_l7_engine::naming::NamingConfig;
use spot_l7_engine::runner::{check_engine_with, run_engine, run_naming};
use spot_l7_engine::world::World;
use spot_l7_invariants::invariant::Report;
use spot_l7_invariants::schedule::Profile;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};

async fn pg_env() -> Option<TestDb> {
    TestDb::create().await
}

async fn connect(db: &TestDb) -> PgBackend {
    let mut config: PgConfig = db.config();
    config.pool_max_size = 12;
    config.pool_wait_timeout = Duration::from_secs(300);
    config.connect_timeout = Duration::from_secs(60);
    match PgBackend::connect(config).await {
        Ok(backend) => backend,
        Err(e) => panic!("cannot connect to the scratch database: {e}"),
    }
}

fn engine_config(seed: u64) -> DriverConfig {
    let mut config = DriverConfig::for_profile(Profile::Smoke, seed, "pg");
    config.workers = 4;
    config.operations = 400;
    config
}

fn maybe_write_report(name: &str, report: &Report) {
    if let Ok(dir) = std::env::var("SPOT_L7_REPORT_DIR") {
        let path = std::path::Path::new(&dir).join(format!("{name}.json"));
        let _ = std::fs::write(path, report.to_json());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_and_naming_are_green_on_postgres() {
    let Some(db) = pg_env().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    };
    let world = Arc::new(World::new());
    let (_run, report) = run_engine(world.clone(), engine_config(20_261_012), connect(&db).await)
        .await
        .unwrap();
    println!("{}", report.to_text());
    maybe_write_report("engine-pg", &report);
    assert!(report.ok, "{}", report.to_text());

    let (_naming, naming_report) = run_naming(
        world,
        NamingConfig {
            seed: 20_261_012,
            workers: 4,
            operations: 400,
            rollback_one_in: 3,
            profile: "smoke".to_owned(),
        },
        Arc::new(connect(&db).await),
    )
    .await
    .unwrap();
    println!("{}", naming_report.to_text());
    maybe_write_report("naming-pg", &naming_report);
    assert!(naming_report.ok, "{}", naming_report.to_text());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_state_corruption_is_caught_on_postgres() {
    let Some(first) = pg_env().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    };
    let mut first = Some(first);
    // The workload commits only a few dozen of its 400 operations and how many of them create a
    // `time_entry` depends on how the four workers interleave, so a run can end with none (the
    // UPDATE below would then change no row and the check would rightly stay green). Run until
    // the store holds at least one, on a fresh database each time; the corruption itself is
    // still asserted to change exactly one row.
    let mut attempt = 0_u64;
    let (world, _db, run, client) = loop {
        let db = match first.take() {
            Some(db) => db,
            None => pg_env().await.unwrap(),
        };
        let world = Arc::new(World::new());
        let (run, before) = run_engine(
            world.clone(),
            engine_config(7_000_007 + attempt),
            connect(&db).await,
        )
        .await
        .unwrap();
        assert!(before.ok, "{}", before.to_text());
        let client = db.raw_client().await;
        let count: i64 = client
            .query_one("SELECT count(*) FROM time_entry", &[])
            .await
            .unwrap()
            .get(0);
        if count > 0 {
            break (world, db, run, client);
        }
        attempt += 1;
        assert!(attempt < 20, "no run of 20 created a time_entry");
    };
    let changed = client
        .execute(
            "UPDATE time_entry SET doc_state = \
             CASE WHEN doc_state = 'draft' THEN 'posted' ELSE 'draft' END \
             WHERE id = (SELECT id FROM time_entry ORDER BY id LIMIT 1)",
            &[],
        )
        .await
        .unwrap_or_else(|e| panic!("the corruption did not apply: {e}"));
    assert_eq!(changed, 1, "the corruption must change exactly one row");
    let after = check_engine_with(&world, &run, run.oracle.clone()).await;
    assert_eq!(
        after.failed_ids(),
        vec!["lifecycle-state_machine"],
        "{}",
        after.to_text()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deleted_outbox_row_is_caught_on_postgres() {
    let Some(db) = pg_env().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    };
    let world = Arc::new(World::new());
    let (run, before) = run_engine(world.clone(), engine_config(7_000_007), connect(&db).await)
        .await
        .unwrap();
    assert!(before.ok, "{}", before.to_text());

    let client = db.raw_client().await;
    client
        .batch_execute(
            "DO $$ DECLARE victim uuid; BEGIN
                PERFORM set_config('spot.feed_maintenance', 'on', true);
                SELECT id INTO victim FROM spot_outbox WHERE topic <> 'doc_state' ORDER BY id OFFSET 3 LIMIT 1;
                IF victim IS NULL THEN RAISE EXCEPTION 'no victim'; END IF;
                DELETE FROM spot_outbox_pub WHERE event_id = victim;
                DELETE FROM spot_outbox WHERE id = victim;
             END $$",
        )
        .await
        .unwrap_or_else(|e| panic!("the corruption did not apply: {e}"));
    let after = check_engine_with(&world, &run, run.oracle.clone()).await;
    assert_eq!(
        after.failed_ids(),
        vec!["events-per_operation"],
        "{}",
        after.to_text()
    );
}
