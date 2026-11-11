//! `validate::check`: each code, several errors at once, row paths, scale refusal.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_app_api::prelude::{Decimal, RecordId, Value};
use spot_app_api::{DocState, ErrorClass};
use spot_document::validate::{self, check};

fn only(doc: &spot_app_api::Document, path: &str, code: &str) -> spot_app_api::AppError {
    let meta = meta();
    let e = check(&meta, doc).unwrap_err();
    assert_eq!(problems(&e), [(path.to_owned(), code.to_owned())], "{e:?}");
    e
}

#[test]
fn a_valid_document_passes() {
    let meta = meta();
    let mut doc = valid_invoice();
    doc.set(n("status"), "Paid");
    doc.set(n("total"), money(350));
    doc.set(n("remarks"), "");
    check(&meta, &doc).unwrap();
}

#[test]
fn a_document_with_no_rows_passes_when_the_table_is_optional() {
    let meta = meta();
    let mut doc = invoice();
    doc.set(n("number"), "SI-1");
    doc.set(n("customer"), RecordId::new_v7());
    check(&meta, &doc).unwrap();
}

#[test]
fn wrong_types_are_invalid_values_in_plain_language() {
    let mut doc = valid_invoice();
    doc.set(n("discount_days"), "many");
    let e = only(&doc, "discount_days", validate::INVALID_VALUE);
    assert_eq!(e.class, ErrorClass::Validation);
    assert_eq!(e.code, "document.validation_failed");
    assert_eq!(
        e.fields[0].message,
        "discount_days must be a whole number, but it holds text."
    );
    for (field, value) in [
        ("urgent", Value::Int(1)),
        ("total", Value::Int(5)),
        ("customer", Value::Text("Acme".into())),
        ("number", Value::Int(1)),
        ("posting_date", Value::Text("2026-09-26".into())),
        ("attachment", Value::Text("x".into())),
    ] {
        let mut doc = valid_invoice();
        doc.set(n(field), value);
        only(&doc, field, validate::INVALID_VALUE);
    }
}

#[test]
fn more_decimal_places_than_the_scale_is_an_error_never_a_rounding() {
    let mut doc = valid_invoice();
    doc.set(n("total"), Value::Decimal(Decimal::new(1_005, 3)));
    let e = only(&doc, "total", validate::INVALID_VALUE);
    assert!(
        e.fields[0].message.contains("at most 2 decimal places"),
        "{e:?}"
    );
    assert!(e.fields[0].message.contains("1.005"), "{e:?}");
    // The document was not changed to make it pass.
    assert_eq!(doc.get("total"), &Value::Decimal(Decimal::new(1_005, 3)));
    // The same number with a trailing zero is the same number and fits.
    let meta = meta();
    doc.set(n("total"), Value::Decimal(Decimal::new(10_500, 4)));
    check(&meta, &doc).unwrap();
    // A row quantity has scale 3.
    let mut doc = valid_invoice();
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("qty"), Value::Decimal(Decimal::new(10_001, 4)));
    only(&doc, "items[1].qty", validate::INVALID_VALUE);
}

#[test]
fn too_many_integer_digits_for_the_precision_is_an_error() {
    // total is precision 10, scale 2: eight digits before the point.
    let meta = meta();
    let mut doc = valid_invoice();
    doc.set(n("total"), Value::Decimal(Decimal::new(9_999_999_999, 2)));
    check(&meta, &doc).unwrap();
    doc.set(n("total"), Value::Decimal(Decimal::new(10_000_000_000, 2)));
    let e = only(&doc, "total", validate::INVALID_VALUE);
    assert!(e.fields[0].message.contains("at most 8 digits"), "{e:?}");
}

#[test]
fn text_over_its_limit_is_too_long_counting_characters() {
    let meta = meta();
    let mut doc = valid_invoice();
    // 16 characters of two bytes each fit; 17 do not.
    doc.set(n("number"), "é".repeat(16));
    check(&meta, &doc).unwrap();
    doc.set(n("number"), "é".repeat(17));
    let e = only(&doc, "number", validate::TOO_LONG);
    assert!(
        e.fields[0].message.contains("at most 16 characters"),
        "{e:?}"
    );
}

#[test]
fn a_select_value_must_be_an_option_exactly() {
    let meta = meta();
    for ok in ["Open", "Paid", "Overdue"] {
        let mut doc = valid_invoice();
        doc.set(n("status"), ok);
        check(&meta, &doc).unwrap();
    }
    for bad in ["paid", "Paid ", "Cancelled", ""] {
        let mut doc = valid_invoice();
        doc.set(n("status"), bad);
        let e = only(&doc, "status", validate::INVALID_OPTION);
        assert!(e.fields[0].message.contains("Open, Paid, Overdue"), "{e:?}");
    }
    let mut doc = valid_invoice();
    doc.rows_mut(n("items"))[1]
        .fields
        .insert(n("kind"), "Gadget".into());
    only(&doc, "items[2].kind", validate::INVALID_OPTION);
}

#[test]
fn required_fields_must_be_non_null_but_an_empty_text_is_a_value() {
    let meta = meta();
    let mut doc = valid_invoice();
    doc.set(n("number"), "");
    check(&meta, &doc).unwrap();
    doc.set(n("number"), Value::Null);
    only(&doc, "number", validate::REQUIRED);
    // Absent counts as null.
    let mut doc = valid_invoice();
    doc.set(n("customer"), Value::Null);
    only(&doc, "customer", validate::REQUIRED);
    let blank = invoice();
    let e = check(&meta, &blank).unwrap_err();
    assert_eq!(
        problems(&e),
        [
            ("number".into(), "document.required".into()),
            ("customer".into(), "document.required".into())
        ]
    );
}

#[test]
fn required_fields_of_child_rows_are_checked_with_row_paths() {
    let mut doc = valid_invoice();
    doc.rows_mut(n("items"))[1].fields.remove(&n("qty"));
    let e = only(&doc, "items[2].qty", validate::REQUIRED);
    assert_eq!(e.fields[0].message, "qty is required.");
}

#[test]
fn unknown_fields_rows_and_tables_are_reported() {
    let mut doc = valid_invoice();
    doc.set(n("colour"), "red");
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("weight"), Value::Int(1));
    doc.rows_mut(n("ghosts"));
    doc.rows_mut(n("ghosts")).push(row("x", qty(1_000)));
    let meta = meta();
    let e = check(&meta, &doc).unwrap_err();
    assert_eq!(
        problems(&e),
        [
            ("colour".into(), "document.unknown_field".into()),
            ("ghosts".into(), "document.unknown_field".into()),
            ("items[1].weight".into(), "document.unknown_field".into()),
        ]
    );
}

#[test]
fn a_table_is_not_a_value_and_a_field_is_not_a_table() {
    let mut doc = valid_invoice();
    doc.set(n("items"), Value::Int(3));
    only(&doc, "items", validate::INVALID_VALUE);
    let mut doc = valid_invoice();
    doc.rows_mut(n("remarks")).push(row("x", qty(1_000)));
    only(&doc, "remarks", validate::INVALID_VALUE);
}

#[test]
fn a_caller_cannot_set_the_engine_managed_state() {
    let mut doc = valid_invoice();
    doc.set(n("doc_state"), "posted");
    let e = only(&doc, "doc_state", validate::READ_ONLY_FIELD);
    assert!(
        e.fields[0].message.contains("managed by the engine"),
        "{e:?}"
    );
    // Even a value equal to the current state is refused: the state is not a field.
    let mut doc = valid_invoice();
    doc.set(n("doc_state"), "draft");
    only(&doc, "doc_state", validate::READ_ONLY_FIELD);
    // The document's own state is fine, and `amended_from` is an ordinary link.
    let meta = meta();
    let mut doc = valid_invoice();
    doc.mark_saved(
        spot_app_api::prelude::Version::FIRST,
        Some(DocState::Posted),
    );
    doc.set(n("amended_from"), RecordId::new_v7());
    check(&meta, &doc).unwrap();
    doc.set(n("amended_from"), "SI-0001");
    only(&doc, "amended_from", validate::INVALID_VALUE);
}

#[test]
fn every_problem_is_reported_at_once_in_order() {
    let mut doc = valid_invoice();
    doc.set(n("number"), Value::Null);
    doc.set(n("status"), "Void");
    doc.set(n("total"), Value::Decimal(Decimal::new(1, 3)));
    doc.set(n("doc_state"), "posted");
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("item"), "x".repeat(11).into());
    doc.rows_mut(n("items"))[1].fields.remove(&n("qty"));
    doc.rows_mut(n("items"))
        .push(row("nut", Value::Text("lots".into())));
    let meta = meta();
    let e = check(&meta, &doc).unwrap_err();
    assert_eq!(
        problems(&e),
        [
            ("doc_state".into(), "document.read_only_field".into()),
            ("number".into(), "document.required".into()),
            ("status".into(), "document.invalid_option".into()),
            ("total".into(), "document.invalid_value".into()),
            ("items[1].item".into(), "document.too_long".into()),
            ("items[2].qty".into(), "document.required".into()),
            ("items[3].qty".into(), "document.invalid_value".into()),
        ]
    );
    assert_eq!(e.message, "7 fields are invalid.");
    assert_eq!(e.class, ErrorClass::Validation);
}

#[test]
fn a_required_table_needs_a_row() {
    let mut defs = definitions();
    for f in &mut defs[1].fields {
        if f.name == "items" {
            f.required = true;
        }
    }
    let meta = spot_meta::Registry::new(defs).unwrap().effective();
    let mut doc = invoice();
    doc.set(n("number"), "SI-1");
    doc.set(n("customer"), RecordId::new_v7());
    let e = check(&meta, &doc).unwrap_err();
    assert_eq!(problems(&e), [("items".into(), "document.required".into())]);
    doc.rows_mut(n("items")).push(row("bolt", qty(1_000)));
    check(&meta, &doc).unwrap();
}

#[test]
fn an_unknown_doctype_is_reported_as_such() {
    let meta = meta();
    let doc = spot_app_api::Document::new(n("nope"), RecordId::new_v7());
    let e = check(&meta, &doc).unwrap_err();
    assert_eq!(e.code, "meta.unknown_doctype");
}
