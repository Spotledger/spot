//! One concrete edge: its type, its two ends and the properties a relation edge carries.
//!
//! An [`Edge`] is the *instance* of an edge type ([`EdgeTypeId`]) between two records
//! ([`NodeRef`]). It is fully ordered — the type id, then the source, then the target, then the
//! properties — so a [`BTreeSet<Edge>`] is a deterministic, comparable edge set and a diff over
//! two sets is stable for tests and for the invariant checker.
//!
//! Properties are [`PropValue`], an ordered projection of [`Value`] (storage values are only
//! `Eq`). The projection keeps decimals and timestamps as their exact canonical text, never a
//! float (ADR-12), and never reorders JSON semantically: a property that differs textually is a
//! property that differs.

use std::collections::BTreeMap;

use spot_storage_contract::{NodeRef, RecordId, Value};

/// An ordered projection of a stored [`Value`], for an edge property.
///
/// Decimals, dates, times and instants are kept as their canonical text: storage values carry
/// no `Ord` and a decimal must never become a float (ADR-12). Bytes stay bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PropValue {
    /// No value.
    Null,
    /// A boolean.
    Bool(bool),
    /// A 64-bit integer.
    Int(i64),
    /// An exact decimal, in its canonical text form.
    Decimal(String),
    /// Text.
    Text(String),
    /// A calendar date, in ISO form.
    Date(String),
    /// A wall-clock time, in ISO form.
    Time(String),
    /// An instant, in UTC ISO form.
    DateTime(String),
    /// A reference to another record.
    Id(RecordId),
    /// A JSON document as text (compared textually).
    Json(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
}

impl PropValue {
    /// Projects a stored value onto its ordered form.
    #[must_use]
    pub fn of(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int(v) => Self::Int(*v),
            Value::Decimal(v) => Self::Decimal(v.to_string()),
            Value::Text(v) => Self::Text(v.clone()),
            Value::Date(v) => Self::Date(v.to_string()),
            Value::Time(v) => Self::Time(v.to_string()),
            Value::DateTime(v) => Self::DateTime(v.to_string()),
            Value::Id(v) => Self::Id(*v),
            Value::Json(v) => Self::Json(v.clone()),
            Value::Bytes(v) => Self::Bytes(v.clone()),
            // ADR-087: no field kind produces these yet (WP-MET-05.T1); the exact debug text.
            Value::GeoPoint(_) | Value::Range(_) => Self::Text(format!("{value:?}")),
        }
    }

    /// Whether this is [`PropValue::Null`].
    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// The identity of an edge: its type, the record that owns it, and its two ends. Properties are
/// *not* part of it, so two edges with the same key but different properties are the same edge
/// **updated**.
///
/// The owner is the record whose write produced the edge. For every edge but a relation edge it
/// is the same as `from`; a relation DocType's edge is owned by the relation record, so two
/// relation records between the same pair are two edges (as two join rows are two edges), told
/// apart by their owner.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EdgeKey {
    /// The edge type's stable id.
    pub edge_type: spot_graph_model::EdgeTypeId,
    /// The record whose write produced the edge.
    pub owner: NodeRef,
    /// The source record.
    pub from: NodeRef,
    /// The target record.
    pub to: NodeRef,
}

/// One edge between two records, with the properties a relation edge carries.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Edge {
    /// The edge type's stable id (the graph model's [`EdgeTypeId`](spot_graph_model::EdgeTypeId)).
    pub edge_type: spot_graph_model::EdgeTypeId,
    /// The record whose write produced the edge (see [`EdgeKey::owner`]).
    pub owner: NodeRef,
    /// The source record (the record holding the link, or the parent of a child-table link).
    pub from: NodeRef,
    /// The target record.
    pub to: NodeRef,
    /// The edge's properties, by field name; empty for every edge that is not a relation edge.
    pub properties: BTreeMap<String, PropValue>,
}

impl Edge {
    /// An edge with no properties, owned by its source (every edge but a relation edge).
    #[must_use]
    pub fn new(edge_type: spot_graph_model::EdgeTypeId, from: NodeRef, to: NodeRef) -> Self {
        Self {
            edge_type,
            owner: from.clone(),
            from,
            to,
            properties: BTreeMap::new(),
        }
    }

    /// The same edge, owned by `owner` (a relation edge is owned by its relation record).
    #[must_use]
    pub fn owned_by(mut self, owner: NodeRef) -> Self {
        self.owner = owner;
        self
    }

    /// The edge's identity, without its properties.
    #[must_use]
    pub fn key(&self) -> EdgeKey {
        EdgeKey {
            edge_type: self.edge_type.clone(),
            owner: self.owner.clone(),
            from: self.from.clone(),
            to: self.to.clone(),
        }
    }

    /// The same edge with one property set.
    #[must_use]
    pub fn with_property(mut self, name: impl Into<String>, value: PropValue) -> Self {
        self.properties.insert(name.into(), value);
        self
    }
}

impl std::fmt::Display for Edge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{} -> {}:{}",
            self.from.entity.as_str(),
            self.from.id,
            self.to.entity.as_str(),
            self.to.id
        )
    }
}
