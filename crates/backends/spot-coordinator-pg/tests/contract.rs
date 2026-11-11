//! The coordinator contract, against PostgreSQL.
//!
//! `spot_coordinator::contract::run_all` drives time through a `ManualClock`; this backend reads
//! the database's clock by design, so it cannot run that suite unchanged. These cases are the
//! suite's, run against a real server with time advanced by waiting short ttls ([`TTL`]); the
//! assertions that would pin an instant to a `ManualClock` are stated as the behaviour they
//! stand for. Every test skips (printing `SKIPPED`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use common::{TTL, db, prepared};
use spot_coordinator::error::{
    COUNTER_OVERFLOW, INVALID_ARGUMENT, INVALID_NAME, LEASE_HELD, LEASE_LOST, LOCK_TIMEOUT,
};
use spot_coordinator::{Admit, Coordinator, FencingToken, LockExt, RateLimit};

const TEN_PER_SECOND: RateLimit = RateLimit {
    capacity: 10,
    per: Duration::from_secs(1),
};

/// A limit that refills too slowly for a test's own clock skew to matter: used where the exact
/// `remaining` after several takes is asserted.
const TEN_PER_MINUTE: RateLimit = RateLimit {
    capacity: 10,
    per: Duration::from_secs(60),
};

/// A ttl long enough that a lease cannot expire in the middle of a racing round, however loaded
/// the test machine is.
const RACE_TTL: Duration = Duration::from_secs(30);

async fn spawn_all<T, Fut>(count: usize, make: impl Fn(usize) -> Fut) -> Vec<T>
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handles: Vec<_> = (0..count).map(|i| tokio::spawn(make(i))).collect();
    let mut out = Vec::with_capacity(count);
    for handle in handles {
        out.push(handle.await.expect("a racing task"));
    }
    out
}

/// Waits until `name` has no live lease (a released guard or an expiry).
async fn becomes_free(c: &Arc<dyn Coordinator>, name: &str) {
    for _ in 0..400 {
        if c.current(name).await.expect("current").is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("lease {name} was still held");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tokens_increase_for_the_life_of_the_store() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let first = c.acquire("tok", "a", TTL).await.expect("first acquire");
    assert_eq!(first.token, FencingToken(1));
    c.release(&first).await.expect("release");

    let second = c
        .acquire("tok", "a", TTL)
        .await
        .expect("acquire after release");
    assert_eq!(second.token, FencingToken(2));
    assert_ne!(second.id, first.id, "two acquisitions share an id");

    tokio::time::sleep(TTL + Duration::from_millis(100)).await;
    let third = c
        .acquire("tok", "b", TTL)
        .await
        .expect("acquire after expiry");
    assert_eq!(third.token, FencingToken(3));

    let other = c
        .acquire("tok-other", "a", TTL)
        .await
        .expect("another name");
    assert_eq!(other.token, FencingToken(1), "another name starts at 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_winner_among_racing_holders() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    for round in 1..=8u64 {
        let results = spawn_all(64, |i| {
            let c = Arc::clone(&c);
            async move { c.acquire("race", &format!("holder-{i}"), RACE_TTL).await }
        })
        .await;
        let mut winners = Vec::new();
        for result in results {
            match result {
                Ok(lease) => winners.push(lease),
                Err(error) => assert_eq!(error.code, LEASE_HELD, "a losing racer"),
            }
        }
        assert_eq!(winners.len(), 1, "round {round}: {} winners", winners.len());
        assert_eq!(
            winners[0].token,
            FencingToken(round),
            "round {round}: a loser consumed a token"
        );
        c.release(&winners[0]).await.expect("release the winner");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stale_handles_are_refused() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    // Expired, nobody took it.
    let expired = c.acquire("stale", "a", TTL).await.expect("acquire");
    tokio::time::sleep(TTL + Duration::from_millis(100)).await;
    assert_eq!(c.renew(&expired, TTL).await.unwrap_err().code, LEASE_LOST);
    assert_eq!(c.release(&expired).await.unwrap_err().code, LEASE_LOST);
    assert_eq!(
        c.check_fence("stale", expired.token)
            .await
            .unwrap_err()
            .code,
        LEASE_LOST
    );

    // Released.
    let released = c.acquire("stale", "a", TTL).await.expect("acquire again");
    c.release(&released).await.expect("release");
    assert_eq!(c.renew(&released, TTL).await.unwrap_err().code, LEASE_LOST);
    assert_eq!(c.release(&released).await.unwrap_err().code, LEASE_LOST);
    assert_eq!(
        c.check_fence("stale", released.token)
            .await
            .unwrap_err()
            .code,
        LEASE_LOST
    );

    // Superseded: the current holder is untouched.
    let old = c.acquire("stale", "a", TTL).await.expect("old");
    tokio::time::sleep(TTL + Duration::from_millis(100)).await;
    let new = c.acquire("stale", "a", TTL).await.expect("takeover");
    assert_ne!(new.id, old.id);
    assert_eq!(c.renew(&old, TTL).await.unwrap_err().code, LEASE_LOST);
    assert_eq!(c.release(&old).await.unwrap_err().code, LEASE_LOST);
    assert_eq!(
        c.check_fence("stale", old.token).await.unwrap_err().code,
        LEASE_LOST
    );
    c.check_fence("stale", new.token).await.expect("live token");
    assert_eq!(c.current("stale").await.expect("current"), Some(new));

    // A held lease refuses acquire, for the same holder too.
    assert_eq!(
        c.acquire("stale", "b", TTL).await.unwrap_err().code,
        LEASE_HELD
    );
    assert_eq!(
        c.acquire("stale", "a", TTL).await.unwrap_err().code,
        LEASE_HELD
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn expiry_is_half_open() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let lease = c.acquire("edge", "a", TTL).await.expect("acquire");
    assert!(c.current("edge").await.expect("current").is_some());
    tokio::time::sleep(TTL + Duration::from_millis(100)).await;
    assert!(
        c.current("edge").await.expect("current").is_none(),
        "the lease is still live past its expiry"
    );
    assert_eq!(
        c.check_fence("edge", lease.token).await.unwrap_err().code,
        LEASE_LOST
    );
    let next = c
        .acquire("edge", "b", TTL)
        .await
        .expect("acquire at expiry");
    assert_eq!(next.token, FencingToken(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn renew_extends_and_keeps_the_token() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    // The original ttl is twice the short ttl the other cases use, so a loaded machine cannot
    // expire the lease before the renew below runs; the renewal then extends it far past that.
    let ttl = TTL * 2;
    let lease = c.acquire("renew", "a", ttl).await.expect("acquire");
    tokio::time::sleep(TTL / 2).await;
    let renewed = c.renew(&lease, RACE_TTL).await.expect("renew");
    assert_eq!(renewed.token, lease.token);
    assert_eq!(renewed.id, lease.id);
    assert!(
        renewed.expires_at > lease.expires_at,
        "renew did not extend the lease"
    );
    // Past the original expiry, still live thanks to the renew.
    tokio::time::sleep(ttl).await;
    assert!(c.current("renew").await.expect("current").is_some());
    // The original handle names the same acquisition and still renews.
    c.renew(&lease, RACE_TTL)
        .await
        .expect("renew by the first handle");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_waiter_gets_the_lock_after_the_guard_drops() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let held = c.lock("lk", "a", Duration::ZERO).await.expect("first lock");
    let waiting = Arc::clone(&c);
    let waiter = tokio::spawn(async move {
        waiting
            .lock("lk", "b", Duration::from_secs(30))
            .await
            .map(|guard| guard.lease().map(|lease| lease.token))
    });
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert!(!waiter.is_finished(), "the waiter got a held lock");
    drop(held);
    let outcome = tokio::time::timeout(Duration::from_secs(30), waiter)
        .await
        .expect("the waiter never got the lock")
        .expect("the waiter task");
    assert_eq!(outcome.expect("the waiter's lock"), Some(FencingToken(2)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lock_times_out_and_expires_by_ttl() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let held = c.lock("lk2", "a", Duration::ZERO).await.expect("lock");
    assert_eq!(
        c.lock("lk2", "b", Duration::ZERO).await.unwrap_err().code,
        LOCK_TIMEOUT
    );
    held.unlock().await.expect("unlock");
    let again = c
        .lock("lk2", "b", Duration::ZERO)
        .await
        .expect("lock again");
    again.unlock().await.expect("unlock");

    // A leaked guard never blocks longer than the ttl.
    let leaked = c
        .lock_with_ttl("lk3", "a", Duration::ZERO, Duration::from_millis(300))
        .await
        .expect("lock with a 300ms ttl");
    std::mem::forget(leaked);
    assert_eq!(
        c.lock("lk3", "b", Duration::ZERO).await.unwrap_err().code,
        LOCK_TIMEOUT
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    let after = c
        .lock("lk3", "b", Duration::ZERO)
        .await
        .expect("after the ttl");
    assert_eq!(after.lease().map(|l| l.token), Some(FencingToken(2)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropped_futures_leave_no_lease() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    for micros in [0u64, 1, 20, 200] {
        for _ in 0..5 {
            match tokio::time::timeout(Duration::from_micros(micros), c.acquire("cx", "a", TTL))
                .await
            {
                Ok(done) => {
                    let lease = done.expect("acquire");
                    c.release(&lease).await.expect("release");
                }
                Err(_elapsed) => becomes_free(&c, "cx").await,
            }
        }
    }
    becomes_free(&c, "cx").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn counters_are_atomic_and_consecutive() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let mut values = spawn_all(64, |_| {
        let c = Arc::clone(&c);
        async move { c.next("ctr", 1).await }
    })
    .await
    .into_iter()
    .map(|value| value.expect("next"))
    .collect::<Vec<u64>>();
    values.sort_unstable();
    assert_eq!(values, (1..=64).collect::<Vec<u64>>());

    assert_eq!(c.next("ctr", 5).await.expect("next by 5"), 65);
    assert_eq!(c.next("ctr", 1).await.expect("next"), 70);
    assert_eq!(c.peek("ctr").await.expect("peek"), 70);
    assert_eq!(c.peek("never-used").await.expect("peek"), 0);
    assert_eq!(c.next("ctr-b", 1).await.expect("next"), 1);
    assert_eq!(c.next("ctr", 0).await.unwrap_err().code, INVALID_ARGUMENT);
    assert_eq!(
        c.peek("ctr").await.expect("peek"),
        70,
        "a refused next moved it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn counter_administration_and_overflow() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    assert_eq!(c.set_at_least("adm", 100).await.expect("set"), 100);
    assert_eq!(c.next("adm", 1).await.expect("next"), 101);
    assert_eq!(
        c.set_at_least("adm", 50).await.expect("set"),
        101,
        "lowered it"
    );
    assert_eq!(c.peek("adm").await.expect("peek"), 101);
    c.set_at_least("adm", u64::MAX - 1)
        .await
        .expect("near the top");
    assert_eq!(c.next("adm", 1).await.expect("next"), u64::MAX);
    assert_eq!(c.next("adm", 1).await.unwrap_err().code, COUNTER_OVERFLOW);
    assert_eq!(c.peek("adm").await.expect("peek"), u64::MAX);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_never_exceeds() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let start = std::time::Instant::now();
    let verdicts = spawn_all(100, |_| {
        let c = Arc::clone(&c);
        async move { c.take("rl", TEN_PER_SECOND, 1).await }
    })
    .await;
    let elapsed = start.elapsed();
    let mut admitted = 0u64;
    let mut denied = 0u64;
    for verdict in verdicts {
        match verdict.expect("take") {
            Admit::Allowed { remaining } => {
                assert!(remaining < 10, "remaining {remaining} is over the capacity");
                admitted += 1;
            }
            Admit::Denied { .. } => denied += 1,
        }
    }
    // GCRA: the bucket starts full (10) and refills one unit per 100ms. The racers do not all
    // run at one instant, so the admissions may be the burst plus the units that refilled while
    // they ran; that is what "never exceeded" means against a real clock.
    let refilled = u64::try_from(elapsed.as_millis() / 100).unwrap_or(0) + 1;
    assert!(
        admitted <= 10 + refilled,
        "{admitted} admitted in {elapsed:?}"
    );
    assert!(
        admitted >= 10,
        "the burst admitted {admitted}, under the capacity"
    );
    assert!(
        denied > 0,
        "a burst of 100 over a capacity of 10 denied nobody"
    );

    // After a whole window the bucket has refilled: a second burst is admitted, again bounded by
    // the burst plus what refills while it runs. (A real clock refills continuously, so a fixed
    // "exactly ten, then denied" cannot be pinned without racing the clock; the bound can.)
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let start = std::time::Instant::now();
    let verdicts = spawn_all(100, |_| {
        let c = Arc::clone(&c);
        async move { c.take("rl", TEN_PER_SECOND, 1).await }
    })
    .await;
    let refilled = u64::try_from(start.elapsed().as_millis() / 100).unwrap_or(0) + 1;
    let admitted = verdicts
        .into_iter()
        .filter(|verdict| matches!(verdict.as_ref().expect("take"), Admit::Allowed { .. }))
        .count() as u64;
    assert!(
        admitted <= 10 + refilled,
        "the refilled bucket admitted {admitted}"
    );
    assert!(
        admitted >= 10,
        "the refilled bucket admitted only {admitted}"
    );

    // A denial takes nothing and says how long to wait: drain a fresh key until it refuses, then
    // the wait is positive and at most one emission interval. A slow limit is used so twenty
    // takes cannot refill the bucket while they run, whatever the machine's load.
    let mut drained = 0u64;
    let mut retry_after = None;
    for _ in 0..20 {
        match c.take("rl-wait", TEN_PER_MINUTE, 1).await.expect("take") {
            Admit::Allowed { .. } => drained += 1,
            Admit::Denied { retry_after: wait } => {
                retry_after = Some(wait);
                break;
            }
        }
    }
    assert!(drained >= 10, "a full bucket admitted only {drained}");
    let retry_after = retry_after.expect("a full bucket denied nothing");
    assert!(
        retry_after > Duration::ZERO && retry_after <= TEN_PER_MINUTE.per,
        "the wait for the next unit is {retry_after:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rate_limit_costs_and_independent_keys() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    assert_eq!(
        c.take("cost", TEN_PER_MINUTE, 4).await.expect("take"),
        Admit::Allowed { remaining: 6 }
    );
    assert_eq!(
        c.take("cost", TEN_PER_MINUTE, 4).await.expect("take"),
        Admit::Allowed { remaining: 2 }
    );
    assert!(matches!(
        c.take("cost", TEN_PER_MINUTE, 4).await.expect("take"),
        Admit::Denied { .. }
    ));
    assert_eq!(
        c.take("cost", TEN_PER_MINUTE, 2).await.expect("take"),
        Admit::Allowed { remaining: 0 },
        "the denial took units"
    );
    assert_eq!(
        c.take("cost-other", TEN_PER_MINUTE, 10)
            .await
            .expect("take"),
        Admit::Allowed { remaining: 0 }
    );
    assert_eq!(
        c.take("cost", TEN_PER_MINUTE, 0).await.unwrap_err().code,
        INVALID_ARGUMENT
    );
    assert_eq!(
        c.take("cost", TEN_PER_MINUTE, 11).await.unwrap_err().code,
        INVALID_ARGUMENT
    );
    assert_eq!(
        c.take(
            "cost",
            RateLimit {
                capacity: 0,
                per: Duration::from_secs(1)
            },
            1
        )
        .await
        .unwrap_err()
        .code,
        INVALID_ARGUMENT
    );
    assert_eq!(
        c.take(
            "cost",
            RateLimit {
                capacity: 1,
                per: Duration::ZERO
            },
            1
        )
        .await
        .unwrap_err()
        .code,
        INVALID_ARGUMENT
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn names_and_arguments_are_validated() {
    let Some(db) = db().await else { return };
    let c: Arc<dyn Coordinator> = Arc::new(prepared(&db).await);

    let too_long = "x".repeat(spot_coordinator::MAX_NAME_BYTES + 1);
    for bad in ["", too_long.as_str(), "line\nbreak", "nul\0byte"] {
        assert_eq!(
            c.acquire(bad, "a", TTL).await.unwrap_err().code,
            INVALID_NAME
        );
        assert_eq!(
            c.acquire("ok-name", bad, TTL).await.unwrap_err().code,
            INVALID_NAME
        );
        assert_eq!(c.current(bad).await.unwrap_err().code, INVALID_NAME);
        assert_eq!(
            c.check_fence(bad, FencingToken(1)).await.unwrap_err().code,
            INVALID_NAME
        );
        assert_eq!(c.next(bad, 1).await.unwrap_err().code, INVALID_NAME);
        assert_eq!(c.peek(bad).await.unwrap_err().code, INVALID_NAME);
        assert_eq!(c.set_at_least(bad, 1).await.unwrap_err().code, INVALID_NAME);
        assert_eq!(
            c.take(bad, TEN_PER_SECOND, 1).await.unwrap_err().code,
            INVALID_NAME
        );
    }
    let longest = "y".repeat(spot_coordinator::MAX_NAME_BYTES);
    let lease = c.acquire(&longest, "a", TTL).await.expect("200-byte name");
    c.release(&lease).await.expect("release");
    c.acquire("unicode-é-名前", "ä", TTL)
        .await
        .expect("unicode name");
    assert_eq!(
        c.acquire("ok-name", "a", Duration::ZERO)
            .await
            .unwrap_err()
            .code,
        INVALID_ARGUMENT
    );
    assert_eq!(
        c.acquire("ok-name", "a", Duration::from_secs(400 * 24 * 3600))
            .await
            .unwrap_err()
            .code,
        INVALID_ARGUMENT
    );
    assert!(c.current("ok-name").await.expect("current").is_none());
}
