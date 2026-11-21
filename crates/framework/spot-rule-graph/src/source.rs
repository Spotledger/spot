//! The fact source seam: where the evaluator reads edges and field values.
//!
//! [`FactSource`] is the only thing the engine reads besides its compiled program. The
//! in-memory graph index (WP-GRA-02) backs the edge half behind the `graph-index` feature; a
//! storage-backed source answers the same trait from a transaction. [`MemoryFacts`] is the
//! source the corpus and property tests use.

use std::collections::BTreeMap;

use spot_storage_contract::{NodeRef, Value};

/// Where the engine reads its base facts.
///
/// Both methods return every matching fact; the engine deduplicates and sorts, so a source
/// need not.
pub trait FactSource {
    /// Every edge of `edge_type`, in storage orientation `(holder, target)`.
    fn edges(&self, edge_type: &str) -> Vec<(NodeRef, NodeRef)>;

    /// Every non-null value of `field` on records of `doctype`, as `(record, value)`.
    fn fields(&self, doctype: &str, field: &str) -> Vec<(NodeRef, Value)>;
}

/// An in-memory fact source: the corpus and property tests, and any host that holds a small
/// graph in memory.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MemoryFacts {
    edges: BTreeMap<String, Vec<(NodeRef, NodeRef)>>,
    fields: BTreeMap<(String, String), Vec<(NodeRef, Value)>>,
}

impl MemoryFacts {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one edge. Duplicates are ignored.
    pub fn add_edge(&mut self, edge_type: &str, from: NodeRef, to: NodeRef) {
        let list = self.edges.entry(edge_type.to_owned()).or_default();
        if !list.iter().any(|(f, t)| *f == from && *t == to) {
            list.push((from, to));
        }
    }

    /// Adds one field value. Duplicates are ignored.
    pub fn add_field(&mut self, doctype: &str, field: &str, record: NodeRef, value: Value) {
        let list = self
            .fields
            .entry((doctype.to_owned(), field.to_owned()))
            .or_default();
        if !list.iter().any(|(r, v)| *r == record && *v == value) {
            list.push((record, value));
        }
    }
}

impl FactSource for MemoryFacts {
    fn edges(&self, edge_type: &str) -> Vec<(NodeRef, NodeRef)> {
        let mut out = self.edges.get(edge_type).cloned().unwrap_or_default();
        out.sort();
        out
    }

    fn fields(&self, doctype: &str, field: &str) -> Vec<(NodeRef, Value)> {
        self.fields
            .get(&(doctype.to_owned(), field.to_owned()))
            .cloned()
            .unwrap_or_default()
    }
}
