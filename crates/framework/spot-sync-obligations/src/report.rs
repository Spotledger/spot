//! The auditor's report: did this change reach every node, and when?

use serde::{Deserialize, Serialize};

use crate::model::MandatoryEvent;

/// One recorded prefix crossing at a node (from its `sys_obligation_satisfied` events).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Crossing {
    /// The applied prefix after the crossing.
    pub through_position: u64,
    /// When the node reported applying, milliseconds.
    pub applied_at_millis: i64,
}

/// What the hub knows about one node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// The node id.
    pub node: String,
    /// The node's latest applied prefix.
    pub applied_prefix: u64,
    /// Positions of mandatory records the node holds back.
    pub holds: Vec<u64>,
    /// When the node was last seen, milliseconds.
    pub last_seen_millis: i64,
    /// Its recorded crossings, any order.
    pub crossings: Vec<Crossing>,
}

/// The answer for one node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportStatus {
    /// Every listed event is applied; the time of the first crossing that covered all of them.
    AppliedAt(i64),
    /// Applied by prefix and not held, but no crossing was recorded, so the time is unknown.
    AppliedUnrecorded,
    /// At least one event is outstanding, the oldest published at `since_millis`.
    OutstandingSince {
        /// Publication time of the oldest outstanding event.
        since_millis: i64,
        /// When the node was last seen.
        last_seen_millis: i64,
    },
}

/// One row of the report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportRow {
    /// The node id.
    pub node: String,
    /// Its status for the listed events.
    pub status: ReportStatus,
}

/// Report, per node, whether all of `events` (for example every version of a tax rate) were
/// applied and when. Rows are in node-id order; no events gives no rows.
#[must_use]
pub fn report(events: &[MandatoryEvent], nodes: &[NodeState]) -> Vec<ReportRow> {
    let Some(highest) = events.iter().map(|e| e.position).max() else {
        return Vec::new();
    };
    let mut rows: Vec<ReportRow> = nodes
        .iter()
        .map(|n| {
            let oldest_open = events
                .iter()
                .filter(|e| e.position > n.applied_prefix || n.holds.contains(&e.position))
                .map(|e| e.published_at_millis)
                .min();
            let status = match oldest_open {
                Some(since_millis) => ReportStatus::OutstandingSince {
                    since_millis,
                    last_seen_millis: n.last_seen_millis,
                },
                None => n
                    .crossings
                    .iter()
                    .filter(|c| c.through_position >= highest)
                    .map(|c| c.applied_at_millis)
                    .min()
                    .map_or(ReportStatus::AppliedUnrecorded, ReportStatus::AppliedAt),
            };
            ReportRow {
                node: n.node.clone(),
                status,
            }
        })
        .collect();
    rows.sort_by(|a, b| a.node.cmp(&b.node));
    rows
}
