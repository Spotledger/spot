//! Deadline and retry shapes for a session.
//!
//! ADR-072 fixes the resilience kit (deadline, retry, breaker, bulkhead) in `spot-core`. That
//! crate is **not merged yet** (the work package is proposed), so this module is a local
//! stand-in with the same shape and the same semantics for the two pieces sync needs:
//!
//! * [`Deadline`] is an absolute point on the monotonic clock, created once at the edge of a
//!   session and passed down, never recreated deeper; [`Deadline::child`] narrows it.
//! * [`RetryPolicy`] is bounded (a required maximum attempt count, a base, a factor and a cap)
//!   and its jitter source is **injected** as an integer in parts per thousand, so tests and the
//!   simulator are deterministic and no floating-point arithmetic is used (ADR-012, workspace
//!   lint `clippy::float_arithmetic`).
//!
//! When `spot-resilience` lands, callers move to it; nothing else in this crate changes, because
//! sessions already take time and jitter as arguments and read no clock of their own.
//!
//! # Time is an argument
//!
//! No function here reads the wall clock or any ambient time. `now` and the clock's monotonic
//! origin are the caller's, so a test drives time exactly.

use std::time::Duration;

use crate::error::{Error, Result};

/// An absolute instant on a monotonic clock, used as a session's deadline.
///
/// The monotonic origin is the caller's; a [`Deadline`] is only ever compared with instants from
/// the same origin, so the value is meaningful only within one process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Deadline {
    at: Duration,
}

impl Deadline {
    /// A deadline `budget` from `now`.
    pub fn new(now: Duration, budget: Duration) -> Self {
        Self {
            at: now.saturating_add(budget),
        }
    }

    /// A deadline no later than this one and no later than `now + max`.
    ///
    /// A child deadline can only tighten the parent's, so a deeper layer can never extend a
    /// caller's patience (ADR-072 section 2).
    pub fn child(&self, now: Duration, max: Duration) -> Self {
        Self {
            at: self.at.min(now.saturating_add(max)),
        }
    }

    /// The absolute instant.
    pub fn at(&self) -> Duration {
        self.at
    }

    /// Time left at `now`, saturating at zero.
    pub fn remaining(&self, now: Duration) -> Duration {
        self.at.saturating_sub(now)
    }

    /// Whether the deadline has passed at `now`.
    pub fn is_expired(&self, now: Duration) -> bool {
        now >= self.at
    }

    /// Time left at `now`, or `sync_protocol.deadline_exceeded` when it has passed.
    pub fn check(&self, now: Duration) -> Result<Duration> {
        if self.is_expired(now) {
            Err(Error::deadline_exceeded("the session deadline passed"))
        } else {
            Ok(self.remaining(now))
        }
    }
}

/// A bounded retry policy with injected jitter: exponential backoff, capped, never unlimited.
///
/// `max_attempts` is required and at least one (ADR-068 rule 8: no default of "forever"); the
/// caller supplies the jitter for each attempt as parts per thousand, so the same seed replays
/// the same delays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The total attempts allowed, including the first.
    pub max_attempts: u32,
    /// The base backoff.
    pub base: Duration,
    /// The exponential factor applied per retry.
    pub factor: u32,
    /// The cap on a single backoff.
    pub cap: Duration,
}

impl RetryPolicy {
    /// A policy; fails on zero attempts or a factor below one.
    pub fn new(max_attempts: u32, base: Duration, factor: u32, cap: Duration) -> Result<Self> {
        if max_attempts == 0 {
            return Err(Error::invalid_argument(
                "a retry policy allows at least one attempt",
            ));
        }
        if factor < 1 {
            return Err(Error::invalid_argument(
                "a retry policy's backoff factor is at least one",
            ));
        }
        Ok(Self {
            max_attempts,
            base,
            factor,
            cap,
        })
    }

    /// Whether an attempt number (1-based) is still allowed.
    pub fn allows(&self, attempt: u32) -> bool {
        attempt >= 1 && attempt <= self.max_attempts
    }

    /// The backoff before `attempt` (1-based), scaled by `jitter_permille` in `0..=1000`.
    ///
    /// The deterministic backoff is `base * factor^(attempt-1)` capped at `cap`; the returned
    /// delay is that value scaled by the jitter. A caller that wants full jitter draws an integer
    /// in `0..=1000` from its injected source; `1000` means no jitter.
    pub fn delay(&self, attempt: u32, jitter_permille: u64) -> Duration {
        let jitter = jitter_permille.min(1000);
        let mut millis = self.base.as_millis() as u64;
        for _ in 1..attempt.max(1) {
            millis = millis.saturating_mul(u64::from(self.factor));
            if millis >= self.cap.as_millis() as u64 {
                break;
            }
        }
        let capped = millis.min(self.cap.as_millis() as u64);
        Duration::from_millis(capped.saturating_mul(jitter) / 1000)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_deadline_only_tightens() {
        let deadline = Deadline::new(Duration::from_millis(100), Duration::from_millis(500));
        let now = Duration::from_millis(300);
        assert_eq!(deadline.at(), Duration::from_millis(600));
        assert_eq!(
            deadline.child(now, Duration::from_millis(50)).at(),
            Duration::from_millis(350)
        );
        assert_eq!(deadline.remaining(now), Duration::from_millis(300));
        assert!(deadline.check(Duration::from_millis(600)).is_err());
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let policy =
            RetryPolicy::new(5, Duration::from_millis(100), 2, Duration::from_millis(350)).unwrap();
        assert_eq!(policy.delay(1, 1000), Duration::from_millis(100));
        assert_eq!(policy.delay(2, 1000), Duration::from_millis(200));
        assert_eq!(policy.delay(3, 1000), Duration::from_millis(350));
        assert_eq!(policy.delay(4, 1000), Duration::from_millis(350));
        assert_eq!(policy.delay(2, 500), Duration::from_millis(100));
        assert!(!policy.allows(0));
        assert!(!policy.allows(6));
    }
}
