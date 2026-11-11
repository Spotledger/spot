//! Shared helpers for the `spot-expand-contract` tests.
//!
//! Builds the plain values the gate needs — a release plan with contract changes, usage
//! manifests and a storage catalogue — with no database, clock or environment.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use spot_expand_contract::{Object, ReleaseId, UsageManifest};
use spot_migration_plan::estimate::Estimate;
use spot_migration_plan::{
    BlockingClass, Change, ChangeKind, ReleasePlan, Risk, SystemDescription,
};
use spot_storage_contract::Name;
use spot_storage_contract::schema::{
    Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType,
};

/// A release label or a panic.
pub fn release_id(label: &str) -> ReleaseId {
    ReleaseId::new(label).unwrap_or_else(|error| panic!("{error}"))
}

/// One plan change with the fields a classification test needs.
pub fn change(id: &str, kind: ChangeKind, rule: &str, target: &str, risk: Risk) -> Change {
    Change::new(
        id,
        kind,
        rule,
        target,
        format!("{kind:?} on {target}"),
        risk,
        BlockingClass::MaintenanceWindow,
    )
}

/// A blocked (refused) schema change: the contract step the backend cannot do online.
pub fn blocked(target: &str) -> Change {
    change(
        &format!("schema/refused/{target}"),
        ChangeKind::SchemaBlocked,
        "schema/blocked",
        target,
        Risk::Destructive,
    )
}

/// A plan holding `changes`, with the risk the rule table would assign.
pub fn plan(changes: Vec<Change>) -> ReleasePlan {
    let risk = changes
        .iter()
        .map(|c| c.risk)
        .max()
        .unwrap_or(Risk::Additive);
    ReleasePlan {
        from_version: 1,
        to_version: 2,
        changes,
        risk,
        estimate: Estimate::empty(),
        plan_digest: "test-digest".to_owned(),
        to: SystemDescription::default(),
    }
}

/// A usage manifest for `release` touching `objects` (`table` or `table.column`).
pub fn manifest(release: &str, objects: &[&str]) -> UsageManifest {
    let mut parsed = Vec::new();
    for name in objects {
        parsed.push(Object::parse(name).unwrap_or_else(|error| panic!("{error}")));
    }
    UsageManifest::new(release_id(release), parsed)
}

/// A storage name or a panic.
fn name(text: &str) -> Name {
    Name::new(text).unwrap_or_else(|error| panic!("{error}"))
}

/// A catalogue of document entities, each with the named text fields.
pub fn catalog(entities: &[(&str, &[&str])]) -> Catalog {
    let defs = entities
        .iter()
        .map(|(table, fields)| EntityDef {
            name: name(table),
            kind: EntityKind::Document,
            fields: fields
                .iter()
                .map(|field| FieldDef {
                    name: name(field),
                    ty: FieldType::Text { max_chars: None },
                    nullable: true,
                })
                .collect(),
            children: Vec::new(),
            indexes: Vec::new(),
            exclusions: vec![],
            search: None,
        })
        .collect();
    let collation = Collation::new("en").unwrap_or_else(|error| panic!("{error}"));
    Catalog::new(1, collation, defs).unwrap_or_else(|error| panic!("{error}"))
}

/// Like [`catalog`], with one extra non-unique B-tree index per `(table, index name, field)`.
pub fn catalog_with_indexes(
    entities: &[(&str, &[&str])],
    indexes: &[(&str, &str, &str)],
) -> Catalog {
    let base = catalog(entities);
    let mut defs: Vec<EntityDef> = base.entities().cloned().collect();
    for (table, index, field) in indexes {
        let entity = defs
            .iter_mut()
            .find(|e| e.name.as_str() == *table)
            .expect("the table is in the catalogue");
        entity
            .indexes
            .push(spot_storage_contract::schema::IndexDef::btree(
                name(index),
                vec![name(field)],
                false,
            ));
    }
    let collation = Collation::new("en").unwrap_or_else(|error| panic!("{error}"));
    Catalog::new(1, collation, defs).unwrap_or_else(|error| panic!("{error}"))
}
