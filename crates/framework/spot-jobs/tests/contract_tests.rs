//! Contract tests for the job store implementation.

#![cfg(feature = "testing")]
#![allow(clippy::expect_used)]

use spot_jobs::contract::run_all;
use spot_jobs::InMemoryJobStore;
use std::sync::Arc;

#[tokio::test]
async fn test_contract_all() {
    let report = run_all(|| Arc::new(InMemoryJobStore::new()))
        .await
        .expect("contract tests failed");

    println!("Contract report: {:?}", report);
    assert!(
        report.all_passed(),
        "Contract tests failed: {:?}",
        report.failures
    );
}
