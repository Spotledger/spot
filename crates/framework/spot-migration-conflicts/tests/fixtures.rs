//! The fixture checks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, system};
use spot_meta::{Customisations, FieldType};
use spot_migration_conflicts::{ArtefactKind, ReleaseContext, SiteFixture, codes, detect};

#[test]
fn a_fixture_setting_a_removed_field_is_reported() {
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
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let fixtures = vec![SiteFixture {
        doctype: "party".to_owned(),
        documents: vec![serde_json::json!({ "name": "Acme", "internal_note": "vip" })],
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_fixtures(&fixtures);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::FIXTURE_FIELD_REMOVED);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::Fixture);
    assert_eq!(item.artefacts[0].id, "party");
}

#[test]
fn a_fixture_on_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy", vec![field("code", FieldType::Text)]),
    ];
    let to = vec![definition("party", vec![field("name", FieldType::Text)])];
    let new_standard = to.clone();
    let plan = release(&system(&from, 1), &system(&to, 2));
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let fixtures = vec![SiteFixture {
        doctype: "legacy".to_owned(),
        documents: vec![serde_json::json!({ "code": "x" })],
    }];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_fixtures(&fixtures);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::FIXTURE_DOCTYPE_REMOVED);
}

#[test]
fn a_fixture_the_release_leaves_alone_is_not_a_conflict() {
    let definitions = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let fixtures = vec![SiteFixture {
        doctype: "party".to_owned(),
        documents: vec![serde_json::json!({ "name": "Acme" })],
    }];
    let context =
        ReleaseContext::new(&plan, &definitions, &meta, &customisations).with_fixtures(&fixtures);
    assert!(detect(&context).is_empty());
}
