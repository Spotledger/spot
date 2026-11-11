//! The backend-independent contract suites, run against PostgreSQL.
//!
//! Skips cleanly (a `SKIPPED:` line from `TestDb::create`) when `SPOT_TEST_PG_URL` is unset;
//! with `SPOT_REQUIRE_PG=1` a missing server fails, as it does in CI.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_campaign_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (campaigns, _) = common::stores(&db, 8, "ar_contract").await;
    spot_access_review::contract::run_all(|| campaigns.clone()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_sod_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (_, sod) = common::stores(&db, 8, "sod_contract").await;
    spot_sod::contract::run_all(|| sod.clone()).await;
}
