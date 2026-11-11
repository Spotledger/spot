//! The in-memory registry passes the backend-independent contract suite (WP-STR-04.T2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_installer::AppRegistryStore;
use spot_installer::testing::contract::run_all;
use spot_installer::testing::{FakeRegistry, Schedule};

#[tokio::test]
async fn in_memory_registry_passes_the_contract_suite() {
    let report = run_all(|| async {
        let store = FakeRegistry::new(Schedule::new());
        Arc::new(store) as Arc<dyn AppRegistryStore>
    })
    .await;
    report.assert_all_passed();
}
