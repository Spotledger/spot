//! Negotiated majors, fleet-gated retirement and upward acceptance.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::SCHEMA_RETIRE_BLOCKED;
use crate::model::{Major, NodeId, NodeReport, NodeState, NodeStatus, Topic};

/// For each topic the producer publishes, the highest published major this node can decode.
/// During a deprecation window both majors exist as separate events and each node gets the one it
/// can read. A topic with no decodable published major is absent from the result (nothing is
/// served; a record that reaches the node anyway is held, see [`crate::on_undecodable`]).
#[must_use]
pub fn served_majors(
    producer_publishes: &BTreeMap<Topic, BTreeSet<Major>>,
    node: &NodeReport,
) -> BTreeMap<Topic, Major> {
    producer_publishes
        .iter()
        .filter_map(|(topic, majors)| {
            majors
                .iter()
                .rev()
                .find(|major| node.can_decode(topic, **major))
                .map(|major| (topic.clone(), *major))
        })
        .collect()
}

/// Whether a producer may stop publishing an old major.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Retirement {
    /// Every active node can decode the new major.
    Allowed,
    /// Active nodes cannot decode the new major yet (the `SCHEMA_RETIRE_BLOCKED` condition).
    Blocked {
        /// The nodes in the way, sorted by id.
        nodes_in_the_way: Vec<NodeId>,
    },
}

impl Retirement {
    /// The catalogue code of the amber condition when blocked.
    #[must_use]
    pub fn condition_code(&self) -> Option<&'static str> {
        match self {
            Self::Allowed => None,
            Self::Blocked { .. } => Some(SCHEMA_RETIRE_BLOCKED),
        }
    }
}

/// May the producer stop publishing `old_major` of `topic`? The new major is `old_major + 1`
/// (majors are consecutive, ADR-056). Allowed only when every `Active` node reports it can decode
/// it; suspended and other nodes do not block.
#[must_use]
pub fn can_retire(topic: &str, old_major: Major, fleet: &[NodeState]) -> Retirement {
    let new_major = old_major.saturating_add(1);
    let mut in_the_way: Vec<NodeId> = fleet
        .iter()
        .filter(|state| state.status == NodeStatus::Active)
        .filter(|state| !state.report.can_decode(topic, new_major))
        .map(|state| state.node.clone())
        .collect();
    if in_the_way.is_empty() {
        Retirement::Allowed
    } else {
        in_the_way.sort();
        in_the_way.dedup();
        Retirement::Blocked {
            nodes_in_the_way: in_the_way,
        }
    }
}

/// How the hub treats an image from a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Upward {
    /// Same major: accepted as is.
    Accepted,
    /// The previous major from an older node: accepted, and the image upcaster (ADR-061,
    /// WP-JRN-06.T1) must lift it from `from` to `to`.
    UpcastRequired {
        /// The node's major.
        from: Major,
        /// The hub's major.
        to: Major,
    },
    /// Older than the previous major, or newer than the hub's: not accepted.
    Refused,
}

/// Does the hub, whose current major for the topic is `hub_major`, accept an image in
/// `node_major`? Only the previous major is accepted from an older node.
#[must_use]
pub fn accepts_upward(hub_major: Major, node_major: Major) -> Upward {
    if node_major == hub_major {
        Upward::Accepted
    } else if node_major.checked_add(1) == Some(hub_major) {
        Upward::UpcastRequired {
            from: node_major,
            to: hub_major,
        }
    } else {
        Upward::Refused
    }
}
