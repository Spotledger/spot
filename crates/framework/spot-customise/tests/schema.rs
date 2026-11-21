#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! How a catalogue change is classified before anything is applied.

use spot_customise::{StepClass, diff_catalogs};
use spot_storage_contract::{Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, Name};

fn name(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn entity(table: &str, fields: &[(&str, FieldType)]) -> EntityDef {
    EntityDef {
        name: name(table),
        kind: EntityKind::Document,
        fields: fields
            .iter()
            .map(|(n, ty)| FieldDef {
                name: name(n),
                ty: ty.clone(),
                nullable: true,
            })
            .collect(),
        children: Vec::new(),
        indexes: Vec::new(),
        exclusions: vec![],
        search: None,
    }
}

fn catalog(version: u64, entities: Vec<EntityDef>) -> Catalog {
    Catalog::new(version, Collation::new("und").unwrap(), entities).unwrap()
}

fn text() -> FieldType {
    FieldType::Text { max_chars: None }
}

#[test]
fn a_new_table_and_a_new_column_are_additive() {
    let old = catalog(1, vec![entity("invoice", &[("title", text())])]);
    let new = catalog(
        2,
        vec![
            entity("invoice", &[("title", text()), ("custom_note", text())]),
            entity("ticket", &[("subject", text())]),
        ],
    );
    let plan = diff_catalogs(&old, &new);
    assert!(plan.refused.is_empty(), "{plan:?}");
    let keys: Vec<&str> = plan.steps.iter().map(|s| s.key.as_str()).collect();
    assert!(keys.contains(&"add_field:invoice.custom_note"));
    assert!(keys.contains(&"create_entity:ticket"));
    assert_eq!(keys.last(), Some(&"finalise"));
    assert!(plan.steps.iter().all(|s| s.class == StepClass::Additive));
}

#[test]
fn a_dropped_table_or_column_and_a_changed_type_are_refused_by_name() {
    let old = catalog(
        1,
        vec![
            entity("invoice", &[("title", text()), ("qty", FieldType::Int)]),
            entity("legacy", &[("x", text())]),
        ],
    );
    let new = catalog(2, vec![entity("invoice", &[("qty", text())])]);
    let plan = diff_catalogs(&old, &new);
    let refused: Vec<String> = plan.refused.iter().map(ToString::to_string).collect();
    assert_eq!(refused.len(), 3, "{refused:?}");
    assert!(
        refused
            .iter()
            .any(|r| r.contains("`invoice.title`") && r.contains("dropping"))
    );
    assert!(
        refused
            .iter()
            .any(|r| r.contains("`invoice.qty`") && r.contains("type"))
    );
    assert!(
        refused
            .iter()
            .any(|r| r.contains("`legacy`") && r.contains("dropping"))
    );
}

#[test]
fn an_unchanged_catalogue_plans_nothing() {
    let c = catalog(1, vec![entity("invoice", &[("title", text())])]);
    assert!(diff_catalogs(&c, &c).is_empty());
}
