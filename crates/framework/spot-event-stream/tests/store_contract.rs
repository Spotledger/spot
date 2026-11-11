//! The in-memory [`ConsumerStore`] passes the backend-independent contract suite.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_event_stream::contract::run_all;
use spot_event_stream::{ConsumerStore, InMemoryConsumerStore};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn in_memory_store_passes_the_contract() {
    let report =
        run_all(|| async { Arc::new(InMemoryConsumerStore::new()) as Arc<dyn ConsumerStore> })
            .await;
    report.assert_all_passed();
}
