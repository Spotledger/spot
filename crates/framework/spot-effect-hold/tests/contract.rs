//! Runs the backend-independent ledger contract suite against the in-memory ledger.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_effect_hold::{InMemoryLedger, LedgerStore, contract::run_all};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn in_memory_ledger_passes_the_contract() {
    let report = run_all(|| Arc::new(InMemoryLedger::new()) as Arc<dyn LedgerStore>).await;
    report.assert_all_passed();
}
