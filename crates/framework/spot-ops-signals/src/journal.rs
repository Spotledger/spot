//! Journal series: shipping lag, durability decisions, snapshot and audit age.

use std::time::Duration;

use crate::count_value;

/// Gauge: age of the oldest outbox entry not yet durable in the journal; 0 when none.
pub const SHIP_LAG_SECONDS: &str = "spot_journal_ship_lag_seconds";
/// Gauge: outbox entries not yet durable.
pub const UNSHIPPED_ENTRIES: &str = "spot_journal_unshipped_entries";
/// Counter: ship batches, by `outcome`.
pub const SHIP_BATCHES_TOTAL: &str = "spot_journal_ship_batches_total";
/// Histogram: how long a commit waited for durability.
pub const DURABILITY_WAIT_SECONDS: &str = "spot_journal_durability_wait_seconds";
/// Counter: durability decisions, by `decision`.
pub const DURABILITY_DECISIONS_TOTAL: &str = "spot_journal_durability_decisions_total";
/// Gauge: age of the newest snapshot.
pub const SNAPSHOT_AGE_SECONDS: &str = "spot_journal_snapshot_age_seconds";
/// Gauge: time since the last clean audit.
pub const AUDIT_AGE_SECONDS: &str = "spot_journal_audit_age_seconds";
/// Counter: audit runs, by `result`.
pub const AUDIT_RUNS_TOTAL: &str = "spot_journal_audit_runs_total";

label_enum! {
    /// How a ship batch ended.
    BatchOutcome {
        /// Shipped and acknowledged.
        Ok => "ok",
        /// Failed this time; will be tried again.
        Retry => "retry",
        /// Failed for good.
        Failed => "failed",
    }
}

label_enum! {
    /// What the durability gate decided for a commit.
    Decision {
        /// The journal acknowledged in time.
        Acked => "acked",
        /// Committed without the acknowledgement, in degraded mode.
        Degraded => "degraded",
        /// Refused because durability could not be had.
        Halted => "halted",
        /// The outcome could not be learned.
        UnknownOutcome => "unknown_outcome",
    }
}

label_enum! {
    /// How an audit run ended.
    AuditResult {
        /// Everything matched.
        Clean => "clean",
        /// The journal and the database disagree.
        Mismatch => "mismatch",
        /// The run itself failed.
        Failed => "failed",
        /// Topics the auditor does not know were found.
        UnknownTopics => "unknown_topics",
    }
}

/// Sets the age of the oldest unshipped outbox entry (zero when none).
pub fn set_ship_lag(lag: Duration) {
    metrics::gauge!(SHIP_LAG_SECONDS).set(lag.as_secs_f64());
}

/// Sets the number of outbox entries not yet durable.
pub fn set_unshipped_entries(count: u64) {
    metrics::gauge!(UNSHIPPED_ENTRIES).set(count_value(count));
}

/// Counts one ship batch.
pub fn record_ship_batch(outcome: BatchOutcome) {
    metrics::counter!(SHIP_BATCHES_TOTAL, "outcome" => outcome.as_str()).increment(1);
}

/// Records how long one commit waited for durability.
pub fn record_durability_wait(wait: Duration) {
    metrics::histogram!(DURABILITY_WAIT_SECONDS).record(wait.as_secs_f64());
}

/// Counts one durability decision.
pub fn record_durability_decision(decision: Decision) {
    metrics::counter!(DURABILITY_DECISIONS_TOTAL, "decision" => decision.as_str()).increment(1);
}

/// Sets the age of the newest snapshot.
pub fn set_snapshot_age(age: Duration) {
    metrics::gauge!(SNAPSHOT_AGE_SECONDS).set(age.as_secs_f64());
}

/// Sets the time since the last clean audit.
pub fn set_audit_age(age: Duration) {
    metrics::gauge!(AUDIT_AGE_SECONDS).set(age.as_secs_f64());
}

/// Counts one audit run.
pub fn record_audit_run(result: AuditResult) {
    metrics::counter!(AUDIT_RUNS_TOTAL, "result" => result.as_str()).increment(1);
}
