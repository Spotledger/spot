//! The engine's node-run events: the crate's only input.
//!
//! One event is one state transition of one node attempt, with the instant it happened. A host
//! derives the feed from the engine's durable node-run records (the `bpm_node_outcome` rows: a
//! `started` row, a `waiting`/`retrying` row and the terminal `done`/`failed` row), or from a
//! live subscription to the same records. Time is always the event's [`NodeRunEvent::at`];
//! nothing here reads a clock, so a backfill and a live feed fold identically.
//!
//! The four states mirror the engine's [`AttemptState`](spot_bpm_engine::AttemptState): a
//! `retrying` attempt is a [`RunState::Failed`] event with [`NodeRunEvent::retry`] set, and it
//! is the attempt that waits, not the node.

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{AttemptState, CaseId, NodeKind};
use spot_dag::NodeId;

use crate::process::{ProcessKey, StepKey};

/// Where one node attempt is after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// The handler is running.
    Started,
    /// The attempt paused: for a timer, a person, an event or its retry.
    Waiting,
    /// The attempt succeeded.
    Done,
    /// The attempt failed. When [`NodeRunEvent::retry`] is set it will be retried; otherwise the
    /// failure is terminal and the error branch is taken.
    Failed,
}

impl RunState {
    /// The run state an engine attempt state maps to.
    ///
    /// `Retrying` is a wait for the next attempt, so it maps to [`RunState::Waiting`]; the event
    /// that carries it also sets [`NodeRunEvent::retry`] and the [`WaitReason::IntegrationRetry`]
    /// reason.
    #[must_use]
    pub fn from_attempt_state(state: AttemptState) -> Self {
        match state {
            AttemptState::Started => Self::Started,
            AttemptState::Waiting | AttemptState::Retrying => Self::Waiting,
            AttemptState::Done => Self::Done,
            AttemptState::Failed => Self::Failed,
        }
    }
}

/// Why an attempt is waiting.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// A person must act: a task in an inbox (a Decision node).
    HumanTask,
    /// A timer or a date.
    Timer,
    /// An event key must arrive.
    Event,
    /// An integration failed and the attempt waits for its retry.
    IntegrationRetry,
}

/// The process version, run, node, attempt and node kind one event belongs to.
///
/// It is the constructor's grouping of the five things that never change across one attempt, so
/// the event constructors take few arguments and read clearly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttemptRef {
    /// The process version the attempt belongs to.
    pub process: ProcessKey,
    /// The run (the Case).
    pub case: CaseId,
    /// The node being run.
    pub node: NodeId,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
    /// The node's execution kind.
    pub kind: NodeKind,
}

impl AttemptRef {
    /// Builds the reference for one attempt.
    #[must_use]
    pub fn new(
        process: ProcessKey,
        case: CaseId,
        node: NodeId,
        attempt: u32,
        kind: NodeKind,
    ) -> Self {
        Self {
            process,
            case,
            node,
            attempt,
            kind,
        }
    }
}

/// One node-run event: one state transition of one attempt, at one instant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeRunEvent {
    /// The process version the attempt belongs to.
    pub process: ProcessKey,
    /// The run (the Case) the attempt belongs to.
    pub case: CaseId,
    /// The node being run.
    #[serde(with = "crate::wire::node_id")]
    pub node: NodeId,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
    /// The node's execution kind.
    pub kind: NodeKind,
    /// When the transition happened (an argument; never read from a clock).
    #[serde(with = "crate::wire::instant")]
    pub at: OffsetDateTime,
    /// Where the attempt is after the transition.
    pub state: RunState,
    /// Why the attempt is waiting, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<WaitReason>,
    /// The failure's stable code, when the attempt failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_code: Option<String>,
    /// Whether a failed attempt will be retried (the error branch is not taken yet).
    #[serde(default)]
    pub retry: bool,
    /// Who owns the waiting work: the assignee of a task, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

impl NodeRunEvent {
    /// Builds an event from an attempt reference and a state.
    #[must_use]
    pub fn new(
        attempt: AttemptRef,
        at: OffsetDateTime,
        state: RunState,
        wait: Option<WaitReason>,
    ) -> Self {
        Self {
            process: attempt.process,
            case: attempt.case,
            node: attempt.node,
            attempt: attempt.attempt,
            kind: attempt.kind,
            at,
            state,
            wait,
            failure_code: None,
            retry: false,
            owner: None,
        }
    }

    /// The attempt started (or resumed after a wait).
    #[must_use]
    pub fn started(attempt: AttemptRef, at: OffsetDateTime) -> Self {
        Self::new(attempt, at, RunState::Started, None)
    }

    /// The attempt entered a wait for the named reason.
    #[must_use]
    pub fn waiting(attempt: AttemptRef, at: OffsetDateTime, reason: WaitReason) -> Self {
        Self::new(attempt, at, RunState::Waiting, Some(reason))
    }

    /// The attempt succeeded.
    #[must_use]
    pub fn done(attempt: AttemptRef, at: OffsetDateTime) -> Self {
        Self::new(attempt, at, RunState::Done, None)
    }

    /// The attempt failed terminally.
    #[must_use]
    pub fn failed(attempt: AttemptRef, at: OffsetDateTime, code: impl Into<String>) -> Self {
        let mut event = Self::new(attempt, at, RunState::Failed, None);
        event.failure_code = Some(code.into());
        event
    }

    /// The attempt failed and will be retried; it waits for the retry.
    #[must_use]
    pub fn retrying(attempt: AttemptRef, at: OffsetDateTime, code: impl Into<String>) -> Self {
        let mut event = Self::new(
            attempt,
            at,
            RunState::Waiting,
            Some(WaitReason::IntegrationRetry),
        );
        event.failure_code = Some(code.into());
        event.retry = true;
        event
    }

    /// Sets the owner of the waiting work.
    #[must_use]
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    /// The step key (process version and node) this event aggregates under.
    #[must_use]
    pub fn key(&self) -> StepKey {
        StepKey::new(self.process.clone(), self.node.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn attempt() -> AttemptRef {
        AttemptRef::new(
            ProcessKey::new("order_flow", 1),
            CaseId::new("case1").unwrap(),
            NodeId::new("approve").unwrap(),
            1,
            NodeKind::Decision,
        )
    }

    #[test]
    fn attempt_states_map_to_run_states() {
        assert_eq!(
            RunState::from_attempt_state(AttemptState::Started),
            RunState::Started
        );
        assert_eq!(
            RunState::from_attempt_state(AttemptState::Waiting),
            RunState::Waiting
        );
        assert_eq!(
            RunState::from_attempt_state(AttemptState::Retrying),
            RunState::Waiting
        );
        assert_eq!(
            RunState::from_attempt_state(AttemptState::Done),
            RunState::Done
        );
        assert_eq!(
            RunState::from_attempt_state(AttemptState::Failed),
            RunState::Failed
        );
    }

    #[test]
    fn retrying_waits_for_the_retry() {
        let event = NodeRunEvent::retrying(attempt(), OffsetDateTime::UNIX_EPOCH, "conn.timeout");
        assert_eq!(event.state, RunState::Waiting);
        assert!(event.retry);
        assert_eq!(event.wait, Some(WaitReason::IntegrationRetry));
        assert_eq!(event.failure_code.as_deref(), Some("conn.timeout"));
    }

    #[test]
    fn events_round_trip_through_json() {
        let event = NodeRunEvent::waiting(
            attempt(),
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
            WaitReason::HumanTask,
        )
        .with_owner("ada");
        let json = serde_json::to_string(&event).unwrap();
        let back: NodeRunEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, event);
    }
}
