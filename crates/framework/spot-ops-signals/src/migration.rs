//! Migration series: the maintenance gate and the verbs.

/// Gauge: 1 while the gate is closed.
pub const GATE_CLOSED: &str = "spot_migration_gate_closed";
/// Counter: verbs by `verb` and `outcome`.
pub const VERBS_TOTAL: &str = "spot_migration_verbs_total";

label_enum! {
    /// A migration verb.
    Verb {
        /// Plan.
        Plan => "plan",
        /// Approve.
        Approve => "approve",
        /// Rehearse.
        Rehearse => "rehearse",
        /// Apply.
        Apply => "apply",
        /// Hold.
        Hold => "hold",
        /// Release.
        Release => "release",
        /// Rescue.
        Rescue => "rescue",
    }
}

label_enum! {
    /// How a verb ended.
    VerbOutcome {
        /// Done.
        Ok => "ok",
        /// Tried and failed.
        Failed => "failed",
        /// Refused before it started.
        Refused => "refused",
    }
}

/// Sets the gate gauge: 1 closed, 0 open.
pub fn set_gate_closed(closed: bool) {
    metrics::gauge!(GATE_CLOSED).set(if closed { 1.0 } else { 0.0 });
}

/// Counts one verb.
pub fn record_verb(verb: Verb, outcome: VerbOutcome) {
    metrics::counter!(VERBS_TOTAL, "verb" => verb.as_str(), "outcome" => outcome.as_str())
        .increment(1);
}
