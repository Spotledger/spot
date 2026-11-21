//! Registry tests: lookups by id, by name, forward, and the reverse lookup
//! ("what links to Customer"), including dynamic targets (brief item 2).

// Test code may unwrap what the test itself built (spot-dag house pattern).
#![allow(clippy::unwrap_used)]

mod common;

use common::{effective, effective_of, link, party, project, project_task, project_watcher, text};
use spot_graph_model::{EdgeTypeRegistry, GraphModel, NodeTypeId, TargetShape};
use spot_meta::{Definition, FieldType};

#[test]
fn lookups_by_id_and_name_agree() {
    let model = GraphModel::derive(&effective());
    let registry = model.registry();
    for edge in model.edges.values() {
        let by_name = registry.by_name(&edge.name).unwrap();
        assert_eq!(by_name.edge_id, edge.edge_id);
        let by_id = registry.by_id(&edge.edge_id).unwrap();
        assert_eq!(by_id.name, edge.name);
    }
    assert!(registry.by_name("no_such_edge").is_none());
    assert!(
        registry
            .by_id(&spot_graph_model::EdgeTypeId::of("no_such_edge"))
            .is_none()
    );
}

#[test]
fn forward_lookup_lists_every_edge_from_a_node() {
    let model = GraphModel::derive(&effective());
    let registry = model.registry();
    let from_project: Vec<String> = registry
        .from_node("project")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    // Sorted by name, deterministic.
    assert_eq!(
        from_project,
        [
            "project.customer",
            "project.manager",
            "project.parent_project",
            "project.watchers",
            "project_contains_project_task",
        ]
    );
    assert!(registry.from_node("no_such").is_empty());
}

#[test]
fn reverse_lookup_answers_what_links_to_customer() {
    let model = GraphModel::derive(&effective());
    let registry = model.registry();
    let into_party: Vec<String> = registry
        .to_node("party")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(
        into_party,
        ["project.customer", "project.manager", "project.watchers"]
    );
    // The self reference `project.parent_project` targets `project`, not
    // `party`; it never answers this lookup.
    assert!(
        into_party
            .iter()
            .all(|name| !name.contains("parent_project"))
    );
    // Containment edges come back too (the child is the target of `contains`).
    let into_child: Vec<String> = registry
        .to_node("project_task")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(into_child, ["project_contains_project_task"]);
    assert!(registry.to_node("no_such").is_empty());
}

#[test]
fn reverse_lookup_resolves_dynamic_targets() {
    // `order.ref` is AnyOf { party, project }: the reverse lookup for `party`
    // includes a dynamic edge, distinguishable through `to_node_dynamic`.
    let model = GraphModel::derive(&effective());
    let registry = model.registry();
    let dynamic_into_party: Vec<String> = registry
        .to_node_dynamic("party")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(dynamic_into_party, ["order.ref"]);
    // `party.about` is Dynamic: it never answers a reverse lookup by name
    // (nothing can be said about its targets from the model alone).
    assert!(registry.to_node_dynamic("about_type").is_empty());
    // And the fixed reverse lookup is unchanged by the dynamic edge's
    // presence (to_node lists only One-shaped targets): `project` is
    // targeted by the self reference only — `order.ref` is AnyOf and
    // answers only `to_node_dynamic`, the containment targets `project_task`.
    let into_project: Vec<String> = registry
        .to_node("project")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(into_project, ["project.parent_project"]);
}

#[test]
fn a_dynamic_anyof_edge_is_answered_by_both_lookups() {
    // A DocType may be reached by both a fixed and a dynamic edge.
    let mut d = Definition::new("note");
    d.fields = vec![
        text("title"),
        link("about", "party"),
        {
            let mut kind = spot_meta::FieldSpec::new("ref_kind", FieldType::Select);
            kind.options = Some("party".into());
            kind
        },
        {
            let mut f = spot_meta::FieldSpec::new("ref", FieldType::DynamicLink);
            f.options = Some("ref_kind".into());
            f
        },
    ];
    let meta = effective_of(vec![
        d,
        party(),
        project(),
        project_task(),
        project_watcher(),
    ]);
    let model = GraphModel::derive(&meta);
    let registry = model.registry();
    let fixed: Vec<String> = registry
        .to_node("party")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert!(fixed.contains(&"note.about".to_owned()));
    let dynamic: Vec<String> = registry
        .to_node_dynamic("party")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(dynamic, ["note.ref"]);
    let ref_edge = registry.by_name("note.ref").unwrap();
    assert!(matches!(ref_edge.to, TargetShape::AnyOf(_)));
}

#[test]
fn node_lookups_by_name_and_id() {
    let model = GraphModel::derive(&effective());
    let registry = model.registry();
    assert_eq!(registry.node("party").unwrap().name, "party");
    assert!(registry.node("project_task").is_none(), "child");
    assert_eq!(
        registry.node_by_id(&NodeTypeId::of("party")).unwrap().name,
        "party"
    );
    assert!(registry.node_by_id(&NodeTypeId::of("no_such")).is_none());
}

#[test]
fn a_registry_is_a_view_of_its_model() {
    let model = GraphModel::derive(&effective());
    let registry = EdgeTypeRegistry::new(&model);
    assert_eq!(registry.model().edges.len(), model.edges.len());
    // A second registry over the same model answers the same.
    let again = model.registry();
    assert_eq!(
        registry.from_node("party").len(),
        again.from_node("party").len()
    );
}

#[test]
fn relation_edges_answer_the_forward_and_reverse_lookup() {
    let mut defs = common::fixtures();
    defs.push(common::task_link());
    let meta = effective_of(defs);
    let specs = [common::task_link_spec("works_on")];
    let model = GraphModel::derive_relations(&meta, &specs);
    let registry = model.registry();
    // Forward: from `project`.
    let from_project: Vec<String> = registry
        .from_node("project")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert!(from_project.contains(&"works_on".to_owned()));
    // Reverse: into `party`.
    let into_party: Vec<String> = registry
        .to_node("party")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert!(into_party.contains(&"works_on".to_owned()));
    // The relation DocType itself is neither end: it is not in the lookups.
    assert!(registry.from_node("task_link").is_empty());
    assert!(registry.to_node("task_link").is_empty());
}

#[test]
fn link_to_a_missing_doctype_is_never_a_registry_node() {
    // A dangling target's edge exists; lookups by its (unknown) target name
    // answer nothing — no node is invented. Reverse lookups only fire for
    // nodes that exist, so the caller sees the problem, not a ghost node.
    // spot-meta refuses a dangling *link* target at build time, so the
    // reachable path is the model over valid metadata whose *select-backed*
    // dynamic edge names the unknown.
    let mut d = Definition::new("note");
    d.fields = vec![
        text("title"),
        {
            let mut kind = spot_meta::FieldSpec::new("ref_kind", FieldType::Select);
            kind.options = Some("nowhere".into());
            kind
        },
        {
            let mut f = spot_meta::FieldSpec::new("ref", FieldType::DynamicLink);
            f.options = Some("ref_kind".into());
            f
        },
    ];
    let meta = effective_of(vec![
        d,
        party(),
        project(),
        project_task(),
        project_watcher(),
    ]);
    let model = GraphModel::derive(&meta);
    let registry = model.registry();
    assert!(registry.to_node("nowhere").is_empty());
    assert!(registry.node("nowhere").is_none());
    // The edge is still registered under its own name.
    assert!(registry.by_name("note.ref").is_some());
    // And the problem was reported.
    assert!(
        model
            .problems()
            .iter()
            .any(|p| p.code() == "graph.dangling_target")
    );
}

#[test]
fn to_node_is_sorted_across_many_sources() {
    // Several DocTypes linking to `party` come back sorted by edge name.
    let mut a = Definition::new("alpha_doc");
    a.fields = vec![text("title"), link("owner", "party")];
    let mut b = Definition::new("beta_doc");
    b.fields = vec![text("title"), link("customer", "party")];
    let meta = effective_of(vec![
        a,
        b,
        party(),
        project(),
        project_task(),
        project_watcher(),
    ]);
    let model = GraphModel::derive(&meta);
    let registry = model.registry();
    let into_party: Vec<String> = registry
        .to_node("party")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    let mut sorted = into_party.clone();
    sorted.sort();
    assert_eq!(into_party, sorted);
    assert!(into_party.contains(&"alpha_doc.owner".to_owned()));
    assert!(into_party.contains(&"beta_doc.customer".to_owned()));
    assert!(into_party.contains(&"project.customer".to_owned()));
}
