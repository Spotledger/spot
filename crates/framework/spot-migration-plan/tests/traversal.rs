//! The reverse index of a traversable Link field (WP-STO-07.T3).
#![allow(missing_docs, clippy::unwrap_used)]

mod common;

use common::{dt, field, schema_steps, step, system};
use spot_meta::FieldType;
use spot_migration_plan::traversal::{next_state, wanted};
use spot_migration_plan::{
    ChangeKind, CostModel, PlanError, RiskTable, SchemaPlan, SchemaStepKind, SystemDescription,
    TraversalLink, TraversalState, index_name, plan_with_traversal,
};

fn sys() -> SystemDescription {
    system(vec![
        dt(
            "edge",
            vec![
                field("parent", FieldType::Link),
                field("note", FieldType::Text),
            ],
        ),
        dt("node", vec![field("title", FieldType::Text)]),
    ])
}

fn state(traversable: &[(&str, &str)], indexed: &[(&str, &str)]) -> TraversalState {
    let links = |l: &[(&str, &str)]| l.iter().map(|(d, f)| TraversalLink::new(*d, *f)).collect();
    TraversalState {
        traversable: links(traversable),
        indexed: links(indexed),
        reserved_names: Vec::new(),
    }
}

fn run(
    schema: &SchemaPlan,
    from: &TraversalState,
    to: &TraversalState,
) -> Result<spot_migration_plan::ReleasePlan, PlanError> {
    plan_with_traversal(
        &sys(),
        &sys(),
        schema,
        &RiskTable::builtin(),
        &CostModel::default(),
        from,
        to,
    )
}

#[test]
fn a_traversable_link_field_gets_its_reverse_index() {
    let plan = run(
        &SchemaPlan::default(),
        &state(&[], &[]),
        &state(&[("edge", "parent")], &[]),
    )
    .unwrap();
    assert_eq!(plan.len(), 1);
    let change = &plan.changes[0];
    assert_eq!(change.kind, ChangeKind::SchemaAddIndex);
    assert_eq!(change.target, "edge.gx_edge_parent_rev");
    assert_eq!(plan.steps().len(), 1, "the apply engine runs it");
}

#[test]
fn a_link_field_that_is_not_traversable_gets_none() {
    let plan = run(&SchemaPlan::default(), &state(&[], &[]), &state(&[], &[])).unwrap();
    assert!(plan.is_empty());
}

#[test]
fn replanning_from_an_indexed_state_plans_nothing() {
    let traversable = [("edge", "parent")];
    let plan = run(
        &SchemaPlan::default(),
        &state(&traversable, &traversable),
        &state(&traversable, &traversable),
    )
    .unwrap();
    assert!(plan.is_empty());

    // And the state after the first plan is exactly that state.
    let first_from = state(&[], &[]);
    let first_to = state(&traversable, &[]);
    let next = next_state(&first_from, &sys(), &first_to).unwrap();
    assert_eq!(next.indexed, vec![TraversalLink::new("edge", "parent")]);
    assert!(
        run(&SchemaPlan::default(), &next, &first_to)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn the_drop_comes_in_a_later_plan_never_the_same_one() {
    let link = [("edge", "parent")];
    // The declaration is dropped: the index stays.
    let same_plan = run(
        &SchemaPlan::default(),
        &state(&link, &link),
        &state(&[], &link),
    )
    .unwrap();
    assert!(same_plan.is_empty(), "{:?}", same_plan.changes);

    // The next plan drops it.
    let from = next_state(&state(&link, &link), &sys(), &state(&[], &link)).unwrap();
    assert_eq!(from.indexed.len(), 1);
    let later = run(&SchemaPlan::default(), &from, &state(&[], &[])).unwrap();
    assert_eq!(later.len(), 1);
    assert_eq!(later.changes[0].target, "edge.gx_edge_parent_rev");
    assert_eq!(later.changes[0].kind, ChangeKind::SchemaDropIndex);
    assert_eq!(
        later.changes[0].rule,
        spot_migration_plan::risk::rules::SCHEMA_DROP_TRAVERSAL_INDEX
    );
    let after = next_state(&from, &sys(), &state(&[], &[])).unwrap();
    assert!(after.indexed.is_empty());

    // Making it traversable again before the drop keeps the index and plans nothing.
    let again = run(&SchemaPlan::default(), &from, &state(&link, &[])).unwrap();
    assert!(again.is_empty());
}

#[test]
fn the_name_is_derived_from_the_doctype_and_the_field_only() {
    assert_eq!(index_name("edge", "parent"), "gx_edge_parent_rev");
    assert_eq!(index_name("edge", "parent"), index_name("edge", "parent"));
    let long = index_name(&"d".repeat(40), &"f".repeat(40));
    assert!(long.len() <= 63, "{long}");
    assert_ne!(long, index_name(&"d".repeat(40), &"g".repeat(40)));
}

#[test]
fn a_name_clash_is_a_plan_error() {
    // `a_b` + `c` and `a` + `b_c` both give `gx_a_b_c_rev`.
    let system = system(vec![
        dt("a_b", vec![field("c", FieldType::Link)]),
        dt("a", vec![field("b_c", FieldType::Link)]),
    ]);
    let both = state(&[("a_b", "c"), ("a", "b_c")], &[]);
    let error = wanted(&system, &both).unwrap_err();
    assert!(
        matches!(error, PlanError::TraversalNameClash { .. }),
        "{error}"
    );
    assert_eq!(error.code(), "migplan.traversal_name_clash");
    assert!(error.to_string().contains("gx_a_b_c_rev"));

    // A catalogue index with the same name is a clash too.
    let mut reserved = state(&[("edge", "parent")], &[]);
    reserved
        .reserved_names
        .push("gx_edge_parent_rev".to_owned());
    let error = wanted(&sys(), &reserved).unwrap_err();
    assert_eq!(error.code(), "migplan.traversal_name_clash");
    let error = run(&SchemaPlan::default(), &state(&[], &[]), &reserved).unwrap_err();
    assert!(matches!(error, PlanError::TraversalNameClash { .. }));
}

#[test]
fn only_a_link_field_of_an_existing_doctype_can_be_traversable() {
    for (doctype, name) in [("edge", "note"), ("edge", "missing"), ("ghost", "parent")] {
        let error = wanted(&sys(), &state(&[(doctype, name)], &[])).unwrap_err();
        assert!(
            matches!(error, PlanError::BadInput { .. }),
            "{doctype}.{name}"
        );
        assert_eq!(error.code(), "migplan.bad_input");
    }
}

#[test]
fn a_schema_step_that_already_builds_the_index_is_not_planned_twice() {
    let schema = schema_steps(vec![step(
        "create_index:edge.gx_edge_parent_rev",
        SchemaStepKind::AddIndex,
        "edge",
        Some("gx_edge_parent_rev"),
    )]);
    let plan = run(
        &schema,
        &state(&[], &[]),
        &state(&[("edge", "parent")], &[]),
    )
    .unwrap();
    assert_eq!(plan.len(), 1);
    assert_eq!(
        plan.changes[0].id,
        "schema/create_index:edge.gx_edge_parent_rev"
    );
}

#[test]
fn the_index_goes_before_the_schema_finalise_step() {
    let schema = schema_steps(vec![
        step("create_table:edge", SchemaStepKind::AddEntity, "edge", None),
        step("finalise", SchemaStepKind::Finalise, "", None),
    ]);
    let plan = run(
        &schema,
        &state(&[], &[]),
        &state(&[("edge", "parent")], &[]),
    )
    .unwrap();
    let kinds: Vec<_> = plan.changes.iter().map(|c| c.kind).collect();
    assert_eq!(
        kinds,
        [
            ChangeKind::SchemaAddEntity,
            ChangeKind::SchemaAddIndex,
            ChangeKind::SchemaFinalise
        ]
    );
}

#[test]
fn the_plan_is_deterministic() {
    let to = state(&[("edge", "parent")], &[]);
    let a = run(&SchemaPlan::default(), &state(&[], &[]), &to).unwrap();
    let b = run(&SchemaPlan::default(), &state(&[], &[]), &to).unwrap();
    assert_eq!(a.plan_digest, b.plan_digest);
}

#[test]
fn a_plan_serialised_before_the_drop_kind_still_reads() {
    // Old plans spelled a traversal drop `schema_cleanup_index`; that kind keeps its meaning.
    let old: ChangeKind = serde_json::from_str("\"schema_cleanup_index\"").unwrap();
    assert_eq!(old, ChangeKind::SchemaCleanupIndex);
    let new: ChangeKind = serde_json::from_str("\"schema_drop_index\"").unwrap();
    assert_eq!(new, ChangeKind::SchemaDropIndex);
    assert_eq!(
        serde_json::to_string(&new).unwrap(),
        "\"schema_drop_index\""
    );
}
