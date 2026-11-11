//! The traversal state derived from the graph model (WP-STO-07.T5).
#![allow(missing_docs, clippy::unwrap_used)]

mod common;

use common::{dt, field, system};
use spot_graph_model::{GraphModel, RelationSpec};
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
use spot_migration_plan::traversal::next_state;
use spot_migration_plan::{
    ChangeKind, CostModel, RiskTable, SchemaPlan, SystemDescription, TraversalLink,
    plan_with_traversal, state_from_model,
};

fn with_options(name: &str, ty: FieldType, options: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, ty);
    f.options = Some(options.to_owned());
    f
}

fn link(name: &str, target: &str) -> FieldSpec {
    with_options(name, FieldType::Link, target)
}

fn party() -> Definition {
    let mut d = Definition::new("party");
    d.fields = vec![field("code", FieldType::Text)];
    d
}

fn line() -> Definition {
    let mut d = Definition::new("line");
    d.is_child = true;
    d.fields = vec![field("note", FieldType::Text), link("item", "party")];
    d
}

fn project(with_manager: bool) -> Definition {
    let mut d = Definition::new("project");
    d.fields = vec![
        field("title", FieldType::Text),
        link("customer", "party"),
        link("sponsor", "party"),
        with_options("lines", FieldType::Table, "line"),
    ];
    if with_manager {
        d.fields.push(link("manager", "party"));
    }
    d
}

fn member() -> Definition {
    let mut d = Definition::new("member");
    d.fields = vec![
        link("project_ref", "project"),
        link("party_ref", "party"),
        field("role", FieldType::Text),
    ];
    d
}

fn defs(with_manager: bool) -> Vec<Definition> {
    vec![party(), line(), project(with_manager), member()]
}

fn spec() -> RelationSpec {
    RelationSpec {
        doctype: "member".into(),
        from_field: "project_ref".into(),
        to_field: "party_ref".into(),
        edge_name: "project_has_member".into(),
        aggregate: None,
    }
}

fn model_of(definitions: Vec<Definition>, relations: &[RelationSpec]) -> GraphModel {
    let meta = Registry::build(definitions, &Customisations::new()).unwrap();
    GraphModel::derive_relations(&meta, relations)
}

fn system_of(definitions: Vec<Definition>) -> SystemDescription {
    system(
        definitions
            .into_iter()
            .map(|d| {
                let fields = d.fields.clone();
                let mut desc = dt(&d.name, fields);
                desc.definition.is_child = d.is_child;
                desc
            })
            .collect(),
    )
}

fn pairs(state: &spot_migration_plan::TraversalState) -> Vec<(&str, &str)> {
    state
        .traversable
        .iter()
        .map(|l| (l.doctype.as_str(), l.field.as_str()))
        .collect()
}

#[test]
fn links_a_relation_and_a_child_table_give_the_expected_set() {
    let model = model_of(defs(true), &[spec()]);
    let state = state_from_model(&model, &[], &[]);
    // Three Link fields of `project`, the relation's two ends; nothing from the child `line`
    // and nothing for the Table field itself.
    assert_eq!(
        pairs(&state),
        vec![
            ("member", "party_ref"),
            ("member", "project_ref"),
            ("project", "customer"),
            ("project", "manager"),
            ("project", "sponsor"),
        ]
    );
}

#[test]
fn without_a_relation_spec_the_relation_doctype_links_are_plain_links() {
    let model = model_of(defs(true), &[]);
    let state = state_from_model(&model, &[], &[]);
    assert!(pairs(&state).contains(&("member", "project_ref")));
    assert!(pairs(&state).contains(&("member", "party_ref")));
}

#[test]
fn the_same_model_twice_gives_the_same_state() {
    let a = state_from_model(&model_of(defs(true), &[spec()]), &[], &[]);
    let mut reversed = defs(true);
    reversed.reverse();
    let b = state_from_model(&model_of(reversed, &[spec()]), &[], &[]);
    assert_eq!(a, b);
}

#[test]
fn indexed_and_reserved_names_pass_through() {
    let indexed = vec![TraversalLink::new("project", "customer")];
    let reserved = vec!["some_index".to_owned()];
    let state = state_from_model(&model_of(defs(true), &[]), &indexed, &reserved);
    assert_eq!(state.indexed, indexed);
    assert_eq!(state.reserved_names, reserved);
}

#[test]
fn a_dangling_link_is_absent() {
    // A hand-built model can carry a dangling target (the registry refuses one); serde is the
    // way to give it the problem the derive would have recorded.
    let model = model_of(defs(true), &[]);
    let mut value = serde_json::to_value(&model).unwrap();
    value["derive_problems"] = serde_json::json!([{
        "DanglingTarget": { "doctype": "project", "field": "sponsor", "target": "ghost" }
    }]);
    let model: GraphModel = serde_json::from_value(value).unwrap();
    assert_eq!(model.problems().len(), 1);
    let state = state_from_model(&model, &[], &[]);
    assert!(!pairs(&state).contains(&("project", "sponsor")));
    assert!(pairs(&state).contains(&("project", "customer")));
}

#[test]
fn a_field_that_stops_being_an_edge_leaves_the_set_and_the_next_plan_drops_its_index() {
    let before = state_from_model(&model_of(defs(true), &[]), &[], &[]);
    let after_model = model_of(defs(false), &[]);
    let system = system_of(defs(false));
    let manager = TraversalLink::new("project", "manager");
    assert!(before.traversable.contains(&manager));

    // The index exists for every traversable field before the change.
    let indexed = before.traversable.clone();
    let from = state_from_model(&model_of(defs(true), &[]), &indexed, &[]);
    let to = state_from_model(&after_model, &indexed, &[]);
    assert!(!to.traversable.contains(&manager));

    let run = |from: &spot_migration_plan::TraversalState,
               to: &spot_migration_plan::TraversalState| {
        plan_with_traversal(
            &system,
            &system,
            &SchemaPlan::default(),
            &RiskTable::builtin(),
            &CostModel::default(),
            from,
            to,
        )
        .unwrap()
    };
    // The plan that changes the declaration keeps the index.
    let first = run(&from, &to);
    assert!(
        first
            .changes
            .iter()
            .all(|c| c.kind != ChangeKind::SchemaDropIndex)
    );
    // The plan after it drops it (T3 rule).
    let settled = next_state(&from, &system, &to).unwrap();
    let second = run(&settled, &to);
    assert_eq!(
        second
            .changes
            .iter()
            .filter(|c| c.kind == ChangeKind::SchemaDropIndex)
            .count(),
        1
    );
}
