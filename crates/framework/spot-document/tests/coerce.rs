//! `coerce::normalise`: the whole coercion table, and what it leaves alone.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_app_api::prelude::{Decimal, Value};
use spot_document::{coerce::normalise, validate::check};

fn normalised(field: &str, value: Value) -> Value {
    let meta = meta();
    let mut doc = invoice();
    doc.set(n(field), value);
    normalise(&meta, &mut doc).unwrap();
    doc.get(field).clone()
}

#[test]
fn an_int_becomes_the_same_decimal() {
    assert_eq!(
        normalised("total", Value::Int(5)),
        Value::Decimal(Decimal::from(5))
    );
    assert_eq!(
        normalised("total", Value::Int(-12)),
        Value::Decimal(Decimal::from(-12))
    );
    assert_eq!(
        normalised("total", Value::Int(i64::MAX)),
        Value::Decimal(Decimal::from(i64::MAX))
    );
}

#[test]
fn a_whole_decimal_becomes_the_same_int() {
    assert_eq!(
        normalised("discount_days", Value::Decimal(Decimal::new(300, 2))),
        Value::Int(3)
    );
    assert_eq!(
        normalised("discount_days", Value::Decimal(Decimal::new(-7, 0))),
        Value::Int(-7)
    );
    assert_eq!(
        normalised("discount_days", Value::Decimal(Decimal::ZERO)),
        Value::Int(0)
    );
}

#[test]
fn nothing_else_changes() {
    let fractional = Value::Decimal(Decimal::new(25, 1));
    let too_big = Value::Decimal(Decimal::from(i64::MAX) + Decimal::from(1));
    let cases = [
        // A fractional decimal stays, so the int field refuses it in validation.
        ("discount_days", fractional.clone()),
        ("discount_days", too_big),
        // Text is neither trimmed nor case-folded; text never becomes a number.
        ("number", Value::Text("  Si-0001 ".into())),
        ("discount_days", Value::Text("3".into())),
        ("total", Value::Text("5".into())),
        // Bools, nulls and decimals that already fit are left alone.
        ("urgent", Value::Int(1)),
        ("total", Value::Null),
        ("total", Value::Decimal(Decimal::new(1_005, 3))),
        ("remarks", Value::Int(5)),
        // Undeclared fields are not touched.
        ("colour", Value::Int(5)),
    ];
    for (field, value) in cases {
        assert_eq!(normalised(field, value.clone()), value, "{field}");
    }
}

#[test]
fn rows_are_normalised_too() {
    let meta = meta();
    let mut doc = valid_invoice();
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("qty"), Value::Int(4));
    doc.rows_mut(n("items"))[1]
        .fields
        .insert(n("rate"), Value::Int(10));
    normalise(&meta, &mut doc).unwrap();
    assert_eq!(
        doc.rows("items")[0].fields[&n("qty")],
        Value::Decimal(Decimal::from(4))
    );
    assert_eq!(
        doc.rows("items")[1].fields[&n("rate")],
        Value::Decimal(Decimal::from(10))
    );
}

#[test]
fn normalising_is_what_lets_validation_pass_and_it_is_idempotent() {
    let meta = meta();
    let mut doc = valid_invoice();
    doc.set(n("total"), Value::Int(5));
    doc.set(n("discount_days"), Value::Decimal(Decimal::new(300, 2)));
    assert!(check(&meta, &doc).is_err());
    normalise(&meta, &mut doc).unwrap();
    check(&meta, &doc).unwrap();
    let once = doc.clone();
    normalise(&meta, &mut doc).unwrap();
    assert_eq!(doc, once);
}

#[test]
fn an_unknown_doctype_is_an_error() {
    let meta = meta();
    let mut doc = spot_app_api::Document::new(n("nope"), spot_app_api::prelude::RecordId::new_v7());
    assert_eq!(
        normalise(&meta, &mut doc).unwrap_err().code,
        "meta.unknown_doctype"
    );
}
