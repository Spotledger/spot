//! The durable run state: [`Run`], [`NodeState`], the [`RunPlan`] a start produces, and the
//! [`Transition`] delta a claim or completion produces.
//!
//! Everything here is plain data: it derives `serde`, uses stable `snake_case` names, and
//! carries no clock and no randomness. `spot_dag::NodeId` serialises as a string but does not
//! deserialise (it has no `Deserialize`), so the private `serde_nodes` helpers validate every
//! node id on the way back in with `NodeId::new`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value as Json;
use spot_dag::NodeId;

use crate::error::ExecError;
use crate::instant::Instant;

/// Serde helpers for `spot_dag::NodeId`, which serialises but does not deserialise.
pub(crate) mod serde_nodes {
    use std::collections::BTreeMap;

    use serde::de::Error as _;
    use serde::{Deserialize, Deserializer};
    use spot_dag::NodeId;

    use super::NodeState;

    /// One node id.
    pub(crate) fn one<'de, D>(deserializer: D) -> Result<NodeId, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        NodeId::new(&raw).map_err(D::Error::custom)
    }

    /// A list of node ids.
    pub(crate) fn list<'de, D>(deserializer: D) -> Result<Vec<NodeId>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = Vec::<String>::deserialize(deserializer)?;
        raw.into_iter()
            .map(|text| NodeId::new(&text).map_err(D::Error::custom))
            .collect()
    }

    /// A map of node id to state.
    pub(crate) fn map<'de, D>(deserializer: D) -> Result<BTreeMap<NodeId, NodeState>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = BTreeMap::<String, NodeState>::deserialize(deserializer)?;
        raw.into_iter()
            .map(|(text, state)| Ok((NodeId::new(&text).map_err(D::Error::custom)?, state)))
            .collect()
    }
}

/// The identifier of one run, with its grammar baked in.
///
/// Valid ids match `[a-z][a-z0-9_]{0,63}`: exactly the [`NodeId`] grammar. Build one with
/// [`RunId::new`]; deserialising validates too, so a bad id in stored JSON is an error before
/// it ever becomes a run.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct RunId(String);

impl RunId {
    /// Validates the grammar and builds the id.
    ///
    /// # Errors
    /// [`ExecError::RunIdInvalid`] when `raw` does not match `[a-z][a-z0-9_]{0,63}`.
    pub fn new(raw: &str) -> Result<Self, ExecError> {
        let mut chars = raw.chars();
        let first = chars.next();
        let ok = raw.len() <= 64
            && first.is_some_and(|c| c.is_ascii_lowercase())
            && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
        if ok {
            Ok(Self(raw.to_owned()))
        } else {
            Err(ExecError::RunIdInvalid {
                raw: raw.to_owned(),
            })
        }
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for RunId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for RunId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::new(&raw).map_err(serde::de::Error::custom)
    }
}

/// How a whole run stands.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    /// Work may still be claimed and completed.
    #[default]
    Running,
    /// Every node of the taken flow is done or dead; nothing more will run.
    Completed,
    /// A node failed and no further work can proceed.
    Failed,
    /// A caller cancelled the run.
    Cancelled,
}

/// The state of one node in a run.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    /// Not yet reachable: an incoming join rule is not satisfied and is not dead either.
    #[default]
    Pending,
    /// Every join rule is satisfied; the node may be claimed.
    Ready,
    /// Claimed: one attempt is in flight.
    Running,
    /// Attempt finished successfully.
    Done,
    /// Dead branch: the join rule can never be satisfied.
    Skipped,
    /// Attempt finished with a failure.
    Failed,
}

impl NodeStatus {
    /// Whether this is a terminal state no further transition may leave.
    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Skipped | Self::Failed)
    }
}

/// One node's run state: its status, attempts, output and times.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeState {
    /// Where the node is.
    pub status: NodeStatus,
    /// The number of the attempt that is running or last finished (`0` before the first).
    pub attempts: u32,
    /// The node's output payload as canonical JSON, when it produced one.
    pub output: Option<Json>,
    /// Why the node failed, when it did.
    pub failure: Option<String>,
    /// When the node was first claimed.
    pub started_at: Option<Instant>,
    /// When the node reached a terminal state.
    pub ended_at: Option<Instant>,
}

/// A taken edge of the run: `from` completed and chose the edge to `to`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TakenEdge {
    /// The edge's source node.
    pub from: NodeId,
    /// The edge's target node.
    pub to: NodeId,
}

impl<'de> Deserialize<'de> for TakenEdge {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            from: String,
            to: String,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            from: NodeId::new(&raw.from).map_err(serde::de::Error::custom)?,
            to: NodeId::new(&raw.to).map_err(serde::de::Error::custom)?,
        })
    }
}

/// One run: the pinned DAG, the caller's business key, the whole node map and the taken edges.
///
/// `version` is the store's optimistic-concurrency counter: [`ExecStore::apply`](crate::ExecStore::apply)
/// writes only against the version the caller read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    /// The run's identity.
    pub id: RunId,
    /// The compiled DAG's key (the definition's name).
    pub dag_key: String,
    /// The published version the run pins.
    pub dag_version: u32,
    /// The published content hash the run pins, as 64 hex characters.
    pub content_hash: String,
    /// The caller-supplied business key: what a host uses to find the run.
    pub key: String,
    /// How the run stands.
    pub status: RunStatus,
    /// When the run started.
    pub started_at: Instant,
    /// When the run ended, once it is no longer running.
    pub ended_at: Option<Instant>,
    /// The optimistic version; the store bumps it on every applied transition.
    pub version: u64,
    /// Every node's state, keyed by node id.
    #[serde(deserialize_with = "serde_nodes::map")]
    pub nodes: BTreeMap<NodeId, NodeState>,
    /// The edges the run has taken, in sorted order.
    #[serde(default)]
    pub taken: BTreeSet<TakenEdge>,
}

impl Run {
    /// The state of one node, when the run tracks it.
    #[must_use]
    pub fn node(&self, id: &NodeId) -> Option<&NodeState> {
        self.nodes.get(id)
    }

    /// The node ids that are [`NodeStatus::Ready`], sorted by id.
    #[must_use]
    pub fn ready_nodes(&self) -> Vec<NodeId> {
        self.nodes
            .iter()
            .filter(|(_, state)| state.status == NodeStatus::Ready)
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Whether the run is finished: nothing is ready and nothing is running.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        !self
            .nodes
            .values()
            .any(|state| matches!(state.status, NodeStatus::Ready | NodeStatus::Running))
    }
}

/// What a [`start`](crate::start) produces: the fresh run and its initially ready nodes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunPlan {
    /// The run, in [`RunStatus::Running`], with entries `Ready` and everything else `Pending`.
    pub run: Run,
    /// The nodes that are ready at the start (the DAG's entry nodes), sorted by id.
    #[serde(deserialize_with = "serde_nodes::list")]
    pub ready: Vec<NodeId>,
}

/// The outcome of one node attempt, as the caller supplies it to [`complete`](crate::complete).
///
/// `branches` names the edge conditions the engine decided were satisfied; an edge with no
/// condition is always taken, and an edge with a condition is taken exactly when its label is
/// in `branches`.
#[derive(Clone, Debug, PartialEq)]
pub enum NodeOutcome {
    /// The attempt succeeded.
    Done {
        /// The node's output payload; floats are rejected.
        output: Option<Json>,
        /// The condition labels the engine took.
        branches: Vec<String>,
    },
    /// The attempt failed.
    Failed {
        /// A plain-language reason, stored for the engine.
        reason: String,
        /// The condition labels the engine took (error branches).
        branches: Vec<String>,
    },
}

impl NodeOutcome {
    /// The condition labels this outcome takes.
    #[must_use]
    pub fn branches(&self) -> &[String] {
        match self {
            Self::Done { branches, .. } | Self::Failed { branches, .. } => branches,
        }
    }
}

/// The terminal half of a completed node, as stored in a [`Transition`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completion {
    /// The attempt succeeded, with an optional output payload.
    Done {
        /// The node's output payload.
        output: Option<Json>,
    },
    /// The attempt failed, with a reason.
    Failed {
        /// Why it failed.
        reason: String,
    },
}

impl Completion {
    /// The [`NodeStatus`] this completion sets.
    #[must_use]
    pub fn status(&self) -> NodeStatus {
        match self {
            Self::Done { .. } => NodeStatus::Done,
            Self::Failed { .. } => NodeStatus::Failed,
        }
    }

    /// The output payload, for a success.
    #[must_use]
    pub fn output(&self) -> Option<&Json> {
        match self {
            Self::Done { output } => output.as_ref(),
            Self::Failed { .. } => None,
        }
    }

    /// The reason, for a failure.
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Failed { reason } => Some(reason),
            Self::Done { .. } => None,
        }
    }
}

/// One step of a run, as [`claim`](crate::claim), [`complete`](crate::complete) or
/// [`cancel`](crate::cancel) produce it and [`ExecStore::apply`](crate::ExecStore::apply) stores it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// A ready node is claimed: it becomes [`NodeStatus::Running`] at a fresh attempt.
    Claim {
        /// The node being claimed.
        #[serde(deserialize_with = "serde_nodes::one")]
        node: NodeId,
        /// The attempt number (the node's previous attempts plus one).
        attempt: u32,
        /// When the claim happened.
        at: Instant,
    },
    /// A running (or ready) node is completed, and the run advances.
    Complete {
        /// The node being completed.
        #[serde(deserialize_with = "serde_nodes::one")]
        node: NodeId,
        /// The attempt being completed (the idempotency key's last part).
        attempt: u32,
        /// What the attempt did.
        completion: Completion,
        /// When the attempt finished.
        at: Instant,
        /// The edges newly taken from this node.
        taken: BTreeSet<TakenEdge>,
        /// Nodes that became ready.
        #[serde(deserialize_with = "serde_nodes::list")]
        ready: Vec<NodeId>,
        /// Nodes that are dead branches.
        #[serde(deserialize_with = "serde_nodes::list")]
        skipped: Vec<NodeId>,
        /// The run's status after this step.
        status: RunStatus,
        /// When the run ended, if this step finished it.
        ended_at: Option<Instant>,
    },
    /// The run is cancelled.
    Cancel {
        /// When the cancellation happened.
        at: Instant,
    },
}

/// The delta one step makes to a run.
///
/// A transition carries its run id, the deterministic step a pure function produced, and the
/// `(run, node, attempt)` idempotency key inside the step. [`ExecStore::apply`](crate::ExecStore::apply)
/// checks that key first, so a retried completion is a no-op rather than a double advance.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Transition {
    /// The run this transition belongs to.
    pub run: RunId,
    /// What the transition does.
    pub step: Step,
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::plan::start;
    use crate::testutil;

    #[test]
    fn run_ids_validate() {
        assert!(RunId::new("run1").is_ok());
        assert!(RunId::new("a_b_c").is_ok());
        for bad in ["", "Run", "1run", "a-b", "run with space"] {
            assert!(RunId::new(bad).is_err(), "{bad}");
        }
        assert!(serde_json::from_str::<RunId>("\"Bad\"").is_err());
    }

    #[test]
    fn a_run_round_trips_through_json() {
        let dag = testutil::dag(
            vec![testutil::node("a"), testutil::node("b")],
            vec![testutil::edge("a", "b")],
        );
        let plan = start(
            &dag,
            &testutil::published(&dag),
            RunId::new("run_json").unwrap(),
            "biz",
            testutil::now(),
        )
        .unwrap();
        let text = serde_json::to_string(&plan.run).unwrap();
        let back: Run = serde_json::from_str(&text).unwrap();
        assert_eq!(back, plan.run);
        assert_eq!(
            back.node(&NodeId::new("a").unwrap()).unwrap().status,
            NodeStatus::Ready
        );
    }

    #[test]
    fn an_unknown_node_key_in_json_is_rejected() {
        let dag = testutil::dag(vec![testutil::node("a")], Vec::new());
        let plan = start(
            &dag,
            &testutil::published(&dag),
            RunId::new("run_bad").unwrap(),
            "biz",
            testutil::now(),
        )
        .unwrap();
        let mut value = serde_json::to_value(&plan.run).unwrap();
        if let Json::Object(map) = &mut value
            && let Some(Json::Object(nodes)) = map.get_mut("nodes")
        {
            let state = nodes.remove("a").unwrap();
            nodes.insert("A".to_owned(), state);
        }
        assert!(serde_json::from_value::<Run>(value).is_err());
    }

    #[test]
    fn a_taken_edge_round_trips() {
        let edge = TakenEdge {
            from: NodeId::new("a").unwrap(),
            to: NodeId::new("b").unwrap(),
        };
        let text = serde_json::to_string(&edge).unwrap();
        assert_eq!(serde_json::from_str::<TakenEdge>(&text).unwrap(), edge);
    }
}
