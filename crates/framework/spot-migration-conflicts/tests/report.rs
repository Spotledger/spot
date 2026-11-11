//! The report as a whole: grouping, determinism, the risk, and the no-false-positive case.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, system};
use spot_meta::{Customisations, FieldType};
use spot_migration_conflicts::{
    ConflictCategory, ReleaseContext, SiteFixture, SiteRule, SiteScript, codes, detect,
};
use spot_migration_plan::Risk;
use spot_views::{LayerKind, LayerName, Op, ViewLayer};

#[test]
fn one_root_cause_breaking_many_artefacts_is_one_item() {
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
    let scripts = (0..3)
        .map(|i| SiteScript {
            name: format!("job_{i}"),
            app: "erp_selling".to_owned(),
            doctype: Some("party".to_owned()),
            hook_point: "validate_save".to_owned(),
            source: "export function on_validate(doc: Party) { return doc.internal_note; }"
                .to_owned(),
        })
        .collect::<Vec<_>>();
    let layers = vec![ViewLayer {
        doctype: "party".to_owned(),
        kind: LayerKind::named(LayerName::Site),
        ops: vec![Op::HideField {
            field: "internal_note".to_owned(),
        }],
    }];
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations)
        .with_scripts(&scripts)
        .with_views(&layers, &[]);
    let report = detect(&context);

    // One item per category: the three scripts group into one, the view into another.
    assert_eq!(report.len(), 2, "{:#?}", report.items);
    let scripts_item = report.by_category(ConflictCategory::ScriptTypeError);
    let [scripts_item] = scripts_item.as_slice() else {
        panic!("one script item, got {:#?}", report.items);
    };
    assert_eq!(scripts_item.artefacts.len(), 3);
    assert_eq!(
        report.by_category(ConflictCategory::ViewFieldRemoved).len(),
        1
    );
    assert_eq!(report.risk(), Risk::Destructive);
}

#[test]
fn the_report_is_deterministic() {
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
    let scripts = vec![
        SiteScript {
            name: "b".to_owned(),
            app: "erp_selling".to_owned(),
            doctype: Some("party".to_owned()),
            hook_point: "validate_save".to_owned(),
            source: "export function on_validate(doc: Party) { return doc.internal_note; }"
                .to_owned(),
        },
        SiteScript {
            name: "a".to_owned(),
            app: "erp_selling".to_owned(),
            doctype: Some("party".to_owned()),
            hook_point: "validate_save".to_owned(),
            source: "export function on_validate(doc: Party) { return doc.internal_note; }"
                .to_owned(),
        },
    ];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_scripts(&scripts);
    let first = detect(&context);
    let second = detect(&context);
    assert_eq!(first, second);
    // Artefacts are sorted, so the report does not depend on the input order.
    let [item] = first.items.as_slice() else {
        panic!("one item, got {:#?}", first.items);
    };
    assert_eq!(item.artefacts[0].id, "erp_selling:a");
    assert_eq!(item.artefacts[1].id, "erp_selling:b");
    assert_eq!(first.plan_digest, plan.plan_digest);
}

#[test]
fn a_two_hundred_customisation_site_with_no_relevant_changes_has_no_conflicts() {
    let definitions = vec![definition("party", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let mut customisations = Customisations::new();
    for i in 0..200 {
        customisations.add_field(
            "party",
            field(&format!("custom_{i}"), FieldType::Text),
            None,
        );
    }
    let meta = meta(&definitions, &customisations);
    let scripts = vec![SiteScript {
        name: "greeting".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("party".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: Party) { return doc.custom_1 ?? doc.name; }"
            .to_owned(),
    }];
    let rules = vec![SiteRule {
        name: "has_tag".to_owned(),
        doctype: Some("party".to_owned()),
        expression: "doc.custom_2 == 'x'".to_owned(),
    }];
    let fixtures = vec![SiteFixture {
        doctype: "party".to_owned(),
        documents: vec![serde_json::json!({ "name": "Acme", "custom_3": "v" })],
    }];
    let layers = vec![ViewLayer {
        doctype: "party".to_owned(),
        kind: LayerKind::named(LayerName::Site),
        ops: vec![Op::HideField {
            field: "custom_4".to_owned(),
        }],
    }];
    let context = ReleaseContext::new(&plan, &definitions, &meta, &customisations)
        .with_scripts(&scripts)
        .with_rules(&rules)
        .with_fixtures(&fixtures)
        .with_views(&layers, &[]);
    let report = detect(&context);
    assert!(report.is_empty(), "{:#?}", report.items);
    assert_eq!(report.risk(), Risk::Additive);
}

#[test]
fn renaming_a_doctype_reports_every_reference_to_the_old_name() {
    // A DocType rename is a removal plus an addition in the plan (no hint exists for DocTypes),
    // so every customisation that names the old DocType is a conflict.
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy_note", vec![field("text", FieldType::Text)]),
    ];
    let to = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("note", vec![field("text", FieldType::Text)]),
    ];
    let new_standard = to.clone();
    let plan = release(&system(&from, 1), &system(&to, 2));
    let mut customisations = Customisations::new();
    customisations.add_field("legacy_note", field("custom_tag", FieldType::Text), None);
    let meta = meta(&new_standard, &Customisations::new());
    let scripts = vec![SiteScript {
        name: "guard".to_owned(),
        app: "erp_selling".to_owned(),
        doctype: Some("legacy_note".to_owned()),
        hook_point: "validate_save".to_owned(),
        source: "export function on_validate(doc: LegacyNote) { return doc.text; }".to_owned(),
    }];
    let fixtures = vec![SiteFixture {
        doctype: "legacy_note".to_owned(),
        documents: vec![serde_json::json!({ "text": "x" })],
    }];
    let layers = vec![ViewLayer {
        doctype: "legacy_note".to_owned(),
        kind: LayerKind::named(LayerName::Site),
        ops: vec![Op::HideField {
            field: "text".to_owned(),
        }],
    }];
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations)
        .with_scripts(&scripts)
        .with_fixtures(&fixtures)
        .with_views(&layers, &[]);
    let report = detect(&context);

    for category in [
        ConflictCategory::CustomFieldDocTypeRemoved,
        ConflictCategory::ScriptDocTypeRemoved,
        ConflictCategory::FixtureDocTypeRemoved,
        ConflictCategory::ViewDocTypeRemoved,
    ] {
        assert_eq!(
            report.by_category(category).len(),
            1,
            "{category:?} in {:#?}",
            report.items
        );
    }
}

#[test]
fn metadata_that_cannot_generate_declarations_is_reported() {
    // `any` is a TypeScript reserved word, so `spot-dts` refuses the interface name.
    let definitions = vec![definition("any", vec![field("name", FieldType::Text)])];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let context = ReleaseContext::new(&plan, &definitions, &meta, &customisations);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::DECLARATIONS_ERROR);
}
