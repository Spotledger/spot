//! Per-step statistics: counts, exact percentiles, waiting versus work time and the rates.
//!
//! One [`StepStats`] is one `(process version, node)` pair. It is built by the
//! [`Accumulator`](crate::Accumulator) and never carries a float: durations are
//! [`Millis`], percentiles are exact integers (the nearest-rank method) and rates are an exact
//! `part / whole` pair with a rounded per-mille rendering.

use serde::{Deserialize, Serialize};
use spot_bpm_engine::NodeKind;
use spot_dag::NodeId;

use crate::duration::Millis;
use crate::process::ProcessKey;

/// An exact rate: a part over a whole. Never a float (ADR-12).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    /// The count in the numerator (for example the retried attempts).
    pub part: u64,
    /// The count in the denominator (for example every attempt).
    pub whole: u64,
}

impl Rate {
    /// Builds a rate. A zero denominator is allowed and reads as zero.
    #[must_use]
    pub const fn new(part: u64, whole: u64) -> Self {
        Self { part, whole }
    }

    /// The rate in whole per-mille (`0..=1000`), rounded half up with integers only.
    #[must_use]
    pub fn per_mille(&self) -> u32 {
        if self.whole == 0 {
            return 0;
        }
        let numerator = u128::from(self.part) * 1000 + u128::from(self.whole) / 2;
        let value = numerator / u128::from(self.whole);
        u32::try_from(value).unwrap_or(1000)
    }

    /// Whether the rate is zero (no part, or no whole to divide by).
    #[must_use]
    pub const fn is_zero(&self) -> bool {
        self.part == 0
    }
}

impl core::fmt::Display for Rate {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}/{}", self.part, self.whole)
    }
}

/// The statistics of one step (one process version and node).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepStats {
    /// The process version.
    pub process: ProcessKey,
    /// The node.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The node's execution kind, as the last event reported it.
    pub kind: NodeKind,
    /// Every attempt that started.
    pub attempts: u64,
    /// Every attempt that finished (succeeded, failed terminally or was retried).
    pub completed: u64,
    /// Attempts that succeeded.
    pub done: u64,
    /// Attempts that failed terminally (the error branch was taken).
    pub failed: u64,
    /// Attempts that failed and were retried.
    pub retries: u64,
    /// The total cycle time of the completed attempts (`work + wait`).
    pub total: Millis,
    /// The work time of the completed attempts (running, not waiting).
    pub work: Millis,
    /// The waiting time of the completed attempts.
    pub wait: Millis,
    /// The shortest completed-attempt cycle time.
    pub min: Option<Millis>,
    /// The exact 50th percentile of the completed-attempt cycle times.
    pub p50: Option<Millis>,
    /// The exact 90th percentile.
    pub p90: Option<Millis>,
    /// The exact 99th percentile.
    pub p99: Option<Millis>,
    /// The longest completed-attempt cycle time.
    pub max: Option<Millis>,
    /// How many times an attempt at this step entered a wait.
    pub queue_entered: u64,
    /// How many times an attempt at this step left a wait.
    pub queue_left: u64,
    /// The net change in the queue (`entered - left`): the attempts still waiting at the end of
    /// the observed window. Positive means the queue grew.
    pub queue_growth: i64,
    /// The most attempts waiting at this step at any one time.
    pub max_waiting: u64,
}

impl StepStats {
    /// The retry rate: retried attempts over every attempt.
    #[must_use]
    pub fn retry_rate(&self) -> Rate {
        Rate::new(self.retries, self.attempts)
    }

    /// The failure rate: terminally failed attempts over every attempt.
    #[must_use]
    pub fn failure_rate(&self) -> Rate {
        Rate::new(self.failed, self.attempts)
    }

    /// The mean cycle time of the completed attempts, truncated to whole milliseconds.
    #[must_use]
    pub fn mean(&self) -> Option<Millis> {
        if self.completed == 0 {
            return None;
        }
        let completed = i64::try_from(self.completed).unwrap_or(i64::MAX);
        Some(Millis::from_millis(self.total.as_millis() / completed))
    }

    /// How much of the completed-attempt time was spent waiting, in whole per-mille.
    #[must_use]
    pub fn wait_per_mille(&self) -> u32 {
        let total = self.total.as_millis();
        if total <= 0 {
            return 0;
        }
        let waited = self.wait.as_millis().max(0);
        let value = (i128::from(waited) * 1000) / i128::from(total);
        u32::try_from(value).unwrap_or(0)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn stats() -> StepStats {
        StepStats {
            process: ProcessKey::new("order_flow", 1),
            node: NodeId::new("approve").unwrap(),
            kind: NodeKind::Decision,
            attempts: 4,
            completed: 4,
            done: 3,
            failed: 1,
            retries: 1,
            total: Millis::from_seconds(40),
            work: Millis::from_seconds(10),
            wait: Millis::from_seconds(30),
            min: Some(Millis::from_seconds(5)),
            p50: Some(Millis::from_seconds(10)),
            p90: Some(Millis::from_seconds(15)),
            p99: Some(Millis::from_seconds(15)),
            max: Some(Millis::from_seconds(15)),
            queue_entered: 4,
            queue_left: 3,
            queue_growth: 1,
            max_waiting: 2,
        }
    }

    #[test]
    fn rates_are_exact_and_rounded_per_mille() {
        let stats = stats();
        assert_eq!(stats.retry_rate(), Rate::new(1, 4));
        assert_eq!(stats.retry_rate().per_mille(), 250);
        assert_eq!(stats.failure_rate().per_mille(), 250);
        assert_eq!(stats.wait_per_mille(), 750);
        assert_eq!(stats.mean(), Some(Millis::from_seconds(10)));
    }

    #[test]
    fn a_zero_denominator_reads_as_zero() {
        assert_eq!(Rate::new(0, 0).per_mille(), 0);
        assert_eq!(Rate::new(5, 0).per_mille(), 0);
        assert!(Rate::new(0, 5).is_zero());
    }
}
