//! Where edges go: the object-safe [`EdgeWriter`], [`apply_in`], and the in-memory writer.
//!
//! The trait is the only place an edge is ever written. A backend implements it over its own
//! transaction ([`crate::apply_in`] then writes the whole change in the caller's transaction, so
//! the document and its edges commit or roll back together); the in-memory
//! [`MemoryEdgeWriter`] is the reference the tests and the invariant checker use.
//!
//! [`EdgeWriter::scan_by_source`] reads the edges leaving one record. It is the bounded read the
//! hot path uses — a document change touches only its own source — so nothing here ever scans
//! the whole edge table.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;

use spot_storage_contract::NodeRef;

use crate::diff::EdgeChanges;
use crate::edge::{Edge, EdgeKey};
use crate::error::EdgeResult;

/// The boxed, `Send` future every [`EdgeWriter`] method returns, so the trait stays object-safe
/// and a backend can be used through `&mut dyn EdgeWriter`.
pub type EdgeFuture<'a, T> = Pin<Box<dyn Future<Output = EdgeResult<T>> + Send + 'a>>;

/// The one way an edge is written.
///
/// Implementations are bound to a transaction: the PostgreSQL writer (WP-DAG-04.T2) holds the
/// caller's `PgTx`, so a change applied through it commits with the document write. The
/// methods take `&mut self` and return boxed futures, which keeps the trait object-safe without
/// this crate depending on any backend.
pub trait EdgeWriter: Send {
    /// Inserts an edge. Writing an edge that already exists replaces it.
    fn add<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()>;

    /// Removes an edge by its key.
    fn remove<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()>;

    /// Replaces the properties of the edge with this edge's key. The edge is written whole, so
    /// a backend may implement this as an upsert; a missing edge is an add.
    fn update<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()>;

    /// Every edge owned by `source`, in edge-key order. `source` is the record whose write
    /// produced the edges: a document for its own links, the parent for a child-table link, and
    /// the relation record for a relation edge. This is the bounded read the hot path uses, so
    /// nothing here ever scans the whole edge table.
    fn scan_by_source<'a>(&'a mut self, source: &'a NodeRef) -> EdgeFuture<'a, Vec<Edge>>;
}

/// Applies a change through a writer: removes first, then updates, then adds.
///
/// The order is deterministic and safe for a backend that enforces a unique key on
/// `(edge_type, from, to)`: a remove frees the key before an add reuses it.
pub fn apply_in<'a>(
    writer: &'a mut dyn EdgeWriter,
    changes: &'a EdgeChanges,
) -> EdgeFuture<'a, ()> {
    Box::pin(async move {
        for edge in &changes.removed {
            writer.remove(edge).await?;
        }
        for update in &changes.updated {
            writer.update(&update.after).await?;
        }
        for edge in &changes.added {
            writer.add(edge).await?;
        }
        Ok(())
    })
}

/// An in-memory [`EdgeWriter`]: the reference the tests and the invariant checker use.
///
/// It indexes by owner, so [`MemoryEdgeWriter::scan_by_source`] is the same bounded read a
/// backend serves with an index on the owner columns. Cloning shares nothing: a clone is an
/// independent edge set.
#[derive(Clone, Debug, Default)]
pub struct MemoryEdgeWriter {
    by_owner: BTreeMap<NodeRef, BTreeMap<EdgeKey, Edge>>,
}

impl MemoryEdgeWriter {
    /// An empty writer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A writer holding `edges`.
    #[must_use]
    pub fn from_edges(edges: impl IntoIterator<Item = Edge>) -> Self {
        let mut writer = Self::new();
        for edge in edges {
            writer
                .by_owner
                .entry(edge.owner.clone())
                .or_default()
                .insert(edge.key(), edge);
        }
        writer
    }

    /// Every edge, sorted.
    #[must_use]
    pub fn all(&self) -> BTreeSet<Edge> {
        self.by_owner
            .values()
            .flat_map(|edges| edges.values().cloned())
            .collect()
    }

    /// How many edges are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_owner.values().map(BTreeMap::len).sum()
    }

    /// Whether nothing is stored.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The stored edge with `key`, when there is one.
    #[must_use]
    pub fn get(&self, key: &EdgeKey) -> Option<&Edge> {
        self.by_owner
            .get(&key.owner)
            .and_then(|edges| edges.get(key))
    }
}

impl MemoryEdgeWriter {
    /// Stores `edge`, replacing the edge with the same key.
    pub fn insert(&mut self, edge: &Edge) {
        self.by_owner
            .entry(edge.owner.clone())
            .or_default()
            .insert(edge.key(), edge.clone());
    }

    /// Removes the edge with `edge`'s key, if stored.
    pub fn delete(&mut self, edge: &Edge) {
        let mut empty = false;
        if let Some(edges) = self.by_owner.get_mut(&edge.owner) {
            edges.remove(&edge.key());
            empty = edges.is_empty();
        }
        if empty {
            self.by_owner.remove(&edge.owner);
        }
    }

    /// Every edge owned by `source`, in edge-key order.
    #[must_use]
    pub fn edges_owned_by(&self, source: &NodeRef) -> Vec<Edge> {
        self.by_owner
            .get(source)
            .map(|edges| edges.values().cloned().collect())
            .unwrap_or_default()
    }
}

impl EdgeWriter for MemoryEdgeWriter {
    fn add<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async move {
            self.insert(edge);
            Ok(())
        })
    }

    fn remove<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        Box::pin(async move {
            self.delete(edge);
            Ok(())
        })
    }

    fn update<'a>(&'a mut self, edge: &'a Edge) -> EdgeFuture<'a, ()> {
        self.add(edge)
    }

    fn scan_by_source<'a>(&'a mut self, source: &'a NodeRef) -> EdgeFuture<'a, Vec<Edge>> {
        Box::pin(async move { Ok(self.edges_owned_by(source)) })
    }
}
