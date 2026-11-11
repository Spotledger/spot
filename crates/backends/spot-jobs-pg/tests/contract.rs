//! The T1 contract suite, run on PostgreSQL. Every check gets its own scratch database.
//!
//! The suite is T1's `spot-jobs/src/contract.rs`, compiled into this test with `#[path]` rather
//! than through `spot-jobs`'s `testing` feature: with the feature on, `cargo clippy` lints that
//! file under the workspace's `unwrap_used = deny` (it unwraps freely, being test code), which
//! fails this crate's clippy acceptance. Here it is test code of this crate, allowed to unwrap. The file is the same source, so a change to T1's
//! suite is picked up here unchanged. Once `contract.rs` carries its own lint allowance this can
//! become `use spot_jobs::contract::run_all` with `features = ["testing"]` on the dev-dependency.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

// The paths `contract.rs` names relative to its own crate root.
use spot_jobs::Result;
mod error {
    pub use spot_jobs::error::*;
}
mod model {
    pub use spot_jobs::model::*;
}
mod store {
    pub use spot_jobs::store::*;
}
#[path = "../../../framework/spot-jobs/src/contract.rs"]
#[rustfmt::skip]
#[allow(missing_docs, clippy::all)]
mod contract;

use std::sync::{Arc, Mutex};

use contract::run_all;
use spot_jobs::store::JobStore;
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;

/// `run_all` asks for a fresh store per check; a check takes the next prepared one.
const CHECKS: usize = 21;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let mut dbs = Vec::new();
    let mut stores: Vec<Arc<dyn JobStore>> = Vec::new();
    for _ in 0..CHECKS {
        let Some(db) = TestDb::create().await else {
            return;
        };
        let store = PgJobStore::new(common::pool(&db, 8, "contract"));
        store.prepare().await.unwrap();
        stores.push(Arc::new(store));
        dbs.push(db);
    }
    let stores = Mutex::new(stores);
    let report = run_all(|| stores.lock().unwrap().pop().expect("a prepared store")).await;
    let report = report.unwrap();
    println!("contract report: {report:?}");
    assert!(
        report.all_passed(),
        "contract failures: {:?}",
        report.failures
    );
    assert!(report.total >= 11);
}

#[tokio::test]
async fn prepare_is_idempotent_and_safe_to_race() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = PgJobStore::new(common::pool(&db, 8, "prepare"));
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let s = store.clone();
        tasks.push(tokio::spawn(async move { s.prepare().await }));
    }
    for t in tasks {
        t.await.unwrap().unwrap();
    }
    store.prepare().await.unwrap();
}
