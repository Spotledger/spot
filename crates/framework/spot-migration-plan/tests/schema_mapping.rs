//! The schema mapping: this crate reads `spot-pg-migrate`'s planner output through
//! [`SchemaPlanSource`] and never duplicates its diff.
//!
//! The backend planner is called with an in-memory catalogue and a hand-built live schema, so
//! the test proves the mapping with no database. The production path is the same
//! [`spot_migration_plan::plan`] call, with a `MigrationPlan` built from a live database.
#![allow(missing_docs)]

use std::collections::BTreeMap;

use spot_migration_plan::{
    BlockingClass, ChangeKind, SchemaPlan, SchemaRunKind, SchemaStepKind, SystemDescription, plan,
};
use spot_pg_migrate::{LiveColumn, LiveSchema, LiveTable};
use spot_storage_contract::Name;
use spot_storage_contract::schema::{
    Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType,
};

/// A storage name or a panic.
fn name(text: &str) -> Name {
    match Name::new(text) {
        Ok(name) => name,
        Err(error) => panic!("{error}"),
    }
}

/// A collation or a panic.
fn collation(locale: &str) -> Collation {
    match Collation::new(locale) {
        Ok(collation) => collation,
        Err(error) => panic!("{error}"),
    }
}

/// A document entity with one field.
fn entity(table: &str, field: &str, ty: FieldType) -> EntityDef {
    EntityDef {
        name: name(table),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: name(field),
            ty,
            nullable: true,
        }],
        children: Vec::new(),
        indexes: Vec::new(),
        exclusions: vec![],
        search: None,
    }
}

/// A catalogue holding one entity.
fn catalog(table: &str, field: &str, ty: FieldType) -> Catalog {
    match Catalog::new(1, collation("en"), vec![entity(table, field, ty)]) {
        Ok(catalog) => catalog,
        Err(error) => panic!("{error}"),
    }
}

/// The target system: one DocType with one text field.
fn target(table: &str, field: &str) -> SystemDescription {
    let mut definition = spot_meta::Definition::new(table);
    definition.fields = vec![spot_meta::FieldSpec::new(field, spot_meta::FieldType::Text)];
    SystemDescription {
        doctypes: vec![spot_migration_plan::DocTypeDescription {
            definition,
            permissions: Vec::new(),
            views: Default::default(),
        }],
        ..SystemDescription::new()
    }
}

#[test]
fn a_new_table_maps_from_the_backend_planner() {
    // An empty live schema against a catalogue with one table: the backend plans the table.
    let live = LiveSchema::default();
    let wanted = catalog("party", "name", FieldType::Text { max_chars: None });
    let migration = spot_pg_migrate::plan(&live, &wanted);

    let schema = SchemaPlan::from_migration(&migration);
    assert!(
        schema
            .steps
            .iter()
            .any(|s| s.kind == SchemaStepKind::AddEntity && s.entity == "party"),
        "the create-table step is mapped: {schema:?}"
    );
    assert!(
        schema
            .steps
            .iter()
            .any(|s| s.kind == SchemaStepKind::Finalise),
        "the finalise step is mapped: {schema:?}"
    );

    // The production call: `plan` takes the backend's `MigrationPlan` directly.
    let from = SystemDescription::new();
    let to = target("party", "name");
    let release = plan(&from, &to, &migration);
    assert!(
        release
            .changes
            .iter()
            .any(|c| c.kind == ChangeKind::SchemaAddEntity),
        "the plan holds the mapped entity change: {:?}",
        release.changes
    );
}

#[test]
fn a_refused_narrowing_maps_from_the_backend_planner() {
    // The live column is numeric(18,4); the catalogue wants numeric(18,2): the backend refuses.
    let mut columns = BTreeMap::new();
    columns.insert(
        "amount".to_owned(),
        LiveColumn {
            name: "amount".to_owned(),
            ty: "numeric(18,4)".to_owned(),
            not_null: false,
            collation: None,
        },
    );
    let mut tables = BTreeMap::new();
    tables.insert(
        "party".to_owned(),
        LiveTable {
            columns,
            has_rows: false,
        },
    );
    let live = LiveSchema {
        tables,
        ..LiveSchema::default()
    };
    let wanted = catalog(
        "party",
        "amount",
        FieldType::Decimal {
            precision: 18,
            scale: 2,
        },
    );
    let migration = spot_pg_migrate::plan(&live, &wanted);
    assert!(
        migration
            .refused
            .iter()
            .any(|r| r.reason.contains("narrow")),
        "the backend refused the narrowing: {:?}",
        migration.refused
    );

    let schema = SchemaPlan::from_migration(&migration);
    assert!(
        schema.refusals.iter().any(|r| r.reason.contains("narrow")),
        "the refusal reason is mapped: {schema:?}"
    );

    let from = SystemDescription::new();
    let to = target("party", "amount");
    let release = plan(&from, &to, &migration);
    let blocked = release.changes.iter().find(|c| {
        c.kind == ChangeKind::SchemaBlocked
            && c.reason.as_deref().is_some_and(|r| r.contains("narrow"))
    });
    match blocked {
        Some(change) => assert!(change.reason.is_some()),
        None => panic!("no narrowing blocked change in {:?}", release.changes),
    }
}

/// A backend plan whose steps are replaced by one hand-built step.
fn plan_with_step(
    key: &str,
    risk: spot_pg_migrate::Risk,
    run: spot_pg_migrate::Run,
) -> spot_pg_migrate::MigrationPlan {
    let wanted = catalog("party", "name", FieldType::Text { max_chars: None });
    let mut migration = spot_pg_migrate::plan(&LiveSchema::default(), &wanted);
    migration.steps = vec![spot_pg_migrate::Step {
        key: key.to_owned(),
        risk,
        sentence: "A step.".to_owned(),
        run,
    }];
    migration
}

#[test]
fn a_validating_step_is_planned_as_a_maintenance_window() {
    let migration = plan_with_step(
        "add_exclusion:party.no_overlap",
        spot_pg_migrate::Risk::NeedsValidation,
        spot_pg_migrate::Run::Outside("exclusion statement".to_owned()),
    );
    let schema = SchemaPlan::from_migration(&migration);
    assert_eq!(schema.steps[0].run, SchemaRunKind::Validation);

    let release = plan(
        &SystemDescription::new(),
        &target("party", "name"),
        &migration,
    );
    let Some(change) = release
        .changes
        .iter()
        .find(|c| c.id.contains("add_exclusion"))
    else {
        panic!("the validating step is a change: {:?}", release.changes);
    };
    assert_eq!(change.blocking, BlockingClass::MaintenanceWindow);
}

#[test]
fn an_extension_requirement_maps_to_its_own_run_kind() {
    let migration = plan_with_step(
        "extension:postgis",
        spot_pg_migrate::Risk::Additive,
        spot_pg_migrate::Run::Extension {
            name: "postgis".to_owned(),
            min_version: "3.0".to_owned(),
        },
    );
    let schema = SchemaPlan::from_migration(&migration);
    assert_eq!(schema.steps[0].run, SchemaRunKind::Extension);
}
