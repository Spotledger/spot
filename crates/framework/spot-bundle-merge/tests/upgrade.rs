//! Checking a bundle against a platform upgrade, through `spot-migration-conflicts`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use spot_bundle_merge::{Conflict, ConflictKind, UpgradeCheck, check_against_upgrade};
use spot_custom_bundle::{Bundle, FakeSite};
use spot_meta::{Customisations, Definition, FieldSpec, FieldType, Registry};
use spot_migration_plan::{DocTypeDescription, ReleasePlan, SchemaPlan, SystemDescription, plan};

/// A definition of `name` with the given fields.
fn definition(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut definition = Definition::new(name);
    definition.fields = fields;
    definition
}

/// A `SystemDescription` carrying `definitions`.
fn system(definitions: &[Definition], version: u64) -> SystemDescription {
    SystemDescription {
        doctypes: definitions
            .iter()
            .map(|definition| DocTypeDescription {
                definition: definition.clone(),
                permissions: Vec::new(),
                views: BTreeMap::new(),
            })
            .collect(),
        meta_version: version,
        ..SystemDescription::default()
    }
}

/// The release plan from `from` to `to` with no schema steps.
fn release(from: &[Definition], to: &[Definition]) -> ReleasePlan {
    plan(&system(from, 1), &system(to, 2), &SchemaPlan::default())
}

/// A site with a single custom field, captured as a bundle.
fn bundle_with_field(doctype: &str, name: &str) -> Bundle {
    let mut site: FakeSite = FakeSite::new();
    add_field(&mut site, doctype, name, None);
    bundle(&site, "site")
}

/// The upgrade code of the single conflict, or `None` when there are none.
fn upgrade_codes(conflicts: &[Conflict]) -> Vec<String> {
    conflicts
        .iter()
        .filter_map(|conflict| {
            conflict
                .upgrade
                .as_ref()
                .map(|upgrade| upgrade.code.clone())
        })
        .collect()
}

#[test]
fn a_custom_field_colliding_with_a_new_standard_field_is_reported() {
    let from = vec![definition(
        "party",
        vec![FieldSpec::new("name", FieldType::Text)],
    )];
    let to = vec![definition(
        "party",
        vec![
            FieldSpec::new("name", FieldType::Text),
            FieldSpec::new("custom_code", FieldType::Text),
        ],
    )];
    let plan = release(&from, &to);
    let new_meta =
        Registry::build(to.clone(), &Customisations::new()).expect("the metadata builds");
    let bundle = bundle_with_field("party", "custom_code");

    let conflicts = check_against_upgrade(&bundle, &plan, &new_meta).expect("check");
    assert_eq!(
        upgrade_codes(&conflicts),
        vec!["migration_conflicts.custom_field_collision".to_owned()]
    );
    let conflict = &conflicts[0];
    assert_eq!(conflict.kind, ConflictKind::Upgrade);
    let upgrade = conflict.upgrade.as_ref().expect("the upgrade facts");
    assert_eq!(upgrade.category, "custom_field_collision");
    assert_eq!(upgrade.risk, "destructive");
    assert_eq!(
        upgrade.release_item,
        "metadata/party/field_added/custom_code"
    );
    assert!(
        upgrade
            .artefacts
            .iter()
            .any(|a| a == "custom_field:party.custom_code")
    );
}

#[test]
fn a_custom_field_on_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![FieldSpec::new("name", FieldType::Text)]),
        definition("legacy", vec![FieldSpec::new("code", FieldType::Text)]),
    ];
    let to = vec![definition(
        "party",
        vec![FieldSpec::new("name", FieldType::Text)],
    )];
    let plan = release(&from, &to);
    let new_meta =
        Registry::build(to.clone(), &Customisations::new()).expect("the metadata builds");
    let bundle = bundle_with_field("legacy", "custom_tag");

    let conflicts = check_against_upgrade(&bundle, &plan, &new_meta).expect("check");
    assert_eq!(
        upgrade_codes(&conflicts),
        vec!["migration_conflicts.custom_field_doctype_removed".to_owned()]
    );
}

#[test]
fn a_bundle_the_release_leaves_alone_has_no_upgrade_conflicts() {
    let definitions = vec![definition(
        "party",
        vec![FieldSpec::new("name", FieldType::Text)],
    )];
    let plan = release(&definitions, &definitions);
    let new_meta =
        Registry::build(definitions.clone(), &Customisations::new()).expect("the metadata builds");
    let bundle = bundle_with_field("party", "custom_tag");

    let conflicts = check_against_upgrade(&bundle, &plan, &new_meta).expect("check");
    assert!(conflicts.is_empty(), "conflicts: {conflicts:?}");
}

/// A fake check that records the call, to prove the trait is the seam.
struct Fake;

impl UpgradeCheck for Fake {
    fn conflicts(
        &self,
        _bundle: &Bundle,
        _plan: &ReleasePlan,
        _new_meta: &spot_meta::EffectiveMeta,
    ) -> Result<Vec<Conflict>, spot_bundle_merge::BundleMergeError> {
        Ok(Vec::new())
    }
}

#[test]
fn the_upgrade_check_is_a_trait() {
    let definitions = vec![definition("party", Vec::new())];
    let plan = release(&definitions, &definitions);
    let new_meta =
        Registry::build(definitions.clone(), &Customisations::new()).expect("the metadata builds");
    let bundle = bundle_with_field("party", "custom_tag");
    assert!(
        Fake.conflicts(&bundle, &plan, &new_meta)
            .expect("check")
            .is_empty()
    );
}
