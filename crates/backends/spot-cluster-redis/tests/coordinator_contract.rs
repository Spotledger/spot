//! The coordinator contract suite, against a real Redis/Valkey server.
//!
//! [`spot_coordinator::contract::run_all`] drives time through a `ManualClock`, and this backend
//! reads time only from the clock it is built on, so the suite runs **unchanged**: every case
//! gets a fresh connection under a fresh key prefix and a fresh `ManualClock` at the Unix epoch.
//! Skips without `SPOT_TEST_REDIS_URL`.

#![cfg(feature = "redis")]
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use spot_cluster_redis::{KeySpace, RedisConfig, RedisConnection, RedisCoordinator};
use spot_coordinator::Coordinator;
use spot_coordinator::contract::{Harness, run_all};
use spot_core::ManualClock;
use time::OffsetDateTime;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redis_passes_the_coordinator_contract() {
    let Some(url) = common::url() else {
        return common::skip();
    };
    let report = run_all(|| {
        let url = url.clone();
        async move {
            let config =
                RedisConfig::new(url).with_keyspace(KeySpace::with_prefix(common::unique_prefix()));
            let connection = RedisConnection::connect(&config)
                .await
                .expect("connect to Redis");
            let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
            let coordinator: Arc<dyn Coordinator> =
                Arc::new(RedisCoordinator::new(connection, clock.clone()));
            Harness { coordinator, clock }
        }
    })
    .await;
    report.assert_all_passed();
}
