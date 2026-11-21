//! `validate_table_shape`: what `validate::check` leaves out.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use spot_app_api::prelude::{RecordId, Value};
use spot_app_api::{ChildRow, Document, ErrorClass};
use spot_doc_tables::{INVALID_ROW, INVALID_VALUE, validate_table_shape};

fn tax(i: u32, description: &str) -> ChildRow {
    let mut fields = BTreeMap::new();
    fields.insert(n("description"), Value::Text(description.to_owned()));
    ChildRow { id: id(i), fields }
}

fn invoice(items: Vec<ChildRow>, taxes: Vec<ChildRow>) -> Document {
    let mut doc = Document::new(n("sales_invoice"), id(1000));
    *doc.rows_mut(n("items")) = items;
    *doc.rows_mut(n("taxes")) = taxes;
    doc
}

fn check(doc: &Document) -> Result<(), spot_app_api::AppError> {
    validate_table_shape(&meta(), doc.doctype().as_str(), doc)
}

fn only(doc: &Document, path: &str, code: &str) -> spot_app_api::AppError {
    let e = check(doc).unwrap_err();
    assert_eq!(problems(&e), [(path.to_owned(), code.to_owned())], "{e:?}");
    assert_eq!(e.class, ErrorClass::Validation);
    assert_eq!(e.code, "document.validation_failed");
    e
}

#[test]
fn a_well_formed_document_passes() {
    check(&invoice(
        vec![item(1, "a", 1), item(2, "b", 2)],
        vec![tax(3, "VAT")],
    ))
    .unwrap();
    check(&invoice(vec![], vec![])).unwrap();
    check(&Document::new(n("sales_invoice"), id(1))).unwrap();
    check(&loaded(&[item(1, "a", 1)])).unwrap();
}

#[test]
fn a_row_id_may_not_repeat_in_a_table() {
    let doc = invoice(
        vec![item(1, "a", 1), item(2, "b", 2), item(1, "c", 3)],
        vec![],
    );
    let e = only(&doc, "items[3].id", INVALID_ROW);
    assert_eq!(
        e.fields[0].message,
        "This row has the same id as row 1 of this table."
    );
    assert_eq!(INVALID_ROW, "document.invalid_row");
}

#[test]
fn a_row_id_may_not_be_shared_between_tables_or_with_the_document() {
    let doc = invoice(vec![item(1, "a", 1)], vec![tax(1, "VAT")]);
    // Tables are checked in name order: `items` first, so the row of `taxes` is the later one.
    let e = only(&doc, "taxes[1].id", INVALID_ROW);
    assert_eq!(
        e.fields[0].message,
        "This row has the same id as items[1], a row of another table."
    );
    let doc = invoice(vec![item(1000, "a", 1)], vec![]);
    let e = only(&doc, "items[1].id", INVALID_ROW);
    assert_eq!(
        e.fields[0].message,
        "This row has the same id as the document itself."
    );
}

#[test]
fn ids_are_checked_in_tables_the_doctype_does_not_declare_too() {
    let mut doc = invoice(vec![item(1, "a", 1)], vec![]);
    doc.rows_mut(n("mystery")).push(item(1, "b", 1));
    only(&doc, "mystery[1].id", INVALID_ROW);
}

#[test]
fn a_row_may_not_hold_a_field_of_another_tables_doctype() {
    let mut row = item(1, "a", 1);
    row.fields
        .insert(n("description"), "belongs to taxes".into());
    let doc = invoice(vec![row], vec![tax(2, "VAT")]);
    let e = only(&doc, "items[1].description", INVALID_VALUE);
    assert_eq!(
        e.fields[0].message,
        "`description` is a field of the rows of `taxes` (DocType `sales_invoice_tax`), not of `items`."
    );
    // And the other way round, with the right row number.
    let mut t = tax(3, "VAT");
    t.fields.insert(n("qty"), qty(1));
    let doc = invoice(vec![item(1, "a", 1)], vec![tax(2, "x"), t]);
    only(&doc, "taxes[2].qty", INVALID_VALUE);
    // A name no table declares is `validate::check`'s business, not ours.
    let mut row = item(1, "a", 1);
    row.fields.insert(n("nonsense"), Value::Int(1));
    check(&invoice(vec![row], vec![])).unwrap();
}

fn product(tags: &[(u32, Option<u32>)]) -> Document {
    let mut doc = Document::new(n("product"), id(1000));
    for (row_id, target) in tags {
        let mut fields = BTreeMap::new();
        if let Some(t) = target {
            fields.insert(n("tag"), Value::Id(id(*t)));
        }
        doc.rows_mut(n("tags")).push(ChildRow {
            id: id(*row_id),
            fields,
        });
    }
    doc
}

#[test]
fn a_multiselect_may_not_repeat_a_target() {
    let doc = product(&[(1, Some(500)), (2, Some(501)), (3, Some(500))]);
    let e = validate_table_shape(&meta(), "product", &doc).unwrap_err();
    assert_eq!(
        problems(&e),
        [("tags[3].tag".to_owned(), INVALID_ROW.to_owned())]
    );
    assert_eq!(
        e.fields[0].message,
        "This target is already selected in row 1."
    );
    // Different targets, and rows without a target, are fine here.
    validate_table_shape(
        &meta(),
        "product",
        &product(&[(1, Some(500)), (2, Some(501)), (3, None), (4, None)]),
    )
    .unwrap();
    // A plain table (`notes`) has no such rule.
    let mut doc = Document::new(n("product"), id(1000));
    for i in 1..=2 {
        let mut fields = BTreeMap::new();
        fields.insert(n("text"), Value::Text("same".into()));
        doc.rows_mut(n("notes"))
            .push(ChildRow { id: id(i), fields });
    }
    validate_table_shape(&meta(), "product", &doc).unwrap();
}

#[test]
fn every_problem_is_reported_at_once() {
    let mut bad = item(1, "a", 1);
    bad.fields.insert(n("description"), "x".into());
    let doc = invoice(
        vec![bad, item(1, "b", 2), item(1000, "c", 3)],
        vec![tax(1, "VAT")],
    );
    let e = check(&doc).unwrap_err();
    assert_eq!(
        problems(&e),
        [
            ("items[2].id".into(), INVALID_ROW.into()),
            ("items[3].id".into(), INVALID_ROW.into()),
            ("taxes[1].id".into(), INVALID_ROW.into()),
            ("items[1].description".into(), INVALID_VALUE.into()),
        ]
    );
    assert_eq!(e.message, "4 fields are invalid.");
}

#[test]
fn an_unknown_doctype_is_a_typed_error() {
    let doc = Document::new(n("sales_invoice"), RecordId::new_v7());
    let e = validate_table_shape(&meta(), "ghost", &doc).unwrap_err();
    assert_eq!(e.code, "meta.unknown_doctype");
    assert_eq!(e.class, ErrorClass::UnknownDocType);
}

#[test]
fn the_check_agrees_with_what_a_patch_produces() {
    // A patch that names the same row id twice is refused by `apply_patch`; a patch that reuses
    // a row id of another table is not, and this check is what catches it.
    let mut doc = invoice(vec![item(1, "a", 1)], vec![tax(2, "VAT")]);
    let patch = spot_app_api::DocumentPatch {
        fields: BTreeMap::new(),
        tables: [(
            n("items"),
            vec![spot_app_api::RowPatch {
                id: Some(id(2)),
                fields: BTreeMap::new(),
            }],
        )]
        .into(),
    };
    doc.apply_patch(&patch, RecordId::new_v7).unwrap();
    // `items` comes first in name order, so the reused id is reported on the row of `taxes`.
    only(&doc, "taxes[1].id", INVALID_ROW);
}
