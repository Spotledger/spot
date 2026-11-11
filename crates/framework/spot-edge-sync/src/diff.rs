//! The change between two edge sets, and the document-level diff that produces it.
//!
//! [`EdgeChanges::between`] compares two sets by [`EdgeKey`] (type and ends): a key only in the
//! new set is an **add**, a key only in the old set a **remove**, and a key in both whose
//! properties differ an **update**. [`diff`] is the same over the edge sets of two documents,
//! which is what a lifecycle operation knows: the document as it was loaded and as it was
//! written.
//!
//! The four lifecycle transitions all map onto the same function:
//!
//! - **create** — `old = None`, every edge is added;
//! - **update** — link values that changed produce removes and adds; a changed relation
//!   property produces an update;
//! - **delete** — `new = None`, every edge is removed;
//! - **cancel** — the links do not change, so the change is empty (the edge set follows the
//!   link set, and a cancelled document still holds its links).
//!
//! The order of the three lists is deterministic (sorted), so the same change always prints and
//! compares the same.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::document::Document;

use crate::edge::{Edge, EdgeKey};
use crate::schema::{EdgeSchema, edges_of};

/// One edge whose properties changed: the same key, a different edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeUpdate {
    /// The identity that stayed the same.
    pub key: EdgeKey,
    /// The edge as it was.
    pub before: Edge,
    /// The edge as it must be.
    pub after: Edge,
}

/// What one document change does to the edge set.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct EdgeChanges {
    /// Edges that must be added, sorted.
    pub added: BTreeSet<Edge>,
    /// Edges that must be removed, sorted.
    pub removed: BTreeSet<Edge>,
    /// Edges whose properties changed, sorted by key.
    pub updated: Vec<EdgeUpdate>,
}

impl EdgeChanges {
    /// The change from `before` to `after`, by edge key.
    #[must_use]
    pub fn between(before: &BTreeSet<Edge>, after: &BTreeSet<Edge>) -> Self {
        let before_by_key: BTreeMap<EdgeKey, &Edge> =
            before.iter().map(|edge| (edge.key(), edge)).collect();
        let after_by_key: BTreeMap<EdgeKey, &Edge> =
            after.iter().map(|edge| (edge.key(), edge)).collect();
        let mut changes = Self::default();
        for (key, edge) in &after_by_key {
            match before_by_key.get(key) {
                None => {
                    changes.added.insert((*edge).clone());
                }
                Some(old) if *old != *edge => changes.updated.push(EdgeUpdate {
                    key: key.clone(),
                    before: (*old).clone(),
                    after: (*edge).clone(),
                }),
                Some(_) => {}
            }
        }
        for (key, edge) in &before_by_key {
            if !after_by_key.contains_key(key) {
                changes.removed.insert((*edge).clone());
            }
        }
        changes
    }

    /// Whether the change touches nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.updated.is_empty()
    }

    /// How many individual changes there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.added.len() + self.removed.len() + self.updated.len()
    }

    /// Applies the change to an in-memory edge set (the reference behaviour the writers follow).
    pub fn apply_to(&self, edges: &mut BTreeSet<Edge>) {
        for edge in &self.removed {
            edges.remove(edge);
        }
        for update in &self.updated {
            edges.retain(|edge| edge.key() != update.key);
            edges.insert(update.after.clone());
        }
        for edge in &self.added {
            edges.insert(edge.clone());
        }
    }
}

/// The change to the edge set between two states of one document.
///
/// `old` is the document as loaded (`None` for a create), `new` the document as written (`None`
/// for a delete). Both are read through `schema`, so the result is the same however often it is
/// computed.
#[must_use]
pub fn diff(schema: &EdgeSchema, old: Option<&Document>, new: Option<&Document>) -> EdgeChanges {
    let before = old.map_or_else(BTreeSet::new, |doc| edges_of(schema, doc));
    let after = new.map_or_else(BTreeSet::new, |doc| edges_of(schema, doc));
    EdgeChanges::between(&before, &after)
}
