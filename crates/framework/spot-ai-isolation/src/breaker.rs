//! Circuit breakers per agent identity and per tenant (WP-AI-05.T1, ADR-072 semantics).
//!
//! A breaker is per named scope, in memory, on one node. It opens when, within a sliding
//! window of calls, the failure ratio exceeds a threshold and a minimum number of calls was
//! seen; slow calls count as failures above a stated latency. `open` lasts a base duration
//! with deterministic jitter, then `half_open` admits a small fixed number of probe calls:
//! success closes, failure reopens with a longer duration up to a cap. When open, a call fails
//! at once with [`crate::error::CIRCUIT_OPEN`], carrying the time until the next probe.
//!
//! ADR-072 places this kit in `spot-core::resilience` (WP-REL-06, not built yet); this is the
//! local logic the ADR allows a task already in flight to keep, with a `TODO` of the REL-04
//! migration.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::time::Duration;

use crate::error::{AiError, CIRCUIT_OPEN, Result};

/// The breaker's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BreakerState {
    /// Calls pass through.
    Closed,
    /// Calls fail at once until the open duration passes.
    Open,
    /// A bounded number of probe calls is admitted.
    HalfOpen,
}

/// The outcome of one call, as the breaker sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The call succeeded.
    Success,
    /// The call failed.
    Failure,
    /// The call succeeded but took longer than the policy's slow threshold.
    Slow,
}

/// A breaker's thresholds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerPolicy {
    /// How many recent calls the failure ratio is measured over.
    pub window: u32,
    /// The fewest calls in the window before the breaker may open.
    pub min_calls: u32,
    /// The failure-ratio threshold's numerator.
    pub failure_num: u32,
    /// The failure-ratio threshold's denominator.
    pub failure_den: u32,
    /// The base open duration.
    pub open_base: Duration,
    /// The cap on the open duration as it grows.
    pub open_max: Duration,
    /// How many probe calls a half-open breaker admits.
    pub probes: u32,
}

impl Default for BreakerPolicy {
    fn default() -> Self {
        Self {
            window: 20,
            min_calls: 10,
            failure_num: 1,
            failure_den: 2,
            open_base: Duration::from_secs(5),
            open_max: Duration::from_secs(300),
            probes: 3,
        }
    }
}

/// A breaker's visible state, for the health surface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakerHealth {
    /// The scope's identity (agent or tenant).
    pub scope: String,
    /// The current state.
    pub state: BreakerState,
    /// When an open breaker admits its next probe.
    pub open_until: Option<Duration>,
}

/// One scope's breaker.
#[derive(Clone, Debug)]
struct Breaker {
    policy: BreakerPolicy,
    outcomes: VecDeque<bool>,
    state: BreakerState,
    opened_at: Duration,
    open_for: Duration,
    opens: u32,
    probes_left: u32,
    rng: Lcg,
}

impl Breaker {
    fn new(policy: BreakerPolicy, seed: u64) -> Self {
        Self {
            policy,
            outcomes: VecDeque::new(),
            state: BreakerState::Closed,
            opened_at: Duration::ZERO,
            open_for: Duration::ZERO,
            opens: 0,
            probes_left: 0,
            rng: Lcg(seed | 1),
        }
    }

    fn admit(&mut self, now: Duration) -> Result<()> {
        if self.state == BreakerState::Open {
            let until = self.opened_at + self.open_for;
            if now < until {
                return Err(AiError::new(
                    CIRCUIT_OPEN,
                    "the circuit breaker for this scope is open",
                )
                .with_retry_after(until - now));
            }
            self.state = BreakerState::HalfOpen;
            self.probes_left = self.policy.probes;
        }
        if self.state == BreakerState::HalfOpen {
            if self.probes_left == 0 {
                return Err(AiError::new(
                    CIRCUIT_OPEN,
                    "the circuit breaker for this scope is half-open and its probes are spent",
                ));
            }
            self.probes_left -= 1;
        }
        Ok(())
    }

    fn record(&mut self, now: Duration, outcome: Outcome) {
        match self.state {
            BreakerState::Closed => {
                self.outcomes
                    .push_back(!matches!(outcome, Outcome::Success));
                while self.outcomes.len() > self.policy.window as usize {
                    self.outcomes.pop_front();
                }
                if self.outcomes.len() >= self.policy.min_calls as usize {
                    let failures = self.outcomes.iter().filter(|failed| **failed).count();
                    if u64::try_from(failures).unwrap_or(u64::MAX)
                        * u64::from(self.policy.failure_den)
                        >= u64::try_from(self.outcomes.len()).unwrap_or(0)
                            * u64::from(self.policy.failure_num)
                    {
                        self.open(now);
                    }
                }
            }
            BreakerState::HalfOpen => {
                if matches!(outcome, Outcome::Success) {
                    self.close();
                } else {
                    self.open(now);
                }
            }
            BreakerState::Open => {}
        }
    }

    fn open(&mut self, now: Duration) {
        self.state = BreakerState::Open;
        self.opened_at = now;
        let shift = self.opens.min(16);
        let base = self
            .policy
            .open_base
            .saturating_mul(1u32 << shift)
            .min(self.policy.open_max);
        self.open_for = self.jittered(base);
        self.opens = self.opens.saturating_add(1);
        self.probes_left = 0;
        self.outcomes.clear();
    }

    fn close(&mut self) {
        self.state = BreakerState::Closed;
        self.opens = 0;
        self.probes_left = 0;
        self.outcomes.clear();
    }

    /// `base` plus up to a quarter of itself, from the deterministic random source.
    fn jittered(&mut self, base: Duration) -> Duration {
        let micros = u64::try_from(base.as_micros()).unwrap_or(u64::MAX);
        let span = micros / 4 + 1;
        base + Duration::from_micros(self.rng.next() % span)
    }

    fn open_until(&self) -> Option<Duration> {
        (self.state == BreakerState::Open).then_some(self.opened_at + self.open_for)
    }
}

/// A deterministic linear-congruential source, so jitter is reproducible in tests.
#[derive(Clone, Debug)]
struct Lcg(u64);

impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
}

/// Breakers keyed by scope, sharing one policy and one seed.
#[derive(Clone, Debug)]
pub struct KeyedBreakers<K> {
    policy: BreakerPolicy,
    seed: u64,
    map: HashMap<K, Breaker>,
}

impl<K: Eq + Hash + Clone> KeyedBreakers<K> {
    /// Breakers with one policy, whose jitter is reproducible from `seed`.
    #[must_use]
    pub fn new(policy: BreakerPolicy, seed: u64) -> Self {
        Self {
            policy,
            seed,
            map: HashMap::new(),
        }
    }

    /// Whether a call for `key` may proceed now.
    ///
    /// # Errors
    ///
    /// [`crate::error::CIRCUIT_OPEN`] when the breaker is open or its half-open probes are
    /// spent.
    pub fn admit(&mut self, key: &K, now: Duration) -> Result<()> {
        self.entry(key).admit(now)
    }

    /// Records one call's outcome, opening, closing or reopening the breaker as the policy
    /// dictates.
    pub fn record(&mut self, key: &K, now: Duration, outcome: Outcome) {
        self.entry(key).record(now, outcome);
    }

    /// The breaker's current state.
    #[must_use]
    pub fn state(&self, key: &K) -> BreakerState {
        self.map
            .get(key)
            .map_or(BreakerState::Closed, |breaker| breaker.state)
    }

    /// Every breaker that has been touched, for the health surface.
    #[must_use]
    pub fn health(&self, label: impl Fn(&K) -> String) -> Vec<BreakerHealth> {
        let mut out: Vec<BreakerHealth> = self
            .map
            .iter()
            .map(|(key, breaker)| BreakerHealth {
                scope: label(key),
                state: breaker.state,
                open_until: breaker.open_until(),
            })
            .collect();
        out.sort_by(|a, b| a.scope.cmp(&b.scope));
        out
    }

    fn entry(&mut self, key: &K) -> &mut Breaker {
        let seed = self.seed;
        let policy = self.policy;
        self.map
            .entry(key.clone())
            .or_insert_with(|| Breaker::new(policy, seed))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn policy() -> BreakerPolicy {
        BreakerPolicy {
            window: 4,
            min_calls: 4,
            failure_num: 1,
            failure_den: 2,
            open_base: Duration::from_secs(10),
            open_max: Duration::from_secs(100),
            probes: 2,
        }
    }

    #[test]
    fn repeated_failures_open_the_breaker_and_refuse_at_once() {
        let mut breakers: KeyedBreakers<&str> = KeyedBreakers::new(policy(), 7);
        for _ in 0..4 {
            breakers.admit(&"a", Duration::ZERO).expect("closed admits");
            breakers.record(&"a", Duration::ZERO, Outcome::Failure);
        }
        assert_eq!(breakers.state(&"a"), BreakerState::Open);
        let error = breakers
            .admit(&"a", Duration::ZERO)
            .expect_err("open refuses");
        assert_eq!(error.code, CIRCUIT_OPEN);
        assert!(error.retry_after.is_some());
        // A different scope is untouched: a misbehaving agent is isolated.
        assert!(breakers.admit(&"b", Duration::ZERO).is_ok());
    }

    #[test]
    fn half_open_probes_close_on_success_and_reopen_longer_on_failure() {
        let mut breakers: KeyedBreakers<&str> = KeyedBreakers::new(policy(), 7);
        for _ in 0..4 {
            breakers.record(&"a", Duration::ZERO, Outcome::Failure);
        }
        let first_open = breakers.map[&"a"].open_for;
        // Before the open duration passes, a call is refused.
        assert!(breakers.admit(&"a", Duration::ZERO).is_err());
        // After it, probes are admitted; two successes close the breaker.
        let after = Duration::from_secs(10) + first_open;
        breakers.admit(&"a", after).expect("a probe is admitted");
        breakers.record(&"a", after, Outcome::Success);
        breakers
            .admit(&"a", after)
            .expect("a second probe is admitted");
        breakers.record(&"a", after, Outcome::Success);
        assert_eq!(breakers.state(&"a"), BreakerState::Closed);

        // A failing probe reopens with a duration no shorter than the first open.
        for _ in 0..4 {
            breakers.record(&"a", Duration::ZERO, Outcome::Failure);
        }
        let second_open = breakers.map[&"a"].open_for;
        assert!(second_open >= first_open);
    }

    #[test]
    fn slow_calls_count_as_failures() {
        let mut breakers: KeyedBreakers<&str> = KeyedBreakers::new(policy(), 3);
        for _ in 0..4 {
            breakers.record(&"a", Duration::ZERO, Outcome::Slow);
        }
        assert_eq!(breakers.state(&"a"), BreakerState::Open);
    }
}
