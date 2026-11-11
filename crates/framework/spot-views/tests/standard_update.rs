//! The property the layer design exists for: an app update that adds a field still shows it,
//! in the standard position, to a site that customised the layout (ADR-068 §7.2).

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_views::layer::{LayerKind, LayerName, Op, ViewLayer};
use spot_views::resolve::resolve;
use spot_views::viewdef::{Item, Section, ViewDef, ViewKind};

/// A form over `fields`; every field is one item in one top section.
fn base(doctype: &str, fields: &[&str]) -> ViewDef {
    ViewDef {
        doctype: doctype.to_owned(),
        kind: ViewKind::Form,
        role: None,
        title: None,
        density: None,
        tabs: vec![],
        sections: vec![Section {
            id: "general".to_owned(),
            title: None,
            columns: None,
            items: fields
                .iter()
                .map(|field| Item::Field {
                    field: (*field).to_owned(),
                    label: None,
                    control: None,
                    lookup: None,
                })
                .collect(),
        }],
        columns: vec![],
        sort: None,
        actions: vec![],
    }
}

fn meta(doctype: &str, fields: &[&str]) -> std::sync::Arc<spot_meta::EffectiveMeta> {
    let mut definition = Definition::new(doctype);
    definition.fields = fields
        .iter()
        .map(|field| FieldSpec::new(field, FieldType::Text))
        .collect();
    Registry::new(vec![definition])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("test metadata is invalid: {errors:?}"))
}

fn order(view: &ViewDef) -> Vec<String> {
    view.sections
        .iter()
        .flat_map(|section| section.items.iter())
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn an_update_adds_a_field_in_its_standard_spot_for_a_customised_site() {
    // Site v1: the customer is tucked behind the date.
    let site_layer = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![Op::MoveField {
            field: "customer".to_owned(),
            tab: None,
            section: Some("general".to_owned()),
            before: None,
            after: Some("date".to_owned()),
        }],
    };
    let v1 = base("invoice", &["customer", "date"]);
    let customised = resolve(
        &v1,
        std::slice::from_ref(&site_layer),
        &meta("invoice", &["customer", "date"]),
    );
    assert_eq!(order(&customised.view), vec!["date", "customer"]);

    // v2 of the app adds `total` right after `date` in the standard view.
    let v2 = base("invoice", &["customer", "date", "total"]);
    let updated = resolve(
        &v2,
        &[site_layer],
        &meta("invoice", &["customer", "date", "total"]),
    );
    // The site's customer move survives; `total` was never mentioned by the site layer, so
    // it appends in definition order at the end of the last section — still shown to the
    // customised site, exactly as §7.2 promises.
    assert_eq!(order(&updated.view), vec!["date", "customer", "total"]);
    assert!(
        updated.warnings.is_empty(),
        "nothing went stale: {:?}",
        updated.warnings
    );
}

#[test]
fn a_stored_field_never_silently_disappears() {
    // A minimal site layer that only adjusts a control: every field of the definition must
    // still be in the resolved view, in definition order.
    let site_layer = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![Op::SetHint {
            field: "customer".to_owned(),
            label: Some("Client".to_owned()),
            control: None,
            lookup: None,
        }],
    };
    let view = base("invoice", &["customer", "date", "total"]);
    let resolved = resolve(
        &view,
        &[site_layer],
        &meta("invoice", &["customer", "date", "total"]),
    );
    assert_eq!(order(&resolved.view), vec!["customer", "date", "total"]);
    let label = match resolved.view.sections[0].items[0] {
        Item::Field {
            label: Some(ref label),
            ..
        } => label.clone(),
        _ => panic!("the hint did not land"),
    };
    assert_eq!(label, "Client");
}

#[test]
fn a_field_a_layer_hid_is_not_appended() {
    let site_layer = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![Op::HideField {
            field: "total".to_owned(),
        }],
    };
    let v2 = base("invoice", &["customer", "date"]);
    let resolved = resolve(
        &v2,
        &[site_layer],
        &meta("invoice", &["customer", "date", "total"]),
    );
    assert_eq!(order(&resolved.view), vec!["customer", "date"]);
    assert_eq!(resolved.hidden, vec!["total"]);
    assert!(resolved.warnings.is_empty(), "{resolved:?}");
}
