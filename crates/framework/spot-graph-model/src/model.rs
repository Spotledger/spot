//! The model types: [`GraphModel`], its [`NodeType`]s and [`EdgeType`]s.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::diff::ModelDiff;
use crate::error::Problem;
use crate::registry::EdgeTypeRegistry;

/// The canonical name of a Table field's `contains` edge.
///
/// Two Table fields of one parent cannot share a child (spot-meta validation:
/// a child DocType is used by exactly one parent's field), so parent and child
/// names alone are unambiguous.
#[must_use]
pub fn contains_name(from: &str, to: &str) -> String {
    format!("{from}_contains_{to}")
}

/// Ids hash a domain tag plus the name, so a node id and an edge id can never
/// collide even for the same text; both are SHA-256 over the tagged bytes.
fn id_hex(domain: &str, name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"spot-graph-model/");
    hasher.update(domain.as_bytes());
    hasher.update([0]);
    hasher.update(name.as_bytes());
    let digest = hasher.finalize();
    hex(&digest)
}

/// Lowercase hex of the digest, as `spot-dag`'s content hashes print.
fn hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// The SHA-256 of the id's canonical bytes, without the domain tag.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

/// One derived model: node types and edge types, keyed by name.
///
/// Immutable once built; rebuild instead of editing
/// ([`GraphModel::derive`]). Equal metadata yields an equal model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphModel {
    /// The metadata version the model was derived from.
    ///
    /// Informational only: it rides along for logs, diagnostics and the text
    /// dump, and **never** decides that anything changed. Two different
    /// metadata sets can carry the same version (every fresh
    /// `spot_meta::Registry` reports 1), so change is [`GraphModel::diff`],
    /// which compares node and edge content and ignores this field.
    pub meta_version: u64,
    /// The node types, by DocType name; one per non-child DocType that no
    /// relation spec names.
    pub nodes: BTreeMap<String, NodeType>,
    /// The edge types, by canonical name.
    pub edges: BTreeMap<String, EdgeType>,
    /// The problems the derive collected, in walk order
    /// ([`GraphModel::problems`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) derive_problems: Vec<Problem>,
}

impl GraphModel {
    /// Derives the graph model from the effective metadata, with no relation
    /// specs (the shape most callers want).
    #[must_use]
    pub fn derive(meta: &spot_meta::EffectiveMeta) -> Self {
        Self::derive_relations(meta, &[])
    }

    /// Derives the graph model from the effective metadata and the relation
    /// specs. Deterministic: the result is equal whatever order the
    /// metadata's definitions or the specs come in (they are matched by name,
    /// and the maps sort).
    ///
    /// A relation DocType named in the specs is **removed as a node** and
    /// answers `node()` with `None`; its edge lives in
    /// [`GraphModel::edges`]. An ignored spec (missing DocType or fields)
    /// degrades to the plain walk for that DocType, so its own Link, Table
    /// and DynamicLink edges still exist, and the reason is in
    /// [`GraphModel::problems`].
    #[must_use]
    pub fn derive_relations(meta: &spot_meta::EffectiveMeta, relations: &[RelationSpec]) -> Self {
        let mut model = Self {
            meta_version: meta.version(),
            nodes: BTreeMap::new(),
            edges: BTreeMap::new(),
            derive_problems: Vec::new(),
        };
        crate::derive::walk(&mut model, meta, relations);
        model
    }

    /// The node type of a DocType, `None` for children and relation DocTypes.
    #[must_use]
    pub fn node(&self, doctype: &str) -> Option<&NodeType> {
        self.nodes.get(doctype)
    }

    /// The edge type by canonical name.
    #[must_use]
    pub fn edge(&self, name: &str) -> Option<&EdgeType> {
        self.edges.get(name)
    }

    /// The model's edge-type registry (freshly built on every call).
    #[must_use]
    pub fn registry(&self) -> EdgeTypeRegistry {
        EdgeTypeRegistry::new(self)
    }

    /// The diff against a previous model; see [`crate::ModelDiff`].
    #[must_use]
    pub fn diff(&self, old: &GraphModel) -> ModelDiff {
        ModelDiff::between(old, self)
    }

    /// Every problem the derive collected, in walk (deterministic) order:
    /// dangling link targets, ignored relation specs, duplicate names.
    ///
    /// A problem never blocks the model: the edges that could be derived
    /// exist, the ones that could not are listed here. Callers match on
    /// [`Problem::code`], never on the message.
    #[must_use]
    pub fn problems(&self) -> &[Problem] {
        &self.derive_problems
    }

    pub(crate) fn extend_problems(&mut self, problems: Vec<Problem>) {
        self.derive_problems.extend(problems);
    }
}

/// A node type: one non-child DocType of the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeType {
    /// The DocType's name (its key in the map).
    pub name: String,
    /// The stable id of this node type, a hash of its canonical name.
    pub node_id: NodeTypeId,
    /// The DocType's label, or its name when it has none.
    pub label: String,
    /// Whether the DocType is posted (`is_posted` on the definition).
    pub is_posted: bool,
}

/// The id of a node type: SHA-256 over a `node`-domain-tagged name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeTypeId(String);

impl NodeTypeId {
    /// The id of the node type named `name` (the DocType's name).
    #[must_use]
    pub fn of(name: &str) -> Self {
        Self(id_hex("node", name))
    }

    /// The id as its 64 lowercase hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for NodeTypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl NodeType {
    /// Builds the node type from a (non-child) definition.
    #[must_use]
    pub(crate) fn from_definition(def: &spot_meta::Definition) -> Self {
        Self {
            name: def.name.clone(),
            node_id: NodeTypeId::of(&def.name),
            label: def.label_or_name().to_owned(),
            is_posted: def.is_posted,
        }
    }
}

/// The id of an edge type: the SHA-256 of its canonical name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EdgeTypeId(String);

impl EdgeTypeId {
    /// The id of the edge type with the canonical name `name`.
    #[must_use]
    pub fn of(canonical_name: &str) -> Self {
        Self(id_hex("edge", canonical_name))
    }

    /// The id as its 64 lowercase hex characters.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EdgeTypeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How many targets one edge's source row may carry, as the brief fixes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Cardinality {
    /// A `Link` field: many records of `from`, each pointing at one record of
    /// `to`.
    ManyToOne,
    /// A `Table` field: one `from` record owning many child rows of `to`.
    OneToMany,
    /// A `TableMultiselect` field: many `from` records each picking many
    /// records of `to`, through the child table's single link field.
    ManyToMany,
    /// A `DynamicLink` field: a link whose target DocType is named per record.
    DynamicLinkManyToOne,
    /// A relation edge: the `from_field` link is the owner side.
    RelationManyToOne,
}

impl Cardinality {
    /// The cardinality's text, as [`to_text`](GraphModel::to_text) prints it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ManyToOne => "many_to_one",
            Self::OneToMany => "one_to_many",
            Self::ManyToMany => "many_to_many",
            Self::DynamicLinkManyToOne => "dynamic_link_many_to_one",
            Self::RelationManyToOne => "relation_many_to_one",
        }
    }
}

/// Where an edge type came from, as the brief fixes the wording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeSource {
    /// A `Link` field.
    LinkField,
    /// A `Table` field (owned containment).
    Table,
    /// A `TableMultiselect` field (a multi-pick of a master).
    TableMultiselect,
    /// A `DynamicLink` field.
    DynamicLink,
    /// A relation DocType spec.
    Relation,
}

impl EdgeSource {
    /// The source's stable text tag, as the brief's wording gives it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LinkField => "link_field",
            Self::Table => "table",
            Self::TableMultiselect => "table_multiselect",
            Self::DynamicLink => "dynamic_link",
            Self::Relation => "relation",
        }
    }
}

/// Who owns what an edge connects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Ownership {
    /// The `from` record owns the `to` rows: they exist only inside it
    /// (Table edges).
    Owned,
    /// A reference: the `from` row points at an independent record (every
    /// link-shaped edge).
    Ref,
}

impl Ownership {
    /// The ownership's text, as `to_text` prints it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owned => "owned",
            Self::Ref => "ref",
        }
    }
}

/// The shape of an edge type's target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetShape {
    /// Every target record is of the one DocType.
    One(String),
    /// The field can name any of these DocTypes (a dynamic link over a select
    /// type field).
    AnyOf(BTreeSet<String>),
    /// The target DocTypes cannot be enumerated at model time (a dynamic link
    /// over a free-text type field).
    Dynamic,
}

/// How an owner names its targets; see [`TargetShape::AnyOf`].
///
/// A named constructor shape, so callers can build the target without
/// spelling the set type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnyOf {
    /// The DocTypes the field may name.
    pub any_of: BTreeSet<String>,
}

impl From<AnyOf> for TargetShape {
    fn from(any_of: AnyOf) -> Self {
        Self::AnyOf(any_of.any_of)
    }
}

/// What an edge property's field holds, in the terms this crate needs (the
/// full metadata shape stays in `spot-meta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PropertyKind {
    /// Text, long text or select: a text-shaped property.
    Text,
    /// A link field.
    Link,
    /// A dynamic link field.
    DynamicLink,
    /// A table field (this crate does not recurse into it).
    Table,
    /// A bool, int, decimal, date, time, datetime, JSON or bytes field.
    Scalar,
}

impl PropertyKind {
    /// Tags a field's metadata type.
    #[must_use]
    pub fn of_field(ty: &spot_meta::FieldType) -> Self {
        use spot_meta::FieldType as MetaFieldType;
        match ty {
            MetaFieldType::Bool
            | MetaFieldType::Int
            | MetaFieldType::Decimal
            | MetaFieldType::Date
            | MetaFieldType::Time
            | MetaFieldType::DateTime
            | MetaFieldType::Json
            | MetaFieldType::Bytes
            | MetaFieldType::Geo
            | MetaFieldType::Range
            | MetaFieldType::Secret => Self::Scalar,
            MetaFieldType::Text | MetaFieldType::LongText | MetaFieldType::Select => Self::Text,
            // An attach is a reference to a file record (ADR-068 §2); a secret holds no value here.
            MetaFieldType::Link | MetaFieldType::Attach => Self::Link,
            MetaFieldType::DynamicLink => Self::DynamicLink,
            MetaFieldType::Table | MetaFieldType::TableMultiselect => Self::Table,
        }
    }

    /// The property kind's text, as `to_text` prints it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Link => "link",
            Self::DynamicLink => "dynamic_link",
            Self::Table => "table",
            Self::Scalar => "scalar",
        }
    }
}

/// One property of a typed relation edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeProperty {
    /// The field's name, verbatim.
    pub name: String,
    /// What the field holds ([`PropertyKind`]).
    pub kind: PropertyKind,
    /// Whether the field is required.
    pub required: bool,
    /// The raw `options` when the field carries one (a select's values, a
    /// link's DocType); never interpreted here.
    pub options: Option<String>,
}

impl EdgeProperty {
    /// Builds an edge property from a relation DocType's field.
    #[must_use]
    pub fn from_field(field: &spot_meta::FieldSpec) -> Self {
        Self {
            name: field.name.clone(),
            kind: PropertyKind::of_field(&field.ty),
            required: field.required,
            options: field.options.clone(),
        }
    }
}

/// One edge type of the model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EdgeType {
    /// The canonical name (the identity the id hashes; see the crate docs).
    pub name: String,
    /// The stable id, the SHA-256 of the canonical name.
    pub edge_id: EdgeTypeId,
    /// The `from` node's DocType (`relation`-sourced edges may name one that
    /// `problems` flags).
    pub from: String,
    /// The target shape ([`TargetShape`]).
    pub to: TargetShape,
    /// Who owns what the edge connects ([`Cardinality`]'s companion).
    pub ownership: Ownership,
    /// The cardinality.
    pub cardinality: Cardinality,
    /// Whether the edge is required (the field's `required`, mirrored).
    pub required: bool,
    /// The properties, for relation edges only; empty for every other source.
    pub properties: Vec<EdgeProperty>,
    /// Where the edge came from.
    pub source: EdgeSource,
    /// For a relation edge, the relation DocType and its two end fields: the storage mapping the
    /// query executor follows (WP-GRA-03.T5). `None` for every other source.
    #[serde(default)]
    pub relation: Option<RelationRef>,
}

/// Where a relation edge lives in storage: one record of `doctype` per edge, its `from_field`
/// naming the edge's `from` record and its `to_field` the `to` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationRef {
    /// The relation DocType.
    pub doctype: String,
    /// The Link field holding the `from` end.
    pub from_field: String,
    /// The Link field holding the `to` end.
    pub to_field: String,
    /// What several relation records joining one pair of records mean; `None` (the default and
    /// every model before WP-GRA-03.T9) keeps them ambiguous.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aggregate: Option<AggregateSpec>,
}

/// How the lines of one pair are merged into one edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AggregateHow {
    /// The property's values are summed (integers and decimals, with checked arithmetic).
    Sum,
}

impl AggregateHow {
    /// The merge rule's stable text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sum => "sum",
        }
    }
}

/// An opt-in declaration that repeated lines of one pair are one edge whose `property` is the
/// combination of the lines' values (a component listed twice in one assembly has the sum of its
/// quantities). Every other property of the lines must be equal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AggregateSpec {
    /// The relation DocType's field to combine: a declared `Int` or `Decimal` field.
    pub property: String,
    /// How the lines' values are combined.
    pub how: AggregateHow,
}

impl EdgeType {
    /// The `to` DocType when the target is one; `None` for
    /// [`TargetShape::AnyOf`] and [`TargetShape::Dynamic`].
    #[must_use]
    pub fn to_one(&self) -> Option<&str> {
        match &self.to {
            TargetShape::One(name) => Some(name),
            _ => None,
        }
    }
}

/// One relation spec, as the brief fixes it.
///
/// A relation DocType carries at least two link fields: `from_field` and
/// `to_field` name them. The edge runs from the DocType the `from_field`
/// points at to the DocType the `to_field` points at; `edge_name` is the
/// edge's canonical name. The DocType's other fields become
/// [`EdgeProperty`]s of the edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationSpec {
    /// The relation DocType's name.
    pub doctype: String,
    /// The link field naming the `from` end.
    pub from_field: String,
    /// The link field naming the `to` end.
    pub to_field: String,
    /// The edge's canonical name.
    pub edge_name: String,
    /// Optional: merge repeated lines of one pair by summing a numeric property. `None` refuses
    /// repeated lines with `graphq.ambiguous_relation_edge`.
    #[serde(default)]
    pub aggregate: Option<AggregateSpec>,
}
