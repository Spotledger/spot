//! The backend-independent contract suite of `spot-dag-exec`, run against PostgreSQL.
//!
//! Skips cleanly (one `SKIPPED:` line) when no server is configured, as the other `*-pg` crates
//! do; the CI Postgres service sets `SPOT_TEST_PG_URL` and the suite runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_dag_exec::contract;
use spot_dag_pg::PgExecStoreFactory;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = common::store(&db, 8, "dag_contract");
    store.prepare().await.unwrap();
    let factory = PgExecStoreFactory::new(
        common::pool(&db, 8, "dag_factory"),
        common::backend(&db),
        common::catalog(),
    );
    if let Err(failure) = contract::run(&factory).await {
        panic!("contract failure: {failure}");
    }
}
