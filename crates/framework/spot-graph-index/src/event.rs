//! Edge events and the vocabulary the index applies them in.
//!
//! An [`EdgeEvent`] is one change to one edge, carrying the **sequence number** of the event
//! stream it came from. The index applies events in sequence: a duplicate or an event older than
//! the watermark is ignored (idempotent redelivery), the next sequence is applied, and a hole
//! marks the graph stale rather than guessing.
//!
//! The event's shape is this crate's contract for the decoder. The outbox topic and payload
//! schema for edge events are not yet in `contracts/events`; the projector (WP-GRA-07) decodes an
//! `OutboxRecord`'s position into [`EdgeEvent::sequence`] and its payload into the rest. See the
//! PR's Open questions.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_storage_contract::{NodeRef, Value};

use crate::key::GraphKey;

/// What an edge event does to an edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeOp {
    /// The edge now exists (the holder's link field points at the target).
    Add,
    /// The edge no longer exists (the link field was cleared, or the holder was deleted).
    Remove,
    /// The edge's properties changed. A missing edge is created, so a redelivered update is
    /// idempotent and never lost.
    Update,
}

impl EdgeOp {
    /// The operation's stable text tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Remove => "remove",
            Self::Update => "update",
        }
    }
}

/// The identity of one edge: its canonical edge-type name and its two ends.
///
/// The ends are in **storage orientation**: `from` is the record holding the link (the storage
/// `EdgeSpec`'s entity) and `to` is the record it points at. That is the orientation the storage
/// traversal and the graph query API use, so a hot answer and a database answer agree.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKey {
    /// The canonical edge-type name from the graph model, for example `invoice.grn`.
    pub edge_type: String,
    /// The record holding the link.
    pub from: NodeRef,
    /// The record the link points at.
    pub to: NodeRef,
}

impl EdgeKey {
    /// An edge identity.
    #[must_use]
    pub fn new(edge_type: impl Into<String>, from: NodeRef, to: NodeRef) -> Self {
        Self {
            edge_type: edge_type.into(),
            from,
            to,
        }
    }
}

/// One edge with its typed properties.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Edge {
    /// The edge's identity.
    pub key: EdgeKey,
    /// The edge's properties, by name. Empty for link, table and dynamic-link edges; a relation
    /// edge carries its relation record's other fields.
    pub properties: BTreeMap<String, Value>,
}

impl Edge {
    /// An edge with no properties.
    #[must_use]
    pub fn bare(key: EdgeKey) -> Self {
        Self {
            key,
            properties: BTreeMap::new(),
        }
    }
}

/// One change to one edge, in the event stream's order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeEvent {
    /// The event stream's sequence number. The index applies strictly increasing sequences and
    /// treats a duplicate or an older one as already applied.
    pub sequence: u64,
    /// The graph the event belongs to.
    pub graph: GraphKey,
    /// What happened.
    pub op: EdgeOp,
    /// The edge the event is about.
    pub edge: EdgeKey,
    /// The edge's properties after the event (for [`EdgeOp::Add`] and [`EdgeOp::Update`]); ignored
    /// for [`EdgeOp::Remove`].
    pub properties: BTreeMap<String, Value>,
}

impl EdgeEvent {
    /// An event with no properties.
    #[must_use]
    pub fn bare(sequence: u64, graph: GraphKey, op: EdgeOp, edge: EdgeKey) -> Self {
        Self {
            sequence,
            graph,
            op,
            edge,
            properties: BTreeMap::new(),
        }
    }
}

/// What applying an event did, so a caller can tell progress from a no-op.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Applied {
    /// The edge did not exist and now does.
    Added,
    /// The edge existed and is gone.
    Removed,
    /// The edge existed and its properties changed.
    Updated,
    /// The event was already reflected (a duplicate, or the same state again): nothing changed.
    Unchanged,
    /// The event was older than the watermark, or the graph is stale: it was not applied.
    Ignored,
}

impl Applied {
    /// The outcome's stable text tag.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Added => "added",
            Self::Removed => "removed",
            Self::Updated => "updated",
            Self::Unchanged => "unchanged",
            Self::Ignored => "ignored",
        }
    }

    /// Whether the index changed.
    #[must_use]
    pub fn changed(self) -> bool {
        matches!(self, Self::Added | Self::Removed | Self::Updated)
    }
}
