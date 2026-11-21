//! The restored-node rule (ADR-066 section 4): when may a node continue incrementally, and when
//! must it bootstrap from a snapshot.
//!
//! The decision is a pure function over three small inputs, so it is a table in the tests. A node
//! that was restored from a backup, or a hub that was rolled back, must never be "continued": its
//! position may belong to a history that no longer exists.

use serde::{Deserialize, Serialize};

/// What the node reports about itself at the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// The node's identity (from its certificate, never from a request body).
    pub node_id: String,
    /// The tenant the node belongs to.
    pub tenant: String,
    /// The stream generation the node last applied from.
    pub generation: u64,
    /// The node's applied prefix, or `None` for a node with nothing (a new or wiped node).
    pub applied_position: Option<u64>,
}

/// The hub's stream as it is now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubHead {
    /// The stream's current generation.
    pub generation: u64,
    /// The last position the stream holds.
    pub head_position: u64,
}

/// Why a full bootstrap was chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartReason {
    /// The node holds nothing.
    NewNode,
    /// The node's generation is older than the hub's: the stream was forked or resynced since.
    StaleGeneration {
        /// The node's generation.
        node: u64,
        /// The hub's generation.
        hub: u64,
    },
    /// The node claims a generation the hub does not have (the hub was rolled back).
    GenerationAheadOfHub {
        /// The node's generation.
        node: u64,
        /// The hub's generation.
        hub: u64,
    },
    /// The node's applied position is past the hub's head (restored hub, or a restored node that
    /// reports a position from a different history).
    PositionAheadOfHub {
        /// The node's applied position.
        node: u64,
        /// The hub's head.
        hub: u64,
    },
    /// The node's position cannot advance.
    PositionOverflow,
}

/// What the node does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StartMode {
    /// Continue from `from` on the same stream. The catch-up planner then picks incremental or
    /// coalesced by cost.
    Incremental {
        /// The first position the node has not applied.
        from: u64,
    },
    /// Bootstrap from the snapshot taken at `snapshot_position`.
    Bootstrap {
        /// The newest snapshot at or below the hub's head.
        snapshot_position: u64,
        /// Why a continue is not safe.
        reason: StartReason,
    },
    /// A bootstrap is required but the hub holds no usable snapshot: the hub must take one first.
    /// The node is never continued meanwhile.
    NeedsSnapshot {
        /// Why a continue is not safe.
        reason: StartReason,
    },
}

/// Decide how a node starts its session.
///
/// `snapshot_positions` are the positions of the snapshots the hub holds for the node's scope in
/// the hub's current generation (already filtered to trusted ones by the caller). The newest one
/// at or below the head is chosen.
#[must_use]
pub fn decide_start(node: &NodeState, hub: HubHead, snapshot_positions: &[u64]) -> StartMode {
    let reason = match node.applied_position {
        None => Some(StartReason::NewNode),
        Some(_) if node.generation < hub.generation => Some(StartReason::StaleGeneration {
            node: node.generation,
            hub: hub.generation,
        }),
        Some(_) if node.generation > hub.generation => Some(StartReason::GenerationAheadOfHub {
            node: node.generation,
            hub: hub.generation,
        }),
        Some(applied) if applied > hub.head_position => Some(StartReason::PositionAheadOfHub {
            node: applied,
            hub: hub.head_position,
        }),
        Some(_) => None,
    };
    let Some(reason) = reason else {
        return match node.applied_position.and_then(|p| p.checked_add(1)) {
            Some(from) => StartMode::Incremental { from },
            None => needs(StartReason::PositionOverflow, hub, snapshot_positions),
        };
    };
    needs(reason, hub, snapshot_positions)
}

fn needs(reason: StartReason, hub: HubHead, snapshot_positions: &[u64]) -> StartMode {
    match snapshot_positions
        .iter()
        .copied()
        .filter(|p| *p <= hub.head_position)
        .max()
    {
        Some(snapshot_position) => StartMode::Bootstrap {
            snapshot_position,
            reason,
        },
        None => StartMode::NeedsSnapshot { reason },
    }
}
