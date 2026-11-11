//! The interactive-load signal and the AIMD admission controller (WP-AI-05.T1).
//!
//! Human traffic comes first. A [`LoadSignal`] reports the interactive load — queue depth,
//! connection-pool wait and the p95 latency of interactive requests, taken from
//! `spot-telemetry` in a real deployment — and [`AimdController`] shrinks AI admission when
//! that load crosses the high thresholds, down to zero, and recovers it gradually (additive
//! increase, multiplicative decrease) when the load is calm. Everything is integer
//! arithmetic; the clock is an argument.

use std::time::Duration;

use crate::error::{AiError, Result};

/// One reading of interactive load.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoadSample {
    /// How many interactive requests are waiting for a connection.
    pub queue_depth: u32,
    /// How long the oldest interactive request has waited for a connection.
    pub pool_wait: Duration,
    /// The p95 latency of recent interactive requests.
    pub p95: Duration,
}

impl LoadSample {
    /// A calm reading: nothing waiting, no latency.
    #[must_use]
    pub fn calm() -> Self {
        Self::default()
    }
}

/// A source of interactive-load readings, sampled at admission time.
pub trait LoadSignal: Send + Sync {
    /// The load at `now`.
    fn sample(&self, now: Duration) -> LoadSample;
}

/// A signal that always reports the same sample (a test or a node with no telemetry yet).
#[derive(Clone, Copy, Debug, Default)]
pub struct StaticLoad {
    sample: LoadSample,
}

impl StaticLoad {
    /// A signal returning `sample` every time.
    #[must_use]
    pub fn new(sample: LoadSample) -> Self {
        Self { sample }
    }

    /// A signal that is always calm.
    #[must_use]
    pub fn calm() -> Self {
        Self::default()
    }
}

impl LoadSignal for StaticLoad {
    fn sample(&self, _now: Duration) -> LoadSample {
        self.sample
    }
}

/// How loaded the system looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadLevel {
    /// Below every low threshold: AI admission may grow.
    Calm,
    /// Between the thresholds: AI admission holds.
    Busy,
    /// At or above any high threshold: AI admission shrinks.
    Overloaded,
}

/// The thresholds that map a [`LoadSample`] to a [`LoadLevel`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadThresholds {
    /// At or above this queue depth the system is overloaded.
    pub high_queue_depth: u32,
    /// At or above this pool wait the system is overloaded.
    pub high_pool_wait: Duration,
    /// At or above this p95 the system is overloaded.
    pub high_p95: Duration,
    /// At or below this queue depth (with the other lows) the system is calm.
    pub low_queue_depth: u32,
    /// At or below this pool wait (with the other lows) the system is calm.
    pub low_pool_wait: Duration,
    /// At or below this p95 (with the other lows) the system is calm.
    pub low_p95: Duration,
}

impl Default for LoadThresholds {
    fn default() -> Self {
        Self {
            high_queue_depth: 8,
            high_pool_wait: Duration::from_millis(50),
            high_p95: Duration::from_millis(250),
            low_queue_depth: 1,
            low_pool_wait: Duration::from_millis(10),
            low_p95: Duration::from_millis(100),
        }
    }
}

impl LoadThresholds {
    /// The defaults.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The level of one sample.
    #[must_use]
    pub fn level(&self, sample: &LoadSample) -> LoadLevel {
        if sample.queue_depth >= self.high_queue_depth
            || sample.pool_wait >= self.high_pool_wait
            || sample.p95 >= self.high_p95
        {
            return LoadLevel::Overloaded;
        }
        if sample.queue_depth <= self.low_queue_depth
            && sample.pool_wait <= self.low_pool_wait
            && sample.p95 <= self.low_p95
        {
            return LoadLevel::Calm;
        }
        LoadLevel::Busy
    }
}

/// Additive-increase, multiplicative-decrease admission control.
///
/// On an overloaded sample the limit is multiplied by `decrease_num / decrease_den` (so it can
/// reach zero); on a calm sample it grows by `increase`, up to `max`. A busy sample changes
/// nothing, so the limit does not oscillate on a single noisy reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AimdController {
    max: u32,
    current: u32,
    decrease_num: u32,
    decrease_den: u32,
    increase: u32,
}

impl AimdController {
    /// A controller at `max`, halving on overload and growing by one on calm.
    ///
    /// # Errors
    ///
    /// [`crate::error::INVALID_ARGUMENT`] when `max` is zero.
    pub fn new(max: u32) -> Result<Self> {
        Self::with_params(max, 1, 2, 1)
    }

    /// A controller with explicit decrease ratio and additive increase.
    ///
    /// # Errors
    ///
    /// [`crate::error::INVALID_ARGUMENT`] when `max` is zero or the ratio is not a proper
    /// fraction (`decrease_den` must be non-zero and greater than `decrease_num`).
    pub fn with_params(
        max: u32,
        decrease_num: u32,
        decrease_den: u32,
        increase: u32,
    ) -> Result<Self> {
        if max == 0 {
            return Err(AiError::invalid_argument(
                "the AIMD maximum must be at least 1",
            ));
        }
        if decrease_den == 0 || decrease_num >= decrease_den {
            return Err(AiError::invalid_argument(
                "the AIMD decrease ratio must be a proper fraction",
            ));
        }
        Ok(Self {
            max,
            current: max,
            decrease_num,
            decrease_den,
            increase,
        })
    }

    /// Applies one load reading.
    pub fn on_sample(&mut self, level: LoadLevel) {
        match level {
            LoadLevel::Overloaded => {
                let scaled = u64::from(self.current) * u64::from(self.decrease_num)
                    / u64::from(self.decrease_den);
                self.current = u32::try_from(scaled).unwrap_or(0);
            }
            LoadLevel::Calm => {
                self.current = self.current.saturating_add(self.increase).min(self.max);
            }
            LoadLevel::Busy => {}
        }
    }

    /// The current admission limit.
    #[must_use]
    pub fn limit(&self) -> u32 {
        self.current
    }

    /// The limit's ceiling.
    #[must_use]
    pub fn max(&self) -> u32 {
        self.max
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn overloaded_shrinks_to_zero_and_calm_recovers_gradually() {
        let mut aimd = AimdController::new(4).expect("a positive maximum");
        aimd.on_sample(LoadLevel::Overloaded);
        assert_eq!(aimd.limit(), 2);
        aimd.on_sample(LoadLevel::Overloaded);
        assert_eq!(aimd.limit(), 1);
        aimd.on_sample(LoadLevel::Overloaded);
        assert_eq!(aimd.limit(), 0, "AI admission shrinks down to zero");
        for _ in 0..10 {
            aimd.on_sample(LoadLevel::Calm);
        }
        assert_eq!(aimd.limit(), 4, "and recovers, one step at a time");
    }

    #[test]
    fn a_busy_reading_changes_nothing() {
        let mut aimd = AimdController::new(8).expect("a positive maximum");
        aimd.on_sample(LoadLevel::Busy);
        assert_eq!(aimd.limit(), 8);
    }

    #[test]
    fn thresholds_classify_readings() {
        let thresholds = LoadThresholds::default();
        assert_eq!(thresholds.level(&LoadSample::calm()), LoadLevel::Calm);
        let busy = LoadSample {
            queue_depth: 3,
            ..LoadSample::calm()
        };
        assert_eq!(thresholds.level(&busy), LoadLevel::Busy);
        let hot = LoadSample {
            queue_depth: 100,
            ..LoadSample::calm()
        };
        assert_eq!(thresholds.level(&hot), LoadLevel::Overloaded);
        let slow = LoadSample {
            p95: Duration::from_secs(1),
            ..LoadSample::calm()
        };
        assert_eq!(thresholds.level(&slow), LoadLevel::Overloaded);
    }
}
