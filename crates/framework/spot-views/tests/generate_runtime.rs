//! The runtime-DocType promise of the work package (brief deliverable 5): a DocType
//! created at runtime through `spot_meta::Registry` — with a custom field added
//! afterwards — yields a form and a list view that pass `validate_layer`; the result is
//! deterministic (equal TOML bytes across runs); and every field of the DocType appears in
//! the form exactly once.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_views::generate::default_view;
use spot_views::validate::validate_layer;
use spot_views::viewdef::{Item, ViewDef, ViewKind};

#[path = "generate/common.rs"]
mod common;

use common::{effective_of, golden_fixtures};

/// Builds the fixture metadata, then adds a custom field at runtime — the full sequence a
/// runtime-created DocType goes through (`Registry::add_custom_field`).
fn runtime_meta_with_custom_field() -> std::sync::Arc<EffectiveMeta> {
    let mut registry = Registry::new(golden_fixtures())
        .unwrap_or_else(|errors| panic!("the fixtures do not build: {errors:?}"));
    let mut custom = FieldSpec::new("custom_site_flag", FieldType::Bool);
    custom.label = Some("Site Flag".to_owned());
    registry
        .add_custom_field("item", custom, None)
        .unwrap_or_else(|errors| panic!("the custom field does not add: {errors:?}"));
    registry
        .add_custom_field(
            "item",
            FieldSpec::new("custom_extra_note", FieldType::Text),
            None,
        )
        .unwrap_or_else(|errors| panic!("the second custom field does not add: {errors:?}"));
    registry.effective()
}

/// Every field item of the view, in order (recursing through groups — the generated views
/// never nest one, but the walk is what the resolution path uses too).
fn field_items(view: &ViewDef) -> Vec<String> {
    let mut fields: Vec<String> = Vec::new();
    view.visit_items(&mut |item| {
        if let Item::Field { field, .. } = item {
            fields.push(field.clone());
        }
    });
    fields
}

#[test]
fn the_runtime_doctype_generates_a_form_and_a_list_that_validate() {
    let meta = runtime_meta_with_custom_field();
    for doctype in ["item", "sales_invoice", "sales_invoice_item"] {
        for kind in [ViewKind::Form, ViewKind::List] {
            let view = default_view(&meta, doctype, kind)
                .unwrap_or_else(|errors| panic!("{doctype} {kind:?}: {errors:?}"));
            let resolved = spot_views::resolve(&view, &[], &meta);
            let problems = validate_layer(&resolved, &meta);
            assert!(
                problems.is_empty(),
                "{doctype} {kind:?} does not validate: {problems:?}"
            );
        }
    }
}

#[test]
fn every_field_of_the_doctype_appears_in_the_form_exactly_once() {
    let meta = runtime_meta_with_custom_field();
    for doctype in ["item", "sales_invoice"] {
        let view = default_view(&meta, doctype, ViewKind::Form)
            .unwrap_or_else(|errors| panic!("{doctype} form does not generate: {errors:?}"));
        let mut in_form = field_items(&view);
        let mut expected: Vec<String> = meta
            .definition(doctype)
            .expect("the fixture is in the metadata")
            .fields
            .iter()
            .map(|field| field.name.clone())
            .collect();
        let expected_len = expected.len();
        in_form.sort();
        expected.sort();
        assert_eq!(
            in_form.len(),
            expected_len,
            "{doctype}: the form shows {in_form:?} for the {expected_len} fields",
        );
        in_form.dedup();
        assert_eq!(
            in_form, expected,
            "{doctype}: every field appears exactly once"
        );
    }
}

#[test]
fn the_generated_views_are_deterministic_across_runs() {
    let first = runtime_meta_with_custom_field();
    let second = runtime_meta_with_custom_field();
    for doctype in ["item", "sales_invoice", "sales_invoice_item"] {
        for kind in [ViewKind::Form, ViewKind::List, ViewKind::Grid] {
            if doctype != "sales_invoice_item" && kind == ViewKind::Grid {
                continue;
            }
            let first_text = default_view(&first, doctype, kind)
                .unwrap_or_else(|errors| panic!("{doctype} {kind:?}: {errors:?}"))
                .to_toml()
                .expect("writes")
                .to_string();
            let second_text = default_view(&second, doctype, kind)
                .unwrap_or_else(|errors| panic!("{doctype} {kind:?}: {errors:?}"))
                .to_toml()
                .expect("writes")
                .to_string();
            assert_eq!(
                first_text, second_text,
                "{doctype} {kind:?}: the TOML bytes drift between runs"
            );
        }
    }
}

#[test]
fn the_custom_fields_land_in_the_custom_section() {
    let meta = runtime_meta_with_custom_field();
    let view = default_view(&meta, "item", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("item form: {errors:?}"));
    let custom = view
        .sections
        .iter()
        .find(|section| section.id == "custom")
        .expect("the item fixtures generate a Custom section");
    let names: Vec<String> = custom
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["custom_site_flag", "custom_extra_note"]);
}

#[test]
fn the_status_section_is_collapsed_and_holds_the_engine_fields() {
    let meta = runtime_meta_with_custom_field();
    let view = default_view(&meta, "sales_invoice", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("sales_invoice form: {errors:?}"));
    let status = view
        .sections
        .iter()
        .find(|section| section.id == "status")
        .expect("the posted fixture generates a Status section");
    assert_eq!(status.title.as_deref(), Some("Status"));
    let names: Vec<String> = status
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["doc_state", "amended_from"]);
    // The fold point: the Status section closes with a divider (the model's unbound set
    // has no `collapsed` control; folding is the renderer's read of the Status title).
    assert!(matches!(
        status.items.last(),
        Some(Item::Control { control, .. }) if control == "divider",
    ));
}

#[test]
fn an_unknown_doctype_reports_a_view_error() {
    let meta = runtime_meta_with_custom_field();
    let errors = default_view(&meta, "phantom", ViewKind::Form).expect_err("no phantom");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind.code(), "view.unknown_field");
}

#[test]
fn a_grid_over_a_parent_doctype_is_a_shape_error() {
    let meta = runtime_meta_with_custom_field();
    let errors = default_view(&meta, "item", ViewKind::Grid).expect_err("no parent grid");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].kind.code(), "view.bad_shape");
}

#[test]
fn the_form_fields_keep_the_definition_order_within_the_fields_section() {
    let meta = runtime_meta_with_custom_field();
    let view = default_view(&meta, "item", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("item form: {errors:?}"));
    let fields_section = &view.sections[0];
    let names: Vec<String> = fields_section
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Field { field, .. } => Some(field.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        vec![
            "item_code",
            "item_group",
            "status",
            "opening_balance",
            "reorder_level",
        ],
        "the definition's fields order is the generated order: {names:?}"
    );
}

/// A definition whose every field lands outside the `fields` section still generates a
/// well-shaped form (the empty `fields` section keeps the top).
#[test]
fn a_table_only_doctype_still_generates_a_well_shaped_form() {
    let mut rows_only = Definition::new("attachments_only");
    rows_only.fields = vec![{
        let mut files = FieldSpec::new("files", FieldType::Table);
        files.options = Some("attachment_row".to_owned());
        files
    }];
    let mut attachment_row = Definition::new("attachment_row");
    attachment_row.is_child = true;
    attachment_row.fields = vec![{
        let mut name = FieldSpec::new("file_name", FieldType::Text);
        name.label = Some("File".to_owned());
        name
    }];
    let registry = Registry::new(vec![rows_only, attachment_row])
        .unwrap_or_else(|errors| panic!("the fixture does not build: {errors:?}"));
    let meta = registry.effective();
    let view = default_view(&meta, "attachments_only", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("attachments_only form: {errors:?}"));
    assert!(!view.sections.is_empty(), "the form keeps its top section");
    let resolved = spot_views::resolve(&view, &[], &meta);
    assert!(validate_layer(&resolved, &meta).is_empty(), "{view:?}");
}

/// A human label overrides the name fallback on the generated items
/// (brief deliverable 4's `label` when present).
#[test]
fn a_labelled_field_uses_its_label_not_the_humanised_name() {
    let mut definition = Definition::new("tagged");
    definition.fields = vec![{
        let mut field = FieldSpec::new("posting_date", FieldType::Date);
        field.label = Some("Booking Date".to_owned());
        field
    }];
    let registry = Registry::new(vec![definition])
        .unwrap_or_else(|errors| panic!("the fixture does not build: {errors:?}"));
    let meta = registry.effective();
    let view = default_view(&meta, "tagged", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("tagged form: {errors:?}"));
    assert_eq!(
        field_label_of(&view, "posting_date"),
        Some("Booking Date".to_owned())
    );
    // The unlabelled one falls back to the humanised name.
    assert_eq!(field_label_of(&view, "doc_state"), None);
}

fn field_label_of(view: &ViewDef, field: &str) -> Option<String> {
    let mut found: Option<String> = None;
    view.visit_items(&mut |item| {
        if let Item::Field {
            field: name, label, ..
        } = item
            && name == field
        {
            found = label.clone();
        }
    });
    found
}

/// The item's `label` is `None` when the definition has none and the humanised fallback
/// did NOT get baked into the item: the fallback belongs to the renderer's read of the
/// metadata, so a site rename of the metadata label shows through the layers (ADR-068 §7).
#[test]
fn the_unlabelled_fields_item_carries_no_label_override() {
    let meta = effective_of(vec![common::item_group()]);
    let view = default_view(&meta, "item_group", ViewKind::Form)
        .unwrap_or_else(|errors| panic!("item_group form: {errors:?}"));
    assert_eq!(field_label_of(&view, "group_name"), None);
}

/// The humanised fallback is what the module documents; the item stays neutral.
#[test]
fn humanize_matches_the_brief_examples() {
    assert_eq!(spot_views::humanize_label("posting_date"), "Posting date");
    assert_eq!(spot_views::humanize_label("vat__note2"), "Vat note 2");
}
