//! The Case record: the DAG run plus the Case-level state the engine keeps around it.
//!
//! A Case *is* a `spot-dag-exec` [`Run`](spot_dag_exec::Run): the run holds where every node is
//! (`Ready`, `Running`, `Done`, `Skipped`, `Failed`), its attempts and its taken edges. The
//! engine adds exactly the bookkeeping the DAG layer does not carry, and keeps it in the same
//! execution store as the run:
//!
//! * [`Attempt`] — the outcome of one engine attempt, so a replay after a crash returns the
//!   stored outcome instead of calling the handler again;
//! * [`TimerRecord`] — a registered wait (an event key, a human task or a due instant);
//! * [`ChildLink`] — the child Case a sub-process node started and now awaits;
//! * the Case's `variables` (canonical JSON) and the `initiator` principal.
//!
//! Everything here is plain data with no clock and no randomness: instants are arguments.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_dag::NodeId;
use spot_dag_exec::{Instant, Run, Transition};

use crate::handler::CaseId;

/// Serde helpers for `spot_dag::NodeId`, which serialises but does not deserialise.
pub(crate) mod serde_nodes {
    use std::collections::BTreeMap;

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer};
    use spot_dag::NodeId;

    /// One node id, validated on the way in.
    pub(crate) fn one<'de, D>(deserializer: D) -> Result<NodeId, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        NodeId::new(&raw).map_err(D::Error::custom)
    }

    /// A map of node id to value, validating every id on the way in.
    pub(crate) fn map<'de, D, V>(deserializer: D) -> Result<BTreeMap<NodeId, V>, D::Error>
    where
        D: Deserializer<'de>,
        V: Deserialize<'de>,
    {
        let raw = BTreeMap::<String, V>::deserialize(deserializer)?;
        raw.into_iter()
            .map(|(text, value)| Ok((NodeId::new(&text).map_err(D::Error::custom)?, value)))
            .collect()
    }
}

/// Where one engine attempt stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptState {
    /// The handler is running and no outcome has been stored yet (a crash resumes here).
    Started,
    /// The handler asked to wait; a timer is registered.
    Waiting,
    /// The handler failed and the engine will retry after `not_before`.
    Retrying,
    /// The handler succeeded; the stored output is the outcome a replay returns.
    Done,
    /// The handler failed with no retries left; the error branch is taken.
    Failed,
}

/// The stored outcome of one engine attempt.
///
/// It is written *before* the DAG transition is acknowledged, so a crash between the two
/// replays the stored outcome without calling the handler again.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attempt {
    /// The node the attempt belongs to.
    #[serde(deserialize_with = "serde_nodes::one")]
    pub node: NodeId,
    /// The engine attempt number (1 for the first try).
    pub attempt: u32,
    /// Where the attempt stands.
    pub state: AttemptState,
    /// The output payload of a success.
    pub output: Option<Json>,
    /// The outgoing edge conditions the attempt chose.
    pub branches: Vec<String>,
    /// A plain-language failure reason, when it failed.
    pub failure: Option<String>,
    /// When the attempt started.
    pub started_at: Instant,
    /// When a retry becomes due, for a `Retrying` attempt.
    pub not_before: Option<Instant>,
    /// Whether this attempt replays a started-but-unfinished one after a crash.
    pub is_retry: bool,
}

impl Attempt {
    /// A fresh, started attempt at `at`.
    #[must_use]
    pub fn started(node: NodeId, attempt: u32, at: Instant, is_retry: bool) -> Self {
        Self {
            node,
            attempt,
            state: AttemptState::Started,
            output: None,
            branches: Vec::new(),
            failure: None,
            started_at: at,
            not_before: None,
            is_retry,
        }
    }
}

/// A registered wait of one node attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimerRecord {
    /// The node waiting.
    #[serde(deserialize_with = "serde_nodes::one")]
    pub node: NodeId,
    /// The attempt that registered the wait.
    pub attempt: u32,
    /// The instant the wait ends, when it has one.
    pub due: Option<Instant>,
    /// The event key a signal must carry, when it waits for an event.
    pub event: Option<String>,
    /// The human-task key a signal must carry, when it waits for a person.
    pub human: Option<String>,
}

impl TimerRecord {
    /// Whether this timer is due at `at`.
    #[must_use]
    pub fn is_due(&self, at: Instant) -> bool {
        self.due.is_some_and(|due| due <= at)
    }
}

/// The child Case a sub-process node started and now awaits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChildLink {
    /// The parent node.
    #[serde(deserialize_with = "serde_nodes::one")]
    pub node: NodeId,
    /// The parent attempt that started the child.
    pub attempt: u32,
    /// The child Case.
    pub child: CaseId,
}

/// One Case: the DAG run and the engine's bookkeeping, kept together in the execution store.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaseRecord {
    /// The DAG run: node states, attempts, taken edges and the run's version.
    pub run: Run,
    /// The Case's variables as canonical JSON.
    pub variables: Json,
    /// The stored outcome of each node's latest attempt, keyed by node.
    #[serde(deserialize_with = "serde_nodes::map")]
    pub attempts: BTreeMap<NodeId, Attempt>,
    /// The registered waits, keyed by node.
    #[serde(deserialize_with = "serde_nodes::map")]
    pub timers: BTreeMap<NodeId, TimerRecord>,
    /// The child links of sub-process nodes, keyed by node.
    #[serde(deserialize_with = "serde_nodes::map")]
    pub children: BTreeMap<NodeId, ChildLink>,
    /// The principal the Case runs as, when it has one.
    pub initiator: Option<String>,
}

impl CaseRecord {
    /// Wraps a fresh run with empty Case-level state.
    #[must_use]
    pub fn new(run: Run, initiator: Option<String>) -> Self {
        Self {
            run,
            variables: Json::Object(serde_json::Map::new()),
            attempts: BTreeMap::new(),
            timers: BTreeMap::new(),
            children: BTreeMap::new(),
            initiator,
        }
    }

    /// The Case's id.
    #[must_use]
    pub fn id(&self) -> &CaseId {
        &self.run.id
    }

    /// The stored outcome of a node's latest attempt, when there is one.
    #[must_use]
    pub fn attempt(&self, node: &NodeId) -> Option<&Attempt> {
        self.attempts.get(node)
    }
}

/// One atomic change to a Case: what the engine commits after one attempt.
///
/// A store applies every present part in one transaction: the DAG transition, the stored
/// attempt outcome, the timer and child bookkeeping and the Case's variables commit or roll
/// back together. This is the "all or nothing" the blueprint's two layers of state require.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CaseUpdate {
    /// The DAG transition to apply, when there is one.
    pub transition: Option<Transition>,
    /// The attempt outcome to store or replace.
    pub attempt: Option<Attempt>,
    /// A timer to register (replacing one already registered for the same node).
    pub timer: Option<TimerRecord>,
    /// A timer to clear, for a node whose wait has fired.
    pub clear_timer: Option<NodeId>,
    /// A child link to store, for a sub-process node.
    pub child: Option<ChildLink>,
    /// The Case's variables, replacing the stored ones when present.
    pub variables: Option<Json>,
    /// The instant this update is committed at; a store that logs an attempt or a timer reads it.
    pub at: Option<Instant>,
}

impl CaseUpdate {
    /// An empty update.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a DAG transition.
    #[must_use]
    pub fn with_transition(mut self, transition: Transition) -> Self {
        self.transition = Some(transition);
        self
    }

    /// Adds a stored attempt outcome.
    #[must_use]
    pub fn with_attempt(mut self, attempt: Attempt) -> Self {
        self.attempt = Some(attempt);
        self
    }

    /// Sets the instant this update is committed at.
    #[must_use]
    pub fn with_at(mut self, at: Instant) -> Self {
        self.at = Some(at);
        self
    }

    /// Adds a timer registration.
    #[must_use]
    pub fn with_timer(mut self, timer: TimerRecord) -> Self {
        self.timer = Some(timer);
        self
    }

    /// Clears a node's timer.
    #[must_use]
    pub fn clearing_timer(mut self, node: NodeId) -> Self {
        self.clear_timer = Some(node);
        self
    }

    /// Adds a child link.
    #[must_use]
    pub fn with_child(mut self, child: ChildLink) -> Self {
        self.child = Some(child);
        self
    }

    /// Replaces the Case's variables.
    #[must_use]
    pub fn with_variables(mut self, variables: Json) -> Self {
        self.variables = Some(variables);
        self
    }
}

// End of the record types; the node-id serde helpers live in `serde_nodes` above.

#[cfg(test)]
mod at_tests {
    use super::*;
    use spot_app_api::prelude::OffsetDateTime;

    #[test]
    fn with_at_sets_the_instant_and_new_leaves_it_none() {
        assert_eq!(CaseUpdate::new().at, None);
        let at = Instant::from_offset(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(CaseUpdate::new().with_at(at).at, Some(at));
    }
}
