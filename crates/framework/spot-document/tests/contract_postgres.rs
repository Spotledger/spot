//! The app API contract suite the engine supports, over PostgreSQL.
//!
//! The same 24 cases as `contract.rs` (see [`host::CASES`]), on `Engine<PgBackend>`: one case
//! per scratch database. Skips (printing why) when no PostgreSQL server is configured.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod host;
mod pg;

use spot_app_api::contract_tests::run_matching;

use host::CASES;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_passes_on_postgres() {
    if !pg::available() {
        return;
    }
    let report = run_matching(&pg::Postgres::new(), |case| CASES.contains(&case)).await;
    assert_eq!(report.outcomes.len(), CASES.len(), "a case went missing");
    report.assert_all_passed();
}
