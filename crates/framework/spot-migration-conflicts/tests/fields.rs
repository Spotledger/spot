//! The custom-field and property-override checks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, select, system};
use spot_meta::{Customisations, FieldType, Property};
use spot_migration_conflicts::{ArtefactKind, ReleaseContext, codes, detect};

#[test]
fn a_custom_field_that_collides_with_a_new_standard_field_is_reported() {
    let from = definition("party", vec![field("name", FieldType::Text)]);
    let to = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("custom_code", FieldType::Text),
        ],
    );
    let new_standard = vec![to.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    let mut customisations = Customisations::new();
    customisations.add_field("party", field("custom_code", FieldType::Text), None);
    // The collision makes the merge impossible, so the new metadata is the app's alone.
    let meta = meta(&new_standard, &Customisations::new());
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::CUSTOM_FIELD_COLLISION);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::CustomField);
    assert_eq!(item.artefacts[0].id, "party.custom_code");
    assert_eq!(
        item.cause.release_item,
        "metadata/party/field_added/custom_code"
    );
}

#[test]
fn a_custom_field_on_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy", vec![field("code", FieldType::Text)]),
    ];
    let to = vec![definition("party", vec![field("name", FieldType::Text)])];
    let new_standard = to.clone();
    let plan = release(&system(&from, 1), &system(&to, 2));
    let mut customisations = Customisations::new();
    customisations.add_field("legacy", field("custom_tag", FieldType::Text), None);
    let meta = meta(&new_standard, &Customisations::new());
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::CUSTOM_FIELD_DOCTYPE_REMOVED);
    assert_eq!(item.artefacts[0].id, "legacy.custom_tag");
}

#[test]
fn a_custom_field_the_release_leaves_alone_is_not_a_conflict() {
    let definitions = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let mut customisations = Customisations::new();
    customisations.add_field("party", field("custom_tag", FieldType::Text), None);
    let meta = meta(&definitions, &customisations);
    let context = ReleaseContext::new(&plan, &definitions, &meta, &customisations);
    assert!(detect(&context).is_empty());
}

#[test]
fn an_override_on_a_removed_field_is_reported() {
    let from = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("internal_note", FieldType::Text),
        ],
    );
    let to = definition("party", vec![field("name", FieldType::Text)]);
    let new_standard = vec![to.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    let mut customisations = Customisations::new();
    customisations.set("party", Some("internal_note"), Property::Required(true));
    let meta = meta(&new_standard, &Customisations::new());
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::OVERRIDE_FIELD_REMOVED);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::PropertyOverride);
    assert_eq!(item.artefacts[0].id, "party.internal_note#required");
}

#[test]
fn an_override_on_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy", vec![field("code", FieldType::Text)]),
    ];
    let to = vec![definition("party", vec![field("name", FieldType::Text)])];
    let new_standard = to.clone();
    let plan = release(&system(&from, 1), &system(&to, 2));
    let mut customisations = Customisations::new();
    customisations.set("legacy", Some("code"), Property::ReadOnly(true));
    let meta = meta(&new_standard, &Customisations::new());
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::OVERRIDE_DOCTYPE_REMOVED);
    assert_eq!(item.artefacts[0].id, "legacy.code#read_only");
}

#[test]
fn an_options_override_that_keeps_a_removed_value_is_reported() {
    let from = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            select("status", &["a", "b", "c"]),
        ],
    );
    let to = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            select("status", &["a", "b"]),
        ],
    );
    let new_standard = vec![to.clone()];
    let old_standard = vec![from.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    let mut customisations = Customisations::new();
    customisations.set(
        "party",
        Some("status"),
        Property::Options(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]),
    );
    let meta = meta(&new_standard, &customisations);
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations)
        .with_old_standard(&old_standard);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::OVERRIDE_OPTIONS_CONFLICT);
    assert!(item.resolution.contains('c'));
}

#[test]
fn an_options_override_that_lacks_a_new_value_is_reported() {
    let from = definition(
        "party",
        vec![field("name", FieldType::Text), select("status", &["a"])],
    );
    let to = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            select("status", &["a", "b"]),
        ],
    );
    let new_standard = vec![to.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    let mut customisations = Customisations::new();
    customisations.set(
        "party",
        Some("status"),
        Property::Options(vec!["a".to_owned()]),
    );
    // The merge would refuse this override, so the metadata is the app's alone.
    let meta = meta(&new_standard, &Customisations::new());
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::OVERRIDE_OPTIONS_CONFLICT);
    assert!(item.resolution.contains('b'));
}

#[test]
fn an_options_override_that_matches_is_not_a_conflict() {
    let definitions = vec![definition(
        "party",
        vec![
            field("name", FieldType::Text),
            select("status", &["a", "b"]),
        ],
    )];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let mut customisations = Customisations::new();
    customisations.set(
        "party",
        Some("status"),
        Property::Options(vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]),
    );
    let meta = meta(&definitions, &customisations);
    let context = ReleaseContext::new(&plan, &definitions, &meta, &customisations);
    assert!(detect(&context).is_empty());
}
