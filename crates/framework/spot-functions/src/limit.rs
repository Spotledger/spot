//! The token-bucket rate limiter over the injectable [`Clock`], with bounded memory.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use spot_core::clock::Clock;

/// One token bucket: tokens in 1/1000 units (integer math; no floats anywhere in the crate).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TokenBucket {
    /// Tokens left, in 1/1000 of a token.
    pub tokens_milli: i64,
    /// The monotonic time of the bucket's last take or refill.
    pub last: Duration,
}

/// Why the limiter refused a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LimitError {
    /// How long until the bucket has one token again.
    pub retry_after: Duration,
}

/// One token bucket per `(function, caller)` key, filled at `permits` per `per` window,
/// refilled continuously in integer arithmetic: one window distributes
/// `permits × 1000` milli-tokens. The clock is read only through [`Clock::monotonic`].
///
/// Buckets live in a [`BTreeMap`] behind one mutex, which no `await` holds: a token take is a
/// few integer operations, so concurrent dispatches serialize briefly and cannot exceed the
/// limit.
pub struct RateLimiter {
    clock: Mutex<Option<Arc<dyn Clock>>>,
    buckets: Mutex<BTreeMap<(String, String), TokenBucket>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        RateLimiter {
            clock: Mutex::new(None),
            buckets: Mutex::new(BTreeMap::new()),
        }
    }
}

impl RateLimiter {
    /// A limiter reading time from `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> RateLimiter {
        let limiter = RateLimiter::default();
        limiter.set_clock(clock);
        limiter
    }

    /// Replaces the clock (hosts set the real one; tests swap in a manual clock).
    pub fn set_clock(&self, clock: Arc<dyn Clock>) {
        *Self::lock(&self.clock) = Some(clock);
    }

    fn lock<'a, T>(mutex: &'a Mutex<T>) -> MutexGuard<'a, T> {
        // The guarded data is a plain map or slot; a panic elsewhere cannot leave it
        // inconsistent, so recovery over a poisoned lock is safe.
        mutex
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn now(&self) -> Duration {
        Self::lock(&self.clock)
            .as_ref()
            .map_or(Duration::ZERO, |c| c.monotonic())
    }

    /// Takes one token for the bucket at `key` (usually `function.display()/caller.bucket()`),
    /// under the limit `permits` per `per`. Returns [`LimitError`] with the `retry_after` when
    /// the bucket is empty; the failed take does not consume or owe a token.
    pub fn check(
        &self,
        key: &(String, String),
        permits: u32,
        per: Duration,
    ) -> Result<(), LimitError> {
        let now = self.now();
        let mut buckets = Self::lock(&self.buckets);
        let bucket = buckets.entry(key.clone()).or_insert_with(|| TokenBucket {
            tokens_milli: i64::from(permits) * 1000,
            last: now,
        });
        refill(bucket, permits, per, now);
        if bucket.tokens_milli >= 1000 {
            bucket.tokens_milli -= 1000;
            bucket.last = now;
            Ok(())
        } else {
            Err(LimitError {
                retry_after: retry_after(permits, per),
            })
        }
    }

    /// Drops every bucket idle for longer than `older_than`, bounding memory; returns how many
    /// were dropped. Idle means: no take since `older_than` ago — a spent bucket still waiting
    /// for tokens is dropped too, and its next call just starts a fresh full bucket.
    pub fn evict_idle(&self, older_than: Duration) -> usize {
        let now = self.now();
        let mut buckets = Self::lock(&self.buckets);
        let before = buckets.len();
        buckets.retain(|_, bucket| now.saturating_sub(bucket.last) <= older_than);
        before - buckets.len()
    }

    /// How many buckets exist (tests and hosts watch the memory bound).
    pub fn len(&self) -> usize {
        Self::lock(&self.buckets).len()
    }

    /// Whether the limiter holds no bucket.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Refills `bucket` with the milli-tokens the time since its last touch earned: a full window
/// distributes `permits × 1000` milli-tokens. Only a positive gain updates `last`, so an idle
/// bucket shows the true last-take time to the eviction.
fn refill(bucket: &mut TokenBucket, permits: u32, per: Duration, now: Duration) {
    let elapsed_ms = now.saturating_sub(bucket.last).as_millis();
    let per_ms = per.as_millis().max(1);
    if permits == 0 {
        bucket.tokens_milli = 0;
        return;
    }
    let gained = elapsed_ms * u128::from(permits) * 1000 / per_ms;
    if gained > 0 {
        let cap = u128::from(permits) * 1000;
        bucket.tokens_milli = (u128::from(u64::try_from(bucket.tokens_milli).unwrap_or(u64::MAX))
            + gained)
            .min(cap) as i64;
        bucket.last = now;
    }
}

/// How long until one token is back: the time one token's worth of the window takes, rounded
/// up to a whole millisecond.
fn retry_after(permits: u32, per: Duration) -> Duration {
    let per_token_ms = per.as_millis() / u128::from(permits.max(1));
    Duration::from_millis(per_token_ms.max(1) as u64)
}
