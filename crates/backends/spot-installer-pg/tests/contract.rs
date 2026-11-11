//! The installer's backend-independent registry contract suite, run against PostgreSQL with a
//! fresh scratch database per case.
//!
//! Skips cleanly when no server is configured (`SPOT_TEST_PG_URL` unset); the CI Postgres service
//! sets it and the suite runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use spot_installer::AppRegistryStore;
use spot_installer::testing::contract::run_all;
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    // The probe database also tells whether a server is configured at all.
    let Some(probe) = TestDb::create().await else {
        return;
    };
    // Every case gets its own database; they are kept alive until the report is in.
    let dbs = Arc::new(Mutex::new(vec![probe]));
    let report = run_all(|| {
        let dbs = Arc::clone(&dbs);
        async move {
            let db = TestDb::create().await.expect("a scratch database");
            let registry = common::registry(&db, 4, "installer_contract").await;
            dbs.lock().unwrap().push(db);
            Arc::new(registry) as Arc<dyn AppRegistryStore>
        }
    })
    .await;
    report.assert_all_passed();
}
