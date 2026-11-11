//! The derive walks: pure edge extraction from the effective metadata.
//!
//! Order is deterministic on three levels: nodes and edges land in
//! `BTreeMap`s; edge properties come out in the relation DocType's field
//! order; [`AnyOf`](crate::AnyOf) sets are `BTreeSet`s. The same metadata —
//! even with its definitions listed in a different order — yields an
//! equal model with equal [`EdgeTypeId`](crate::EdgeTypeId)s.
//!
//! Every edge the walks build is checked against the metadata's known
//! DocTypes here rather than in a separate pass, so [`GraphModel::problems`]
//! only collects what the walks recorded.

use std::collections::BTreeSet;

use spot_meta::{Definition, FieldSpec, FieldType as MetaFieldType};

use crate::error::Problem;
use crate::model::{
    AnyOf, Cardinality, EdgeProperty, EdgeSource, EdgeType, EdgeTypeId, GraphModel, Ownership,
    RelationSpec, TargetShape, contains_name,
};

/// Derives every edge of the metadata's definitions into `model`.
///
/// `meta` is read for two things: the definitions being walked and the
/// existence checks (`problems` never fires for a known non-child target).
/// Nothing here panics: unfinished metadata yields edges plus problems.
pub(crate) fn walk(
    model: &mut GraphModel,
    meta: &spot_meta::EffectiveMeta,
    relations: &[RelationSpec],
) {
    // A spec naming an unknown DocType never reaches a DocType walk: reported
    // here, once (the specs may repeat).
    for spec in relations {
        if meta.definition(&spec.doctype).is_none() {
            let problem = Problem::UnknownRelationDoctype {
                doctype: spec.doctype.clone(),
            };
            if !model.derive_problems.contains(&problem) {
                model.extend_problems(vec![problem]);
            }
        }
    }
    // Nodes: one per non-child DocType that no spec names.
    for def in meta.definitions() {
        if !def.is_child && !relations.iter().any(|spec| spec.doctype == def.name) {
            model.nodes.insert(
                def.name.clone(),
                crate::model::NodeType::from_definition(def),
            );
        }
    }
    // Edges: per DocType, in definition (name) order; per field, in field order.
    for def in meta.definitions() {
        if def.is_child {
            continue;
        }
        // Every spec naming this DocType is walked in order: a second spec
        // for the same DocType inserts a second edge (or reports
        // `duplicate_edge_name` when their names collide, below). Without a
        // spec the plain walk runs.
        if relations.is_empty() {
            plain_edges(model, meta, def, relations);
        } else {
            let mut named = false;
            for spec in relations.iter().filter(|spec| spec.doctype == def.name) {
                named = true;
                relation_edges(model, meta, def, relations, spec);
            }
            if !named {
                plain_edges(model, meta, def, relations);
            }
        }
    }
}

/// Walks a plain DocType: an edge per Link, Table, TableMultiselect and DynamicLink field,
/// skipping the fields named as relation ends (a DocType is either a plain
/// node with plain fields or a relation DocType; a spec naming a field of a
/// DocType not in the specs' set is itself a problem the plain walk reports).
pub(crate) fn plain_edges(
    model: &mut GraphModel,
    meta: &spot_meta::EffectiveMeta,
    def: &Definition,
    relations: &[RelationSpec],
) {
    // Only the fields the *applied* spec named are skipped; when the spec was
    // refused (degrade), the DocType's own edges must all exist, so the plain
    // walk skips nothing in that case. A spec for a DocType that was refused
    // is detectable: its fields were reported, its edge never inserted. The
    // walk cannot see the refusal here, so the skip only applies when the
    // relation edge actually exists under one of this DocType's fields — that
    // is the success case.
    for field in &def.fields {
        let is_end_of_named_spec = relations.iter().any(|spec| {
            spec.doctype == def.name
                && (spec.from_field == field.name || spec.to_field == field.name)
        });
        let ends_carried_by_an_edge = relations.iter().any(|spec| {
            spec.doctype == def.name
                && (spec.from_field == field.name || spec.to_field == field.name)
                && model.edges.contains_key(&spec.edge_name)
        });
        if is_end_of_named_spec && ends_carried_by_an_edge {
            continue;
        }
        match field.ty {
            MetaFieldType::Link => link_edge(model, meta, def, field),
            MetaFieldType::Table => table_edge(model, def, field),
            MetaFieldType::TableMultiselect => multiselect_edge(model, meta, def, field),
            MetaFieldType::DynamicLink => dynamic_edge(model, meta, def, field),
            // `Attach` stays a property only (`PropertyKind::Link`), not an
            // edge: its target is the platform `file` record, a leaf with no
            // traversal value, so an edge per attach field would only add
            // noise to every walk.
            _ => {}
        }
    }
}

/// One `Link` edge: `<doctype>.<field>` from the DocType to the target.
fn link_edge(
    model: &mut GraphModel,
    meta: &spot_meta::EffectiveMeta,
    def: &Definition,
    field: &FieldSpec,
) {
    let name = format!("{}.{}", def.name, field.name);
    let target = field.options.as_deref().unwrap_or("");
    let problems = target_problems(meta, &def.name, &field.name, target);
    model.edges.insert(
        name.clone(),
        EdgeType {
            name,
            edge_id: EdgeTypeId::of(&format!("{}.{}", def.name, field.name)),
            from: def.name.clone(),
            to: TargetShape::One(target.to_owned()),
            cardinality: Cardinality::ManyToOne,
            required: field.required,
            ownership: Ownership::Ref,
            properties: Vec::new(),
            source: EdgeSource::LinkField,
            relation: None,
        },
    );
    record(model, problems);
}

/// One `Table` edge: `<parent>_contains_<child>` from parent to child, owned.
fn table_edge(model: &mut GraphModel, def: &Definition, field: &FieldSpec) {
    let target = field.options.as_deref().unwrap_or("");
    let name = contains_name(&def.name, target);
    model.edges.insert(
        name.clone(),
        EdgeType {
            name: name.clone(),
            edge_id: EdgeTypeId::of(&name),
            from: def.name.clone(),
            to: TargetShape::One(target.to_owned()),
            cardinality: Cardinality::OneToMany,
            required: false,
            ownership: Ownership::Owned,
            properties: Vec::new(),
            source: EdgeSource::Table,
            relation: None,
        },
    );
}

/// One `TableMultiselect` edge: `<doctype>.<field>` from the owner straight to
/// the multi-picked master, through the child table's single link field.
///
/// The child itself gets no node (children never do); the edge is a reference
/// (`Ref`), not containment, since the picked master lives on its own. A child
/// that is missing or lacks exactly one link field yields no edge: spot-meta
/// refuses that shape before metadata is effective, so it cannot be reached.
fn multiselect_edge(
    model: &mut GraphModel,
    meta: &spot_meta::EffectiveMeta,
    def: &Definition,
    field: &FieldSpec,
) {
    let child = field.options.as_deref().unwrap_or("");
    let Some(child) = meta.definition(child) else {
        return;
    };
    let mut links = child.fields.iter().filter(|f| f.ty == MetaFieldType::Link);
    let (Some(pick), None) = (links.next(), links.next()) else {
        return;
    };
    let name = format!("{}.{}", def.name, field.name);
    let target = pick.options.as_deref().unwrap_or("");
    let problems = target_problems(meta, &def.name, &field.name, target);
    model.edges.insert(
        name.clone(),
        EdgeType {
            name: name.clone(),
            edge_id: EdgeTypeId::of(&name),
            from: def.name.clone(),
            to: TargetShape::One(target.to_owned()),
            cardinality: Cardinality::ManyToMany,
            required: field.required,
            ownership: Ownership::Ref,
            properties: Vec::new(),
            source: EdgeSource::TableMultiselect,
            relation: None,
        },
    );
    record(model, problems);
}

/// One `DynamicLink` edge: `AnyOf` the type field's values when the type
/// field is a select (values name DocTypes), `Dynamic` otherwise. Values that
/// are not known DocTypes are problems, not silently accepted names.
fn dynamic_edge(
    model: &mut GraphModel,
    meta: &spot_meta::EffectiveMeta,
    def: &Definition,
    field: &FieldSpec,
) {
    let name = format!("{}.{}", def.name, field.name);
    let type_field_name = field.options.as_deref().unwrap_or("");
    let type_field = def.fields.iter().find(|f| f.name == type_field_name);
    let to = match type_field {
        Some(tf) if tf.ty == MetaFieldType::Select => {
            let values: BTreeSet<String> = tf.select_values().into_iter().collect();
            AnyOf { any_of: values }.into()
        }
        _ => TargetShape::Dynamic,
    };
    let problems = match &to {
        TargetShape::AnyOf(targets) => targets
            .iter()
            .filter(|value| meta.definition(value).is_none())
            .map(|value| Problem::DanglingTarget {
                doctype: def.name.clone(),
                field: field.name.clone(),
                target: value.clone(),
            })
            .collect(),
        _ => Vec::new(),
    };
    model.edges.insert(
        name.clone(),
        EdgeType {
            name,
            edge_id: EdgeTypeId::of(&format!("{}.{}", def.name, field.name)),
            from: def.name.clone(),
            to,
            cardinality: Cardinality::DynamicLinkManyToOne,
            required: field.required,
            ownership: Ownership::Ref,
            properties: Vec::new(),
            source: EdgeSource::DynamicLink,
            relation: None,
        },
    );
    record(model, problems);
}

/// Walks a relation DocType: one typed edge between the two ends, the other
/// fields as properties.
///
/// Refusals (missing end field, an end that is not a Link) restore the
/// DocType as a node and degrade to the plain walk, so a mistyped spec loses
/// nothing: the DocType's own edges still exist and the reason is a problem.
pub(crate) fn relation_edges(
    model: &mut GraphModel,
    meta: &spot_meta::EffectiveMeta,
    def: &Definition,
    relations: &[RelationSpec],
    spec: &RelationSpec,
) {
    let degrade = |model: &mut GraphModel, problems: Vec<Problem>| {
        // The node loop skipped a spec-named DocType; a refused spec leaves
        // the DocType an ordinary node, with an ordinary node type.
        model.nodes.insert(
            def.name.clone(),
            crate::model::NodeType::from_definition(def),
        );
        record(model, problems);
        plain_edges(model, meta, def, relations);
    };
    let from = def.fields.iter().find(|f| f.name == spec.from_field);
    let to = def.fields.iter().find(|f| f.name == spec.to_field);
    let (Some(from), Some(to)) = (from, to) else {
        let missing = if from.is_some() {
            &spec.to_field
        } else {
            &spec.from_field
        };
        degrade(
            model,
            vec![Problem::RelationFieldNotLink {
                doctype: def.name.clone(),
                field: missing.clone(),
            }],
        );
        return;
    };
    if from.ty != MetaFieldType::Link || to.ty != MetaFieldType::Link {
        let bad = if from.ty != MetaFieldType::Link {
            &spec.from_field
        } else {
            &spec.to_field
        };
        degrade(
            model,
            vec![Problem::RelationFieldNotLink {
                doctype: def.name.clone(),
                field: bad.clone(),
            }],
        );
        return;
    }
    let from_target = from.options.as_deref().unwrap_or("");
    let to_target = to.options.as_deref().unwrap_or("");
    let mut problems = target_problems(meta, &def.name, &spec.from_field, from_target);
    problems.extend(target_problems(meta, &def.name, &spec.to_field, to_target));
    let mut properties: Vec<EdgeProperty> = Vec::new();
    // Reported only when the edge is kept, so a spec listed twice reports once.
    let mut unsupported: Vec<Problem> = Vec::new();
    for f in def
        .fields
        .iter()
        .filter(|f| f.name != spec.from_field && f.name != spec.to_field)
    {
        if let Some(field_type) = unsupported_property_type(&f.ty) {
            unsupported.push(Problem::RelationPropertyUnsupported {
                doctype: def.name.clone(),
                field: f.name.clone(),
                field_type: field_type.to_owned(),
            });
        } else {
            properties.push(EdgeProperty::from_field(f));
        }
    }
    let aggregate = match &spec.aggregate {
        None => None,
        Some(aggregate) => match def.fields.iter().find(|f| f.name == aggregate.property) {
            None => {
                problems.push(Problem::AggregatePropertyUnknown {
                    doctype: def.name.clone(),
                    field: aggregate.property.clone(),
                });
                None
            }
            Some(f)
                if !matches!(f.ty, MetaFieldType::Int | MetaFieldType::Decimal)
                    || f.name == spec.from_field
                    || f.name == spec.to_field =>
            {
                problems.push(Problem::AggregatePropertyNotNumeric {
                    doctype: def.name.clone(),
                    field: aggregate.property.clone(),
                });
                None
            }
            Some(_) => Some(aggregate.clone()),
        },
    };
    let edge = EdgeType {
        name: spec.edge_name.clone(),
        edge_id: EdgeTypeId::of(&spec.edge_name),
        from: from_target.to_owned(),
        to: TargetShape::One(to_target.to_owned()),
        cardinality: Cardinality::RelationManyToOne,
        required: from.required,
        ownership: Ownership::Ref,
        properties,
        source: EdgeSource::Relation,
        relation: Some(crate::model::RelationRef {
            doctype: def.name.clone(),
            from_field: spec.from_field.clone(),
            to_field: spec.to_field.clone(),
            aggregate,
        }),
    };
    if model.edges.contains_key(&spec.edge_name) {
        // The first spec's edge stays (walk order wins); the refused
        // duplicate edge is reported, never silently overwritten.
        problems.push(Problem::DuplicateEdgeName(spec.edge_name.clone()));
        record(model, problems);
        return;
    }
    model.edges.insert(spec.edge_name.clone(), edge);
    problems.extend(unsupported);
    record(model, problems);
}

/// The text of a field type the query side cannot return as an edge property
/// (`spot-graph-query`'s `exec.rs::supported`: null, bool, int, decimal, text, date and id
/// only), or `None` when the type is fine. `DateTime` is refused: the executor returns a
/// `Date` for a date and has no datetime edge value.
fn unsupported_property_type(ty: &MetaFieldType) -> Option<&'static str> {
    match ty {
        MetaFieldType::Json => Some("json"),
        MetaFieldType::Bytes => Some("bytes"),
        MetaFieldType::Time => Some("time"),
        MetaFieldType::DateTime => Some("datetime"),
        MetaFieldType::Geo => Some("geo"),
        MetaFieldType::Range => Some("range"),
        MetaFieldType::Secret => Some("secret"),
        MetaFieldType::Table => Some("table"),
        MetaFieldType::TableMultiselect => Some("table_multiselect"),
        _ => None,
    }
}

/// The problems of one link target: unknown DocType, or a child DocType (the
/// parent is the thing to link at).
fn target_problems(
    meta: &spot_meta::EffectiveMeta,
    doctype: &str,
    field: &str,
    target: &str,
) -> Vec<Problem> {
    match meta.definition(target) {
        None => vec![Problem::DanglingTarget {
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            target: target.to_owned(),
        }],
        Some(t) if t.is_child => vec![Problem::DanglingTarget {
            doctype: doctype.to_owned(),
            field: field.to_owned(),
            target: target.to_owned(),
        }],
        Some(_) => Vec::new(),
    }
}

/// Appends one field-walk's problems to the model's collected list.
fn record(model: &mut GraphModel, problems: Vec<Problem>) {
    if !problems.is_empty() {
        model.extend_problems(problems);
    }
}
