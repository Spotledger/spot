//! Graph traversal over links (ADR-50, WP-STO-07).
//!
//! Every link field is an edge, so relationships such as PO → receipt → invoice → payment,
//! party hierarchies or BOMs are walked here without hand-written recursive queries. v0.1
//! compiles to recursive queries on PostgreSQL; no graph extension is used (SPK-14 closed).
//! Permission filtering of reached nodes happens above storage, in the graph query API
//! (WP-GRA-03).

use std::future::Future;

use crate::error::StorageResult;
use crate::id::{Name, RecordId};

/// A record of some entity.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeRef {
    /// The entity.
    pub entity: Name,
    /// The record.
    pub id: RecordId,
}

/// An edge type: the link field `field` of `entity`, or the system column `parent` of a child
/// entity (child row → its document).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeSpec {
    /// The entity holding the link.
    pub entity: Name,
    /// The link field.
    pub field: Name,
}

/// Which way edges are followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// From the record holding the link to the record it points at.
    Outgoing,
    /// From a record to the records whose link points at it.
    Incoming,
    /// Both ways.
    Both,
}

/// A breadth-first walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Traversal {
    /// Starting records (depth 0). Missing ones are left out of the result.
    pub start: Vec<NodeRef>,
    /// Edge types that may be followed; at least one.
    pub edges: Vec<EdgeSpec>,
    /// Direction for every edge type.
    pub direction: Direction,
    /// Deepest level returned, 1 to 64.
    pub max_depth: u16,
    /// Most nodes the walk may reach, start nodes included; more fails with
    /// `TraversalTooLarge` instead of silently truncating.
    pub max_nodes: u32,
}

/// A node reached by a traversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reached {
    /// The record.
    pub node: NodeRef,
    /// Its smallest distance from a start node.
    pub depth: u16,
    /// For depth > 0: the smallest predecessor at `depth - 1` (by entity, then id) and the edge
    /// type used from it.
    pub via: Option<(NodeRef, EdgeSpec)>,
}

/// Traversal output.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TraversalResult {
    /// Every node once, at its smallest depth, ordered by depth, then entity, then id. Cycles
    /// terminate because a node is never visited twice.
    pub nodes: Vec<Reached>,
}

/// Runs traversals inside a transaction, seeing the transaction's own writes.
pub trait GraphTraversal: Send {
    /// Walks the graph. Edge specs must name link fields (or `parent` of a child entity) in the
    /// catalog; anything else fails with `InvalidQuery`.
    fn traverse(
        &mut self,
        traversal: &Traversal,
    ) -> impl Future<Output = StorageResult<TraversalResult>> + Send;
}
