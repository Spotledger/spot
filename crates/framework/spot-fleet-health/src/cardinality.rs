//! Cardinality control per ADR-076 part 3: aggregate by subject, never by arbitrary labels.
//!
//! A [`SeriesKey`] has exactly two parts: the tenant or node and the signal. There is no
//! free-form label map anywhere in this crate, so a caller cannot accidentally create one
//! series per user, request or path. [`SeriesBudget`] caps the number of distinct series and
//! folds every further series into **one counted overflow bucket**, which is what an exporter
//! should publish as `telemetry.series_dropped` rather than letting the metrics store grow
//! without bound.
//!
//! Health evaluation still sees every observation: the budget governs the exported series,
//! not whether a tenant's failure is visible.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::signal::{Observation, Signal, Subject};

/// The identity of one exported series: a subject and a signal, and nothing else.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SeriesKey {
    /// The tenant or node the series belongs to.
    pub subject: Subject,
    /// The signal the series carries.
    pub signal: Signal,
}

impl SeriesKey {
    /// The series an observation belongs to.
    #[must_use]
    pub fn of(observation: &Observation) -> Self {
        Self {
            subject: observation.subject.clone(),
            signal: observation.signal,
        }
    }
}

/// What a series budget did with an observation's series.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    /// The series was already admitted.
    Existing,
    /// The series was admitted for the first time.
    New,
    /// The budget is full; the series was folded into the overflow bucket.
    Overflow,
}

/// A cap on distinct exported series, with one counted overflow bucket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeriesBudget {
    max_series: usize,
    admitted: BTreeSet<SeriesKey>,
    folded: u64,
}

impl SeriesBudget {
    /// A budget for `max_series` distinct series. A budget of zero admits none and folds every
    /// observation.
    #[must_use]
    pub fn new(max_series: usize) -> Self {
        Self {
            max_series,
            admitted: BTreeSet::new(),
            folded: 0,
        }
    }

    /// Classifies one series against the budget.
    pub fn admit(&mut self, key: SeriesKey) -> Admission {
        if self.admitted.contains(&key) {
            return Admission::Existing;
        }
        if self.admitted.len() < self.max_series {
            self.admitted.insert(key);
            return Admission::New;
        }
        self.folded = self.folded.saturating_add(1);
        Admission::Overflow
    }

    /// The most distinct series this budget admits.
    #[must_use]
    pub const fn max_series(&self) -> usize {
        self.max_series
    }

    /// How many distinct series are admitted.
    #[must_use]
    pub fn distinct_series(&self) -> usize {
        self.admitted.len()
    }

    /// How many observations were folded into the overflow bucket.
    #[must_use]
    pub const fn folded(&self) -> u64 {
        self.folded
    }

    /// How many overflow buckets exist: one if anything was folded, otherwise zero.
    ///
    /// The bucket itself is a single aggregate; this is the number an exporter publishes, not
    /// the number of series that were dropped.
    #[must_use]
    pub const fn overflow_buckets(&self) -> usize {
        if self.folded == 0 { 0 } else { 1 }
    }

    /// The admitted series, in stable order.
    #[must_use]
    pub fn series(&self) -> &BTreeSet<SeriesKey> {
        &self.admitted
    }
}

/// Tenant and node telemetry on its way to the evaluator and the exporter.
///
/// Every observation is retained for health evaluation; the [`SeriesBudget`] only decides how
/// many distinct series the exporter may publish.
#[derive(Clone, Debug)]
pub struct TelemetryIngest {
    observations: Vec<Observation>,
    budget: SeriesBudget,
}

impl TelemetryIngest {
    /// An ingest whose exported series are capped at `max_series`.
    #[must_use]
    pub fn new(max_series: usize) -> Self {
        Self {
            observations: Vec::new(),
            budget: SeriesBudget::new(max_series),
        }
    }

    /// Records one observation and classifies its series.
    pub fn push(&mut self, observation: Observation) -> Admission {
        let admission = self.budget.admit(SeriesKey::of(&observation));
        self.observations.push(observation);
        admission
    }

    /// Every observation recorded so far, including those whose series overflowed.
    #[must_use]
    pub fn observations(&self) -> &[Observation] {
        &self.observations
    }

    /// The series budget and its counters.
    #[must_use]
    pub fn budget(&self) -> &SeriesBudget {
        &self.budget
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::{NodeId, TenantId};
    use crate::time::Timestamp;

    fn observation(id: &str, signal: Signal) -> Observation {
        let subject = Subject::Tenant(TenantId::try_new(id).expect("tenant"));
        Observation::new(subject, signal, 1, Timestamp::from_secs(1)).expect("observation")
    }

    #[test]
    fn the_budget_admits_until_full_then_folds_into_one_counted_bucket() {
        let mut budget = SeriesBudget::new(2);
        assert_eq!(
            budget.admit(SeriesKey::of(&observation("a", Signal::HealthProbe))),
            Admission::New
        );
        assert_eq!(
            budget.admit(SeriesKey::of(&observation("b", Signal::HealthProbe))),
            Admission::New
        );
        assert_eq!(
            budget.admit(SeriesKey::of(&observation("c", Signal::HealthProbe))),
            Admission::Overflow
        );
        assert_eq!(
            budget.admit(SeriesKey::of(&observation("d", Signal::ErrorRate))),
            Admission::Overflow
        );
        assert_eq!(budget.distinct_series(), 2);
        assert_eq!(budget.folded(), 2);
        assert_eq!(budget.overflow_buckets(), 1);
    }

    #[test]
    fn a_repeated_series_is_existing_not_a_new_series() {
        let mut budget = SeriesBudget::new(1);
        let first = observation("a", Signal::QueueLag);
        assert_eq!(budget.admit(SeriesKey::of(&first)), Admission::New);
        assert_eq!(budget.admit(SeriesKey::of(&first)), Admission::Existing);
        assert_eq!(budget.distinct_series(), 1);
        assert_eq!(budget.folded(), 0);
    }

    #[test]
    fn an_overflowing_observation_is_still_visible_to_health() {
        let mut ingest = TelemetryIngest::new(1);
        let _ = ingest.push(observation("a", Signal::HealthProbe));
        assert_eq!(
            ingest.push(observation("b", Signal::HealthProbe)),
            Admission::Overflow
        );
        assert_eq!(ingest.observations().len(), 2);
        assert_eq!(ingest.budget().folded(), 1);
        let node = Subject::Node(NodeId::try_new("ln-1").expect("node"));
        let key = SeriesKey {
            subject: node,
            signal: Signal::ReplicationLag,
        };
        assert_eq!(key.signal, Signal::ReplicationLag);
    }
}
