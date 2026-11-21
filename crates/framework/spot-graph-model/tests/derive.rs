//! Derive tests: every kind of edge, determinism, self references, distinct
//! parallel edges, custom fields and removal (brief item 5).

// Test code may unwrap what the test itself built (spot-dag house pattern).
#![allow(clippy::unwrap_used)]

mod common;

use std::collections::BTreeSet;

use common::WithOptions;
use common::{
    effective, effective_of, fixtures, link, order, party, project, project_task, project_watcher,
    registry, text,
};
use spot_graph_model::{
    AggregateHow, AggregateSpec, Cardinality, EdgeSource, EdgeTypeId, GraphModel, NodeType,
    NodeTypeId, Ownership, RelationSpec, TargetShape,
};
use spot_meta::{Customisations, Definition, FieldType, Registry};

fn node_names(model: &GraphModel) -> Vec<&str> {
    model.nodes.keys().map(String::as_str).collect()
}

fn edge_names(model: &GraphModel) -> Vec<&str> {
    model.edges.keys().map(String::as_str).collect()
}

#[test]
fn every_field_shape_yields_its_edge_kind() {
    let model = GraphModel::derive(&effective());
    // Link: `project.customer` (also `project.manager`, `project.parent`).
    let customer = model.edge("project.customer").unwrap();
    assert_eq!(customer.source, EdgeSource::LinkField);
    assert_eq!(customer.to_one(), Some("party"));
    assert_eq!(customer.cardinality, Cardinality::ManyToOne);
    assert_eq!(customer.ownership, Ownership::Ref);
    assert!(!customer.required);
    assert!(customer.properties.is_empty());
    // Table: `project` contains `project_task`, owned, one_to_many, never required.
    let contains = model.edge("project_contains_project_task").unwrap();
    assert_eq!(contains.source, EdgeSource::Table);
    assert_eq!(contains.cardinality, Cardinality::OneToMany);
    assert_eq!(contains.ownership, Ownership::Owned);
    assert!(!contains.required);
    assert_eq!(contains.to_one(), Some("project_task"));
    // TableMultiselect: an edge from the owner to the multi-picked master
    // (not the child), many_to_many, a reference rather than containment.
    let watchers = model.edge("project.watchers").unwrap();
    assert_eq!(watchers.source, EdgeSource::TableMultiselect);
    assert_eq!(watchers.from, "project");
    assert_eq!(watchers.to_one(), Some("party"));
    assert_eq!(watchers.cardinality, Cardinality::ManyToMany);
    assert_eq!(watchers.ownership, Ownership::Ref);
    assert!(!watchers.required);
    assert!(model.problems().is_empty());
    // DynamicLink over a free-text type field: `Dynamic`.
    let about = model.edge("party.about").unwrap();
    assert_eq!(about.source, EdgeSource::DynamicLink);
    assert_eq!(about.to, TargetShape::Dynamic);
    // DynamicLink over a select type field: `AnyOf` the select's values.
    let order_ref = model.edge("order.ref").unwrap();
    assert_eq!(
        order_ref.to,
        TargetShape::AnyOf(BTreeSet::from(["party".to_owned(), "project".to_owned()]))
    );
    // Required mirrors the field: build party with a required link.
    let mut required = party();
    let mut field = link("code_owner", "party");
    field.required = true;
    required.fields.push(field);
    let model = GraphModel::derive(&effective_of(vec![
        required,
        project(),
        project_task(),
        project_watcher(),
        order(),
    ]));
    assert!(model.edge("party.code_owner").unwrap().required);
}

#[test]
fn nodes_are_the_non_child_doctypes() {
    let model = GraphModel::derive(&effective());
    // `project_task` is a child: a node for every top DocType, none for it.
    assert_eq!(node_names(&model), ["order", "party", "project"]);
    assert!(!model.nodes.contains_key("project_task"));
    // A child DocType's own link field (`project_task.assignee`) is *not* an
    // edge of its own: the containment edge carries the rows.
    assert!(model.edge("project_task.assignee").is_none());
}

#[test]
fn node_and_edge_ids_are_sha256_of_the_canonical_name() {
    let model = GraphModel::derive(&effective());
    let project_node = model.node("project").unwrap();
    assert_eq!(project_node.node_id, NodeTypeId::of("project"));
    assert_eq!(
        model.edge("project.customer").unwrap().edge_id,
        EdgeTypeId::of("project.customer")
    );
    assert_eq!(
        model.edge("project_contains_project_task").unwrap().edge_id,
        EdgeTypeId::of("project_contains_project_task")
    );
    // A node id and an edge id of the same text differ (domain tag).
    assert_ne!(NodeTypeId::of("x").as_str(), EdgeTypeId::of("x").as_str());
}

#[test]
fn ids_are_deterministic_across_runs_and_definition_order() {
    let plain = GraphModel::derive(&effective());
    // The same metadata, definitions listed in a different order.
    let mut shuffled = fixtures();
    shuffled.reverse();
    let reversed = GraphModel::derive(&effective_of(shuffled));
    assert_eq!(plain, reversed, "order of definitions must not matter");
    // And two fresh derives in a row are equal too.
    assert_eq!(plain, GraphModel::derive(&effective()));
    // Ids are stable hex regardless of the build order.
    assert_eq!(
        plain.edge("project.customer").unwrap().edge_id.as_str(),
        reversed.edge("project.customer").unwrap().edge_id.as_str()
    );
}

#[test]
fn self_referencing_link_is_a_loop_edge_not_a_problem() {
    let model = GraphModel::derive(&effective());
    let parent = model.edge("project.parent_project").unwrap();
    assert_eq!(parent.from, "project");
    assert_eq!(parent.to_one(), Some("project"));
    // The target is a known non-child DocType: not a problem.
    assert!(model.problems().is_empty(), "{:?}", model.problems());
}

#[test]
fn two_link_fields_to_one_target_stay_distinct() {
    let model = GraphModel::derive(&effective());
    let customer = model.edge("project.customer").unwrap();
    let manager = model.edge("project.manager").unwrap();
    assert_ne!(customer.name, manager.name);
    assert_ne!(customer.edge_id, manager.edge_id);
    // Both live in the by-name and by-id lookups at once.
    let registry = model.registry();
    assert_eq!(
        registry.by_id(&customer.edge_id).unwrap().name,
        "project.customer"
    );
    assert_eq!(
        registry.by_id(&manager.edge_id).unwrap().name,
        "project.manager"
    );
}

#[test]
fn child_doctypes_are_never_nodes_and_never_linked_at() {
    let model = GraphModel::derive(&effective());
    // The child itself is not a node ...
    assert!(!model.nodes.contains_key("project_task"));
    // ... and the only edge into it is the owned containment edge.
    let registry = model.registry();
    let into_child: Vec<String> = registry
        .to_node("project_task")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(into_child, ["project_contains_project_task"]);
    assert!(registry.to_node_dynamic("project_task").is_empty());
}

#[test]
fn dynamic_link_anyof_naming_an_unknown_doctype_is_reported_not_panic() {
    // A select type field's values are free-form (spot-meta validates them as
    // select options, not as DocType names), so an `AnyOf` value that no
    // DocType has is the reachable dangling-target case.
    let mut broken = order();
    let mut kind = spot_meta::FieldSpec::new("ref_kind", FieldType::Select);
    kind.options = Some("party\nnowhere".into());
    let mut ref_field = spot_meta::FieldSpec::new("ref", FieldType::DynamicLink);
    ref_field.options = Some("ref_kind".into());
    broken.fields = vec![text("number"), kind, ref_field];
    let meta = effective_of(vec![
        broken,
        party(),
        project(),
        project_task(),
        project_watcher(),
    ]);
    let model = GraphModel::derive(&meta);
    // Derive completed; the edge is there with the unreachable name in its set.
    let edge = model.edge("order.ref").unwrap();
    match &edge.to {
        TargetShape::AnyOf(targets) => {
            assert!(targets.contains("party"));
            assert!(targets.contains("nowhere"));
        }
        other => panic!("expected AnyOf, got {other:?}"),
    }
    // And the problem is reported exactly once.
    let problems: Vec<_> = model
        .problems()
        .iter()
        .filter(|p| p.code() == "graph.dangling_target")
        .collect();
    assert_eq!(problems.len(), 1, "{:?}", model.problems());
    assert!(problems[0].to_string().contains("nowhere"));
}

#[test]
fn relation_end_pointing_at_a_missing_doctype_is_reported() {
    // A relation spec may name ends spot-meta cannot know (the specs come
    // from a later task's metadata): a spec whose `from_field` names a
    // DocType that does not exist is a reported problem, never a panic.
    let mut defs = fixtures();
    let mut dangled = Definition::new("dangling_relation");
    dangled.fields = vec![link("project", "project"), link("party", "party")];
    // The field is patched to point nowhere only in the model's view: rewrite
    // options after validation is impossible, so name a select value instead.
    defs.push(dangled);
    // An empty-options link cannot pass validation; instead make the *spec*
    // name a field whose options were trimmed: use `text("role")` as
    // `from_field` to hit the not-a-link refusal, and a second spec whose
    // DocType dangles entirely (unknown DocType).
    let specs = [
        RelationSpec {
            doctype: "dangling_relation".into(),
            from_field: "role".into(),
            to_field: "party".into(),
            edge_name: "no_such_edge".into(),
            aggregate: None,
        },
        RelationSpec {
            doctype: "ghost".into(),
            from_field: "a".into(),
            to_field: "b".into(),
            edge_name: "ghost_edge".into(),
            aggregate: None,
        },
    ];
    let model = GraphModel::derive_relations(&effective_of(defs), &specs);
    let _ = common::assert_one_problem(&model, "graph.relation_field_not_link");
    let _ = common::assert_one_problem(&model, "graph.relation_doctype_unknown");
    // Both refusals degrade to the plain walk: the DocType's own edges exist.
    // `dangling_relation` carried no spec-named fields, so it stays a node;
    // `ghost` never existed, so only its spec's problem is reported.
    assert!(model.node("dangling_relation").is_some());
    assert!(model.edge("dangling_relation.project").is_some());
    assert!(model.edge("dangling_relation.party").is_some());
    assert!(model.edge("no_such_edge").is_none());
    assert!(model.edge("ghost_edge").is_none());
}

#[test]
fn removed_link_field_removes_its_edge() {
    let mut reg = registry();
    let before = GraphModel::derive(&reg.effective());
    assert!(before.edge("party.custom_drop").is_none());

    reg.add_custom_field(
        "party",
        spot_meta::FieldSpec::new("custom_drop", FieldType::Link).with_options("party"),
        None,
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    let after = GraphModel::derive(&reg.effective());
    assert!(after.edge("party.custom_drop").is_some());

    reg.remove_custom_field("party", "custom_drop")
        .unwrap_or_else(|e| panic!("{e:?}"));
    let final_model = GraphModel::derive(&reg.effective());
    assert!(
        final_model.edge("party.custom_drop").is_none(),
        "removed field's edge must disappear"
    );
    assert!(
        final_model.edge("project.customer").is_some(),
        "others stay"
    );
}

#[test]
fn custom_link_field_yields_an_edge_in_the_next_derive() {
    let mut reg = registry();
    let before = GraphModel::derive(&reg.effective());
    assert!(before.edge("party.custom_spouse").is_none());

    let version = reg
        .add_custom_field(
            "party",
            spot_meta::FieldSpec::new("custom_spouse", FieldType::Link).with_options("party"),
            None,
        )
        .unwrap_or_else(|e| panic!("{e:?}"));

    // One rebuild, no other change: the version moved, one new edge, nothing else.
    let after = GraphModel::derive(&reg.effective());
    assert!(after.edge("party.custom_spouse").is_some());
    let diff = after.diff(&before);
    assert_eq!(diff.added_edges, vec!["party.custom_spouse".to_owned()]);
    assert!(diff.removed_edges.is_empty());
    assert!(diff.changed_edges.is_empty());
    assert!(version > before.meta_version);
    // The edge is a real one: registry lookups and the dump agree.
    let edge = after.edge("party.custom_spouse").unwrap();
    assert_eq!(edge.to_one(), Some("party"));
    assert_eq!(edge.source, EdgeSource::LinkField);
}

#[test]
fn runtime_created_doctype_gets_edges_for_all_its_links() {
    // A DocType created at runtime is an ordinary definition to the registry:
    // `set_definitions` is the seam the host uses.
    let mut reg = registry();
    let before = GraphModel::derive(&reg.effective());

    let mut tree = Definition::new("site_tree");
    tree.fields = vec![
        text("name"),
        link("parent_site", "site_tree"),
        link("owner", "party"),
    ];
    let mut defs = fixtures();
    defs.push(tree);
    reg.set_definitions(defs)
        .unwrap_or_else(|e| panic!("{e:?}"));

    let after = GraphModel::derive(&reg.effective());
    // Edges for every link of the runtime-created DocType, including the self link.
    assert!(after.edge("site_tree.parent_site").is_some());
    assert!(after.edge("site_tree.owner").is_some());
    assert!(after.node("site_tree").is_some());
    let diff = after.diff(&before);
    assert!(
        diff.added_edges
            .contains(&"site_tree.parent_site".to_owned())
    );
    assert!(diff.added_edges.contains(&"site_tree.owner".to_owned()));
    assert!(diff.added_nodes.contains(&"site_tree".to_owned()));
    // And every link indeed: the node's own edge list is exactly those two.
    let reg_r = after.registry();
    let from_site_tree: Vec<String> = reg_r
        .from_node("site_tree")
        .iter()
        .map(|e| e.name.clone())
        .collect();
    assert_eq!(from_site_tree, ["site_tree.owner", "site_tree.parent_site"]);
}

#[test]
fn posted_doctypes_carry_the_engine_link_edge() {
    // A posted DocType gets `amended_from` (a self link) from the engine:
    // it is metadata, so it is an edge like any other link.
    let mut d = Definition::new("journal_entry");
    d.is_posted = true;
    d.fields = vec![text("entry_number"), link("owner", "party")];
    let meta = effective_of(vec![
        d,
        party(),
        project(),
        project_task(),
        project_watcher(),
    ]);
    let model = GraphModel::derive(&meta);
    let amended = model.edge("journal_entry.amended_from").unwrap();
    assert_eq!(amended.to_one(), Some("journal_entry"));
    assert_eq!(amended.source, EdgeSource::LinkField);
}

#[test]
fn empty_metadata_yields_an_empty_model() {
    let meta = effective_of(vec![]);
    let model = GraphModel::derive(&meta);
    assert!(model.nodes.is_empty());
    assert!(model.edges.is_empty());
    assert!(model.problems().is_empty());
    assert!(model.to_text().starts_with("spot-graph-text-1"));
}

#[test]
fn labels_and_flags_travel_to_nodes() {
    let model = GraphModel::derive(&effective());
    let party_node = model.node("party").unwrap();
    assert_eq!(party_node.label, "Party");
    assert!(!party_node.is_posted);
    let project_node = model.node("project").unwrap();
    assert_eq!(project_node.label, "project");
    // The unlabelled DocType falls back to its name.
}

/// Builds a registry with one custom field, for reuse in the diff tests.
fn registry_with_custom_field() -> (Registry, &'static str) {
    let mut reg = registry();
    reg.add_custom_field(
        "party",
        spot_meta::FieldSpec::new("custom_extra", FieldType::Link).with_options("party"),
        None,
    )
    .unwrap_or_else(|e| panic!("{e:?}"));
    (reg, "party.custom_extra")
}

#[test]
fn fixtures_smoke() {
    let model = GraphModel::derive(&effective());
    assert_eq!(node_names(&model), ["order", "party", "project"]);
    assert_eq!(
        edge_names(&model),
        [
            "order.ref",
            "party.about",
            "project.customer",
            "project.manager",
            "project.parent_project",
            "project.watchers",
            "project_contains_project_task",
        ]
    );
}

#[test]
fn select_type_field_with_unknown_values_reports_them() {
    let mut d = order();
    let mut kind = spot_meta::FieldSpec::new("ref_kind", FieldType::Select);
    kind.options = Some("party\nghost_kind".into());
    let mut ref_field = spot_meta::FieldSpec::new("ref", FieldType::DynamicLink);
    ref_field.options = Some("ref_kind".into());
    d.fields = vec![text("number"), kind, ref_field];
    let meta = effective_of(vec![
        d,
        party(),
        project(),
        project_task(),
        project_watcher(),
    ]);
    let model = GraphModel::derive(&meta);
    // The edge still exists, with the value in its AnyOf set...
    let edge = model.edge("order.ref").unwrap();
    match &edge.to {
        TargetShape::AnyOf(targets) => assert!(targets.contains("ghost_kind")),
        other => panic!("expected AnyOf, got {other:?}"),
    }
    // ...and the unknown value is a problem.
    let _ = common::assert_one_problem(&model, "graph.dangling_target");
}

#[test]
fn node_ids_are_unique_across_the_model() {
    let model = GraphModel::derive(&effective());
    let ids: BTreeSet<NodeTypeId> = model
        .nodes
        .values()
        .map(|n: &NodeType| n.node_id.clone())
        .collect();
    assert_eq!(ids.len(), model.nodes.len());
    let edge_ids: BTreeSet<EdgeTypeId> = model.edges.values().map(|e| e.edge_id.clone()).collect();
    assert_eq!(edge_ids.len(), model.edges.len());
}

#[test]
fn custom_field_via_the_official_seam_counts() {
    // The exact seam the brief names: spot_meta::Registry::add_custom_field.
    let (reg, edge_name) = registry_with_custom_field();
    let model = GraphModel::derive(&reg.effective());
    assert!(model.edge(edge_name).is_some());
}

#[test]
fn two_customisations_build_and_derive_independently() {
    let mut a = Customisations::new();
    let mut b = Customisations::new();
    a.add_field("party", link("custom_alpha", "party"), None);
    b.add_field("party", link("custom_beta", "project"), None);
    let ma = Registry::build(fixtures(), &a).unwrap_or_else(|e| panic!("{e:?}"));
    let mb = Registry::build(fixtures(), &b).unwrap_or_else(|e| panic!("{e:?}"));
    let ma_model = GraphModel::derive(&ma);
    let mb_model = GraphModel::derive(&mb);
    assert!(ma_model.edge("party.custom_alpha").is_some());
    assert!(ma_model.edge("party.custom_beta").is_none());
    assert!(mb_model.edge("party.custom_beta").is_some());
    assert!(mb_model.edge("party.custom_alpha").is_none());
    assert_eq!(
        ma_model
            .diff(&GraphModel::derive(&effective()))
            .added_edges
            .len(),
        1
    );
}

#[test]
fn relation_spec_removes_the_doctype_as_a_node_and_adds_the_typed_edge() {
    let mut defs = fixtures();
    defs.push(common::task_link());
    let meta = effective_of(defs);
    let specs = [common::task_link_spec("works_on")];

    let with_relations = GraphModel::derive_relations(&meta, &specs);
    // The relation DocType is not a node...
    assert!(!with_relations.nodes.contains_key("task_link"));
    // ...but is replaced by one typed edge between its ends.
    let edge = with_relations.edge("works_on").unwrap();
    assert_eq!(edge.from, "project");
    assert_eq!(edge.to_one(), Some("party"));
    assert_eq!(edge.source, EdgeSource::Relation);
    assert_eq!(edge.cardinality, Cardinality::RelationManyToOne);
    // The storage mapping: the relation DocType and its two end fields.
    let relation = edge.relation.as_ref().unwrap();
    assert_eq!(relation.doctype, "task_link");
    assert!(with_relations.problems().is_empty());
    assert!(
        GraphModel::derive(&meta)
            .edges
            .values()
            .all(|e| e.relation.is_none())
    );
    // Its property is the *other* field, `role`, typed as text.
    assert_eq!(edge.properties.len(), 1);
    assert_eq!(edge.properties[0].name, "role");
    // No dangling problems: both ends exist.
    assert!(
        with_relations.problems().is_empty(),
        "{:?}",
        with_relations.problems()
    );

    // The same metadata without the spec keeps the DocType a node and its
    // plain link edges.
    let plain = GraphModel::derive(&meta);
    assert!(plain.node("task_link").is_some());
    assert!(plain.edge("task_link.project").is_some());
    assert!(plain.edge("works_on").is_none());
}

#[test]
fn relation_specs_are_order_independent() {
    let mut defs = fixtures();
    defs.push(common::task_link());
    let meta = effective_of(defs);
    let one = [common::task_link_spec("works_on")];
    let mut two = one.clone();
    two.reverse();
    let a = GraphModel::derive_relations(&meta, &one);
    let b = GraphModel::derive_relations(&meta, &two);
    assert_eq!(a, b);
}

#[test]
fn relation_spec_on_a_missing_doctype_is_a_problem_and_the_plain_walk_runs() {
    let meta = effective_of(fixtures());
    let specs = [RelationSpec {
        doctype: "ghost".into(),
        from_field: "a".into(),
        to_field: "b".into(),
        edge_name: "ghost_edge".into(),
        aggregate: None,
    }];
    let model = GraphModel::derive_relations(&meta, &specs);
    let _ = common::assert_one_problem(&model, "graph.relation_doctype_unknown");
    assert!(model.edge("ghost_edge").is_none());
    assert!(model.node("party").is_some());
}

#[test]
fn two_specs_for_one_edge_name_report_a_duplicate() {
    let mut defs = fixtures();
    defs.push(common::task_link());
    let meta = effective_of(defs);
    // Two *different* relation DocTypes naming the same edge name is not
    // constructible here (one DocType, two specs): the second spec's insert
    // hits the first's name.
    let specs = [
        common::task_link_spec("works_on"),
        RelationSpec {
            doctype: "task_link".into(),
            from_field: "party".into(),
            to_field: "project".into(),
            edge_name: "works_on".into(),
            aggregate: None,
        },
    ];
    let model = GraphModel::derive_relations(&meta, &specs);
    let _ = common::assert_one_problem(&model, "graph.duplicate_edge_name");
    // The second spec was refused: the first spec's edge survived.
    let edge = model.edge("works_on").unwrap();
    assert_eq!(edge.from, "project");
    assert_eq!(edge.to_one(), Some("party"));
    assert_eq!(
        edge.properties
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>(),
        ["role"]
    );
}

#[test]
fn attach_stays_a_property_not_an_edge() {
    let mut d = spot_meta::Definition::new("doc_with_file");
    d.fields = vec![spot_meta::FieldSpec::new("scan", FieldType::Attach)];
    let file = spot_meta::Definition::new("file");
    let meta = effective_of(vec![d, file]);
    let model = GraphModel::derive(&meta);
    assert!(
        model.edges.keys().all(|name| !name.contains("scan")),
        "an attach field yields no edge: {:?}",
        edge_names(&model)
    );
}

/// A `task_link`-shaped relation DocType carrying one extra field of type `ty`.
fn task_link_with(ty: FieldType, options: Option<&str>) -> Vec<Definition> {
    let mut d = Definition::new("task_link");
    let mut extra = spot_meta::FieldSpec::new("extra", ty);
    if let Some(o) = options {
        extra = extra.with_options(o);
    }
    if ty == FieldType::Range {
        extra.format = Some("date_range".into());
    }
    d.fields = vec![
        common::link("project", "project"),
        common::link("party", "party"),
        extra,
    ];
    let mut defs = fixtures();
    defs.push(d);
    // Children of their own: a child can have only one parent table field.
    if options == Some("link_row") {
        let mut rows = Definition::new("link_row");
        rows.is_child = true;
        rows.fields = vec![common::text("note")];
        defs.push(rows);
    }
    if options == Some("link_pick") {
        let mut picks = Definition::new("link_pick");
        picks.is_child = true;
        picks.fields = vec![common::link("party", "party")];
        defs.push(picks);
    }
    defs
}

#[test]
fn relation_property_of_a_type_the_query_cannot_return_is_rejected_at_derive() {
    let kinds = [
        (FieldType::Json, None, "json"),
        (FieldType::Bytes, None, "bytes"),
        (FieldType::Time, None, "time"),
        (FieldType::DateTime, None, "datetime"),
        (FieldType::Geo, None, "geo"),
        (FieldType::Range, None, "range"),
        (FieldType::Secret, None, "secret"),
        (FieldType::Table, Some("link_row"), "table"),
        (
            FieldType::TableMultiselect,
            Some("link_pick"),
            "table_multiselect",
        ),
    ];
    for (ty, options, text) in kinds {
        let meta = effective_of(task_link_with(ty, options));
        let model = GraphModel::derive_relations(&meta, &[common::task_link_spec("works_on")]);
        let problem = common::assert_one_problem(&model, "graph.relation_property_unsupported");
        assert_eq!(
            problem,
            &spot_graph_model::Problem::RelationPropertyUnsupported {
                doctype: "task_link".into(),
                field: "extra".into(),
                field_type: text.into(),
            },
            "{text}"
        );
        let edge = model.edge("works_on").unwrap();
        assert!(
            edge.properties.iter().all(|p| p.name != "extra"),
            "{text} must not be an edge property"
        );
    }
}

#[test]
fn relation_properties_the_query_can_return_are_clean() {
    let mut d = Definition::new("task_link");
    let mut dec = spot_meta::FieldSpec::new("quantity", FieldType::Decimal);
    dec.scale = Some(4);
    d.fields = vec![
        common::link("project", "project"),
        common::link("party", "party"),
        dec,
        common::text("note"),
        spot_meta::FieldSpec::new("effective_from", FieldType::Date),
        spot_meta::FieldSpec::new("scrap", FieldType::Int),
    ];
    let mut defs = fixtures();
    defs.push(d);
    let model =
        GraphModel::derive_relations(&effective_of(defs), &[common::task_link_spec("works_on")]);
    assert!(model.problems().is_empty(), "{:?}", model.problems());
    let names: Vec<&str> = model
        .edge("works_on")
        .unwrap()
        .properties
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    assert_eq!(names, ["quantity", "note", "effective_from", "scrap"]);
}

#[test]
fn unsupported_relation_property_is_reported_once_for_a_spec_listed_twice() {
    let meta = effective_of(task_link_with(FieldType::Json, None));
    let spec = common::task_link_spec("works_on");
    let model = GraphModel::derive_relations(&meta, &[spec.clone(), spec]);
    let _ = common::assert_one_problem(&model, "graph.relation_property_unsupported");
    let _ = common::assert_one_problem(&model, "graph.duplicate_edge_name");
}

fn aggregate_on(property: &str) -> RelationSpec {
    RelationSpec {
        aggregate: Some(AggregateSpec {
            property: property.into(),
            how: AggregateHow::Sum,
        }),
        ..common::task_link_spec("works_on")
    }
}

fn meta_with_task_link() -> spot_meta::EffectiveMeta {
    let mut defs = fixtures();
    defs.push(common::task_link());
    effective_of(defs)
}

#[test]
fn an_aggregate_on_a_numeric_property_is_kept_on_the_edge() {
    let mut defs = fixtures();
    let mut link = common::task_link();
    let mut hours = spot_meta::FieldSpec::new("hours", FieldType::Decimal);
    hours.scale = Some(2);
    link.fields.push(hours);
    defs.push(link);
    let model = GraphModel::derive_relations(&effective_of(defs), &[aggregate_on("hours")]);
    assert!(model.problems().is_empty(), "{:?}", model.problems());
    let relation = model.edge("works_on").unwrap().relation.clone().unwrap();
    assert_eq!(relation.aggregate.unwrap().property, "hours");
}

#[test]
fn an_aggregate_on_an_unknown_property_is_a_problem_and_the_edge_stays_ambiguous() {
    let model = GraphModel::derive_relations(&meta_with_task_link(), &[aggregate_on("ghost")]);
    let _ = common::assert_one_problem(&model, "graph.aggregate_property_unknown");
    assert!(
        model
            .edge("works_on")
            .unwrap()
            .relation
            .as_ref()
            .unwrap()
            .aggregate
            .is_none()
    );
}

#[test]
fn an_aggregate_on_a_text_property_is_a_problem() {
    let model = GraphModel::derive_relations(&meta_with_task_link(), &[aggregate_on("role")]);
    let _ = common::assert_one_problem(&model, "graph.aggregate_property_not_numeric");
    assert!(
        model
            .edge("works_on")
            .unwrap()
            .relation
            .as_ref()
            .unwrap()
            .aggregate
            .is_none()
    );
}

#[test]
fn a_model_without_an_aggregate_prints_and_serialises_as_before() {
    let model = GraphModel::derive_relations(
        &meta_with_task_link(),
        &[common::task_link_spec("works_on")],
    );
    assert!(!model.to_text().contains("aggregate"));
}
