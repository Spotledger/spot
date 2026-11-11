//! The usage manifest: its TOML form, the derivation from the storage schema registry, the
//! checker against that registry, and the `spot-release-meta` context.

#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use common::{catalog, manifest, release_id};
use spot_expand_contract::usage::check_manifest;
use spot_expand_contract::{Error, ManifestProblemKind, Object, StopContext, UsageManifest, codes};
use spot_release_meta::check::ReleaseContext;

#[test]
fn a_manifest_round_trips_through_toml_in_a_stable_order() {
    let first = UsageManifest::from_toml(
        "release = \"2026.10\"\nobjects = [\"sales_invoice\", \"party.note\", \"party\"]\n",
    )
    .expect("a manifest");
    let text = first.to_toml();
    assert_eq!(
        text,
        "release = \"2026.10\"\nobjects = [\"party\", \"party.note\", \"sales_invoice\"]\n"
    );
    let second = UsageManifest::from_toml(&text).expect("a manifest");
    assert_eq!(first, second);
    assert_eq!(first.release().as_str(), "2026.10");
    assert_eq!(first.len(), 3);
    assert!(!first.is_empty());
}

#[test]
fn a_table_entry_covers_its_columns() {
    let table = UsageManifest::from_toml("release = \"1\"\nobjects = [\"party\"]\n").expect("m");
    assert!(table.uses(&Object::parse("party").unwrap()));
    assert!(table.uses(&Object::parse("party.note").unwrap()));

    let column =
        UsageManifest::from_toml("release = \"1\"\nobjects = [\"party.note\"]\n").expect("m");
    assert!(column.uses(&Object::parse("party").unwrap()));
    assert!(column.uses(&Object::parse("party.note").unwrap()));
    assert!(!column.uses(&Object::parse("party.name").unwrap()));
    assert!(!column.uses(&Object::parse("other.note").unwrap()));
}

#[test]
fn a_manifest_derived_from_the_catalogue_covers_it() {
    let registry = catalog(&[("party", &["name", "note"]), ("sales_invoice", &["total"])]);
    let derived = UsageManifest::from_catalog(release_id("2026.10"), &registry);
    assert!(derived.uses(&Object::parse("party.note").unwrap()));
    assert!(derived.uses(&Object::parse("sales_invoice.total").unwrap()));
    assert!(check_manifest(&derived, &registry).is_empty());
}

#[test]
fn a_manifest_that_misses_a_registry_object_is_rejected() {
    let registry = catalog(&[("party", &["name", "note"])]);
    // A manifest that names the table covers every column: it is an over-approximation in the
    // safe direction, so a new column of an existing table is already covered.
    let table_only = manifest("1", &["party"]);
    assert!(check_manifest(&table_only, &registry).is_empty());

    // A tighter manifest that names only some columns misses the new one.
    let older = manifest("1", &["party.name"]);
    let problems = check_manifest(&older, &registry);
    assert_eq!(problems.len(), 1);
    assert_eq!(problems[0].kind, ManifestProblemKind::MissingInManifest);
    assert_eq!(problems[0].object.to_string(), "party.note");

    // A registry with a table the manifest does not know at all is unsound for every object of
    // that table.
    let two_tables = catalog(&[("party", &["name"]), ("sales_invoice", &["total"])]);
    let problems = check_manifest(&manifest("1", &["party"]), &two_tables);
    assert_eq!(problems.len(), 2);
    assert_eq!(problems[0].object.to_string(), "sales_invoice");
    assert_eq!(problems[1].object.to_string(), "sales_invoice.total");
}

#[test]
fn a_manifest_naming_an_unknown_object_is_rejected() {
    let registry = catalog(&[("party", &["name"])]);
    let stale = manifest("1", &["party", "party.gone", "other"]);
    let problems = check_manifest(&stale, &registry);
    let kinds: Vec<_> = problems.iter().map(|p| p.kind).collect();
    assert_eq!(
        kinds,
        vec![
            ManifestProblemKind::UnknownToCatalogue,
            ManifestProblemKind::UnknownToCatalogue
        ]
    );
    assert_eq!(problems[0].object.to_string(), "other");
    assert_eq!(problems[1].object.to_string(), "party.gone");
}

#[test]
fn a_manifest_can_come_from_the_release_metadata_doctypes() {
    let from_doctypes = UsageManifest::from_doctypes(
        release_id("1"),
        vec!["party".to_owned(), "sales_invoice".to_owned()],
    )
    .expect("a manifest");
    assert!(from_doctypes.uses(&Object::parse("party.note").unwrap()));
    assert!(!from_doctypes.uses(&Object::parse("account.note").unwrap()));
}

#[test]
fn stopped_using_reports_what_the_new_release_no_longer_touches() {
    let previous = manifest("1", &["party.note", "sales_invoice", "account.name"]);
    let current = manifest("2", &["sales_invoice"]);
    let stopped = current.stopped_using(&previous);
    let names: Vec<String> = stopped.iter().map(ToString::to_string).collect();
    assert_eq!(names, vec!["account.name", "party.note"]);
}

#[test]
fn the_release_metadata_context_reports_stopped_tables() {
    let previous = manifest("1", &["party.note", "sales_invoice"]);
    let current = manifest("2", &["sales_invoice"]);
    let context = StopContext {
        previous: &previous,
        current: &current,
    };
    assert_eq!(context.stopped_using(), vec!["party".to_owned()]);
    assert!(context.knows_doctype("anything"));
}

#[test]
fn malformed_input_is_a_typed_error() {
    let bad_object = Object::parse("party..note").unwrap_err();
    assert_eq!(bad_object.code(), codes::BAD_OBJECT);

    let bad_release = UsageManifest::from_toml("release = \" \"\nobjects = []\n").unwrap_err();
    assert_eq!(bad_release.code(), codes::BAD_RELEASE);

    let bad_manifest = UsageManifest::from_toml("release = 1\n").unwrap_err();
    assert_eq!(bad_manifest.code(), codes::BAD_MANIFEST);
    assert!(matches!(bad_manifest, Error::BadManifest { .. }));

    let long = "a".repeat(64);
    assert_eq!(Object::table(&long).unwrap_err().code(), codes::BAD_OBJECT);
}
