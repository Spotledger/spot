//! The metric names a reconciliation cycle exports, through `spot-telemetry`'s Prometheus
//! recorder.
//!
//! Every label is bounded: `node` is a source id (`<site>#<partition>`, ADR-056), `scope` is a
//! scope key's value, and `kind` is one of `missing`, `extra`, `version` or `total`. No label
//! carries a record name, a payload or a secret.
//!
//! The names are served by [`spot_telemetry::render_prometheus`] like every other platform
//! metric. `spot-telemetry` installs the recorder; this crate only emits.

use std::time::Duration;

use crate::lag::Lag;
use crate::reconcile::ReconcileReport;

/// Counter: findings, labelled `node`, `scope` and `kind`.
pub const FINDINGS_TOTAL: &str = "spot_sync_reconcile_findings_total";
/// Gauge: the lag in stream entries, labelled `node` and `scope`.
pub const LAG_SEQ: &str = "spot_sync_reconcile_lag_seq";
/// Gauge: the age of the oldest unapplied record in seconds, labelled `node` and `scope`.
pub const OLDEST_UNAPPLIED_AGE_SECONDS: &str = "spot_sync_reconcile_oldest_unapplied_age_seconds";
/// Gauge: the bisection rounds the last reconciliation took, labelled `node` and `scope`.
pub const ROUNDS: &str = "spot_sync_reconcile_rounds";
/// Counter: the digest bytes the last reconciliation compared, labelled `node` and `scope`.
pub const DIGEST_BYTES_TOTAL: &str = "spot_sync_reconcile_digest_bytes_total";
/// Counter: reconciliation cycles run, labelled `node`, `scope` and `outcome`.
pub const CYCLES_TOTAL: &str = "spot_sync_reconcile_cycles_total";

/// Records one finding.
pub fn record_finding(node: &str, scope: &str, kind: &str) {
    metrics::counter!(
        FINDINGS_TOTAL,
        "node" => node.to_owned(),
        "scope" => scope.to_owned(),
        "kind" => kind.to_owned()
    )
    .increment(1);
}

/// Records the findings and the bisection cost of one report.
pub fn record_report(node: &str, report: &ReconcileReport) {
    let scope = report.scope.label();
    for finding in &report.findings {
        record_finding(node, &scope, finding.kind());
    }
    metrics::gauge!(
        ROUNDS,
        "node" => node.to_owned(),
        "scope" => scope.clone()
    )
    .set(report.rounds as f64);
    metrics::counter!(
        DIGEST_BYTES_TOTAL,
        "node" => node.to_owned(),
        "scope" => scope
    )
    .increment(report.digest_bytes as u64);
}

/// Records one node's lag: the entry gap and the age of the oldest unapplied record.
pub fn record_lag(node: &str, scope: &str, lag: &Lag) {
    metrics::gauge!(
        LAG_SEQ,
        "node" => node.to_owned(),
        "scope" => scope.to_owned()
    )
    .set(lag.lag as f64);
    if let Some(age) = lag.oldest_unapplied_age {
        metrics::gauge!(
            OLDEST_UNAPPLIED_AGE_SECONDS,
            "node" => node.to_owned(),
            "scope" => scope.to_owned()
        )
        .set(age.as_secs_f64());
    }
}

/// Records that one reconciliation cycle finished, with `outcome` (`clean` or `drift`).
pub fn record_cycle(node: &str, scope: &str, outcome: &str) {
    metrics::counter!(
        CYCLES_TOTAL,
        "node" => node.to_owned(),
        "scope" => scope.to_owned(),
        "outcome" => outcome.to_owned()
    )
    .increment(1);
}

/// The age of an unapplied record in whole seconds, for a caller that wants the number without
/// the metric facade.
pub fn age_seconds(age: Option<Duration>) -> u64 {
    age.map_or(0, |age| age.as_secs())
}
