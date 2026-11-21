//! Shared helpers for the `spot-migration-conflicts` tests.
//!
//! Builds the plain values a detection run needs: definitions, two system descriptions, the
//! release plan between them, and the effective metadata. Nothing here touches a database, a
//! clock or the environment.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::collections::BTreeMap;

use spot_meta::{Customisations, Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_migration_plan::{DocTypeDescription, ReleasePlan, SchemaPlan, SystemDescription, plan};

/// A field of `name` and `ty`.
pub fn field(name: &str, ty: FieldType) -> FieldSpec {
    FieldSpec::new(name, ty)
}

/// A select field with the given option values.
pub fn select(name: &str, options: &[&str]) -> FieldSpec {
    let mut spec = FieldSpec::new(name, FieldType::Select);
    spec.options = Some(options.join("\n"));
    spec
}

/// A definition of `name` with `fields`.
pub fn definition(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut d = Definition::new(name);
    d.fields = fields;
    d
}

/// A `SystemDescription` carrying `definitions` at `version`.
pub fn system(definitions: &[Definition], version: u64) -> SystemDescription {
    SystemDescription {
        doctypes: definitions
            .iter()
            .map(|d| DocTypeDescription {
                definition: d.clone(),
                permissions: Vec::new(),
                views: BTreeMap::new(),
            })
            .collect(),
        meta_version: version,
        ..SystemDescription::default()
    }
}

/// The release plan from `from` to `to` with no schema steps.
pub fn release(from: &SystemDescription, to: &SystemDescription) -> ReleasePlan {
    plan(from, to, &SchemaPlan::default())
}

/// The effective metadata of `definitions` with `customisations` merged.
pub fn meta(definitions: &[Definition], customisations: &Customisations) -> EffectiveMeta {
    Registry::build(definitions.to_vec(), customisations).expect("the metadata builds")
}
