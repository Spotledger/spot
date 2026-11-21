//! `defaults::apply`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_app_api::ErrorClass;
use spot_app_api::prelude::{Decimal, Value};
use spot_document::defaults;
use spot_meta::{Definition, FieldSpec, FieldType, Registry};

#[test]
fn unset_fields_get_their_defaults() {
    let meta = meta();
    let mut doc = invoice();
    defaults::apply(&meta, &mut doc, now()).unwrap();
    assert_eq!(doc.get("discount_days"), &Value::Int(0));
    assert_eq!(doc.get("urgent"), &Value::Bool(false));
    // Fields without a default stay unset.
    assert!(doc.fields().keys().all(|k| k.as_str() != "remarks"));
    assert!(doc.fields().keys().all(|k| k.as_str() != "number"));
}

#[test]
fn a_value_the_caller_set_is_never_overwritten_even_an_explicit_null() {
    let meta = meta();
    let mut doc = invoice();
    doc.set(n("discount_days"), 5_i64);
    doc.set(n("urgent"), Value::Null);
    defaults::apply(&meta, &mut doc, now()).unwrap();
    assert_eq!(doc.get("discount_days"), &Value::Int(5));
    assert_eq!(doc.get("urgent"), &Value::Null);
    assert!(
        doc.fields().contains_key(&n("urgent")),
        "the null is kept, not removed"
    );
    // A field that was left alone is still filled.
    assert_eq!(doc.get("urgent"), &Value::Null);
    assert_eq!(doc.get("discount_days"), &Value::Int(5));
}

#[test]
fn child_rows_get_their_defaults_too() {
    let meta = meta();
    let mut doc = valid_invoice();
    doc.rows_mut(n("items"))[1]
        .fields
        .insert(n("rate"), Value::Null);
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("kind"), "Goods".into());
    defaults::apply(&meta, &mut doc, now()).unwrap();
    let rows = doc.rows("items");
    assert_eq!(rows[0].fields[&n("rate")], money(0));
    assert_eq!(rows[0].fields[&n("kind")], Value::Text("Goods".into()));
    assert_eq!(
        rows[1].fields[&n("rate")],
        Value::Null,
        "an explicit null is kept"
    );
    assert!(!rows[1].fields.contains_key(&n("kind")));
}

#[test]
fn applying_twice_changes_nothing_more() {
    let meta = meta();
    let mut doc = valid_invoice();
    defaults::apply(&meta, &mut doc, now()).unwrap();
    let once = doc.clone();
    defaults::apply(&meta, &mut doc, now()).unwrap();
    assert_eq!(doc, once);
}

#[test]
fn static_defaults_are_parsed_by_type() {
    let mut def = Definition::new("thing");
    let with = |name: &str, ty, default: &str| {
        let mut f = FieldSpec::new(name, ty);
        f.default = Some(default.to_owned());
        f
    };
    let mut price = with("price", FieldType::Decimal, "12.50");
    price.scale = Some(2);
    def.fields = vec![
        price,
        with("day", FieldType::Date, "2026-09-26"),
        with("at", FieldType::Time, "09:30"),
        with("when", FieldType::DateTime, "2026-09-26T15:00:00+05:00"),
        with("label", FieldType::Text, "  keep  Case "),
        with("big", FieldType::Int, "-9"),
        with("ok", FieldType::Bool, "true"),
    ];
    let meta = Registry::new(vec![def]).unwrap().effective();
    let mut doc =
        spot_app_api::Document::new(n("thing"), spot_app_api::prelude::RecordId::new_v7());
    defaults::apply(&meta, &mut doc, now()).unwrap();
    assert_eq!(doc.get("price"), &Value::Decimal(Decimal::new(1250, 2)));
    assert_eq!(
        doc.get("day").clone(),
        Value::Date(common_date(2026, 9, 26))
    );
    assert_eq!(
        doc.get("at"),
        &Value::Time(spot_app_api::prelude::Time::from_hms(9, 30, 0).unwrap())
    );
    let Value::DateTime(when) = doc.get("when") else {
        panic!("a date-time")
    };
    assert_eq!(when.unix_timestamp(), 1_790_416_800);
    assert_eq!(when.offset().whole_seconds(), 0, "stored as UTC");
    assert_eq!(
        doc.get("label"),
        &Value::Text("  keep  Case ".into()),
        "text is not trimmed"
    );
    assert_eq!(doc.get("big"), &Value::Int(-9));
    assert_eq!(doc.get("ok"), &Value::Bool(true));
}

fn common_date(y: i32, m: u8, d: u8) -> spot_app_api::prelude::Date {
    let month = spot_app_api::prelude::Date::from_ordinal_date(y, 1)
        .unwrap()
        .replace_month(m.try_into().unwrap())
        .unwrap();
    month.replace_day(d).unwrap()
}

#[test]
fn an_unknown_doctype_is_an_error() {
    let meta = meta();
    let mut doc = spot_app_api::Document::new(n("nope"), spot_app_api::prelude::RecordId::new_v7());
    let e = defaults::apply(&meta, &mut doc, now()).unwrap_err();
    assert_eq!(e.code, "meta.unknown_doctype");
    assert_eq!(e.class, ErrorClass::UnknownDocType);
}
