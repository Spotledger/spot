//! The stuck queue: the runs not progressing past a per-node expected duration.
//!
//! An operator sets an expected duration per node (or one default); every open attempt whose
//! current state has lasted at least that long is stuck, with the reason (waiting for a task, a
//! timer, or an integration retry, or simply running too long) and the owner of the waiting
//! work. `now` is an argument — nothing here reads a clock.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, NodeKind};
use spot_dag::NodeId;

use crate::accumulator::OpenAttempt;
use crate::duration::Millis;
use crate::event::{RunState, WaitReason};
use crate::process::ProcessKey;

/// The expected durations a stuck run is detected with. They are policy data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StuckPolicy {
    /// The expected duration for a node the policy does not name.
    pub default_expected: Millis,
    /// The expected duration per node, keyed by node id.
    #[serde(default)]
    pub per_node: BTreeMap<String, Millis>,
}

impl Default for StuckPolicy {
    fn default() -> Self {
        Self {
            default_expected: Millis::from_seconds(3_600),
            per_node: BTreeMap::new(),
        }
    }
}

impl StuckPolicy {
    /// A policy with one default expected duration and no per-node overrides.
    #[must_use]
    pub fn new(default_expected: Millis) -> Self {
        Self {
            default_expected,
            per_node: BTreeMap::new(),
        }
    }

    /// Sets the expected duration for one node.
    #[must_use]
    pub fn with_node(mut self, node: impl Into<String>, expected: Millis) -> Self {
        self.per_node.insert(node.into(), expected);
        self
    }

    /// The expected duration for a node: its override, or the default.
    #[must_use]
    pub fn expected_for(&self, node: &NodeId) -> Millis {
        self.per_node
            .get(node.as_str())
            .copied()
            .unwrap_or(self.default_expected)
    }
}

/// Why a run is stuck.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StuckReason {
    /// Waiting for a person to act on a task.
    WaitingForTask,
    /// Waiting for a timer, a date or an event.
    Timer,
    /// An integration failed and the attempt waits for its retry.
    IntegrationRetry,
    /// Running, but for longer than the node expects.
    Running,
}

impl StuckReason {
    /// The user-visible label of the reason. It never says the internal word for a run.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::WaitingForTask => "waiting for a task",
            Self::Timer => "waiting for a timer",
            Self::IntegrationRetry => "waiting for an integration retry",
            Self::Running => "running too long",
        }
    }
}

/// One stuck run: the attempt that has not progressed, why, and who owns it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StuckCase {
    /// The process version.
    pub process: ProcessKey,
    /// The run (the Case).
    pub case: CaseId,
    /// The node the run is stuck at.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The attempt number.
    pub attempt: u32,
    /// The node's execution kind.
    pub kind: NodeKind,
    /// Who owns the waiting work, when someone does.
    pub owner: Option<String>,
    /// Why the run is stuck.
    pub reason: StuckReason,
    /// When the attempt entered its current state.
    #[serde(with = "crate::wire::instant")]
    pub since: OffsetDateTime,
    /// When the attempt first started.
    #[serde(with = "crate::wire::instant")]
    pub started_at: OffsetDateTime,
    /// How long the attempt has been in its current state (`now - since`).
    pub elapsed: Millis,
    /// The expected duration it has passed.
    pub expected: Millis,
}

/// The open attempts stuck at `now`: those in their current state for at least the expected
/// duration of their node.
///
/// The result is sorted by elapsed time (longest first), then run and node, so it is stable.
#[must_use]
pub fn stuck_cases(
    open: &[OpenAttempt],
    now: OffsetDateTime,
    policy: &StuckPolicy,
) -> Vec<StuckCase> {
    let mut stuck: Vec<StuckCase> = open
        .iter()
        .filter_map(|attempt| {
            let expected = policy.expected_for(&attempt.node);
            let elapsed = Millis::between(attempt.since, now);
            (elapsed >= expected).then(|| StuckCase {
                process: attempt.process.clone(),
                case: attempt.case.clone(),
                node: attempt.node.clone(),
                attempt: attempt.attempt,
                kind: attempt.kind.clone(),
                owner: attempt.owner.clone(),
                reason: reason_for(attempt),
                since: attempt.since,
                started_at: attempt.started_at,
                elapsed,
                expected,
            })
        })
        .collect();
    stuck.sort_by(|a, b| {
        b.elapsed
            .cmp(&a.elapsed)
            .then_with(|| a.case.cmp(&b.case))
            .then_with(|| a.node.cmp(&b.node))
    });
    stuck
}

fn reason_for(attempt: &OpenAttempt) -> StuckReason {
    if attempt.state == RunState::Waiting {
        match attempt.wait {
            Some(WaitReason::HumanTask) => StuckReason::WaitingForTask,
            Some(WaitReason::IntegrationRetry) => StuckReason::IntegrationRetry,
            Some(WaitReason::Timer | WaitReason::Event) | None => StuckReason::Timer,
        }
    } else {
        StuckReason::Running
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn open(
        node: &str,
        case: &str,
        state: RunState,
        wait: Option<WaitReason>,
        since: i64,
    ) -> OpenAttempt {
        OpenAttempt {
            process: ProcessKey::new("order_flow", 1),
            case: CaseId::new(case).unwrap(),
            node: NodeId::new(node).unwrap(),
            attempt: 1,
            kind: NodeKind::Decision,
            owner: Some("ada".to_owned()),
            state,
            wait,
            since: OffsetDateTime::from_unix_timestamp(1_700_000_000 + since).unwrap(),
            started_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        }
    }

    #[test]
    fn a_run_past_its_expected_duration_is_stuck_with_its_reason_and_owner() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000 + 400).unwrap();
        let policy = StuckPolicy::new(Millis::from_seconds(300))
            .with_node("slow_task", Millis::from_seconds(60));
        let attempts = vec![
            open(
                "approve",
                "case1",
                RunState::Waiting,
                Some(WaitReason::HumanTask),
                0,
            ),
            open(
                "call",
                "case2",
                RunState::Waiting,
                Some(WaitReason::IntegrationRetry),
                0,
            ),
            open(
                "tick",
                "case3",
                RunState::Waiting,
                Some(WaitReason::Timer),
                0,
            ),
            open("quick", "case4", RunState::Started, None, 0),
            open("fresh", "case5", RunState::Started, None, 390),
        ];
        let stuck = stuck_cases(&attempts, now, &policy);
        assert_eq!(stuck.len(), 4, "the fresh attempt is not stuck yet");
        assert_eq!(stuck[0].case.as_str(), "case1", "longest first");
        assert_eq!(stuck[0].reason, StuckReason::WaitingForTask);
        assert_eq!(stuck[0].owner.as_deref(), Some("ada"));
        assert_eq!(stuck[0].elapsed, Millis::from_seconds(400));
        assert_eq!(stuck[1].reason, StuckReason::IntegrationRetry);
        assert_eq!(stuck[2].reason, StuckReason::Timer);
        assert_eq!(stuck[3].reason, StuckReason::Running);
    }

    #[test]
    fn a_per_node_expected_duration_is_honoured() {
        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000 + 100).unwrap();
        let policy = StuckPolicy::new(Millis::from_seconds(300))
            .with_node("approve", Millis::from_seconds(50));
        let attempts = vec![open("approve", "case1", RunState::Started, None, 0)];
        assert_eq!(stuck_cases(&attempts, now, &policy).len(), 1);
    }

    #[test]
    fn reason_labels_never_say_case() {
        for reason in [
            StuckReason::WaitingForTask,
            StuckReason::Timer,
            StuckReason::IntegrationRetry,
            StuckReason::Running,
        ] {
            assert!(!reason.label().contains("Case"));
        }
    }
}
