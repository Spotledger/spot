use serde_json::json;

use super::img;
use crate::UpcastError;
use crate::steps::{
    add_field_with_default, drop_field, move_field_to_child, rename_field, retype_field,
};

// --- rename_field -------------------------------------------------------------------------

#[test]
fn rename_moves_the_value_and_keeps_the_rest() {
    let mut image = img("party", 1, json!({"old_name": "Acme", "code": "P1"}));
    rename_field("old_name", "name")(&mut image).expect("renamed");
    assert_eq!(image.field("name"), Some(&json!("Acme")));
    assert!(image.field("old_name").is_none());
    assert_eq!(image.field("code"), Some(&json!("P1")));
}

#[test]
fn rename_of_an_absent_field_is_a_no_op() {
    let mut image = img("party", 1, json!({"code": "P1"}));
    rename_field("old_name", "name")(&mut image).expect("no-op");
    assert_eq!(image.body().len(), 1);
}

#[test]
fn rename_refuses_to_overwrite_the_target_and_leaves_the_image_as_it_was() {
    let mut image = img("party", 1, json!({"old_name": "Acme", "name": "Beta"}));
    let err = rename_field("old_name", "name")(&mut image).expect_err("refused");
    assert_eq!(err.code(), "image_upcast.step_shape");
    assert_eq!(image.field("name"), Some(&json!("Beta")));
    assert_eq!(image.field("old_name"), Some(&json!("Acme")));
}

#[test]
fn rename_to_the_same_name_changes_nothing() {
    let mut image = img("party", 1, json!({"name": "Acme"}));
    rename_field("name", "name")(&mut image).expect("no-op");
    assert_eq!(image.field("name"), Some(&json!("Acme")));
}

// --- drop_field ---------------------------------------------------------------------------

#[test]
fn drop_removes_the_field_and_keeps_the_rest() {
    let mut image = img("party", 1, json!({"legacy": 1, "code": "P1"}));
    drop_field("legacy")(&mut image).expect("dropped");
    assert!(image.field("legacy").is_none());
    assert_eq!(image.field("code"), Some(&json!("P1")));
}

#[test]
fn drop_of_an_absent_field_is_a_no_op() {
    let mut image = img("party", 1, json!({"code": "P1"}));
    drop_field("legacy")(&mut image).expect("no-op");
    drop_field("legacy")(&mut image).expect("still a no-op");
    assert_eq!(image.body().len(), 1);
}

// --- add_field_with_default ---------------------------------------------------------------

#[test]
fn add_inserts_the_default_when_the_field_is_absent() {
    let mut image = img("party", 1, json!({"code": "P1"}));
    add_field_with_default("status", json!("draft"))(&mut image).expect("added");
    assert_eq!(image.field("status"), Some(&json!("draft")));
}

#[test]
fn add_keeps_a_stored_value_instead_of_resetting_it() {
    let mut image = img("party", 1, json!({"status": "active"}));
    add_field_with_default("status", json!("draft"))(&mut image).expect("added");
    assert_eq!(image.field("status"), Some(&json!("active")));
}

#[test]
fn add_then_add_again_is_the_same_image() {
    let mut once = img("party", 1, json!({"code": "P1"}));
    add_field_with_default("status", json!("draft"))(&mut once).expect("added");
    let mut twice = once.clone();
    add_field_with_default("status", json!("draft"))(&mut twice).expect("added again");
    assert_eq!(once, twice);
}

// --- move_field_to_child ------------------------------------------------------------------

#[test]
fn move_writes_the_value_into_every_row_and_removes_the_parent_field() {
    let mut image = img(
        "sales_invoice",
        1,
        json!({"currency": "PKR", "items": [{"item": "A"}, {"item": "B"}]}),
    );
    move_field_to_child("items", "currency", "currency")(&mut image).expect("moved");
    assert!(image.field("currency").is_none());
    let rows = image.rows("items").expect("rows");
    assert_eq!(rows[0]["currency"], json!("PKR"));
    assert_eq!(rows[1]["currency"], json!("PKR"));
}

#[test]
fn move_of_an_absent_parent_field_is_a_no_op() {
    let mut image = img("sales_invoice", 1, json!({"items": [{"item": "A"}]}));
    move_field_to_child("items", "currency", "currency")(&mut image).expect("no-op");
    assert!(
        image.rows("items").expect("rows")[0]
            .get("currency")
            .is_none()
    );
}

#[test]
fn move_refuses_a_child_that_is_not_an_array() {
    let mut image = img(
        "sales_invoice",
        1,
        json!({"currency": "PKR", "items": "none"}),
    );
    let err =
        move_field_to_child("items", "currency", "currency")(&mut image).expect_err("refused");
    assert_eq!(err.code(), "image_upcast.step_shape");
    assert_eq!(image.field("currency"), Some(&json!("PKR")));
}

#[test]
fn move_refuses_a_row_that_is_not_an_object() {
    let mut image = img("sales_invoice", 1, json!({"currency": "PKR", "items": [7]}));
    let err =
        move_field_to_child("items", "currency", "currency")(&mut image).expect_err("refused");
    match err {
        UpcastError::StepShape { step, field, .. } => {
            assert_eq!(step, "move_field_to_child");
            assert_eq!(field, "items");
        }
        other => panic!("expected StepShape, got {other:?}"),
    }
    assert_eq!(image.field("currency"), Some(&json!("PKR")));
}

#[test]
fn move_refuses_to_overwrite_a_value_a_row_already_holds() {
    let mut image = img(
        "sales_invoice",
        1,
        json!({"currency": "PKR", "items": [{"currency": "USD"}]}),
    );
    let err =
        move_field_to_child("items", "currency", "currency")(&mut image).expect_err("refused");
    assert_eq!(err.code(), "image_upcast.step_shape");
    assert_eq!(
        image.rows("items").expect("rows")[0]["currency"],
        json!("USD")
    );
}

#[test]
fn move_into_no_rows_leaves_the_parent_field_alone() {
    let mut image = img("sales_invoice", 1, json!({"currency": "PKR", "items": []}));
    move_field_to_child("items", "currency", "currency")(&mut image).expect("no rows");
    assert_eq!(image.field("currency"), Some(&json!("PKR")));
}

// --- retype_field -------------------------------------------------------------------------

/// Integer minor units from a decimal string, without touching a float.
fn minor_units(value: &serde_json::Value) -> Result<serde_json::Value, String> {
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

#[test]
fn retype_replaces_the_value_with_the_converted_one() {
    let mut image = img(
        "sales_invoice",
        1,
        json!({"amount": "1250.00", "code": "A"}),
    );
    retype_field("amount", minor_units)(&mut image).expect("retyped");
    assert_eq!(image.field("amount"), Some(&json!(125_000)));
    assert_eq!(image.field("code"), Some(&json!("A")));
}

#[test]
fn retype_of_an_absent_field_is_a_no_op() {
    let mut image = img("sales_invoice", 1, json!({"code": "A"}));
    retype_field("amount", minor_units)(&mut image).expect("no-op");
    assert_eq!(image.body().len(), 1);
}

#[test]
fn retype_reports_the_field_and_the_reason_when_the_converter_refuses() {
    let mut image = img("sales_invoice", 1, json!({"amount": "not a number"}));
    let err = retype_field("amount", minor_units)(&mut image).expect_err("refused");
    assert_eq!(err.code(), "image_upcast.convert_failed");
    match err {
        UpcastError::ConvertFailed { field, reason } => {
            assert_eq!(field, "amount");
            assert_eq!(reason, "is not a decimal amount");
        }
        other => panic!("expected ConvertFailed, got {other:?}"),
    }
    assert_eq!(image.field("amount"), Some(&json!("not a number")));
}
