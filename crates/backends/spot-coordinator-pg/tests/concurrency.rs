//! The load line behind "counters and rate limits are single atomic statements": 200 sessions
//! racing one coordinator. `take` never over-admits, and 200 racing `next` calls return exactly
//! the next 200 values, so a cluster's shared counter has no gap and no duplicate.
//!
//! Skips (printing `SKIPPED`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use common::{coordinator, db};
use spot_coordinator::{Admit, Coordinator, RateLimit};
use spot_coordinator_pg::PgCoordinatorConfig;

/// 200 concurrent sessions on one coordinator ask the same rate limit for one unit each. A
/// capacity-100, one-second bucket admits its burst plus whatever refilled while the racers ran,
/// and never more: 200 sessions can never be admitted inside one window.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn take_never_over_admits_under_200_concurrent_sessions() {
    let Some(db) = db().await else { return };
    let c = Arc::new(coordinator(&db, PgCoordinatorConfig::default(), 16).await);
    let limit = RateLimit {
        capacity: 100,
        per: Duration::from_secs(1),
    };

    let start = Instant::now();
    let mut sessions = Vec::with_capacity(200);
    for _ in 0..200 {
        let c = Arc::clone(&c);
        sessions.push(tokio::spawn(async move { c.take("burst", limit, 1).await }));
    }
    let mut admitted = 0u64;
    for session in sessions {
        if let Admit::Allowed { .. } = session.await.expect("a session").expect("take") {
            admitted += 1;
        }
    }
    let elapsed = start.elapsed();
    // GCRA: 100 units burst, refilling one per 10ms. The bound is the burst plus the units that
    // refilled while the 200 racers ran; 200 sessions can never all be admitted in one window.
    let refilled = u64::try_from(elapsed.as_millis() / 10).unwrap_or(0) + 1;
    assert!(
        admitted <= 100 + refilled,
        "200 concurrent takers were admitted {admitted} times in {elapsed:?} over a 100/s limit"
    );
    assert!(
        admitted >= 100,
        "the limit admitted {admitted}, under its capacity"
    );
}

/// 200 concurrent sessions reserve one value each from one counter: the 200 values are exactly
/// `1..=200`, with no duplicate and no gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_hundred_concurrent_counters_take_exact_values() {
    let Some(db) = db().await else { return };
    let c = Arc::new(coordinator(&db, PgCoordinatorConfig::default(), 16).await);

    let mut sessions = Vec::with_capacity(200);
    for _ in 0..200 {
        let c = Arc::clone(&c);
        sessions.push(tokio::spawn(async move { c.next("seq", 1).await }));
    }
    let mut values = Vec::with_capacity(200);
    for session in sessions {
        values.push(session.await.expect("a session").expect("next"));
    }
    values.sort_unstable();
    assert_eq!(values, (1..=200u64).collect::<Vec<u64>>());
}
