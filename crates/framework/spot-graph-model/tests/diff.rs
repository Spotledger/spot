//! Diff tests (brief item 2): added, removed and changed edge types between
//! two derives, plus the no-change case and node moves.

// Test code may unwrap what the test itself built (spot-dag house pattern).
#![allow(clippy::unwrap_used)]

mod common;

use common::{WithOptions, fixtures, registry};
use spot_graph_model::GraphModel;
use spot_meta::{Customisations, FieldType, Registry};

#[test]
fn identical_metadata_diffs_empty() {
    let old = GraphModel::derive(&registry().effective());
    let new = GraphModel::derive(&registry().effective());
    assert!(new.diff(&old).is_empty());
    // The meta version is not part of equality: the same content at a
    // different version diffs clean. Prove it by touching (a no-op change).
    let mut c = Customisations::new();
    c.touch();
    let meta = Registry::build(fixtures(), &c).unwrap_or_else(|e| panic!("{e:?}"));
    let touched = GraphModel::derive(&meta);
    assert_ne!(touched.meta_version, old.meta_version, "touch must bump");
    assert!(touched.diff(&old).is_empty());
}

#[test]
fn a_new_doctype_is_added_nodes_and_edges() {
    let mut reg = registry();
    let old = GraphModel::derive(&reg.effective());
    let mut extra = spot_meta::Definition::new("warehouse_zone");
    extra.fields = vec![
        common::text("name"),
        common::link("parent_zone", "warehouse_zone"),
        common::link("supervisor", "party"),
    ];
    let mut defs = fixtures();
    defs.push(extra);
    reg.set_definitions(defs)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let new = GraphModel::derive(&reg.effective());
    let diff = new.diff(&old);
    assert_eq!(diff.added_nodes, ["warehouse_zone".to_owned()]);
    let mut added = diff.added_edges.clone();
    added.sort();
    assert_eq!(
        added,
        vec![
            "warehouse_zone.parent_zone".to_owned(),
            "warehouse_zone.supervisor".to_owned(),
        ]
    );
    assert!(diff.removed_edges.is_empty());
    assert!(diff.changed_edges.is_empty());
    assert!(diff.removed_nodes.is_empty());
}

#[test]
fn a_removed_doctype_is_removed_nodes_and_edges() {
    let mut reg = registry();
    let old = GraphModel::derive(&reg.effective());
    // Remove `order` (the select-backed dynamic edge goes with it).
    reg.set_definitions(vec![
        common::party(),
        common::project(),
        common::project_task(),
        common::project_watcher(),
    ])
    .unwrap_or_else(|e| panic!("{e:?}"));
    let new = GraphModel::derive(&reg.effective());
    let diff = new.diff(&old);
    assert_eq!(diff.removed_nodes, ["order".to_owned()]);
    assert_eq!(diff.removed_edges, ["order.ref".to_owned()]);
    assert!(diff.added_edges.is_empty());
    assert!(diff.added_nodes.is_empty());
}

#[test]
fn a_changed_field_is_a_changed_edge() {
    let mut reg = registry();
    let old = GraphModel::derive(&reg.effective());
    // `project.customer`'s target moves from `party` to `project_task`? Not
    // possible (child): make it required instead — the edge's `required`
    // changes, everything else stays.
    let mut project = common::project();
    let mut customer = common::link("customer", "party");
    customer.required = true;
    project.fields = vec![
        common::text("name"),
        customer,
        common::link("manager", "party"),
        common::link("parent_project", "project"),
        spot_meta::FieldSpec::new("tasks", FieldType::Table).with_options("project_task"),
        spot_meta::FieldSpec::new("watchers", FieldType::TableMultiselect)
            .with_options("project_watcher"),
    ];
    let mut defs = fixtures();
    let position = defs.iter().position(|d| d.name == "project");
    defs[position.unwrap()] = project;
    reg.set_definitions(defs)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let new = GraphModel::derive(&reg.effective());
    let diff = new.diff(&old);
    assert!(diff.added_edges.is_empty(), "{:?}", diff.added_edges);
    assert!(diff.removed_edges.is_empty());
    assert_eq!(diff.changed_edges.len(), 1);
    let change = &diff.changed_edges[0];
    assert_eq!(change.name, "project.customer");
    assert!(!change.old.required);
    assert!(change.new.required);
    assert_eq!(change.old.to_one(), change.new.to_one());
    // Nodes unchanged.
    assert!(diff.added_nodes.is_empty());
    assert!(diff.removed_nodes.is_empty());
}

#[test]
fn a_relation_spec_applied_is_edges_added_and_a_node_removed() {
    let mut defs = fixtures();
    defs.push(common::task_link());
    let spec = common::task_link_spec("works_on");
    let meta = spot_meta::Registry::build(defs, &Customisations::new())
        .unwrap_or_else(|e| panic!("{e:?}"));

    let without = GraphModel::derive(&meta);
    let with = GraphModel::derive_relations(&meta, &[spec]);
    let diff = with.diff(&without);
    // The DocType stops being a node and becomes one edge.
    assert_eq!(diff.added_edges, ["works_on".to_owned()]);
    assert_eq!(diff.removed_nodes, ["task_link".to_owned()]);
    // The DocType's own link edges disappear from the model (they were
    // consumed into the typed edge): they come back as removals.
    assert!(diff.removed_edges.contains(&"task_link.project".to_owned()));
    assert!(diff.removed_edges.contains(&"task_link.party".to_owned()));
    assert!(diff.added_nodes.is_empty());
}

#[test]
fn the_diff_runs_in_both_directions() {
    let mut defs = fixtures();
    defs.push(common::task_link());
    let meta = spot_meta::Registry::build(defs, &Customisations::new())
        .unwrap_or_else(|e| panic!("{e:?}"));
    let spec = common::task_link_spec("works_on");
    let without = GraphModel::derive(&meta);
    let with = GraphModel::derive_relations(&meta, &[spec]);
    let forward = with.diff(&without);
    let back = without.diff(&with);
    assert_eq!(forward.added_edges, back.removed_edges);
    assert_eq!(forward.removed_edges, back.added_edges);
    assert_eq!(forward.added_nodes, back.removed_nodes);
    assert_eq!(forward.removed_nodes, back.added_nodes);
    assert!(forward.changed_edges.is_empty());
    assert!(back.changed_edges.is_empty());
}

#[test]
fn changed_edges_carry_old_and_new_side_by_side() {
    let mut reg = registry();
    let old = GraphModel::derive(&reg.effective());
    // `party.about`'s type field becomes a select: the dynamic edge turns
    // from `Dynamic` to `AnyOf` — a real structural change.
    let mut party = common::party();
    let mut kind = spot_meta::FieldSpec::new("about_type", FieldType::Select);
    kind.options = Some("party\nproject".into());
    party.fields = vec![
        common::text("code"),
        kind,
        spot_meta::FieldSpec::new("about", FieldType::DynamicLink).with_options("about_type"),
    ];
    let mut defs = fixtures();
    defs[0] = party;
    reg.set_definitions(defs)
        .unwrap_or_else(|e| panic!("{e:?}"));
    let new = GraphModel::derive(&reg.effective());
    let diff = new.diff(&old);
    assert_eq!(diff.changed_edges.len(), 1);
    let change = &diff.changed_edges[0];
    assert_eq!(change.name, "party.about");
    assert_eq!(change.old.to, spot_graph_model::TargetShape::Dynamic);
    assert!(matches!(
        change.new.to,
        spot_graph_model::TargetShape::AnyOf(_)
    ));
    // `order.ref` (which reads `ref_kind`) is untouched.
    assert!(new.edge("order.ref").is_some());
}

#[test]
fn a_custom_fields_options_change_is_a_change() {
    let mut reg = registry();
    reg.add_custom_field(
        "party",
        spot_meta::FieldSpec::new("custom_peer", FieldType::Link).with_options("party"),
        None,
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let old = GraphModel::derive(&reg.effective());
    // The same custom field now required (an override on it).
    let mut c = Customisations::new();
    c.add_field(
        "party",
        {
            let mut f =
                spot_meta::FieldSpec::new("custom_peer", FieldType::Link).with_options("party");
            f.required = true;
            f
        },
        None,
    );
    let meta = Registry::build(fixtures(), &c).unwrap_or_else(|e| panic!("{e:?}"));
    let new = GraphModel::derive(&meta);
    let diff = new.diff(&old);
    assert!(diff.added_edges.is_empty());
    assert!(diff.removed_edges.is_empty());
    assert_eq!(diff.changed_edges.len(), 1);
    assert_eq!(diff.changed_edges[0].name, "party.custom_peer");
}
