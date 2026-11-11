//! Bottleneck detection: which step costs the most cycle time, and which queue is growing.
//!
//! A step is a bottleneck when it takes a large share of its process's total cycle time or when
//! its queue grows past the policy's threshold. Both thresholds are [`BottleneckPolicy`] data,
//! never constants in the code, so an operator tunes them per process. The ranking is
//! deterministic (share, then queue growth, then process and node) and every result carries a
//! stable [`Bottleneck::explanation`] string an operator can read.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_bpm_engine::NodeKind;
use spot_dag::NodeId;

use crate::duration::Millis;
use crate::process::ProcessKey;
use crate::stats::StepStats;

/// The thresholds a bottleneck is detected with. They are policy data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BottleneckPolicy {
    /// The share of the process's total cycle time (in whole per-mille) at or above which a
    /// step is a bottleneck by time.
    pub min_share_per_mille: u32,
    /// The net queue growth at or above which a step is a bottleneck by queue.
    pub min_queue_growth: i64,
}

impl Default for BottleneckPolicy {
    fn default() -> Self {
        Self {
            min_share_per_mille: 200,
            min_queue_growth: 1,
        }
    }
}

impl BottleneckPolicy {
    /// A policy with the given time and queue thresholds.
    #[must_use]
    pub const fn new(min_share_per_mille: u32, min_queue_growth: i64) -> Self {
        Self {
            min_share_per_mille,
            min_queue_growth,
        }
    }
}

/// One ranked bottleneck step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bottleneck {
    /// The process version.
    pub process: ProcessKey,
    /// The node.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The node's execution kind.
    pub kind: NodeKind,
    /// The step's share of its process's total completed-attempt cycle time, in whole per-mille.
    pub share_per_mille: u32,
    /// The step's total completed-attempt cycle time.
    pub total: Millis,
    /// The exact 90th percentile of the step's completed-attempt cycle times.
    pub p90: Option<Millis>,
    /// The net queue growth at the step.
    pub queue_growth: i64,
    /// The most attempts waiting at the step at any one time.
    pub max_waiting: u64,
    /// A stable, operator-readable explanation of why the step ranked.
    pub explanation: String,
}

/// Ranks the bottleneck steps across every process in `stats`.
///
/// A step is a candidate when its share of its process's total cycle time is at or above
/// [`BottleneckPolicy::min_share_per_mille`], or when its queue growth is at or above
/// [`BottleneckPolicy::min_queue_growth`]. Candidates are ranked by share (descending), then
/// queue growth (descending), then process and node, so the result is deterministic and the
/// seeded bottleneck is first.
#[must_use]
pub fn bottlenecks(stats: &[StepStats], policy: &BottleneckPolicy) -> Vec<Bottleneck> {
    let mut by_process: BTreeMap<&ProcessKey, Vec<&StepStats>> = BTreeMap::new();
    for step in stats {
        by_process.entry(&step.process).or_default().push(step);
    }

    let mut ranked: Vec<Bottleneck> = Vec::new();
    for (process, steps) in by_process {
        let process_total: i128 = steps
            .iter()
            .map(|step| i128::from(step.total.as_millis().max(0)))
            .sum();
        for step in steps {
            let share = share_per_mille(step.total, process_total);
            let is_candidate =
                share >= policy.min_share_per_mille || step.queue_growth >= policy.min_queue_growth;
            if !is_candidate {
                continue;
            }
            ranked.push(Bottleneck {
                process: process.clone(),
                node: step.node.clone(),
                kind: step.kind.clone(),
                share_per_mille: share,
                total: step.total,
                p90: step.p90,
                queue_growth: step.queue_growth,
                max_waiting: step.max_waiting,
                explanation: explain(step, share, process_total),
            });
        }
    }

    ranked.sort_by(|a, b| {
        b.share_per_mille
            .cmp(&a.share_per_mille)
            .then_with(|| b.queue_growth.cmp(&a.queue_growth))
            .then_with(|| a.process.cmp(&b.process))
            .then_with(|| a.node.cmp(&b.node))
    });
    ranked
}

/// A step's share of the process's total cycle time, in whole per-mille, rounded half up.
fn share_per_mille(total: Millis, process_total: i128) -> u32 {
    if process_total <= 0 {
        return 0;
    }
    let part = i128::from(total.as_millis().max(0));
    let value = (part * 1000 + process_total / 2) / process_total;
    u32::try_from(value).unwrap_or(1000)
}

/// The stable explanation string for a ranked step. It never says the internal word for a run.
fn explain(step: &StepStats, share: u32, process_total: i128) -> String {
    let p90 = step.p90.map_or_else(
        || "no completed attempt".to_owned(),
        |p90| format!("p90 {p90}"),
    );
    format!(
        "step `{}` of {} uses {} per-mille of cycle time ({p90} of {}) across {} completed \
         attempt(s) and its queue grew by {}",
        step.node,
        step.process,
        share,
        Millis::from_millis(i64::try_from(process_total).unwrap_or(i64::MAX)),
        step.completed,
        step.queue_growth,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn step(node: &str, total_seconds: i64, growth: i64) -> StepStats {
        StepStats {
            process: ProcessKey::new("order_flow", 1),
            node: NodeId::new(node).unwrap(),
            kind: NodeKind::Decision,
            attempts: 4,
            completed: 4,
            done: 4,
            failed: 0,
            retries: 0,
            total: Millis::from_seconds(total_seconds),
            work: Millis::from_seconds(total_seconds),
            wait: Millis::ZERO,
            min: Some(Millis::from_seconds(1)),
            p50: Some(Millis::from_seconds(1)),
            p90: Some(Millis::from_seconds(total_seconds)),
            p99: Some(Millis::from_seconds(total_seconds)),
            max: Some(Millis::from_seconds(total_seconds)),
            queue_entered: 4,
            queue_left: 0,
            queue_growth: growth,
            max_waiting: 1,
        }
    }

    #[test]
    fn the_seeded_slow_step_ranks_first_with_a_stable_explanation() {
        let mut stats = Vec::new();
        for name in [
            "draft",
            "check_stock",
            "reserve",
            "pick",
            "pack",
            "ship",
            "invoice",
            "approve",
        ] {
            let seconds = if name == "approve" { 100 } else { 10 };
            stats.push(step(name, seconds, 0));
        }
        let ranked = bottlenecks(&stats, &BottleneckPolicy::default());
        assert_eq!(
            ranked.len(),
            1,
            "only the slow step passes the default threshold"
        );
        assert_eq!(ranked[0].node.as_str(), "approve");
        assert_eq!(ranked[0].share_per_mille, 588);
        assert_eq!(
            ranked[0].explanation,
            "step `approve` of order_flow v1 uses 588 per-mille of cycle time \
             (p90 100000 ms of 170000 ms) across 4 completed attempt(s) and its queue grew by 0"
        );
    }

    #[test]
    fn a_growing_queue_is_a_bottleneck_even_when_fast() {
        let stats = vec![step("fast_but_stuck", 1, 5), step("slow", 10, 0)];
        let ranked = bottlenecks(&stats, &BottleneckPolicy::default());
        assert_eq!(ranked.len(), 2);
        assert_eq!(
            ranked[0].node.as_str(),
            "slow",
            "share ranks above queue growth"
        );
        assert_eq!(ranked[1].node.as_str(), "fast_but_stuck");
    }
}
