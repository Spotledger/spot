//! The backend-independent `MeterStore` contract suite, run against PostgreSQL.
//!
//! Skips cleanly (one `SKIPPED:` line) when no server is configured; the CI Postgres service
//! sets `SPOT_TEST_PG_URL` and the suite runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_metering::StoreContractCase;
use spot_metering::contract::run_store_contract;
use spot_metering_pg::PgMeterStore;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    // The tables exist before any case runs; every case then builds its own store, bound to its
    // own tenant, on the shared pool.
    PgMeterStore::new(common::pool(&db, 8, "metering_contract"), "contract")
        .prepare()
        .await
        .unwrap();
    let factory = common::factory(&db, 8);
    let passed = run_store_contract(&factory)
        .await
        .unwrap_or_else(|(case, error)| panic!("contract case {case:?} failed: {error}"));
    assert_eq!(passed, StoreContractCase::all());
}
