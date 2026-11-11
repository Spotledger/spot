//! The reverse index through the compiled catalogue (WP-STO-07.T8, T6 item 3): the catalogue
//! compiled from `traversal::wanted` output, the backend planner and the traversal plan agree,
//! so a site's schema never flip-flops and the index is never planned twice.
#![allow(missing_docs, clippy::unwrap_used)]

mod common;

use std::collections::BTreeMap;

use common::{dt, field, system};
use spot_meta::{Customisations, FieldSpec, FieldType, Registry, ReverseIndex};
use spot_migration_plan::traversal::{next_state, wanted};
use spot_migration_plan::{
    ChangeKind, CostModel, ReleasePlan, RiskTable, SchemaPlan, SchemaStepKind, SystemDescription,
    TraversalLink, TraversalState, plan_with_traversal,
};
use spot_pg_migrate::{LiveColumn, LiveConstraint, LiveIndex, LiveSchema, LiveTable};
use spot_storage_contract::schema::{Catalog, Collation};

const INDEX: &str = "gx_edge_owner_rev";

fn link(name: &str) -> FieldSpec {
    let mut f = field(name, FieldType::Link);
    f.options = Some("node".to_owned());
    f
}

fn sys() -> SystemDescription {
    system(vec![
        dt("edge", vec![link("owner"), field("note", FieldType::Text)]),
        dt("node", vec![field("title", FieldType::Text)]),
    ])
}

fn state(on: bool) -> TraversalState {
    let links = if on {
        vec![TraversalLink::new("edge", "owner")]
    } else {
        vec![]
    };
    TraversalState {
        traversable: links.clone(),
        indexed: links,
        reserved_names: Vec::new(),
    }
}

/// A state where the link is traversable but its index is not built yet.
fn declared_only() -> TraversalState {
    TraversalState {
        indexed: Vec::new(),
        ..state(true)
    }
}

/// The catalogue compiled from `wanted` output for `traversal`.
fn compiled(system: &SystemDescription, traversal: &TraversalState) -> Catalog {
    let defs = system.doctypes.iter().map(|d| d.definition.clone());
    let meta = Registry::build(defs.collect(), &Customisations::new()).unwrap();
    let reverse: Vec<ReverseIndex> = wanted(system, traversal)
        .unwrap()
        .into_iter()
        .map(|i| ReverseIndex {
            table: i.table,
            column: i.column,
            name: i.name,
        })
        .collect();
    meta.catalog_with_reverse_indexes(Collation::new("en").unwrap(), &reverse)
        .unwrap()
}

fn column(name: &str, ty: &str, collation: Option<&str>) -> (String, LiveColumn) {
    (
        name.to_owned(),
        LiveColumn {
            name: name.to_owned(),
            ty: ty.to_owned(),
            not_null: matches!(name, "id" | "version"),
            collation: collation.map(str::to_owned),
        },
    )
}

/// The database a release of `sys()` leaves behind, with or without the reverse index.
fn live(with_index: bool) -> LiveSchema {
    let table = |extra: Vec<(String, LiveColumn)>| LiveTable {
        columns: [
            column("id", "uuid", None),
            column("version", "bigint", None),
        ]
        .into_iter()
        .chain(extra)
        .collect(),
        has_rows: false,
    };
    let mut tables = BTreeMap::new();
    tables.insert(
        "edge".to_owned(),
        table(vec![
            column("owner", "uuid", None),
            column("note", "text", Some("C")),
        ]),
    );
    tables.insert(
        "node".to_owned(),
        table(vec![column("title", "text", Some("C"))]),
    );
    let mut constraints = BTreeMap::new();
    constraints.insert(
        ("edge".to_owned(), "edge_owner_fkey".to_owned()),
        LiveConstraint {
            name: "edge_owner_fkey".to_owned(),
            table: "edge".to_owned(),
            kind: 'f',
            validated: true,
            definition: "FOREIGN KEY (owner) REFERENCES node(id) DEFERRABLE INITIALLY DEFERRED"
                .to_owned(),
        },
    );
    let mut indexes = BTreeMap::new();
    if with_index {
        indexes.insert(
            INDEX.to_owned(),
            LiveIndex {
                name: INDEX.to_owned(),
                table: "edge".to_owned(),
                unique: false,
                columns: vec!["owner".to_owned()],
                valid: true,
            },
        );
    }
    LiveSchema {
        tables,
        indexes,
        constraints,
        collation: Some("en".to_owned()),
        ..LiveSchema::default()
    }
}

fn run(
    schema: &SchemaPlan,
    from: &TraversalState,
    to: &TraversalState,
) -> Result<ReleasePlan, spot_migration_plan::PlanError> {
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

fn schema_of(live: &LiveSchema, wanted: &Catalog) -> SchemaPlan {
    SchemaPlan::from_migration(&spot_pg_migrate::plan(live, wanted))
}

fn index_changes(plan: &ReleasePlan, kind: ChangeKind) -> usize {
    plan.changes.iter().filter(|c| c.kind == kind).count()
}

#[test]
fn the_compiled_catalogue_matches_the_live_schema_with_the_index() {
    // The hand-built live schema is the real shape: the backend plans nothing against it.
    let migration = spot_pg_migrate::plan(&live(true), &compiled(&sys(), &state(true)));
    assert!(migration.refused.is_empty(), "{:?}", migration.refused);
    let keys: Vec<&str> = migration.steps.iter().map(|s| s.key.as_str()).collect();
    assert!(
        keys.iter().all(|k| *k == "finalise"),
        "no schema step is left: {keys:?}"
    );
}

#[test]
fn the_plan_from_the_compiled_catalogue_to_itself_is_empty() {
    let cat = compiled(&sys(), &state(true));
    let schema = schema_of(&live(true), &cat);
    let plan = run(&schema, &state(true), &state(true)).unwrap();
    assert_eq!(index_changes(&plan, ChangeKind::SchemaAddIndex), 0);
    assert_eq!(index_changes(&plan, ChangeKind::SchemaDropIndex), 0);
    // And again from the state a plan leaves behind: no flip-flop.
    let after = next_state(&state(true), &sys(), &state(true)).unwrap();
    assert_eq!(after, state(true));
    let again = run(&schema, &after, &state(true)).unwrap();
    assert_eq!(again.changes.len(), plan.changes.len());
}

#[test]
fn adding_the_index_is_planned_once_and_the_catalogue_agrees() {
    // Without the index, the traversal plan adds it; the backend planner sees the old catalogue.
    let old = compiled(&sys(), &state(false));
    let schema = schema_of(&live(false), &old);
    assert!(
        schema
            .steps
            .iter()
            .all(|s| s.kind == SchemaStepKind::Finalise),
        "{schema:?}"
    );
    let plan = run(&schema, &state(false), &declared_only()).unwrap();
    assert_eq!(index_changes(&plan, ChangeKind::SchemaAddIndex), 1);

    // The new catalogue declares the index too: the backend plans the create, and the traversal
    // plan does not repeat it.
    let new = compiled(&sys(), &declared_only());
    let schema = schema_of(&live(false), &new);
    let creates = schema
        .steps
        .iter()
        .filter(|s| s.kind == SchemaStepKind::AddIndex)
        .count();
    assert_eq!(creates, 1, "{schema:?}");
    let plan = run(&schema, &state(false), &declared_only()).unwrap();
    assert_eq!(
        index_changes(&plan, ChangeKind::SchemaAddIndex),
        1,
        "one add, from the schema step: {:?}",
        plan.changes
    );
    assert!(
        plan.changes
            .iter()
            .all(|c| !c.id.starts_with("schema/traversal_index/")),
        "the traversal plan skips what the schema already creates: {:?}",
        plan.changes
    );
}

#[test]
fn removing_the_index_is_planned_once_and_back_again() {
    // Traversal off: the catalogue drops the declaration; the live index is the traversal plan's
    // to drop (the backend planner never drops an index it does not know).
    let off = compiled(&sys(), &state(false));
    let schema = schema_of(&live(true), &off);
    let from = TraversalState {
        traversable: Vec::new(),
        ..state(true)
    };
    let down = run(&schema, &from, &state(false)).unwrap();
    assert_eq!(index_changes(&down, ChangeKind::SchemaDropIndex), 1);
    // The state after the drop has no index, and turning it back on adds it again.
    let after = next_state(&from, &sys(), &state(false)).unwrap();
    assert!(after.indexed.is_empty());
    let up = run(&schema_of(&live(false), &off), &after, &declared_only()).unwrap();
    assert_eq!(index_changes(&up, ChangeKind::SchemaAddIndex), 1);
}
