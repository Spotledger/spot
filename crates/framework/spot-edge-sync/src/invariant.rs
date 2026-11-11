//! The invariant: the edge set always equals the link set, and the repair that fixes drift.
//!
//! [`check_invariant`] takes the documents a tenant holds and the edges its edge table holds and
//! reports every difference: an edge the documents imply but storage lacks (**missing**), an
//! edge storage holds that no document implies (**extra**), and an edge present on both sides
//! whose properties differ (**mismatch**). It is pure and total — it reads only its arguments,
//! never storage — so the property test can call it after every step and a background job can
//! call it over a page of documents.
//!
//! [`repair`] returns the [`EdgeChanges`] that would fix the drift. It never applies them: the
//! caller decides whether to write them, and does so through a writer in its own transaction.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::document::Document;

use crate::diff::EdgeChanges;
use crate::edge::{Edge, EdgeKey};
use crate::schema::{EdgeSchema, edges_of};

/// One difference between the documents' edge set and storage's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Drift {
    /// The documents imply this edge and storage does not hold it.
    Missing {
        /// The edge that should exist.
        expected: Box<Edge>,
    },
    /// Storage holds this edge and no document implies it.
    Extra {
        /// The edge that should not exist.
        found: Box<Edge>,
    },
    /// Both sides hold the edge and its properties differ.
    Mismatch {
        /// The shared identity.
        key: EdgeKey,
        /// The edge the documents imply.
        expected: Box<Edge>,
        /// The edge storage holds.
        found: Box<Edge>,
    },
}

impl Drift {
    /// The stable machine-readable code for this kind of drift.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Missing { .. } => "edge_sync.missing_edge",
            Self::Extra { .. } => "edge_sync.extra_edge",
            Self::Mismatch { .. } => "edge_sync.edge_mismatch",
        }
    }

    /// The edge's identity, whatever the kind of drift.
    #[must_use]
    pub fn key(&self) -> EdgeKey {
        match self {
            Self::Missing { expected } => expected.key(),
            Self::Extra { found } => found.key(),
            Self::Mismatch { key, .. } => key.clone(),
        }
    }
}

/// The drift between the edge set the documents imply and the edge set storage holds.
///
/// `docs` is every live document of the tenant (or the page being checked) and `edges` every
/// stored edge. Both are consumed into sorted maps, so the result is deterministic and sorted
/// by edge key.
#[must_use]
pub fn check_invariant<'a, D, E>(schema: &EdgeSchema, docs: D, edges: E) -> Vec<Drift>
where
    D: IntoIterator<Item = &'a Document>,
    E: IntoIterator<Item = Edge>,
{
    let desired = desired_edges(schema, docs);
    let actual: BTreeSet<Edge> = edges.into_iter().collect();
    let desired_by_key: BTreeMap<EdgeKey, &Edge> =
        desired.iter().map(|edge| (edge.key(), edge)).collect();
    let actual_by_key: BTreeMap<EdgeKey, &Edge> =
        actual.iter().map(|edge| (edge.key(), edge)).collect();
    let mut drift = Vec::new();
    for (key, expected) in &desired_by_key {
        match actual_by_key.get(key) {
            None => drift.push(Drift::Missing {
                expected: Box::new((*expected).clone()),
            }),
            Some(found) if found != expected => drift.push(Drift::Mismatch {
                key: key.clone(),
                expected: Box::new((*expected).clone()),
                found: Box::new((*found).clone()),
            }),
            Some(_) => {}
        }
    }
    for (key, found) in &actual_by_key {
        if !desired_by_key.contains_key(key) {
            drift.push(Drift::Extra {
                found: Box::new((*found).clone()),
            });
        }
    }
    drift.sort_by_key(Drift::key);
    drift
}

/// The change that would fix the drift: add what is missing, remove what is extra, and update
/// what mismatches. The caller applies it (through a writer, in its own transaction).
#[must_use]
pub fn repair<'a, D, E>(schema: &EdgeSchema, docs: D, edges: E) -> EdgeChanges
where
    D: IntoIterator<Item = &'a Document>,
    E: IntoIterator<Item = Edge>,
{
    let mut changes = EdgeChanges::default();
    for item in check_invariant(schema, docs, edges) {
        match item {
            Drift::Missing { expected } => {
                changes.added.insert(*expected);
            }
            Drift::Extra { found } => {
                changes.removed.insert(*found);
            }
            Drift::Mismatch {
                key,
                expected,
                found,
            } => changes.updated.push(crate::diff::EdgeUpdate {
                key,
                before: *found,
                after: *expected,
            }),
        }
    }
    changes
}

/// The union of every document's edge set.
fn desired_edges<'a, D>(schema: &EdgeSchema, docs: D) -> BTreeSet<Edge>
where
    D: IntoIterator<Item = &'a Document>,
{
    let mut desired = BTreeSet::new();
    for doc in docs {
        desired.extend(edges_of(schema, doc));
    }
    desired
}
