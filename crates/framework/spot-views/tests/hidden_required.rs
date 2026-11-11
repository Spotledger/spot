//! The hidden-required rule (ADR-068 §7.5): a view that hides a field that is `required` and
//! has no `default` cannot strand a save — it is refused when the view is saved.

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_views::error::ViewError;
use spot_views::layer::{LayerKind, LayerName, Op, ViewLayer};
use spot_views::resolve::resolve;
use spot_views::validate::validate_layer;
use spot_views::viewdef::{Item, Section, ViewDef, ViewKind};

/// Effective metadata: `customer` (text), `date` (required), and `total` as configured.
fn build_meta(
    total: &str,
    required: bool,
    default: Option<&str>,
) -> std::sync::Arc<spot_meta::EffectiveMeta> {
    let mut definition = Definition::new("invoice");
    definition.fields = vec![
        FieldSpec::new("customer", FieldType::Text),
        {
            let mut date = FieldSpec::new("date", FieldType::Date);
            date.required = true;
            date
        },
        {
            let mut field = FieldSpec::new(total, FieldType::Text);
            field.required = required;
            field.default = default.map(str::to_owned);
            field
        },
    ];
    Registry::new(vec![definition])
        .map(|registry| registry.effective())
        .unwrap_or_else(|errors| panic!("test metadata is invalid: {errors:?}"))
}

fn form_view(fields: &[&str]) -> ViewDef {
    ViewDef {
        doctype: "invoice".to_owned(),
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

fn hide_site(field: &str) -> ViewLayer {
    ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Site,
            role: None,
            step: None,
        },
        ops: vec![Op::HideField {
            field: field.to_owned(),
        }],
    }
}

fn codes(errors: &[ViewError]) -> Vec<&'static str> {
    errors.iter().map(|error| error.kind.code()).collect()
}

#[test]
fn hiding_a_required_field_without_a_default_is_refused() {
    let meta = build_meta("total", true, None);
    let resolved = resolve(
        &form_view(&["customer", "date", "total"]),
        &[hide_site("total")],
        &meta,
    );
    let problems = validate_layer(&resolved, &meta);
    assert_eq!(
        codes(&problems),
        vec!["view.hidden_required"],
        "{problems:?}"
    );
    match problems.first() {
        Some(problem) => assert_eq!(problem.field.as_deref(), Some("total")),
        None => panic!("the error names the field"),
    }
}

#[test]
fn a_default_or_an_optional_field_is_fine() {
    // A default covers the hidden required field.
    let meta = build_meta("total", true, Some("0"));
    let resolved = resolve(
        &form_view(&["customer", "date", "total"]),
        &[hide_site("total")],
        &meta,
    );
    assert!(
        validate_layer(&resolved, &meta).is_empty(),
        "a default covers it"
    );

    // Not required at all: hiding is fine.
    let meta = build_meta("total", false, None);
    let resolved = resolve(
        &form_view(&["customer", "date", "total"]),
        &[hide_site("total")],
        &meta,
    );
    assert!(validate_layer(&resolved, &meta).is_empty());
}

#[test]
fn a_shown_field_passes_validation() {
    let meta = build_meta("total", true, None);
    // Site hides, the step context shows: nothing hidden stays.
    let show = ViewLayer {
        doctype: "invoice".to_owned(),
        kind: LayerKind {
            kind: LayerName::Step,
            step: Some("start".to_owned()),
            role: None,
        },
        ops: vec![Op::ShowField {
            field: "total".to_owned(),
        }],
    };
    let resolved = resolve(
        &form_view(&["customer", "date", "total"]),
        &[hide_site("total"), show],
        &meta,
    );
    assert!(resolved.hidden.is_empty());
    let problems = validate_layer(&resolved, &meta);
    assert!(problems.is_empty(), "{problems:?}");
}

#[test]
fn an_unbound_control_the_model_does_not_know_is_reported() {
    let meta = build_meta("total", false, None);
    let mut view = form_view(&["customer", "date", "total"]);
    view.sections[0].items.insert(
        0,
        Item::Control {
            control: "carousel".to_owned(),
            text: None,
        },
    );
    let resolved = resolve(&view, &[], &meta);
    let problems = validate_layer(&resolved, &meta);
    assert_eq!(
        codes(&problems),
        vec!["view.unknown_control"],
        "{problems:?}"
    );
}

#[test]
fn every_problem_reports_at_once() {
    let meta = build_meta("total", true, None);
    let mut view = form_view(&["customer", "date", "total"]);
    view.sections[0].items.insert(
        0,
        Item::Control {
            control: "carousel".to_owned(),
            text: None,
        },
    );
    let resolved = resolve(&view, &[hide_site("total")], &meta);
    let problems = validate_layer(&resolved, &meta);
    assert_eq!(
        codes(&problems),
        vec!["view.hidden_required", "view.unknown_control"]
    );
}
