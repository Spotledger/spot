//! The L3 document behaviour suite (WP-TST-10.T1), on both backends.
//!
//! The same 30 cases run over the engine on the in-memory backend and on PostgreSQL, so the
//! whole `DocumentService` → lifecycle → storage → outbox path is pinned end to end. The
//! PostgreSQL run skips (printing why) when no server is configured.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod host;
mod pg;

use spot_app_api::contract_tests::{Fixture, TestHost};
use spot_l3_documents::run_all as run_l3;

struct Fake;

impl Fixture for Fake {
    fn fresh(&self) -> Box<dyn TestHost> {
        Box::new(host::Host::new())
    }
}

#[tokio::test]
async fn the_l3_suite_passes_on_the_fake() {
    let report = run_l3(&Fake).await;
    assert!(report.outcomes.len() >= 30, "the suite shrank");
    report.assert_all_passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_l3_suite_passes_on_postgres() {
    if !pg::available() {
        return;
    }
    let report = run_l3(&pg::Postgres::new()).await;
    assert!(report.outcomes.len() >= 30, "the suite shrank");
    report.assert_all_passed();
}
