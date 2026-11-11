//! The edge set of a document, derived from the effective metadata.
//!
//! [`EdgeSchema::derive`] reads the effective metadata and the relation specs and precomputes,
//! per DocType, how each field becomes edges. It reuses [`spot_graph_model`] for the edge
//! *types*: a `Link` field, a `DynamicLink` field, a `Table` field (owned containment), a
//! `TableMultiselect` field and a relation DocType all take their stable
//! [`EdgeTypeId`](spot_graph_model::EdgeTypeId) from the graph model derived from the same
//! metadata, so an edge written here is the edge the model, the index and the query API name.
//!
//! [`edges_of`] is the pure step: a document in, its edge set out. It is a function of the
//! document's own values only — no clock, no storage, no randomness — so the same document
//! always yields the same [`BTreeSet`].
//!
//! # What becomes an edge
//!
//! - a `Link` field: one edge from the record to the record it names;
//! - a `DynamicLink` field: one edge from the record to the record the value names, of the
//!   DocType its type field names;
//! - a `Table` field: one owned containment edge from the parent to each child row, **plus**
//!   one edge from the parent for each link field of a child row (a child row has no node of
//!   its own, so its links belong to the parent);
//! - a `TableMultiselect` field: one edge from the parent to the multi-picked master, through
//!   the child table's single link field;
//! - a relation DocType: one typed edge between the two DocTypes its `from_field` and
//!   `to_field` link to, carrying the DocType's other fields as properties.
//!
//! A field whose target is unknown, a `DynamicLink` whose type field names nothing, and a
//! `TableMultiselect` whose child is missing contribute no edge: the metadata problems are
//! [`spot_graph_model`]'s to report, and this crate never panics over unfinished metadata.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::document::{ChildRow, Document};
use spot_graph_model::{EdgeTypeId, GraphModel, RelationSpec, contains_name};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType};
use spot_storage_contract::{Name, NodeRef, Value};

use crate::edge::{Edge, PropValue};

/// How a field names its target.
#[derive(Clone, Debug)]
enum Target {
    /// The target DocType is fixed by the field's `options`.
    Fixed(Name),
    /// The target DocType is named per record by this text field.
    Dynamic(String),
}

/// One edge a child row's link field contributes, attributed to the parent.
#[derive(Clone, Debug)]
struct ChildLink {
    field: String,
    edge_type: EdgeTypeId,
    target: Name,
}

/// How one field of one DocType becomes edges.
#[derive(Clone, Debug)]
enum Plan {
    /// A `Link` or `DynamicLink` field.
    Field {
        field: String,
        edge_type: EdgeTypeId,
        target: Target,
    },
    /// A `Table` field: containment edges to the child rows, and the child rows' own links.
    Table {
        field: String,
        child: Name,
        containment: EdgeTypeId,
        child_links: Vec<ChildLink>,
    },
    /// A `TableMultiselect` field.
    MultiSelect {
        field: String,
        pick_field: String,
        edge_type: EdgeTypeId,
        target: Name,
    },
    /// A relation DocType.
    Relation {
        from_field: String,
        to_field: String,
        edge_type: EdgeTypeId,
        from_target: Name,
        to_target: Name,
        property_fields: Vec<String>,
    },
}

/// The metadata-derived plan of how each DocType's fields become edges.
///
/// Build it once per metadata version ([`EdgeSchema::derive`]) and reuse it for every document;
/// rebuilding is cheap and deterministic, so a runtime-created DocType (a custom Link field
/// added to the metadata later) gets its edges from the next derive with no code change.
#[derive(Clone, Debug)]
pub struct EdgeSchema {
    model: GraphModel,
    plans: BTreeMap<String, Vec<Plan>>,
}

impl EdgeSchema {
    /// Derives the plan from the effective metadata and the relation specs.
    ///
    /// The specs are the ones [`GraphModel::derive_relations`] takes (spot-meta has no relation
    /// flag yet, so a later task reads them from metadata). A spec that does not name two link
    /// fields is ignored here, exactly as the model degrades it.
    #[must_use]
    pub fn derive(meta: &EffectiveMeta, relations: &[RelationSpec]) -> Self {
        let model = GraphModel::derive_relations(meta, relations);
        let mut plans = BTreeMap::new();
        for def in meta.definitions() {
            if def.is_child {
                continue;
            }
            plans.insert(def.name.clone(), plans_for(meta, def, relations));
        }
        Self { model, plans }
    }

    /// The graph model the plan was derived with.
    #[must_use]
    pub fn model(&self) -> &GraphModel {
        &self.model
    }

    /// Whether the schema knows a DocType at all.
    #[must_use]
    pub fn knows(&self, doctype: &str) -> bool {
        self.plans.contains_key(doctype)
    }

    /// The edge set of one document, as a sorted set.
    #[must_use]
    pub fn edges_of(&self, doc: &Document) -> BTreeSet<Edge> {
        edges_of(self, doc)
    }
}

/// The edge set of one document, from a schema derived once.
#[must_use]
pub fn edges_of(schema: &EdgeSchema, doc: &Document) -> BTreeSet<Edge> {
    let mut out = BTreeSet::new();
    let Some(plans) = schema.plans.get(doc.doctype().as_str()) else {
        return out;
    };
    let from = NodeRef {
        entity: doc.doctype().clone(),
        id: doc.id(),
    };
    for plan in plans {
        match plan {
            Plan::Field {
                field,
                edge_type,
                target,
            } => {
                if let Some(id) = link_value(doc.get(field))
                    && let Some(entity) = resolve(target, doc)
                {
                    out.insert(Edge::new(
                        edge_type.clone(),
                        from.clone(),
                        NodeRef { entity, id },
                    ));
                }
            }
            Plan::Table {
                field,
                child,
                containment,
                child_links,
            } => {
                for row in doc.rows(field) {
                    out.insert(Edge::new(
                        containment.clone(),
                        from.clone(),
                        NodeRef {
                            entity: child.clone(),
                            id: row.id,
                        },
                    ));
                    for link in child_links {
                        if let Some(id) = link_value(row_field(row, &link.field)) {
                            out.insert(Edge::new(
                                link.edge_type.clone(),
                                from.clone(),
                                NodeRef {
                                    entity: link.target.clone(),
                                    id,
                                },
                            ));
                        }
                    }
                }
            }
            Plan::MultiSelect {
                field,
                pick_field,
                edge_type,
                target,
            } => {
                for row in doc.rows(field) {
                    if let Some(id) = link_value(row_field(row, pick_field)) {
                        out.insert(Edge::new(
                            edge_type.clone(),
                            from.clone(),
                            NodeRef {
                                entity: target.clone(),
                                id,
                            },
                        ));
                    }
                }
            }
            Plan::Relation {
                from_field,
                to_field,
                edge_type,
                from_target,
                to_target,
                property_fields,
            } => {
                if let (Some(source), Some(target)) = (
                    link_value(doc.get(from_field)),
                    link_value(doc.get(to_field)),
                ) {
                    let mut edge = Edge::new(
                        edge_type.clone(),
                        NodeRef {
                            entity: from_target.clone(),
                            id: source,
                        },
                        NodeRef {
                            entity: to_target.clone(),
                            id: target,
                        },
                    )
                    // The relation record owns the edge: two relation records between the same
                    // pair are two edges, distinguished by their owner.
                    .owned_by(from.clone());
                    for name in property_fields {
                        edge.properties
                            .insert(name.clone(), PropValue::of(doc.get(name)));
                    }
                    out.insert(edge);
                }
            }
        }
    }
    out
}

/// Builds one DocType's plans: a valid relation spec wins, otherwise the plain walk.
fn plans_for(meta: &EffectiveMeta, def: &Definition, relations: &[RelationSpec]) -> Vec<Plan> {
    let mut plans = Vec::new();
    let mut related = false;
    for spec in relations.iter().filter(|spec| spec.doctype == def.name) {
        if let Some(plan) = relation_plan(meta, def, spec) {
            plans.push(plan);
            related = true;
        }
    }
    if related {
        return plans;
    }
    for field in &def.fields {
        match field.ty {
            FieldType::Link => {
                if let Some(plan) = field_plan(def, field, TargetKind::Fixed) {
                    plans.push(plan);
                }
            }
            FieldType::DynamicLink => {
                if let Some(plan) = field_plan(def, field, TargetKind::Dynamic) {
                    plans.push(plan);
                }
            }
            FieldType::Table => {
                if let Some(plan) = table_plan(meta, def, field) {
                    plans.push(plan);
                }
            }
            FieldType::TableMultiselect => {
                if let Some(plan) = multiselect_plan(meta, def, field) {
                    plans.push(plan);
                }
            }
            _ => {}
        }
    }
    plans
}

/// Which kind of target a `Field` plan resolves.
enum TargetKind {
    Fixed,
    Dynamic,
}

/// A `Link` (fixed) or `DynamicLink` (dynamic) plan.
fn field_plan(def: &Definition, field: &FieldSpec, kind: TargetKind) -> Option<Plan> {
    let name = format!("{}.{}", def.name, field.name);
    let options = field.options.as_deref().unwrap_or("");
    let target = match kind {
        TargetKind::Fixed => Target::Fixed(storage_name(options)?),
        TargetKind::Dynamic => Target::Dynamic(options.to_owned()),
    };
    Some(Plan::Field {
        field: field.name.clone(),
        edge_type: EdgeTypeId::of(&name),
        target,
    })
}

/// A `Table` plan: containment edges plus the child's own link fields.
fn table_plan(meta: &EffectiveMeta, def: &Definition, field: &FieldSpec) -> Option<Plan> {
    let child_name = field.options.as_deref().unwrap_or("");
    let child = meta.definition(child_name)?;
    let containment = EdgeTypeId::of(&contains_name(&def.name, child_name));
    let child_links = child
        .fields
        .iter()
        .filter(|f| f.ty == FieldType::Link)
        .filter_map(|f| {
            let target = storage_name(f.options.as_deref().unwrap_or(""))?;
            Some(ChildLink {
                field: f.name.clone(),
                edge_type: EdgeTypeId::of(&format!("{}.{}", child.name, f.name)),
                target,
            })
        })
        .collect();
    Some(Plan::Table {
        field: field.name.clone(),
        child: storage_name(child_name)?,
        containment,
        child_links,
    })
}

/// A `TableMultiselect` plan: the parent points straight at the child's single link target.
fn multiselect_plan(meta: &EffectiveMeta, def: &Definition, field: &FieldSpec) -> Option<Plan> {
    let child_name = field.options.as_deref().unwrap_or("");
    let child = meta.definition(child_name)?;
    let mut links = child.fields.iter().filter(|f| f.ty == FieldType::Link);
    let (Some(pick), None) = (links.next(), links.next()) else {
        return None;
    };
    let name = format!("{}.{}", def.name, field.name);
    Some(Plan::MultiSelect {
        field: field.name.clone(),
        pick_field: pick.name.clone(),
        edge_type: EdgeTypeId::of(&name),
        target: storage_name(pick.options.as_deref().unwrap_or(""))?,
    })
}

/// A relation plan, when both ends are link fields.
fn relation_plan(meta: &EffectiveMeta, def: &Definition, spec: &RelationSpec) -> Option<Plan> {
    let from = def.fields.iter().find(|f| f.name == spec.from_field)?;
    let to = def.fields.iter().find(|f| f.name == spec.to_field)?;
    if from.ty != FieldType::Link || to.ty != FieldType::Link {
        return None;
    }
    let withheld = withheld_fields(meta, def);
    let property_fields = def
        .fields
        .iter()
        .filter(|f| f.name != spec.from_field && f.name != spec.to_field)
        .filter(|f| carries_as_property(f) && !withheld.contains(&f.name))
        .map(|f| f.name.clone())
        .collect();
    Some(Plan::Relation {
        from_field: spec.from_field.clone(),
        to_field: spec.to_field.clone(),
        edge_type: EdgeTypeId::of(&spec.edge_name),
        from_target: storage_name(from.options.as_deref().unwrap_or(""))?,
        to_target: storage_name(to.options.as_deref().unwrap_or(""))?,
        property_fields,
    })
}

/// Whether a field's value may ever be copied onto an edge: never a secret, never raw bytes,
/// never a child table (it has no value of its own on the record).
fn carries_as_property(field: &FieldSpec) -> bool {
    !matches!(field.ty, FieldType::Secret | FieldType::Bytes) && !field.ty.is_table()
}

/// The fields of `def` whose values must never be copied onto an edge: masked fields, fields
/// in a protection group (declared or inherited), and fields carrying a classification tag.
/// An edge is readable by anyone who may traverse the graph, so it must not carry a value the
/// field's own access control would withhold.
fn withheld_fields(meta: &EffectiveMeta, def: &Definition) -> BTreeSet<String> {
    let prefix = format!("{}.", def.name);
    meta.field_protection()
        .into_iter()
        .filter(|p| p.mask.is_some() || !p.groups.is_empty() || !p.tags.is_empty())
        .filter_map(|p| p.path.strip_prefix(&prefix).map(str::to_owned))
        .collect()
}

/// Resolves a `Field` plan's target for one document.
fn resolve(target: &Target, doc: &Document) -> Option<Name> {
    match target {
        Target::Fixed(name) => Some(name.clone()),
        Target::Dynamic(type_field) => match doc.get(type_field) {
            Value::Text(value) if !value.is_empty() => storage_name(value),
            _ => None,
        },
    }
}

/// The record id a value holds, when it is a link.
fn link_value(value: &Value) -> Option<spot_storage_contract::RecordId> {
    match value {
        Value::Id(id) => Some(*id),
        _ => None,
    }
}

/// A child row's field value.
fn row_field<'a>(row: &'a ChildRow, field: &str) -> &'a Value {
    row.fields
        .iter()
        .find(|(name, _)| name.as_str() == field)
        .map_or(&Value::Null, |(_, value)| value)
}

/// A valid storage name, or `None` for anything the metadata let through that is not one.
fn storage_name(value: &str) -> Option<Name> {
    Name::new(value).ok()
}
