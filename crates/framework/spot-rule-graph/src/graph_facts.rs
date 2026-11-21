//! The `FactSource` adapter over the in-memory graph index (feature `graph-index`, WP-GRA-02).
//!
//! The index holds a hot graph's **structure** — which records are connected by which edge
//! types — so it answers the edge half of [`FactSource`] without touching storage. A record's
//! field values still come from elsewhere (the index stores none), so the adapter takes a
//! [`FieldFacts`] for the field half; a caller with no field predicates passes [`NoFields`].
//!
//! This mirrors `spot-graph-index`'s own hot/cold split: the index shortens the walk, the value
//! source answers values, and a stale index is never read.

use spot_graph_index::GraphIndex;
use spot_storage_contract::{NodeRef, Value};

use crate::source::{FactSource, MemoryFacts};

/// Where the adapter reads field values (the index holds none).
pub trait FieldFacts {
    /// Every non-null value of `field` on records of `doctype`, as `(record, value)`.
    fn fields(&self, doctype: &str, field: &str) -> Vec<(NodeRef, Value)>;
}

/// A field source with no values: a program with edge predicates only.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoFields;

impl FieldFacts for NoFields {
    fn fields(&self, _doctype: &str, _field: &str) -> Vec<(NodeRef, Value)> {
        Vec::new()
    }
}

/// The single [`NoFields`] instance the edge-only constructor borrows.
static NO_FIELDS: NoFields = NoFields;

/// A [`FactSource`] whose edges come from a hot [`GraphIndex`] and whose fields come from a
/// [`FieldFacts`].
pub struct GraphIndexFacts<'a> {
    index: &'a GraphIndex,
    fields: &'a dyn FieldFacts,
}

impl<'a> GraphIndexFacts<'a> {
    /// Edges from `index`, fields from `fields`.
    #[must_use]
    pub fn new(index: &'a GraphIndex, fields: &'a dyn FieldFacts) -> Self {
        Self { index, fields }
    }

    /// Edges from `index`, no field values.
    #[must_use]
    pub fn edges_only(index: &'a GraphIndex) -> Self {
        Self {
            index,
            fields: &NO_FIELDS,
        }
    }
}

impl FactSource for GraphIndexFacts<'_> {
    fn edges(&self, edge_type: &str) -> Vec<(NodeRef, NodeRef)> {
        let mut out: Vec<(NodeRef, NodeRef)> = self
            .index
            .edges()
            .filter(|edge| edge.key.edge_type == edge_type)
            .map(|edge| (edge.key.from, edge.key.to))
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn fields(&self, doctype: &str, field: &str) -> Vec<(NodeRef, Value)> {
        self.fields.fields(doctype, field)
    }
}

impl FieldFacts for MemoryFacts {
    fn fields(&self, doctype: &str, field: &str) -> Vec<(NodeRef, Value)> {
        <MemoryFacts as FactSource>::fields(self, doctype, field)
    }
}
