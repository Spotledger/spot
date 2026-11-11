//! The backend-independent `SentLog` contract suite, run against PostgreSQL.
//!
//! Skips cleanly (one `SKIPPED:` line) when no server is configured; the CI Postgres service sets
//! `SPOT_TEST_PG_URL` and the suite runs, and `SPOT_REQUIRE_PG=1` makes a missing server a
//! failure rather than a skip.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mail_queue::{SentLog, SentLogContractCase, run_sent_log_contract};
use spot_postgres::testing::TestDb;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suite_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 8, "sent_log_contract").await;
    let passed = run_sent_log_contract(&log)
        .await
        .unwrap_or_else(|(case, error)| panic!("contract case {case:?} failed: {error}"));
    assert_eq!(passed, SentLogContractCase::all().to_vec());
    // One receipt for each of the four recording cases — the concurrent case wrote one row for
    // its two racing writers, not two, and `SitesAreIsolated` only records on its own site.
    assert_eq!(
        common::rows(&db).await,
        4,
        "four recordings are four rows, not one per call"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_log_reads_as_unsent_for_every_site() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 2, "sent_log_empty").await;
    for name in ["alpha.test", "beta.test"] {
        let sent = log
            .is_sent(&common::site(name), "nothing@contract.test")
            .await
            .unwrap();
        assert!(!sent, "{name} must not have a receipt");
    }
    assert_eq!(common::rows(&db).await, 0);
}
