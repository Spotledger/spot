//! The backend-independent `ConsumerStore` contract suite, run against PostgreSQL.
//!
//! Skips cleanly (one `SKIPPED:` line) when no server is configured; the CI Postgres service
//! sets `SPOT_TEST_PG_URL` and the suite runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_event_stream::ConsumerStore;
use spot_event_stream::contract::run_all;
use spot_event_stream_pg::PgConsumerStore;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = common::pool(&db, 8, "event_stream_contract");
    PgConsumerStore::new(pool.clone(), common::site("setup"))
        .prepare()
        .await
        .unwrap();
    // Each case gets its own site, which is also what proves site scoping starts clean.
    let report = run_all(|| {
        let pool = pool.clone();
        async move {
            let store = PgConsumerStore::new(pool, common::site(&common::fresh_site()));
            Arc::new(store) as Arc<dyn ConsumerStore>
        }
    })
    .await;
    report.assert_all_passed();
}
