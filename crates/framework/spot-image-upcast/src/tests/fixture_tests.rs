//! One image as an older release wrote it, upcast to the current schema and compared with a
//! hand-written image of that schema.

use serde_json::{Value, json};

use super::img;
use crate::steps::{add_field_with_default, move_field_to_child, rename_field, retype_field};
use crate::{Image, Registry, Upcaster, upcast};

/// The schema this build reads.
const CURRENT: u64 = 6;

/// A decimal string becomes whole minor units, without a float.
fn minor_units(value: &Value) -> Result<Value, String> {
    let Some(text) = value.as_str() else {
        return Err("is not a decimal string".to_owned());
    };
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (text, ""),
    };
    let whole: u64 = whole
        .parse()
        .map_err(|_| "is not a decimal amount".to_owned())?;
    let mut fraction = fraction.to_owned();
    while fraction.len() < 2 {
        fraction.push('0');
    }
    if fraction.len() > 2 {
        return Err("has more than two decimal places".to_owned());
    }
    let fraction: u64 = fraction
        .parse()
        .map_err(|_| "is not a decimal amount".to_owned())?;
    let scaled = whole
        .checked_mul(100)
        .and_then(|scaled| scaled.checked_add(fraction))
        .ok_or_else(|| "is too large to convert".to_owned())?;
    Ok(json!(scaled))
}

/// The registry a rebuild of this release holds for `sales_invoice`.
fn registry() -> Registry {
    let mut registry = Registry::new();
    let steps = [
        Upcaster::new(
            "sales_invoice",
            2,
            3,
            rename_field("legacy_ref", "supplier_ref"),
        ),
        Upcaster::new(
            "sales_invoice",
            3,
            4,
            add_field_with_default("tax_treatment", json!("standard")),
        ),
        Upcaster::new(
            "sales_invoice",
            4,
            5,
            move_field_to_child("items", "currency", "currency"),
        ),
        Upcaster::new("sales_invoice", 5, 6, retype_field("amount", minor_units)),
    ];
    for step in steps {
        registry.register(step).expect("registered");
    }
    registry
}

/// An image as the previous release wrote it: schema 2, before all four changes.
fn old_fixture() -> Image {
    img(
        "sales_invoice",
        2,
        json!({
            "number": "INV-0001",
            "posting_date": "2026-01-31",
            "currency": "PKR",
            "amount": "1250.00",
            "legacy_ref": "PO-77",
            "items": [
                {"item": "A", "qty": "2", "rate": "500.00"},
                {"item": "B", "qty": "1", "rate": "250.00"}
            ]
        }),
    )
}

/// The same document as schema 6 stores it, written by hand.
fn expected() -> Image {
    img(
        "sales_invoice",
        6,
        json!({
            "number": "INV-0001",
            "posting_date": "2026-01-31",
            "amount": 125000,
            "supplier_ref": "PO-77",
            "tax_treatment": "standard",
            "items": [
                {"item": "A", "qty": "2", "rate": "500.00", "currency": "PKR"},
                {"item": "B", "qty": "1", "rate": "250.00", "currency": "PKR"}
            ]
        }),
    )
}

#[test]
fn an_old_image_upcasts_to_exactly_the_current_shape() {
    let upcasted = upcast(&registry(), old_fixture(), CURRENT).expect("upcast");
    assert_eq!(upcasted, expected());
    assert_eq!(upcasted.entity_schema(), CURRENT);
    assert_eq!(upcasted.entity(), "sales_invoice");
}

#[test]
fn the_fixture_differs_from_the_expected_image_in_every_step() {
    let old = old_fixture();
    let new = expected();
    assert!(old.field("legacy_ref").is_some() && new.field("legacy_ref").is_none());
    assert!(new.field("supplier_ref").is_some());
    assert!(old.field("tax_treatment").is_none() && new.field("tax_treatment").is_some());
    assert!(old.field("currency").is_some() && new.field("currency").is_none());
    assert_eq!(old.field("amount"), Some(&json!("1250.00")));
    assert_eq!(new.field("amount"), Some(&json!(125000)));
    assert_eq!(
        new.rows("items").expect("rows")[1]["currency"],
        json!("PKR")
    );
}

#[test]
fn upcasting_the_fixture_again_changes_nothing() {
    let once = upcast(&registry(), old_fixture(), CURRENT).expect("upcast");
    let twice = upcast(&registry(), once.clone(), CURRENT).expect("no work");
    assert_eq!(twice, once);
}

#[test]
fn a_registry_missing_the_last_step_refuses_the_fixture() {
    let mut partial = Registry::new();
    partial
        .register(Upcaster::new(
            "sales_invoice",
            2,
            3,
            rename_field("legacy_ref", "supplier_ref"),
        ))
        .expect("registered");
    partial
        .register(Upcaster::new(
            "sales_invoice",
            3,
            4,
            add_field_with_default("tax_treatment", json!("standard")),
        ))
        .expect("registered");
    partial
        .register(Upcaster::new(
            "sales_invoice",
            4,
            5,
            move_field_to_child("items", "currency", "currency"),
        ))
        .expect("registered");
    let err = upcast(&partial, old_fixture(), CURRENT).expect_err("no last step");
    assert_eq!(err.code(), "image_upcast.no_path");
    assert!(err.to_string().contains('5'), "{err}");
}
