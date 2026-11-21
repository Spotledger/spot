//! The backend-independent contract suite (feature `testing`).
//!
//! Every [`Coordinator`] backend must pass [`run_all`]: the in-process one here
//! (`tests/in_process.rs`), the database one (WP-PLT-18.T2) and the Redis plugin (WP-PLT-19).
//! The suite reads time only through the [`ManualClock`] the backend was built on and never
//! sleeps for real; the one real waits are bounded polls for a dropped guard's background
//! release and the lock waiter's own wake-ups.
//!
//! ```ignore
//! let report = run_all(|| async {
//!     let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
//!     let coordinator = MyBackend::start(clock.clone()).await;
//!     Harness { coordinator, clock }
//! })
//! .await;
//! report.assert_all_passed();
//! ```
//!
//! Run it on a multi-thread tokio runtime, so the racing cases really race.

use std::fmt::Debug;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use spot_core::{Clock, ManualClock};

use crate::coordinator::{Coordinator, LockExt};
use crate::error::{
    COUNTER_OVERFLOW, INVALID_ARGUMENT, INVALID_NAME, LEASE_HELD, LEASE_LOST, LOCK_TIMEOUT,
};
use crate::types::{Admit, FencingToken, Lease, MAX_NAME_BYTES, RateLimit};

/// A fresh coordinator and the clock it runs on.
pub struct Harness {
    /// The backend under test.
    pub coordinator: Arc<dyn Coordinator>,
    /// The clock the backend reads; the suite moves it to expire leases and refill buckets.
    pub clock: Arc<ManualClock>,
}

/// How one case ended: `Err` carries what was wrong.
pub type CheckResult = Result<(), String>;

/// One case's name and outcome.
#[derive(Debug)]
pub struct CaseResult {
    /// The case's name.
    pub name: &'static str,
    /// `Ok` when the backend behaved.
    pub outcome: CheckResult,
}

/// The outcome of [`run_all`].
#[derive(Debug, Default)]
pub struct Report {
    /// One entry per case, in run order.
    pub cases: Vec<CaseResult>,
}

impl Report {
    /// The cases that failed.
    pub fn failures(&self) -> Vec<&CaseResult> {
        self.cases.iter().filter(|c| c.outcome.is_err()).collect()
    }

    /// Panics, listing every failed case, unless all passed (and at least one ran).
    pub fn assert_all_passed(&self) {
        assert!(!self.cases.is_empty(), "the contract suite ran no cases");
        let failures = self.failures();
        if failures.is_empty() {
            return;
        }
        let mut text = format!(
            "{} of {} contract cases failed:",
            failures.len(),
            self.cases.len()
        );
        for case in failures {
            if let Err(why) = &case.outcome {
                text.push_str(&format!("\n  - {}: {why}", case.name));
            }
        }
        panic!("{text}");
    }
}

/// Runs every case, each on a fresh coordinator from `make`.
pub async fn run_all<F, Fut>(make: F) -> Report
where
    F: Fn() -> Fut,
    Fut: Future<Output = Harness>,
{
    let mut report = Report::default();
    macro_rules! case {
        ($case:path) => {{
            let outcome = $case(make().await).await;
            report.cases.push(CaseResult {
                name: stringify!($case),
                outcome,
            });
        }};
    }
    case!(tokens_increase_for_the_life_of_the_store);
    case!(one_winner_among_racing_holders);
    case!(stale_handles_are_refused);
    case!(expiry_is_half_open);
    case!(renew_extends_and_keeps_the_token);
    case!(lock_waiter_gets_the_lock_after_the_guard_drops);
    case!(lock_times_out_and_expires_by_ttl);
    case!(dropped_futures_leave_no_lease);
    case!(counters_are_atomic_and_consecutive);
    case!(counter_administration_and_overflow);
    case!(rate_limit_never_exceeds_and_refills);
    case!(rate_limit_costs_and_independent_keys);
    case!(names_and_arguments_are_validated);
    report
}

// ---- helpers ----------------------------------------------------------------------------

macro_rules! ensure {
    ($cond:expr, $($why:tt)+) => {
        if !$cond {
            return Err(format!($($why)+));
        }
    };
}

/// The result is `Ok`; a failure becomes the case's error.
fn ok<T, E: Debug>(result: Result<T, E>, what: &str) -> Result<T, String> {
    result.map_err(|error| format!("{what} failed: {error:?}"))
}

/// The result is an error with `code`.
fn refused<T: Debug>(
    result: crate::error::Result<T>,
    code: &str,
    what: &str,
) -> Result<(), String> {
    match result {
        Err(error) if error.code == code => Ok(()),
        Err(error) => Err(format!("{what}: expected {code}, got {error}")),
        Ok(value) => Err(format!(
            "{what}: expected {code}, but it succeeded: {value:?}"
        )),
    }
}

async fn spawn_all<T, Fut>(count: usize, make: impl Fn(usize) -> Fut) -> Result<Vec<T>, String>
where
    Fut: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handles: Vec<_> = (0..count).map(|i| tokio::spawn(make(i))).collect();
    let mut out = Vec::with_capacity(count);
    for handle in handles {
        out.push(ok(handle.await, "a racing task")?);
    }
    Ok(out)
}

/// Polls `future` exactly once and drops it: the cancellation at its first `.await`.
fn poll_once_then_drop<F: Future>(future: F) -> Option<F::Output> {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    }
}

/// Waits (bounded) until `name` has no live lease: a dropped guard releases in the background.
async fn becomes_free(c: &Arc<dyn Coordinator>, name: &str) -> CheckResult {
    for round in 0..2_000u32 {
        if ok(c.current(name).await, "current")?.is_none() {
            return Ok(());
        }
        if round < 100 {
            tokio::task::yield_now().await;
        } else {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }
    Err(format!(
        "lease {name} was still held long after its guard dropped"
    ))
}

const TTL: Duration = Duration::from_secs(10);

// ---- leases -----------------------------------------------------------------------------

async fn tokens_increase_for_the_life_of_the_store(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let first = ok(c.acquire("tok", "a", TTL).await, "first acquire")?;
    ensure!(
        first.token == FencingToken(1),
        "first token is {:?}",
        first.token
    );
    ok(c.release(&first).await, "release")?;
    let second = ok(c.acquire("tok", "a", TTL).await, "acquire after release")?;
    ensure!(
        second.token == FencingToken(2),
        "token after release is {:?}",
        second.token
    );
    ensure!(
        second.id != first.id,
        "two acquisitions share record id {}",
        second.id
    );
    h.clock.advance(TTL);
    let third = ok(c.acquire("tok", "b", TTL).await, "acquire after expiry")?;
    ensure!(
        third.token == FencingToken(3),
        "token after expiry is {:?}",
        third.token
    );
    let other = ok(
        c.acquire("tok-other", "a", TTL).await,
        "acquire of another name",
    )?;
    ensure!(
        other.token == FencingToken(1),
        "another name starts at 1, got {:?}",
        other.token
    );
    ensure!(
        third.expires_at == h.clock.now_utc() + TTL,
        "expires_at is not now + ttl"
    );
    Ok(())
}

async fn one_winner_among_racing_holders(h: Harness) -> CheckResult {
    for round in 1..=50u64 {
        let results = spawn_all(64, |i| {
            let c = Arc::clone(&h.coordinator);
            async move { c.acquire("race", &format!("holder-{i}"), TTL).await }
        })
        .await?;
        let mut winners = Vec::new();
        for result in results {
            match result {
                Ok(lease) => winners.push(lease),
                Err(error) => refused::<()>(Err(error), LEASE_HELD, "a losing racer")?,
            }
        }
        ensure!(
            winners.len() == 1,
            "round {round}: {} holders won the lease",
            winners.len()
        );
        let winner = &winners[0];
        ensure!(
            winner.token == FencingToken(round),
            "round {round}: the winner's token is {:?}",
            winner.token
        );
        ok(h.coordinator.release(winner).await, "release the winner")?;
    }
    Ok(())
}

async fn stale_handles_are_refused(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    // Expired, and nobody else took it.
    let expired = ok(c.acquire("stale", "a", TTL).await, "acquire")?;
    h.clock.advance(TTL);
    refused(
        c.renew(&expired, TTL).await,
        LEASE_LOST,
        "renew of an expired handle",
    )?;
    refused(
        c.release(&expired).await,
        LEASE_LOST,
        "release of an expired handle",
    )?;
    refused(
        c.check_fence("stale", expired.token).await,
        LEASE_LOST,
        "fence check of an expired token",
    )?;
    // Released.
    let released = ok(c.acquire("stale", "a", TTL).await, "acquire again")?;
    ok(c.release(&released).await, "release")?;
    refused(
        c.renew(&released, TTL).await,
        LEASE_LOST,
        "renew of a released handle",
    )?;
    refused(c.release(&released).await, LEASE_LOST, "second release")?;
    refused(
        c.check_fence("stale", released.token).await,
        LEASE_LOST,
        "fence check of a released token",
    )?;
    // Superseded by the same holder and by another: the current holder stays untouched.
    let old = ok(c.acquire("stale", "a", TTL).await, "acquire the old lease")?;
    h.clock.advance(TTL);
    let new = ok(
        c.acquire("stale", "a", TTL).await,
        "same holder acquires again",
    )?;
    ensure!(
        new.id != old.id,
        "two acquisitions by one holder share an id"
    );
    refused(
        c.renew(&old, TTL).await,
        LEASE_LOST,
        "renew of a superseded handle",
    )?;
    refused(
        c.release(&old).await,
        LEASE_LOST,
        "release of a superseded handle",
    )?;
    refused(
        c.check_fence("stale", old.token).await,
        LEASE_LOST,
        "fence check of a superseded token",
    )?;
    ok(
        c.check_fence("stale", new.token).await,
        "fence check of the live token",
    )?;
    let current = ok(c.current("stale").await, "current")?;
    ensure!(
        current.as_ref() == Some(&new),
        "the stale calls changed the live lease: {current:?}"
    );
    // A held lease refuses acquire, for the same holder too.
    refused(
        c.acquire("stale", "b", TTL).await,
        LEASE_HELD,
        "acquire of a held lease",
    )?;
    refused(
        c.acquire("stale", "a", TTL).await,
        LEASE_HELD,
        "second acquire by the holder",
    )?;
    Ok(())
}

async fn expiry_is_half_open(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let lease = ok(c.acquire("edge", "a", TTL).await, "acquire")?;
    h.clock.advance(TTL - Duration::from_nanos(1));
    ok(
        c.check_fence("edge", lease.token).await,
        "fence check a nanosecond before expiry",
    )?;
    ensure!(
        ok(c.current("edge").await, "current")?.is_some(),
        "the lease is over a nanosecond before expires_at"
    );
    refused(
        c.acquire("edge", "b", TTL).await,
        LEASE_HELD,
        "acquire a nanosecond before expiry",
    )?;
    h.clock.advance(Duration::from_nanos(1));
    ensure!(
        h.clock.now_utc() == lease.expires_at,
        "the test is not at expires_at: {} vs {}",
        h.clock.now_utc(),
        lease.expires_at
    );
    ensure!(
        ok(c.current("edge").await, "current")?.is_none(),
        "at expires_at the lease is still live"
    );
    refused(
        c.check_fence("edge", lease.token).await,
        LEASE_LOST,
        "fence check at expires_at",
    )?;
    let next = ok(c.acquire("edge", "b", TTL).await, "acquire at expires_at")?;
    ensure!(
        next.token == FencingToken(2),
        "token at expiry is {:?}",
        next.token
    );
    Ok(())
}

async fn renew_extends_and_keeps_the_token(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let lease = ok(c.acquire("renew", "a", TTL).await, "acquire")?;
    h.clock.advance(Duration::from_secs(6));
    let renewed = ok(c.renew(&lease, TTL).await, "renew")?;
    ensure!(
        renewed.token == lease.token && renewed.id == lease.id,
        "renew changed token or id"
    );
    ensure!(
        renewed.expires_at == h.clock.now_utc() + TTL,
        "renew did not extend to now + ttl"
    );
    h.clock.advance(Duration::from_secs(9));
    ensure!(
        ok(c.current("renew").await, "current")?.is_some(),
        "the renewed lease ended at its old expiry"
    );
    // The original handle, same record, still renews: it names the same acquisition.
    ok(
        c.renew(&lease, TTL).await,
        "renew by the first handle of the same acquisition",
    )?;
    Ok(())
}

// ---- locks ------------------------------------------------------------------------------

async fn lock_waiter_gets_the_lock_after_the_guard_drops(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let held = ok(c.lock("lk", "a", Duration::ZERO).await, "first lock")?;
    let waiting = Arc::clone(c);
    let waiter = tokio::spawn(async move {
        waiting
            .lock("lk", "b", Duration::from_secs(30))
            .await
            .map(|guard| guard.lease().map(|lease| lease.token))
    });
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    ensure!(
        !waiter.is_finished(),
        "the waiter got the lock while it was held"
    );
    drop(held);
    let outcome = ok(
        tokio::time::timeout(Duration::from_secs(30), waiter).await,
        "the waiter never got the lock after the guard dropped",
    )?;
    let token = ok(ok(outcome, "the waiter task")?, "the waiter's lock")?;
    ensure!(
        token == Some(FencingToken(2)),
        "the waiter's token is {token:?}"
    );
    becomes_free(c, "lk").await
}

async fn lock_times_out_and_expires_by_ttl(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let held = ok(c.lock("lk2", "a", Duration::ZERO).await, "first lock")?;
    refused(
        c.lock("lk2", "b", Duration::ZERO).await,
        LOCK_TIMEOUT,
        "lock of a held name",
    )?;
    ok(held.unlock().await, "unlock")?;
    let again = ok(
        c.lock("lk2", "b", Duration::ZERO).await,
        "lock after unlock",
    )?;
    ok(again.unlock().await, "unlock")?;
    // A leaked guard never blocks longer than the ttl.
    let leaked = ok(
        c.lock_with_ttl("lk3", "a", Duration::ZERO, Duration::from_secs(5))
            .await,
        "lock with a 5s ttl",
    )?;
    std::mem::forget(leaked);
    refused(
        c.lock("lk3", "b", Duration::ZERO).await,
        LOCK_TIMEOUT,
        "lock under a leaked guard",
    )?;
    h.clock.advance(Duration::from_secs(5));
    let after = ok(
        c.lock("lk3", "b", Duration::ZERO).await,
        "lock after the leaked ttl",
    )?;
    ensure!(
        after.lease().map(|l| l.token) == Some(FencingToken(2)),
        "the lock after the leak did not get the next token"
    );
    Ok(())
}

async fn dropped_futures_leave_no_lease(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    // Dropped after one poll.
    if let Some(done) = poll_once_then_drop(c.acquire("cx", "a", TTL)) {
        let lease = ok(done, "acquire that finished at its first poll")?;
        ok(c.release(&lease).await, "release")?;
    }
    becomes_free(c, "cx").await?;
    // Dropped by timeouts of several lengths, many times.
    for micros in [0u64, 1, 5, 20, 100, 500] {
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_micros(micros), c.acquire("cx", "a", TTL))
                .await
            {
                Ok(done) => {
                    let lease = ok(done, "acquire that beat its timeout")?;
                    ok(c.release(&lease).await, "release")?;
                }
                Err(_elapsed) => becomes_free(c, "cx").await?,
            }
        }
    }
    // A lock future dropped while it waits must not take the lock afterwards.
    let held = ok(c.lock("cy", "a", Duration::ZERO).await, "lock")?;
    let waiting = poll_once_then_drop(c.lock("cy", "b", Duration::from_secs(60)));
    ensure!(
        waiting.is_none(),
        "a lock on a held name finished at its first poll"
    );
    ok(held.unlock().await, "unlock")?;
    becomes_free(c, "cy").await?;
    for micros in [0u64, 1, 5, 20, 100, 500] {
        for _ in 0..20 {
            match tokio::time::timeout(
                Duration::from_micros(micros),
                c.lock("cy", "b", Duration::from_secs(60)),
            )
            .await
            {
                Ok(done) => drop(ok(done, "lock that beat its timeout")?),
                Err(_elapsed) => {}
            }
            becomes_free(c, "cy").await?;
        }
    }
    Ok(())
}

// ---- counters ---------------------------------------------------------------------------

async fn counters_are_atomic_and_consecutive(h: Harness) -> CheckResult {
    let mut values = spawn_all(64, |_| {
        let c = Arc::clone(&h.coordinator);
        async move { c.next("ctr", 1).await }
    })
    .await?
    .into_iter()
    .map(|value| ok(value, "next"))
    .collect::<Result<Vec<_>, _>>()?;
    values.sort_unstable();
    ensure!(
        values == (1..=64).collect::<Vec<u64>>(),
        "64 racing next(1) did not return 1..=64: {values:?}"
    );
    let c = &h.coordinator;
    ensure!(
        ok(c.next("ctr", 5).await, "next by 5")? == 65,
        "next by 5 did not start at 65"
    );
    ensure!(
        ok(c.next("ctr", 1).await, "next")? == 70,
        "the 5 values were not reserved"
    );
    ensure!(
        ok(c.peek("ctr").await, "peek")? == 70,
        "peek is not the last value issued"
    );
    ensure!(
        ok(c.peek("never-used").await, "peek")? == 0,
        "an unused counter does not peek 0"
    );
    ensure!(
        ok(c.next("ctr-b", 1).await, "next")? == 1,
        "counters are not independent"
    );
    refused(c.next("ctr", 0).await, INVALID_ARGUMENT, "next by 0")?;
    ensure!(
        ok(c.peek("ctr").await, "peek")? == 70,
        "a refused next moved the counter"
    );
    Ok(())
}

async fn counter_administration_and_overflow(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    ensure!(
        ok(c.set_at_least("adm", 100).await, "set_at_least")? == 100,
        "set_at_least 100"
    );
    ensure!(
        ok(c.next("adm", 1).await, "next")? == 101,
        "next after set_at_least(100)"
    );
    ensure!(
        ok(c.set_at_least("adm", 50).await, "set_at_least")? == 101,
        "set_at_least lowered the counter"
    );
    ensure!(
        ok(c.peek("adm").await, "peek")? == 101,
        "peek after a lower set_at_least"
    );
    ok(
        c.set_at_least("adm", u64::MAX - 1).await,
        "set_at_least near the top",
    )?;
    ensure!(
        ok(c.next("adm", 1).await, "next")? == u64::MAX,
        "the last value is not u64::MAX"
    );
    refused(
        c.next("adm", 1).await,
        COUNTER_OVERFLOW,
        "next past the top",
    )?;
    ensure!(
        ok(c.peek("adm").await, "peek")? == u64::MAX,
        "an overflow moved the counter"
    );
    Ok(())
}

// ---- rate limits ------------------------------------------------------------------------

const TEN_PER_SECOND: RateLimit = RateLimit {
    capacity: 10,
    per: Duration::from_secs(1),
};

async fn rate_limit_never_exceeds_and_refills(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let verdicts = spawn_all(100, |_| {
        let c = Arc::clone(&h.coordinator);
        async move { c.take("rl", TEN_PER_SECOND, 1).await }
    })
    .await?;
    let mut remaining = Vec::new();
    let mut denied = 0;
    for verdict in verdicts {
        match ok(verdict, "take")? {
            Admit::Allowed { remaining: left } => remaining.push(left),
            Admit::Denied { .. } => denied += 1,
        }
    }
    remaining.sort_unstable();
    ensure!(
        remaining == (0..10).collect::<Vec<u64>>(),
        "100 racing takers: the admitted ones saw remaining {remaining:?}, expected 0..=9"
    );
    ensure!(
        denied == 90,
        "100 racing takers: {denied} denied, expected 90"
    );
    // The 11th: one unit short, one unit refills in 100ms.
    let retry = |verdict| match verdict {
        Admit::Denied { retry_after } => Ok(retry_after),
        other => Err(format!("expected a denial, got {other:?}")),
    };
    ensure!(
        retry(ok(c.take("rl", TEN_PER_SECOND, 1).await, "take")?)? == Duration::from_millis(100),
        "the 11th take's retry_after is not 100ms"
    );
    h.clock.advance(Duration::from_millis(99));
    ensure!(
        retry(ok(c.take("rl", TEN_PER_SECOND, 1).await, "take")?)? == Duration::from_millis(1),
        "99ms into the wait, retry_after is not 1ms"
    );
    h.clock.advance(Duration::from_millis(1));
    ensure!(
        ok(c.take("rl", TEN_PER_SECOND, 1).await, "take")? == Admit::Allowed { remaining: 0 },
        "after retry_after the take was not admitted"
    );
    retry(ok(c.take("rl", TEN_PER_SECOND, 1).await, "take")?)?;
    // A whole window later the bucket is full again, and no fuller.
    h.clock.advance(Duration::from_secs(5));
    for left in (0..10).rev() {
        ensure!(
            ok(c.take("rl", TEN_PER_SECOND, 1).await, "take")?
                == Admit::Allowed { remaining: left },
            "after a quiet window, the take leaving {left} was not admitted"
        );
    }
    retry(ok(c.take("rl", TEN_PER_SECOND, 1).await, "take")?)?;
    Ok(())
}

async fn rate_limit_costs_and_independent_keys(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    ensure!(
        ok(c.take("cost", TEN_PER_SECOND, 4).await, "take")? == Admit::Allowed { remaining: 6 },
        "first take of 4"
    );
    ensure!(
        ok(c.take("cost", TEN_PER_SECOND, 4).await, "take")? == Admit::Allowed { remaining: 2 },
        "second take of 4"
    );
    // 2 left, 4 asked: 2 short, 200ms. A denial takes nothing.
    let denied = ok(c.take("cost", TEN_PER_SECOND, 4).await, "take")?;
    ensure!(
        denied
            == Admit::Denied {
                retry_after: Duration::from_millis(200)
            },
        "third take of 4: {denied:?}"
    );
    ensure!(
        ok(c.take("cost", TEN_PER_SECOND, 2).await, "take")? == Admit::Allowed { remaining: 0 },
        "the denial took units"
    );
    ensure!(
        ok(c.take("cost-other", TEN_PER_SECOND, 10).await, "take")?
            == Admit::Allowed { remaining: 0 },
        "another key shares the bucket"
    );
    // Refill is continuous: 150ms is 1.5 units, so one whole unit is there.
    h.clock.advance(Duration::from_millis(150));
    ensure!(
        ok(c.take("cost", TEN_PER_SECOND, 1).await, "take")? == Admit::Allowed { remaining: 0 },
        "150ms of refill did not give one unit"
    );
    refused(
        c.take("cost", TEN_PER_SECOND, 0).await,
        INVALID_ARGUMENT,
        "cost 0",
    )?;
    refused(
        c.take("cost", TEN_PER_SECOND, 11).await,
        INVALID_ARGUMENT,
        "cost above capacity",
    )?;
    let no_capacity = RateLimit {
        capacity: 0,
        per: Duration::from_secs(1),
    };
    refused(
        c.take("cost", no_capacity, 1).await,
        INVALID_ARGUMENT,
        "capacity 0",
    )?;
    let no_window = RateLimit {
        capacity: 1,
        per: Duration::ZERO,
    };
    refused(
        c.take("cost", no_window, 1).await,
        INVALID_ARGUMENT,
        "window 0",
    )?;
    Ok(())
}

// ---- validation -------------------------------------------------------------------------

async fn names_and_arguments_are_validated(h: Harness) -> CheckResult {
    let c = &h.coordinator;
    let too_long = "x".repeat(MAX_NAME_BYTES + 1);
    for bad in [
        "",
        too_long.as_str(),
        "line\nbreak",
        "nul\0byte",
        "tab\there",
    ] {
        refused(c.acquire(bad, "a", TTL).await, INVALID_NAME, "acquire name")?;
        refused(
            c.acquire("ok-name", bad, TTL).await,
            INVALID_NAME,
            "acquire holder",
        )?;
        refused(c.current(bad).await, INVALID_NAME, "current name")?;
        refused(
            c.check_fence(bad, FencingToken(1)).await,
            INVALID_NAME,
            "check_fence name",
        )?;
        refused(c.next(bad, 1).await, INVALID_NAME, "next name")?;
        refused(c.peek(bad).await, INVALID_NAME, "peek name")?;
        refused(
            c.set_at_least(bad, 1).await,
            INVALID_NAME,
            "set_at_least name",
        )?;
        refused(
            c.take(bad, TEN_PER_SECOND, 1).await,
            INVALID_NAME,
            "take key",
        )?;
    }
    let longest = "y".repeat(MAX_NAME_BYTES);
    let lease: Lease = ok(
        c.acquire(&longest, "a", TTL).await,
        "acquire of a 200-byte name",
    )?;
    ok(c.release(&lease).await, "release")?;
    ok(
        c.acquire("unicode-é-名前", "ä", TTL).await,
        "acquire of a printable Unicode name",
    )?;
    refused(
        c.acquire("ok-name", "a", Duration::ZERO).await,
        INVALID_ARGUMENT,
        "ttl 0",
    )?;
    refused(
        c.acquire("ok-name", "a", Duration::from_secs(400 * 24 * 3600))
            .await,
        INVALID_ARGUMENT,
        "ttl of over a year",
    )?;
    ensure!(
        ok(c.current("ok-name").await, "current")?.is_none(),
        "a refused acquire left a lease"
    );
    Ok(())
}
