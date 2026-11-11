//! Per-key rate limiting in integer arithmetic, driven by a monotonic `now` argument.
//!
//! A [`RateLimiter`] is keyed by something already authenticated (a link the caller is the peer
//! of, or an enrolled party), so an outsider cannot create keys. The table is still bounded:
//! idle full buckets are dropped first, and when the table is full of busy keys a new key is
//! refused rather than letting the map grow.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

/// Token-bucket settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RateConfig {
    /// The most requests that can arrive at once.
    pub burst: u32,
    /// Requests added back per second.
    pub per_second: u32,
    /// The most keys tracked at once.
    pub max_keys: usize,
}

impl Default for RateConfig {
    fn default() -> Self {
        Self {
            burst: 60,
            per_second: 20,
            max_keys: 10_000,
        }
    }
}

const MILLI: u64 = 1_000;

struct Bucket {
    /// Tokens in thousandths.
    tokens: u64,
    at: Duration,
}

/// A bounded set of token buckets.
pub struct RateLimiter {
    config: RateConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// A limiter with `config`; a zero burst or rate is raised to one so the limiter can never
    /// refuse forever.
    #[must_use]
    pub fn new(config: RateConfig) -> Self {
        Self {
            config: RateConfig {
                burst: config.burst.max(1),
                per_second: config.per_second.max(1),
                max_keys: config.max_keys.max(1),
            },
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one token for `key` at monotonic `now`.
    ///
    /// # Errors
    /// The time until the next token, when the bucket is empty or the table is full of busy keys.
    pub fn take(&self, key: &str, now: Duration) -> Result<(), Duration> {
        let full = u64::from(self.config.burst).saturating_mul(MILLI);
        let rate = u64::from(self.config.per_second);
        let mut buckets = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !buckets.contains_key(key) && buckets.len() >= self.config.max_keys {
            buckets.retain(|_, bucket| refilled(bucket, now, full, rate) < full);
            if buckets.len() >= self.config.max_keys {
                return Err(Duration::from_secs(1));
            }
        }
        let bucket = buckets.entry(key.to_owned()).or_insert(Bucket {
            tokens: full,
            at: now,
        });
        bucket.tokens = refilled(bucket, now, full, rate);
        bucket.at = now;
        if bucket.tokens >= MILLI {
            bucket.tokens -= MILLI;
            Ok(())
        } else {
            let missing = MILLI - bucket.tokens;
            Err(Duration::from_millis(missing.div_ceil(rate)))
        }
    }

    /// How many keys are tracked (tests and diagnostics).
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.buckets.lock().map_or(0, |buckets| buckets.len())
    }
}

fn refilled(bucket: &Bucket, now: Duration, full: u64, rate: u64) -> u64 {
    let elapsed = u64::try_from(now.saturating_sub(bucket.at).as_millis()).unwrap_or(u64::MAX);
    bucket
        .tokens
        .saturating_add(elapsed.saturating_mul(rate))
        .min(full)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limiter(burst: u32, per_second: u32, max_keys: usize) -> RateLimiter {
        RateLimiter::new(RateConfig {
            burst,
            per_second,
            max_keys,
        })
    }

    #[test]
    fn a_burst_passes_then_the_bucket_empties_and_refills() {
        let limiter = limiter(3, 2, 10);
        let t0 = Duration::from_secs(10);
        for _ in 0..3 {
            assert!(limiter.take("a", t0).is_ok());
        }
        let wait = limiter.take("a", t0).unwrap_err();
        assert_eq!(wait, Duration::from_millis(500));
        assert!(limiter.take("a", t0 + Duration::from_millis(499)).is_err());
        assert!(limiter.take("a", t0 + Duration::from_millis(500)).is_ok());
    }

    #[test]
    fn keys_do_not_share_a_bucket() {
        let limiter = limiter(1, 1, 10);
        let t0 = Duration::ZERO;
        assert!(limiter.take("a", t0).is_ok());
        assert!(limiter.take("a", t0).is_err());
        assert!(limiter.take("b", t0).is_ok());
    }

    #[test]
    fn the_table_is_bounded_and_idle_keys_are_dropped_first() {
        let limiter = limiter(1, 1, 2);
        let t0 = Duration::ZERO;
        assert!(limiter.take("a", t0).is_ok());
        assert!(limiter.take("b", t0).is_ok());
        // Both are empty (busy): a third key is refused, the table does not grow.
        assert!(limiter.take("c", t0).is_err());
        assert_eq!(limiter.tracked(), 2);
        // Once they refill they are idle and make room.
        assert!(limiter.take("c", t0 + Duration::from_secs(5)).is_ok());
        assert_eq!(limiter.tracked(), 1);
    }

    #[test]
    fn a_zero_configuration_never_refuses_forever() {
        let limiter = limiter(0, 0, 0);
        assert!(limiter.take("a", Duration::ZERO).is_ok());
        assert!(limiter.take("a", Duration::from_secs(2)).is_ok());
    }
}
