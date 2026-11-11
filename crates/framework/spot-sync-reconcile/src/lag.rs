//! The sync lag measure: how far behind a node is, and for how long.
//!
//! Lag is a property of a node's sync consumer against the stream it reads (ADR-053): the
//! producer publishes entries at increasing positions, and the consumer applies them and keeps
//! a bookmark. Two numbers say everything an operator needs:
//!
//! - `lag = newest produced seq - last applied seq`: how many entries are outstanding.
//! - the **age** of the oldest unapplied record: how long the node has been behind, which is
//!   what turns "behind" into "stale" (ADR-065).
//!
//! Both are measured **per node and per scope**; the caller groups the produced records it
//! holds and calls [`measure_lag`] once per group. Time is always an argument: this crate never
//! reads the wall clock.

use std::time::Duration;

use time::OffsetDateTime;

use crate::scope::Scope;

/// One produced stream entry a consumer has not necessarily applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProducedRecord {
    /// The entry's sequence number within its stream generation (ADR-053).
    pub seq: u64,
    /// When the producer wrote it.
    pub produced_at: OffsetDateTime,
}

impl ProducedRecord {
    /// A produced entry.
    pub fn new(seq: u64, produced_at: OffsetDateTime) -> Self {
        Self { seq, produced_at }
    }
}

/// One node's (and one scope's) view of a stream and its consumer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LagSample {
    /// The node, a source id (`<site>#<partition>`, ADR-056).
    pub node: String,
    /// The scope this sample is for, when the consumer tracks scopes separately.
    pub scope: Option<Scope>,
    /// The produced entries the consumer could have applied.
    pub produced: Vec<ProducedRecord>,
    /// The highest sequence the consumer has applied; `0` means none.
    pub last_applied: u64,
}

impl LagSample {
    /// A sample for `node` at `last_applied` with no produced entries yet.
    pub fn new(node: impl Into<String>, last_applied: u64) -> Self {
        Self {
            node: node.into(),
            scope: None,
            produced: Vec::new(),
            last_applied,
        }
    }

    /// The same sample, for one scope.
    pub fn for_scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    /// The same sample, with the produced entries.
    pub fn with_produced(mut self, produced: Vec<ProducedRecord>) -> Self {
        self.produced = produced;
        self
    }
}

/// A node's lag at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Lag {
    /// The node, repeated for the caller's convenience when building metrics.
    pub newest_produced: u64,
    /// `newest produced seq - last applied seq`, never negative.
    pub lag: u64,
    /// The age of the oldest entry the consumer has not applied, when any is outstanding.
    pub oldest_unapplied_age: Option<Duration>,
}

impl Lag {
    /// True when the consumer has applied everything produced.
    pub fn is_current(&self) -> bool {
        self.lag == 0
    }
}

/// Measures one node's lag from its produced entries and last applied sequence.
///
/// `now` is the instant the measurement is taken; the age of the oldest unapplied record is
/// `now - its produced_at`, or zero when the clock is behind it.
pub fn measure_lag(sample: &LagSample, now: OffsetDateTime) -> Lag {
    let newest_produced = sample
        .produced
        .iter()
        .map(|record| record.seq)
        .max()
        .unwrap_or(0);
    let lag = newest_produced.saturating_sub(sample.last_applied);
    let oldest_unapplied = sample
        .produced
        .iter()
        .filter(|record| record.seq > sample.last_applied)
        .map(|record| record.produced_at)
        .min();
    let oldest_unapplied_age = oldest_unapplied.map(|produced_at| {
        std::time::Duration::try_from(now - produced_at).unwrap_or(Duration::ZERO)
    });
    Lag {
        newest_produced,
        lag,
        oldest_unapplied_age,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    fn at(seconds: i64) -> OffsetDateTime {
        OffsetDateTime::from_unix_timestamp(1_767_225_600)
            .unwrap()
            .saturating_add(time::Duration::seconds(seconds))
    }

    #[test]
    fn lag_is_the_gap_between_produced_and_applied() {
        let sample = LagSample::new("hub", 3).with_produced(vec![
            ProducedRecord::new(3, at(0)),
            ProducedRecord::new(4, at(10)),
            ProducedRecord::new(5, at(20)),
        ]);
        let lag = measure_lag(&sample, at(30));
        assert_eq!(lag.newest_produced, 5);
        assert_eq!(lag.lag, 2);
        // The oldest unapplied is seq 4, produced 20 seconds before `now`.
        assert_eq!(lag.oldest_unapplied_age, Some(Duration::from_secs(20)));
    }

    #[test]
    fn a_current_consumer_has_no_lag_and_no_age() {
        let sample = LagSample::new("hub", 5).with_produced(vec![ProducedRecord::new(5, at(0))]);
        let lag = measure_lag(&sample, at(30));
        assert_eq!(lag.lag, 0);
        assert_eq!(lag.oldest_unapplied_age, None);
        assert!(lag.is_current());
    }

    #[test]
    fn a_clock_behind_the_producer_gives_zero_age_not_a_negative() {
        let sample = LagSample::new("hub", 0).with_produced(vec![ProducedRecord::new(1, at(100))]);
        let lag = measure_lag(&sample, at(10));
        assert_eq!(lag.lag, 1);
        assert_eq!(lag.oldest_unapplied_age, Some(Duration::ZERO));
    }
}
