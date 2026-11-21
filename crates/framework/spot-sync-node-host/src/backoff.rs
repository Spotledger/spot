//! Exponential backoff with deterministic jitter.
//!
//! The delay after the `n`th failure in a row is `min * 2^(n-1)`, capped at `max`, then jittered
//! between half and all of itself from an injected source, and never below `min`: so a fleet of
//! edges spreads out, a test with [`NoJitter`](spot_sync_http::NoJitter) sees the plain doubling,
//! and a seed replays the same schedule.

use std::time::Duration;

use spot_sync_http::JitterSource;

/// The backoff state of one loop.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    failures: u32,
}

impl Backoff {
    /// A backoff between `min` and `max` (already validated by the settings).
    pub fn new(min: Duration, max: Duration) -> Self {
        Self {
            min,
            max: max.max(min),
            failures: 0,
        }
    }

    /// Consecutive failures so far.
    pub fn failures(&self) -> u32 {
        self.failures
    }

    /// A session got through: the next failure starts from `min` again.
    pub fn reset(&mut self) {
        self.failures = 0;
    }

    /// Records a failure and returns how long to wait before the next attempt.
    pub fn next_delay(&mut self, jitter: &mut dyn JitterSource) -> Duration {
        self.failures = self.failures.saturating_add(1);
        let exponent = self.failures.saturating_sub(1).min(32);
        let full = self
            .min
            .checked_mul(1_u32.checked_shl(exponent).unwrap_or(u32::MAX))
            .unwrap_or(self.max)
            .min(self.max);
        let permille = jitter.permille().min(1_000);
        // Half fixed, half scaled by the jitter: `full/2 .. full`.
        let millis = full.as_millis();
        let jittered_millis = millis / 2 + (millis / 2) * u128::from(permille) / 1_000;
        let jittered = Duration::from_millis(u64::try_from(jittered_millis).unwrap_or(u64::MAX));
        jittered.max(self.min).min(self.max)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_sync_http::{NoJitter, SeededJitter};

    use super::*;

    #[test]
    fn the_schedule_doubles_to_the_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(300));
        let mut jitter = NoJitter;
        let schedule: Vec<u64> = (0..12)
            .map(|_| backoff.next_delay(&mut jitter).as_secs())
            .collect();
        assert_eq!(
            schedule,
            vec![1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300, 300]
        );
        backoff.reset();
        assert_eq!(backoff.next_delay(&mut jitter), Duration::from_secs(1));
    }

    #[test]
    fn jitter_stays_between_half_and_all_and_replays_from_a_seed() {
        let run = |seed| {
            let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(300));
            let mut jitter = SeededJitter::new(seed);
            (0..20)
                .map(|_| backoff.next_delay(&mut jitter))
                .collect::<Vec<_>>()
        };
        let first = run(7);
        assert_eq!(first, run(7));
        assert_ne!(first, run(8));
        for delay in first {
            assert!(delay >= Duration::from_secs(1));
            assert!(delay <= Duration::from_secs(300));
        }
    }

    #[test]
    fn a_huge_failure_count_does_not_overflow() {
        let mut backoff = Backoff::new(Duration::from_secs(1), Duration::from_secs(300));
        let mut jitter = NoJitter;
        for _ in 0..200 {
            let _ = backoff.next_delay(&mut jitter);
        }
        assert_eq!(backoff.next_delay(&mut jitter), Duration::from_secs(300));
    }
}
