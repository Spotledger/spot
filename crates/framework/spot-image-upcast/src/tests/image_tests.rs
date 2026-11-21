use serde_json::{Value, json};

use super::{empty, img};
use crate::{Image, UpcastError};

#[test]
fn new_carries_entity_schema_and_body() {
    let image = Image::new("party", 3, empty());
    assert_eq!(image.entity(), "party");
    assert_eq!(image.entity_schema(), 3);
    assert!(image.body().is_empty());
}

#[test]
fn field_and_rows_read_the_body() {
    let image = img(
        "party",
        1,
        json!({"name": "Acme", "addresses": [{"city": "Lahore"}, {"city": "Karachi"}]}),
    );
    assert_eq!(image.field("name"), Some(&json!("Acme")));
    assert_eq!(image.field("missing"), None);
    let rows = image.rows("addresses").expect("a child table");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["city"], json!("Lahore"));
    assert!(image.rows("name").is_none());
}

#[test]
fn from_parts_refuses_a_body_that_is_not_an_object() {
    let err = Image::from_parts("party", 1, json!([1, 2, 3])).unwrap_err();
    assert_eq!(err.code(), "image_upcast.image_invalid");
    match err {
        UpcastError::ImageInvalid(reason) => assert!(reason.contains("object"), "{reason}"),
        other => panic!("expected ImageInvalid, got {other:?}"),
    }
}

#[test]
fn json_round_trip_keeps_every_field() {
    let image = img("party", 4, json!({"name": "Acme", "items": [{"qty": 2}]}));
    let wire: Value = image.to_json().expect("wire form");
    assert_eq!(wire["entity"], json!("party"));
    assert_eq!(wire["entity_schema"], json!(4));
    assert_eq!(wire["body"]["items"][0]["qty"], json!(2));
    assert_eq!(Image::from_json(&wire).expect("parsed"), image);
}

#[test]
fn from_json_refuses_a_body_that_is_not_an_object() {
    let err = Image::from_json(&json!({"entity": "party", "entity_schema": 1, "body": 7}))
        .expect_err("a number is not a body");
    assert_eq!(err.code(), "image_upcast.image_invalid");
}
