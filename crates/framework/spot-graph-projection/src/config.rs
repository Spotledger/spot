//! The projection config and the permission model.
//!
//! The projection runs as the platform's **system principal**, so it is allowed to read every
//! field — but a projection is not a reader. It carries **only the fields the config lists** for
//! each edge type, and it drops any field the metadata marks **sensitive** (a field in a
//! protection group, or one carrying a classification tag such as `secret` or `pii`) even when
//! the config lists it. So a sensitive field cannot reach an external graph engine **by
//! construction**: no grant, no principal and no configuration mistake opens it.
//!
//! [`MetaSensitivity`] computes the sensitive set from
//! [`EffectiveMeta::field_protection`](spot_meta::EffectiveMeta::field_protection); a
//! declassified field is not sensitive. [`NothingSensitive`] is for tests that isolate the
//! allow-list rule.

use std::collections::{BTreeMap, BTreeSet};

use spot_graph_index::{EdgeEvent, GraphKey};
use spot_meta::EffectiveMeta;
use spot_storage_contract::Value;

/// Whether the metadata marks a field sensitive.
pub trait FieldSensitivity: Send + Sync {
    /// Whether `doctype`'s `field` must never leave the platform in a projection.
    fn is_sensitive(&self, doctype: &str, field: &str) -> bool;
}

/// Nothing is sensitive: the rule under test is the allow-list alone.
#[derive(Debug, Default, Clone, Copy)]
pub struct NothingSensitive;

impl FieldSensitivity for NothingSensitive {
    fn is_sensitive(&self, _doctype: &str, _field: &str) -> bool {
        false
    }
}

/// The sensitive fields of the effective metadata: those in a protection group, or carrying a
/// classification tag, unless the metadata deliberately declassifies them.
#[derive(Debug, Default, Clone)]
pub struct MetaSensitivity {
    sensitive: BTreeMap<String, BTreeSet<String>>,
}

impl MetaSensitivity {
    /// Computes the sensitive set of `meta`.
    #[must_use]
    pub fn from_meta(meta: &EffectiveMeta) -> Self {
        let declassified: BTreeSet<&str> = meta
            .declassifications()
            .iter()
            .map(|(path, _)| path.as_str())
            .collect();
        let mut sensitive: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for protection in meta.field_protection() {
            if declassified.contains(protection.path.as_str()) {
                continue;
            }
            if protection.groups.is_empty() && protection.tags.is_empty() {
                continue;
            }
            if let Some((doctype, field)) = protection.path.split_once('.') {
                sensitive
                    .entry(doctype.to_owned())
                    .or_default()
                    .insert(field.to_owned());
            }
        }
        Self { sensitive }
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sensitive.is_empty()
    }
}

impl FieldSensitivity for MetaSensitivity {
    fn is_sensitive(&self, doctype: &str, field: &str) -> bool {
        self.sensitive
            .get(doctype)
            .is_some_and(|fields| fields.contains(field))
    }
}

/// The fields one edge type carries, and the DocType whose metadata guards them.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EdgeFields {
    /// The DocType the edge's properties come from (a relation DocType, for a relation edge).
    doctype: String,
    /// The allow-listed property names, or `None` to allow every field the metadata does not
    /// mark sensitive.
    fields: Option<BTreeSet<String>>,
}

/// What one projection carries: one named graph, and per edge type the fields to project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionConfig {
    graph: GraphKey,
    edges: BTreeMap<String, EdgeFields>,
}

impl ProjectionConfig {
    /// A config for `graph` that carries no edge properties until an edge type is added.
    #[must_use]
    pub fn new(graph: GraphKey) -> Self {
        Self {
            graph,
            edges: BTreeMap::new(),
        }
    }

    /// Allows `edge_type`'s properties from `doctype`, keeping only the listed `fields` (and
    /// still dropping sensitive ones).
    #[must_use]
    pub fn with_edge(
        mut self,
        edge_type: impl Into<String>,
        doctype: impl Into<String>,
        fields: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.edges.insert(
            edge_type.into(),
            EdgeFields {
                doctype: doctype.into(),
                fields: Some(fields.into_iter().map(Into::into).collect()),
            },
        );
        self
    }

    /// Allows `edge_type`'s properties from `doctype`, every field the metadata does not mark
    /// sensitive.
    #[must_use]
    pub fn with_all_fields(
        mut self,
        edge_type: impl Into<String>,
        doctype: impl Into<String>,
    ) -> Self {
        self.edges.insert(
            edge_type.into(),
            EdgeFields {
                doctype: doctype.into(),
                fields: None,
            },
        );
        self
    }

    /// The graph this projection feeds.
    #[must_use]
    pub fn graph(&self) -> &GraphKey {
        &self.graph
    }

    /// Whether `edge_type` carries `field`: it must be configured, listed when the edge type
    /// lists fields, and not sensitive.
    #[must_use]
    pub fn carries(
        &self,
        edge_type: &str,
        field: &str,
        sensitivity: &dyn FieldSensitivity,
    ) -> bool {
        let Some(entry) = self.edges.get(edge_type) else {
            return false;
        };
        let listed = entry
            .fields
            .as_ref()
            .is_none_or(|fields| fields.contains(field));
        listed && !sensitivity.is_sensitive(&entry.doctype, field)
    }

    /// Projects one edge event's properties through the config. The edge itself is unchanged.
    #[must_use]
    pub fn project(&self, event: &EdgeEvent, sensitivity: &dyn FieldSensitivity) -> EdgeEvent {
        let mut projected = event.clone();
        projected.properties =
            self.project_properties(&event.edge.edge_type, &event.properties, sensitivity);
        projected
    }

    /// Keeps the carried fields of a property map, in key order.
    #[must_use]
    pub fn project_properties(
        &self,
        edge_type: &str,
        properties: &BTreeMap<String, Value>,
        sensitivity: &dyn FieldSensitivity,
    ) -> BTreeMap<String, Value> {
        properties
            .iter()
            .filter(|(field, _)| self.carries(edge_type, field, sensitivity))
            .map(|(field, value)| (field.clone(), value.clone()))
            .collect()
    }
}
