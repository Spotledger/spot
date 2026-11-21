//! The lineage questions and their answers.
//!
//! [`LineageQuery`] is the named question a user asks of the business graph: the full document
//! chain, a batch traced forward and back, a bill of materials exploded with exact quantities,
//! and where an item is used. [`LineageResult`] is the typed answer: visible nodes and edges in
//! a deterministic order, per-level totals, and a reason when the walk was cut short.

use std::collections::BTreeMap;

use spot_graph_query::Direction;
use spot_storage_contract::{Name, NodeRef, RecordId, Value};

use crate::quantity::Quantity;

/// One question about the business graph.
///
/// The `direction` is the graph query API's direction: `In` follows from a record to the records
/// that link to it (forward along an order-to-cash chain, where each document links to its
/// predecessor), `Out` follows from a record to the records it links to (back along the chain),
/// and `Both` walks the whole chain.
#[derive(Clone, Debug, PartialEq)]
pub enum LineageQuery {
    /// The full document chain of one record of `doctype`, named `name`.
    Document {
        /// The DocType of the start record.
        doctype: Name,
        /// The start record's id.
        name: RecordId,
        /// Which way the chain is followed.
        direction: Direction,
        /// The deepest level returned.
        max_depth: u16,
    },
    /// A batch traced along its DocType's chain: forward to what consumed it, back to where it
    /// came from, or both.
    BatchTrace {
        /// The batch record.
        batch: NodeRef,
        /// Which way the batch is traced.
        direction: Direction,
    },
    /// A bill of materials exploded from an item: every component, with the quantity each level
    /// contributes, as exact decimals.
    BomExplosion {
        /// The item the explosion starts from.
        item: NodeRef,
        /// The quantity the trace starts from.
        qty_basis: Quantity,
        /// The deepest level returned.
        max_depth: u16,
    },
    /// Where an item is used: the records that consume it, transitively.
    WhereUsed {
        /// The item to look for.
        item: NodeRef,
    },
}

/// Why a lineage walk was cut short. The reason is reported, never the hidden content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Truncation {
    /// The walk stopped at the depth limit while more visible nodes were reachable.
    Depth {
        /// The depth the walk stopped at.
        max_depth: u16,
    },
    /// The node budget stopped the walk.
    Nodes {
        /// The budget that was reached.
        limit: u32,
    },
    /// A node the caller may not read was reached and removed. This is the graph query API's
    /// redaction marker: the node is neither returned nor counted, and this says only that
    /// something was removed.
    Permission,
}

/// One visible node of a lineage result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageNode {
    /// The record.
    pub node: NodeRef,
    /// Its distance from the start, within the nodes the caller may see.
    pub depth: u16,
    /// The node it was reached from and the edge type used, for depth above 0.
    pub via: Option<(NodeRef, String)>,
    /// The requested fields the caller may read, masked where the grant says so.
    pub fields: BTreeMap<Name, Value>,
    /// The quantity this node contributes, on a bill-of-materials trace.
    pub quantity: Option<Quantity>,
}

/// One followed edge between two returned nodes, in traversal orientation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineageEdge {
    /// The canonical edge-type name.
    pub edge: String,
    /// The node the traversal left.
    pub from: NodeRef,
    /// The node it reached.
    pub to: NodeRef,
}

/// The total of one level: how many visible nodes it holds, and their summed quantity when the
/// trace carries quantities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelTotal {
    /// The level's depth.
    pub depth: u16,
    /// How many visible nodes are at this depth. A node the caller may not read is not counted.
    pub nodes: u32,
    /// The summed quantity at this depth, on a bill-of-materials trace.
    pub quantity: Option<Quantity>,
}

/// The answer to a lineage question.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LineageResult {
    /// The visible nodes, ordered by depth then record.
    pub nodes: Vec<LineageNode>,
    /// The edges that reached each returned node, ordered by `(from, to, edge)`.
    pub edges: Vec<LineageEdge>,
    /// One total per level that holds a node, ordered by depth.
    pub levels: Vec<LevelTotal>,
    /// Why the walk was cut short, when it was. When more than one reason applies, the
    /// permission redaction is reported first, then the node budget, then the depth limit.
    pub truncated: Option<Truncation>,
}
