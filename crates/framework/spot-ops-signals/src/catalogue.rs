//! The list of every series: the one place the documentation and the tests read.

use crate::{conditions, journal, migration, sync};

/// What kind of series it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Goes up only.
    Counter,
    /// A value that is set.
    Gauge,
    /// A distribution of observations.
    Histogram,
}

/// The unit of a series' value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    /// Seconds.
    Seconds,
    /// A count of things (no unit suffix in the name).
    Count,
}

/// One label of a series and every value it can take.
#[derive(Debug, Clone, Copy)]
pub struct LabelSpec {
    /// The label name.
    pub name: &'static str,
    /// Every value the typed helper can record.
    pub values: &'static [&'static str],
}

/// One series.
#[derive(Debug, Clone, Copy)]
pub struct SeriesSpec {
    /// The Prometheus name.
    pub name: &'static str,
    /// Counter, gauge or histogram.
    pub kind: Kind,
    /// The unit of the value.
    pub unit: Unit,
    /// The labels (empty for a series with none).
    pub labels: &'static [LabelSpec],
    /// What it means, one sentence.
    pub help: &'static str,
}

impl SeriesSpec {
    /// The largest number of label combinations (so of Prometheus series) this series can have.
    #[must_use]
    pub fn max_series(&self) -> usize {
        self.labels.iter().map(|l| l.values.len()).product()
    }
}

// The enums' values as static slices, written out so they are visible in one place.
const OUTCOME_BATCH: &[&str] = &["ok", "retry", "failed"];
const DECISION: &[&str] = &["acked", "degraded", "halted", "unknown_outcome"];
const AUDIT_RESULT: &[&str] = &["clean", "mismatch", "failed", "unknown_topics"];
const APPLY: &[&str] = &["applied", "duplicate", "parked", "quarantined", "refused"];
const LAG_STAT: &[&str] = &["p50", "p95", "max"];
const NODE_STATE: &[&str] = &["healthy", "lagging", "silent", "suspended", "too_old"];
const OBLIGATION: &[&str] = &["due", "overdue", "red"];
const SEVERITY: &[&str] = &["info", "amber", "red"];
const VERB: &[&str] = &[
    "plan", "approve", "rehearse", "apply", "hold", "release", "rescue",
];
const VERB_OUTCOME: &[&str] = &["ok", "failed", "refused"];

const fn label(name: &'static str, values: &'static [&'static str]) -> LabelSpec {
    LabelSpec { name, values }
}

const fn spec(
    name: &'static str,
    kind: Kind,
    unit: Unit,
    labels: &'static [LabelSpec],
    help: &'static str,
) -> SeriesSpec {
    SeriesSpec {
        name,
        kind,
        unit,
        labels,
        help,
    }
}

/// Every series, in the order the documentation lists them.
pub const SERIES: &[SeriesSpec] = &[
    spec(
        journal::SHIP_LAG_SECONDS,
        Kind::Gauge,
        Unit::Seconds,
        &[],
        "Age of the oldest outbox entry not yet durable in the journal; 0 when none",
    ),
    spec(
        journal::UNSHIPPED_ENTRIES,
        Kind::Gauge,
        Unit::Count,
        &[],
        "Outbox entries not yet durable in the journal",
    ),
    spec(
        journal::SHIP_BATCHES_TOTAL,
        Kind::Counter,
        Unit::Count,
        &[label("outcome", OUTCOME_BATCH)],
        "Journal ship batches by outcome",
    ),
    spec(
        journal::DURABILITY_WAIT_SECONDS,
        Kind::Histogram,
        Unit::Seconds,
        &[],
        "How long a commit waited for journal durability",
    ),
    spec(
        journal::DURABILITY_DECISIONS_TOTAL,
        Kind::Counter,
        Unit::Count,
        &[label("decision", DECISION)],
        "Durability decisions by decision",
    ),
    spec(
        journal::SNAPSHOT_AGE_SECONDS,
        Kind::Gauge,
        Unit::Seconds,
        &[],
        "Age of the newest journal snapshot",
    ),
    spec(
        journal::AUDIT_AGE_SECONDS,
        Kind::Gauge,
        Unit::Seconds,
        &[],
        "Time since the last clean journal audit",
    ),
    spec(
        journal::AUDIT_RUNS_TOTAL,
        Kind::Counter,
        Unit::Count,
        &[label("result", AUDIT_RESULT)],
        "Journal audit runs by result",
    ),
    spec(
        sync::APPLY_TOTAL,
        Kind::Counter,
        Unit::Count,
        &[label("outcome", APPLY)],
        "Sync records by apply outcome",
    ),
    spec(
        sync::SEND_QUEUE_DEPTH,
        Kind::Gauge,
        Unit::Count,
        &[],
        "Records queued to send across all nodes (hub)",
    ),
    spec(
        sync::NODE_LAG_SECONDS,
        Kind::Gauge,
        Unit::Seconds,
        &[label("stat", LAG_STAT)],
        "Sync lag over nodes",
    ),
    spec(
        sync::NODES,
        Kind::Gauge,
        Unit::Count,
        &[label("state", NODE_STATE)],
        "Sync nodes by state",
    ),
    spec(
        sync::OBLIGATIONS,
        Kind::Gauge,
        Unit::Count,
        &[label("level", OBLIGATION)],
        "Sync obligations by level",
    ),
    spec(
        sync::HOLDS,
        Kind::Gauge,
        Unit::Count,
        &[],
        "Records held for version skew",
    ),
    spec(
        crate::quarantine::OPEN,
        Kind::Gauge,
        Unit::Count,
        &[],
        "Open quarantine items",
    ),
    spec(
        crate::quarantine::OLDEST_AGE_SECONDS,
        Kind::Gauge,
        Unit::Seconds,
        &[],
        "Age of the oldest open quarantine item",
    ),
    spec(
        conditions::OPEN,
        Kind::Gauge,
        Unit::Count,
        &[label("severity", SEVERITY)],
        "Open conditions by severity",
    ),
    spec(
        migration::GATE_CLOSED,
        Kind::Gauge,
        Unit::Count,
        &[],
        "1 while the maintenance gate is closed, else 0",
    ),
    spec(
        migration::VERBS_TOTAL,
        Kind::Counter,
        Unit::Count,
        &[label("verb", VERB), label("outcome", VERB_OUTCOME)],
        "Migration verbs by verb and outcome",
    ),
];

/// Registers every series' description and unit with the facade.
pub(crate) fn describe() {
    for series in SERIES {
        let unit = match series.unit {
            Unit::Seconds => metrics::Unit::Seconds,
            Unit::Count => metrics::Unit::Count,
        };
        match series.kind {
            Kind::Counter => metrics::describe_counter!(series.name, unit, series.help),
            Kind::Gauge => metrics::describe_gauge!(series.name, unit, series.help),
            Kind::Histogram => metrics::describe_histogram!(series.name, unit, series.help),
        }
    }
}
