//! Property tests: `plan(a, a)` is empty, and applying `plan(a, b)` to `a` gives `b` for a
//! model of metadata operations.
#![allow(missing_docs)]

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use spot_meta::{Definition, FieldSpec, FieldType};
use spot_migration_plan::{
    Area, Change, ChangeKind, DocTypeDescription, SchemaPlan, SystemDescription, plan,
};

/// One generated field: name, type index, required, default, protection group.
type GenField = (usize, u8, bool, Option<usize>, Option<usize>);

/// A strategy over a small pool of fields.
fn fields() -> impl Strategy<Value = Vec<GenField>> {
    prop::collection::vec(
        (
            0usize..4,
            0u8..3,
            any::<bool>(),
            prop::option::of(0usize..2),
            prop::option::of(0usize..2),
        ),
        0..4,
    )
}

/// Turns a generated field into a [`FieldSpec`].
fn spec(field: &GenField) -> FieldSpec {
    let (name, ty, required, default, protection) = field;
    let ty = match ty {
        0 => FieldType::Text,
        1 => FieldType::Int,
        _ => FieldType::Decimal,
    };
    let mut s = FieldSpec::new(["a", "b", "c", "d"][*name], ty);
    s.required = *required;
    s.default = default.map(|_| "0".to_owned());
    s.protection_group = protection.map(|_| "group_a".to_owned());
    if ty == FieldType::Decimal {
        s.precision = Some(18);
        s.scale = Some(2);
    }
    s
}

/// A system with one DocType built from the generated fields (first name wins).
fn build(generated: &[GenField]) -> SystemDescription {
    let mut seen = BTreeSet::new();
    let mut fields = Vec::new();
    for field in generated {
        let name = ["a", "b", "c", "d"][field.0].to_owned();
        if seen.insert(name) {
            fields.push(spec(field));
        }
    }
    let mut definition = Definition::new("party");
    definition.fields = fields;
    let doctype = DocTypeDescription {
        definition,
        permissions: Vec::new(),
        views: Default::default(),
    };
    SystemDescription {
        doctypes: vec![doctype],
        ..SystemDescription::new()
    }
}

/// A field map of a system's single DocType.
fn field_map(system: &SystemDescription) -> BTreeMap<String, FieldSpec> {
    system.doctypes[0]
        .definition
        .fields
        .iter()
        .map(|f| (f.name.clone(), f.clone()))
        .collect()
}

/// Applies the metadata changes of `changes` to `a`, taking the new values from `b`.
fn apply(a: &SystemDescription, changes: &[Change], b: &SystemDescription) -> SystemDescription {
    let mut result = a.clone();
    let target = field_map(b);
    let fields = &mut result.doctypes[0].definition.fields;
    let find = |fields: &Vec<FieldSpec>, name: &str| fields.iter().position(|f| f.name == name);
    for change in changes.iter().filter(|c| c.area == Area::Metadata) {
        let name = change.target.split('.').nth(1).unwrap_or("").to_owned();
        match change.kind {
            ChangeKind::FieldAdded => {
                if let Some(spec) = target.get(&name) {
                    fields.push(spec.clone());
                }
            }
            ChangeKind::FieldRemoved => {
                fields.retain(|f| f.name != name);
            }
            ChangeKind::FieldRetyped
            | ChangeKind::FieldRequiredChanged
            | ChangeKind::FieldDefaultChanged
            | ChangeKind::ProtectionChanged => {
                if let (Some(ix), Some(spec)) = (find(fields, &name), target.get(&name)) {
                    fields[ix] = spec.clone();
                }
            }
            ChangeKind::FieldRenamed => {
                let to = change.after.clone().unwrap_or_default();
                if let Some(ix) = find(fields, &name) {
                    fields[ix].name = to;
                }
            }
            _ => {}
        }
    }
    result
}

proptest! {
    #[test]
    fn plan_of_a_system_with_itself_is_empty(generated in fields()) {
        let a = build(&generated);
        let release = plan(&a, &a, &SchemaPlan::default());
        prop_assert!(release.is_empty(), "plan(a, a) has {} changes", release.len());
    }

    #[test]
    fn applying_the_metadata_plan_gives_the_target(a in fields(), b in fields()) {
        let from = build(&a);
        let to = build(&b);
        let release = plan(&from, &to, &SchemaPlan::default());
        let applied = apply(&from, &release.changes, &to);
        prop_assert_eq!(field_map(&applied), field_map(&to));
    }
}
