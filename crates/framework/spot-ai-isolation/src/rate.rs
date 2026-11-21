//! Request-rate limiting for AI calls: a token bucket per scope, and the gate trait admission
//! uses.
//!
//! The in-process bucket is the fallback; a clustered deployment takes the same tokens from a
//! [`spot_coordinator::Coordinator`] instead (see
//! [`crate::admission::AiAdmission::admit_cluster`]), so a fleet never admits more than the
//! rate however many nodes race. All arithmetic is integer microseconds: a wall-clock jump
//! cannot refill a bucket, and there are no floats.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use spot_coordinator::RateLimit;

/// One scope's token bucket.
///
/// Refills continuously at `capacity` units per `per` and holds at most `capacity`. The bucket
/// is exact: the time corresponding to a whole token's refill is consumed from `last`, so
/// partial refills are never lost.
#[derive(Clone, Copy, Debug)]
pub struct TokenBucket {
    capacity: u64,
    per_micros: u64,
    tokens: u64,
    last: Duration,
}

impl TokenBucket {
    /// A full bucket whose clock starts at `now`.
    #[must_use]
    pub fn new(limit: RateLimit, now: Duration) -> Self {
        Self {
            capacity: limit.capacity,
            per_micros: micros(limit.per).max(1),
            tokens: limit.capacity,
            last: now,
        }
    }

    /// Takes `cost` units, returning the units left, or the time until enough have refilled.
    ///
    /// # Errors
    ///
    /// The `Duration` a caller must wait when the bucket is short of `cost` units.
    pub fn try_take(&mut self, now: Duration, cost: u64) -> Result<u64, Duration> {
        self.refill(now);
        if self.tokens >= cost {
            self.tokens -= cost;
            return Ok(self.tokens);
        }
        let missing = cost - self.tokens;
        let needed_micros = missing.saturating_mul(self.per_micros);
        Err(Duration::from_micros(
            needed_micros.div_ceil(self.capacity.max(1)),
        ))
    }

    /// The units currently in the bucket.
    #[must_use]
    pub fn tokens(&self) -> u64 {
        self.tokens
    }

    fn refill(&mut self, now: Duration) {
        let elapsed = now.saturating_sub(self.last);
        let refill = micros(elapsed).saturating_mul(self.capacity) / self.per_micros;
        if refill == 0 {
            return;
        }
        self.tokens = (self.tokens + refill).min(self.capacity);
        let consumed_micros = refill.saturating_mul(self.per_micros) / self.capacity.max(1);
        self.last += Duration::from_micros(consumed_micros);
    }
}

/// A gate that decides whether one more request may be admitted now.
pub trait RateGate: Send + Sync {
    /// Takes `cost` units under `key` for `limit`.
    ///
    /// # Errors
    ///
    /// The `Duration` a caller must wait when the bucket is short of `cost` units.
    fn take(&self, key: &str, limit: RateLimit, cost: u64, now: Duration) -> Result<(), Duration>;
}

/// The in-process fallback gate: one bucket per key, under one mutex.
#[derive(Debug, Default)]
pub struct LocalRateGate {
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl LocalRateGate {
    /// An empty gate.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl RateGate for LocalRateGate {
    fn take(&self, key: &str, limit: RateLimit, cost: u64, now: Duration) -> Result<(), Duration> {
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let bucket = buckets
            .entry(key.to_owned())
            .or_insert_with(|| TokenBucket::new(limit, now));
        bucket.try_take(now, cost).map(|_| ())
    }
}

/// `value` in whole microseconds, saturating rather than wrapping.
fn micros(value: Duration) -> u64 {
    u64::try_from(value.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn limit(capacity: u64, per_secs: u64) -> RateLimit {
        RateLimit {
            capacity,
            per: Duration::from_secs(per_secs),
        }
    }

    #[test]
    fn a_bucket_refuses_past_its_capacity_and_says_when_to_retry() {
        let mut bucket = TokenBucket::new(limit(3, 1), Duration::ZERO);
        assert_eq!(bucket.try_take(Duration::ZERO, 3), Ok(0));
        let wait = bucket
            .try_take(Duration::ZERO, 1)
            .expect_err("the bucket is empty");
        assert!(wait > Duration::ZERO && wait <= Duration::from_secs(1));
    }

    #[test]
    fn a_bucket_refills_over_time_and_keeps_partial_refills() {
        let mut bucket = TokenBucket::new(limit(2, 1), Duration::ZERO);
        assert!(bucket.try_take(Duration::ZERO, 2).is_ok());
        // Half a second buys exactly one token at two per second.
        assert!(bucket.try_take(Duration::from_millis(500), 1).is_ok());
        // A sub-token wait buys nothing, and does not lose the fraction already elapsed.
        assert!(bucket.try_take(Duration::from_millis(999), 1).is_err());
        assert!(bucket.try_take(Duration::from_millis(1000), 1).is_ok());
    }

    #[test]
    fn the_gate_shares_one_bucket_per_key() {
        let gate = LocalRateGate::new();
        let l = limit(1, 60);
        assert!(gate.take("agent:a", l, 1, Duration::ZERO).is_ok());
        assert!(gate.take("agent:a", l, 1, Duration::ZERO).is_err());
        assert!(gate.take("agent:b", l, 1, Duration::ZERO).is_ok());
    }
}
