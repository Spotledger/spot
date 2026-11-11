//! `spot-graph-query`: the typed, permission-aware graph query API (WS8, WP-GRA-03.T1, ADR-50).
//!
//! A query answers "what is connected to this record": start records, the edge types to follow
//! (by canonical name from the graph model), a direction per edge type, a depth, a node budget,
//! node and edge filters, and the fields to return. The caller learns only about records it may
//! read. A record it may not read is removed together with every path through it, and the result
//! says only that something was removed (`truncated_by_permission`).
//!
//! # Pieces
//!
//! - [`GraphQuery`] and its builder: validated at `build()`, every problem reported at once.
//! - [`GraphExecutor`]: the seam. [`StorageExecutor`] runs a query inside one transaction, over the
//!   storage traversal of `spot-storage-contract`. An in-memory index implements the same trait
//!   later; the `testing` feature's `contract::same_answers` checks that both give equal answers.
//! - [`GraphResult`]: the visible nodes and edges, with [`GraphResult::view`] giving a
//!   `spot_graph_algo::GraphView` for the algorithms.
//! - [`GraphQueryError`]: typed errors with stable `graphq.*` codes; storage failures pass through.
//!
//! # Permissions, exactly
//!
//! Every reached record is read through `PermissionEngine::can` with the `read` right. A walk
//! expands only visible records, so the edges of a hidden record are never followed. Fields are
//! cut with `field_access`: hidden fields are absent, masked fields carry their masked text. A
//! start record the caller cannot read gives the same answer as a missing one.
//!
//! # Not in this version
//!
//! Node and edge filters are evaluated over the caller's view of each record: a hidden field reads
//! as null and a masked field as its masked text, so a filter cannot reveal a hidden value. The
//! permission check of a node runs before the edge filter, so a denied node always sets the flag.
//!
//! Relation edges and multi-select edges are refused (`graphq.unsupported_edge`) until their
//! storage mapping exists. The storage executor's link edges carry no properties; an executor
//! whose [`NodeSource`] implements `edge_properties` (the in-memory index) returns them.
//!
//! # Edge properties (WP-GRA-03.T2)
//!
//! `GraphQuery::edge_var` names the edge variable; a filter field whose alias is that name reads
//! the crossed edge's properties (`e.qty >= 2`). `Include::edge_properties` returns them per hop
//! ([`PathHop`]), and `GraphQuery::rollup` multiplies one numeric property along each path.
//! Values are typed (null, bool, integer, decimal, text, date, link), never floats. A property the
//! edge type declares nothing about is a compile-time `graphq.unknown_edge_property`; when it
//! declares no properties at all, a missing property is the same code at run time. The rollup is
//! overflow-checked: integers with `checked_mul`; decimals multiply exactly, the scale of the
//! product is the sum of the scales, and a product that would need more than 28 fractional digits
//! or more than 96 bits is `graphq.rollup_overflow`, never a rounded or wrapped value. The rollup
//! follows the path of the walk (the smallest visible predecessor), so a part reachable by two
//! paths is multiplied along the one the walk keeps; it is a path product, not a sum over paths.
//!
//! # Summed rollup (WP-GRA-03.T3)
//!
//! `GraphQueryBuilder::rollup_sum` gives, per reached node, the sum over every simple path from a
//! start node of the product along the path (the BOM explosion), with the number of paths in
//! `GraphNode::path_count`. Only visible nodes and edges that pass the edge filter take part, so a
//! path through a hidden node contributes nothing. On a graph without cycles the sum is computed
//! per (node, path length), so it costs edges times depth, not the number of paths, and the count
//! may be astronomically large (`path_count_saturated`). The bound `max_paths` (default 10,000,
//! ceiling 1,000,000) limits the work: edge extensions evaluated there, enumerated simple paths on
//! a graph with a cycle (which is not an error, but cannot be memoised); past it the query fails
//! with `graphq.too_many_paths`, never with a partial sum.
//! Properties are shown only on edges between two visible nodes, so a hidden end hides the edge
//! and its properties. Text ordering in filters is code-point order, not the catalog collation.
//!
//! # Field-level permission on edge properties (WP-GRA-03.T4)
//!
//! An edge type that declares properties is backed by a relation DocType, whose fields can be
//! protected. [`EdgeFieldAccess`] answers per property for the caller; [`compile_with`] applies it.
//! A refused property is absent from the hop, and a filter or rollup that names it fails with
//! `graphq.unknown_edge_property`, in the same text as a name the edge type never had, so the
//! query cannot be used to find out that it exists. The default allows all; a production
//! implementer must override it.
//!
//! # Relation edges over storage (WP-GRA-03.T5)
//!
//! A relation edge is the pair of link fields of a relation DocType (a BOM line: assembly,
//! component, quantity). The storage executor reads the relation records that name the current
//! node (through the traversal, so `max_nodes` bounds them and too many is `TraversalTooLarge`),
//! keeps only those the caller may read (relation DocType and its link fields), and returns the
//! other end; the properties are the record's declared fields the caller may read. Two relation
//! records joining the same pair are `graphq.ambiguous_relation_edge`. A multi-select edge is
//! still unsupported.
//!
//! # Repeated lines of one pair (WP-GRA-03.T9)
//!
//! A relation spec may declare `aggregate = { property, how = "sum" }`: the edge type then treats
//! the lines of one pair as one edge whose property is their checked sum (`Int` or `Decimal`
//! arithmetic, `graphq.edge_value_overflow` on overflow). Every other property the caller can
//! read must be equal on the lines, else `graphq.ambiguous_relation_edge` names the property. The
//! merge is [`merge_lines`], the one function every executor uses. **The sum is the sum of the
//! lines the caller may read**: a line the caller cannot read is not added in, so the total is the
//! total of what the caller can see (the same rule that hides an edge to a hidden record), and
//! two callers may see different totals. `max_nodes` and `max_paths` bound lines, not merged
//! edges. A hot index holds one edge per pair and cannot tell which lines a caller may read, so
//! the index executor leaves a step of an aggregating edge type to the database path.
//!
//! `state` and `created_by` are read from the fields a caller names in [`FactFields`].

pub mod aggregate;
pub mod compile;
pub mod error;
pub mod exec;
pub mod filter;
mod paths;
pub mod query;
pub mod result;
pub mod storage;

#[cfg(feature = "testing")]
pub mod contract;

pub use aggregate::merge_lines;
pub use compile::{
    EdgeFieldAccess, RelationStep, Step, UnrestrictedEdgeFields, compile, compile_with,
};
pub use error::{GraphQueryError, QueryProblem};
pub use exec::{EdgeProps, GraphExecutor, Invocation, NodeData, NodeSource, walk};
pub use query::{
    DEFAULT_MAX_NODES, DEFAULT_MAX_PATHS, Direction, EdgeSelector, GraphQuery, GraphQueryBuilder,
    Include, MAX_DEPTH, MAX_NODES_CEILING, MAX_PATHS_CEILING, Order,
};
pub use result::{GraphEdge, GraphNode, GraphResult, PathHop, ResultGraph};
pub use storage::{FactFields, StorageExecutor};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-graph-query";
