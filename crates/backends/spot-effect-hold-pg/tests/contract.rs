//! The backend-independent ledger contract suite of `spot-effect-hold`, run against PostgreSQL.
//!
//! Skips cleanly (one `SKIPPED:` line) when no server is configured, as the other `*-pg` crates
//! do; the CI Postgres service sets `SPOT_TEST_PG_URL` and the suite runs.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::*;
use spot_effect_hold::{LedgerStore, contract};
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store: Arc<dyn LedgerStore> = Arc::new(ledger(&db, 8, "hold_contract").await);
    // Every case names its own site, so one prepared ledger serves all of them.
    let report = contract::run_all(move || Arc::clone(&store)).await;
    report.assert_all_passed();
}
