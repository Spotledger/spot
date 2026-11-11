//! Per-request cost accounting.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::time::Duration;

use spot_telemetry::cost;

async fn work(statements: u64) {
    for _ in 0..statements {
        tokio::task::yield_now().await;
        cost::record_db(Duration::from_millis(2), 3, 1);
        tokio::task::yield_now().await;
        cost::record_cache(true);
        cost::record_cache(false);
    }
    cost::record_outbox_events(2);
    cost::record_external_call();
}

#[tokio::test]
async fn accumulates_across_awaits() {
    let ((), c) = cost::scope(work(3)).await;
    assert_eq!(c.db_statements, 3);
    assert_eq!(c.db_time, Duration::from_millis(6));
    assert_eq!(c.rows_read, 9);
    assert_eq!(c.rows_written, 3);
    assert_eq!(c.cache_hits, 3);
    assert_eq!(c.cache_misses, 3);
    assert_eq!(c.outbox_events, 2);
    assert_eq!(c.external_calls, 1);
}

#[tokio::test]
async fn wall_time_covers_the_scope() {
    let ((), c) = cost::scope(tokio::time::sleep(Duration::from_millis(20))).await;
    assert!(
        c.wall_time >= Duration::from_millis(20),
        "{:?}",
        c.wall_time
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_keep_separate_costs() {
    let a = tokio::spawn(cost::scope(work(5)));
    let b = tokio::spawn(cost::scope(work(2)));
    let ((), ca) = a.await.unwrap();
    let ((), cb) = b.await.unwrap();
    assert_eq!(ca.db_statements, 5);
    assert_eq!(cb.db_statements, 2);
    assert_eq!(ca.rows_read, 15);
    assert_eq!(cb.rows_read, 6);
}

#[tokio::test]
async fn recording_outside_a_scope_is_a_no_op() {
    cost::record_db(Duration::from_secs(1), 1, 1);
    cost::record_cache(true);
    cost::record_outbox_events(1);
    cost::record_external_call();
    assert_eq!(cost::current(), None);
    // And it does not leak into a scope started afterwards.
    let ((), c) = cost::scope(async {}).await;
    assert_eq!(c.db_statements, 0);
    assert_eq!(c.cache_hits, 0);
}

#[tokio::test]
async fn nested_scope_is_separate() {
    let (inner, outer) = cost::scope(async {
        cost::record_db(Duration::ZERO, 1, 0);
        let ((), inner) = cost::scope(async { cost::record_db(Duration::ZERO, 10, 0) }).await;
        inner
    })
    .await;
    assert_eq!(inner.rows_read, 10);
    assert_eq!(outer.rows_read, 1);
}
