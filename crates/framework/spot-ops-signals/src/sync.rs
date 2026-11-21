//! Sync series: apply outcomes, send queue, node lag and states, obligations, holds.

use std::time::Duration;

use crate::count_value;

/// Counter: records by apply `outcome`.
pub const APPLY_TOTAL: &str = "spot_sync_apply_total";
/// Gauge: records queued to send across all nodes (hub).
pub const SEND_QUEUE_DEPTH: &str = "spot_sync_send_queue_depth";
/// Gauge: lag over nodes, by `stat`.
pub const NODE_LAG_SECONDS: &str = "spot_sync_node_lag_seconds";
/// Gauge: nodes by `state`.
pub const NODES: &str = "spot_sync_nodes";
/// Gauge: obligations by `level`.
pub const OBLIGATIONS: &str = "spot_sync_obligations";
/// Gauge: records held for version skew.
pub const HOLDS: &str = "spot_sync_holds";

label_enum! {
    /// What became of one record offered to the apply step.
    ApplyOutcome {
        /// Applied.
        Applied => "applied",
        /// Already applied before.
        Duplicate => "duplicate",
        /// Parked until a dependency arrives.
        Parked => "parked",
        /// Moved to quarantine.
        Quarantined => "quarantined",
        /// Refused.
        Refused => "refused",
    }
}

label_enum! {
    /// The statistic of the lag over nodes.
    LagStat {
        /// Median.
        P50 => "p50",
        /// 95th percentile.
        P95 => "p95",
        /// Worst node.
        Max => "max",
    }
}

label_enum! {
    /// The state of a sync node.
    NodeState {
        /// Syncing on time.
        Healthy => "healthy",
        /// Behind but moving.
        Lagging => "lagging",
        /// Not heard from.
        Silent => "silent",
        /// Suspended by an operator or the hub.
        Suspended => "suspended",
        /// Runs a version the hub no longer accepts.
        TooOld => "too_old",
    }
}

label_enum! {
    /// How late an obligation is.
    ObligationLevel {
        /// Due.
        Due => "due",
        /// Overdue.
        Overdue => "overdue",
        /// Overdue past the red line.
        Red => "red",
    }
}

/// Nodes per state; every state is set every time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeCounts {
    /// Syncing on time.
    pub healthy: u64,
    /// Behind but moving.
    pub lagging: u64,
    /// Not heard from.
    pub silent: u64,
    /// Suspended.
    pub suspended: u64,
    /// Too old.
    pub too_old: u64,
}

impl NodeCounts {
    /// The count of one state.
    #[must_use]
    pub fn of(&self, state: NodeState) -> u64 {
        match state {
            NodeState::Healthy => self.healthy,
            NodeState::Lagging => self.lagging,
            NodeState::Silent => self.silent,
            NodeState::Suspended => self.suspended,
            NodeState::TooOld => self.too_old,
        }
    }
}

/// Obligations per level; every level is set every time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObligationCounts {
    /// Due.
    pub due: u64,
    /// Overdue.
    pub overdue: u64,
    /// Red.
    pub red: u64,
}

impl ObligationCounts {
    /// The count of one level.
    #[must_use]
    pub fn of(&self, level: ObligationLevel) -> u64 {
        match level {
            ObligationLevel::Due => self.due,
            ObligationLevel::Overdue => self.overdue,
            ObligationLevel::Red => self.red,
        }
    }
}

/// The lag over nodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NodeLag {
    /// Median.
    pub p50: Duration,
    /// 95th percentile.
    pub p95: Duration,
    /// Worst node.
    pub max: Duration,
}

impl NodeLag {
    /// One statistic.
    #[must_use]
    pub fn of(&self, stat: LagStat) -> Duration {
        match stat {
            LagStat::P50 => self.p50,
            LagStat::P95 => self.p95,
            LagStat::Max => self.max,
        }
    }
}

/// Counts one record's apply outcome.
pub fn record_apply(outcome: ApplyOutcome) {
    metrics::counter!(APPLY_TOTAL, "outcome" => outcome.as_str()).increment(1);
}

/// Sets the records queued to send across all nodes.
pub fn set_send_queue_depth(depth: u64) {
    metrics::gauge!(SEND_QUEUE_DEPTH).set(count_value(depth));
}

/// Sets the lag over nodes (all three statistics).
pub fn set_node_lag(lag: &NodeLag) {
    for stat in LagStat::ALL {
        metrics::gauge!(NODE_LAG_SECONDS, "stat" => stat.as_str()).set(lag.of(*stat).as_secs_f64());
    }
}

/// Sets the nodes per state (all five states, zeros included).
pub fn set_nodes(counts: &NodeCounts) {
    for state in NodeState::ALL {
        metrics::gauge!(NODES, "state" => state.as_str()).set(count_value(counts.of(*state)));
    }
}

/// Sets the obligations per level (all three levels, zeros included).
pub fn set_obligations(counts: &ObligationCounts) {
    for level in ObligationLevel::ALL {
        metrics::gauge!(OBLIGATIONS, "level" => level.as_str()).set(count_value(counts.of(*level)));
    }
}

/// Sets the records held for version skew.
pub fn set_holds(holds: u64) {
    metrics::gauge!(HOLDS).set(count_value(holds));
}
