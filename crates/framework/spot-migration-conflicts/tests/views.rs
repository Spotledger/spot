//! The view-layer checks.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{definition, field, meta, release, system};
use spot_meta::{Customisations, FieldType};
use spot_migration_conflicts::{ArtefactKind, ReleaseContext, codes, detect};
use spot_views::{Column, LayerKind, LayerName, Op, ViewKind, ViewLayer, default_view};

/// A site layer over `doctype` with `ops`.
fn site_layer(doctype: &str, ops: Vec<Op>) -> ViewLayer {
    ViewLayer {
        doctype: doctype.to_owned(),
        kind: LayerKind::named(LayerName::Site),
        ops,
    }
}

#[test]
fn a_view_layer_hiding_a_removed_field_is_reported() {
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
    let layers = vec![site_layer(
        "party",
        vec![Op::HideField {
            field: "internal_note".to_owned(),
        }],
    )];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_views(&layers, &[]);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::VIEW_FIELD_REMOVED);
    assert_eq!(item.artefacts[0].kind, ArtefactKind::ViewLayer);
    assert_eq!(item.artefacts[0].id, "party:site");
}

#[test]
fn a_list_column_naming_a_removed_field_is_reported() {
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
    let base = default_view(&meta, "party", ViewKind::List).unwrap();
    let layers = vec![site_layer(
        "party",
        vec![Op::SetColumns {
            columns: vec![
                Column {
                    field: "name".to_owned(),
                    label: None,
                },
                Column {
                    field: "internal_note".to_owned(),
                    label: None,
                },
            ],
            sort: None,
        }],
    )];
    let context = ReleaseContext::new(&plan, &new_standard, &meta, &customisations)
        .with_views(&layers, std::slice::from_ref(&base));
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::VIEW_FIELD_REMOVED);
}

#[test]
fn a_view_layer_on_a_removed_doctype_is_reported() {
    let from = vec![
        definition("party", vec![field("name", FieldType::Text)]),
        definition("legacy", vec![field("code", FieldType::Text)]),
    ];
    let to = vec![definition("party", vec![field("name", FieldType::Text)])];
    let new_standard = to.clone();
    let plan = release(&system(&from, 1), &system(&to, 2));
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let layers = vec![site_layer(
        "legacy",
        vec![Op::HideField {
            field: "code".to_owned(),
        }],
    )];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_views(&layers, &[]);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::VIEW_DOCTYPE_REMOVED);
}

#[test]
fn a_view_that_hides_a_field_the_release_makes_required_is_reported() {
    let from = definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("code", FieldType::Text),
        ],
    );
    let mut required = field("code", FieldType::Text);
    required.required = true;
    let to = definition("party", vec![field("name", FieldType::Text), required]);
    let new_standard = vec![to.clone()];
    let plan = release(&system(&[from], 1), &system(&[to], 2));
    let customisations = Customisations::new();
    let meta = meta(&new_standard, &customisations);
    let layers = vec![site_layer(
        "party",
        vec![Op::HideField {
            field: "code".to_owned(),
        }],
    )];
    let context =
        ReleaseContext::new(&plan, &new_standard, &meta, &customisations).with_views(&layers, &[]);
    let report = detect(&context);

    let [item] = report.items.as_slice() else {
        panic!("exactly one conflict, got {:#?}", report.items);
    };
    assert_eq!(item.code, codes::VIEW_HIDDEN_REQUIRED);
}

#[test]
fn a_view_layer_the_release_leaves_alone_is_not_a_conflict() {
    let definitions = vec![definition(
        "party",
        vec![
            field("name", FieldType::Text),
            field("internal_note", FieldType::Text),
        ],
    )];
    let plan = release(&system(&definitions, 1), &system(&definitions, 1));
    let customisations = Customisations::new();
    let meta = meta(&definitions, &customisations);
    let layers = vec![site_layer(
        "party",
        vec![Op::HideField {
            field: "internal_note".to_owned(),
        }],
    )];
    let context =
        ReleaseContext::new(&plan, &definitions, &meta, &customisations).with_views(&layers, &[]);
    assert!(detect(&context).is_empty());
}
