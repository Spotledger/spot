use serde_json::{Value, json};

use super::img;
use crate::steps::{add_field_with_default, drop_field, rename_field, retype_field};
use crate::{Registry, Step, UpcastError, Upcaster, upcast};

/// A whole number stored as a string becomes a number.
fn as_number(value: &Value) -> Result<Value, String> {
    match value.as_str() {
        Some(text) => text
            .parse::<u64>()
            .map(|number| json!(number))
            .map_err(|_| "is not a whole number".to_owned()),
        None => Err("is not a whole number".to_owned()),
    }
}

fn register(registry: &mut Registry, entity: &str, from: u64, to: u64, f: Step) {
    registry
        .register(Upcaster::new(entity, from, to, f))
        .expect("registered");
}

/// Four consecutive steps for `party`: schema 1 -> 5.
fn four_steps() -> Registry {
    let mut registry = Registry::new();
    register(
        &mut registry,
        "party",
        1,
        2,
        rename_field("legacy_name", "name"),
    );
    register(
        &mut registry,
        "party",
        2,
        3,
        add_field_with_default("status", json!("draft")),
    );
    register(
        &mut registry,
        "party",
        3,
        4,
        retype_field("credit_limit", as_number),
    );
    register(&mut registry, "party", 4, 5, drop_field("legacy_flag"));
    registry
}

#[test]
fn a_four_step_chain_applies_every_step_in_order() {
    let registry = four_steps();
    let image = img(
        "party",
        1,
        json!({
            "legacy_name": "Acme",
            "credit_limit": "5000",
            "legacy_flag": true,
            "code": "P1"
        }),
    );
    let upcasted = registry.upcast(image, 5).expect("upcast");
    assert_eq!(upcasted.entity_schema(), 5);
    assert_eq!(upcasted.field("name"), Some(&json!("Acme")));
    assert!(upcasted.field("legacy_name").is_none());
    assert_eq!(upcasted.field("status"), Some(&json!("draft")));
    assert_eq!(upcasted.field("credit_limit"), Some(&json!(5000)));
    assert!(upcasted.field("legacy_flag").is_none());
    assert_eq!(upcasted.field("code"), Some(&json!("P1")));
}

#[test]
fn a_partial_chain_applies_only_the_steps_it_has() {
    let registry = four_steps();
    let image = img(
        "party",
        1,
        json!({"legacy_name": "Acme", "credit_limit": "5000"}),
    );
    let upcasted = registry.upcast(image, 3).expect("upcast");
    assert_eq!(upcasted.entity_schema(), 3);
    assert_eq!(upcasted.field("name"), Some(&json!("Acme")));
    assert_eq!(upcasted.field("status"), Some(&json!("draft")));
    assert_eq!(upcasted.field("credit_limit"), Some(&json!("5000")));
}

#[test]
fn a_missing_middle_step_names_the_entity_the_version_and_the_target() {
    let mut registry = Registry::new();
    register(
        &mut registry,
        "party",
        1,
        2,
        rename_field("legacy_name", "name"),
    );
    register(&mut registry, "party", 3, 4, drop_field("legacy_flag"));
    register(&mut registry, "party", 4, 5, drop_field("status"));
    let err = registry
        .upcast(img("party", 1, json!({"legacy_name": "Acme"})), 5)
        .expect_err("no path");
    assert_eq!(
        err,
        UpcastError::NoPath {
            entity: "party".to_owned(),
            from: 2,
            to: 5,
        }
    );
    assert_eq!(err.code(), "image_upcast.no_path");
    let message = err.to_string();
    assert!(
        message.contains("party") && message.contains('2') && message.contains('5'),
        "{message}"
    );
}

#[test]
fn a_duplicate_registration_is_refused() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 1, 2, rename_field("a", "b"));
    let err = registry
        .register(Upcaster::new("party", 1, 3, drop_field("a")))
        .expect_err("duplicate");
    assert_eq!(
        err,
        UpcastError::DuplicateStep {
            entity: "party".to_owned(),
            from: 1,
            to: 3,
        }
    );
    assert_eq!(err.code(), "image_upcast.duplicate_step");
    assert_eq!(registry.step_count(), 1);
}

#[test]
fn a_step_that_does_not_move_forward_is_refused() {
    let mut registry = Registry::new();
    let err = registry
        .register(Upcaster::new("party", 3, 3, drop_field("a")))
        .expect_err("backwards");
    assert_eq!(err.code(), "image_upcast.backwards_step");
    let err = registry
        .register(Upcaster::new("party", 4, 3, drop_field("a")))
        .expect_err("backwards");
    assert_eq!(
        err,
        UpcastError::BackwardsStep {
            entity: "party".to_owned(),
            from: 4,
            to: 3,
        }
    );
    assert!(registry.is_empty());
}

#[test]
fn the_same_from_version_is_free_for_another_entity() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 1, 2, drop_field("a"));
    register(&mut registry, "item", 1, 2, drop_field("a"));
    assert_eq!(registry.step_count(), 2);
    assert!(registry.next("party", 1).is_some());
    assert!(registry.next("item", 1).is_some());
}

#[test]
fn an_image_already_at_the_target_is_returned_unchanged() {
    let registry = four_steps();
    let image = img(
        "party",
        3,
        json!({"credit_limit": "5000", "legacy_flag": true}),
    );
    let upcasted = registry.upcast(image.clone(), 3).expect("no work");
    assert_eq!(upcasted, image);
}

#[test]
fn an_image_newer_than_the_target_is_refused() {
    let registry = four_steps();
    let err = registry
        .upcast(img("party", 6, json!({})), 5)
        .expect_err("from the future");
    assert_eq!(
        err,
        UpcastError::FromTheFuture {
            entity: "party".to_owned(),
            schema: 6,
            target: 5,
        }
    );
    assert_eq!(err.code(), "image_upcast.from_the_future");
}

#[test]
fn a_step_that_would_jump_past_the_target_leaves_no_path() {
    let mut registry = Registry::new();
    register(&mut registry, "party", 1, 3, drop_field("a"));
    let err = registry
        .upcast(img("party", 1, json!({"a": 1})), 2)
        .expect_err("no path");
    assert_eq!(
        err,
        UpcastError::NoPath {
            entity: "party".to_owned(),
            from: 1,
            to: 2,
        }
    );
}

#[test]
fn an_unknown_entity_leaves_no_path() {
    let registry = four_steps();
    let err = registry
        .upcast(img("item", 1, json!({})), 5)
        .expect_err("no path");
    assert_eq!(
        err,
        UpcastError::NoPath {
            entity: "item".to_owned(),
            from: 1,
            to: 5,
        }
    );
}

#[test]
fn a_step_that_refuses_stops_the_chain_and_reports_its_own_reason() {
    let mut registry = Registry::new();
    register(
        &mut registry,
        "party",
        1,
        2,
        rename_field("legacy_name", "name"),
    );
    let err = registry
        .upcast(img("party", 1, json!({"legacy_name": "A", "name": "B"})), 2)
        .expect_err("refused");
    assert_eq!(err.code(), "image_upcast.step_shape");
}

#[test]
fn missing_step_reports_the_first_version_the_chain_cannot_cross() {
    let registry = four_steps();
    assert_eq!(registry.missing_step("party", 1, 5), None);
    assert_eq!(registry.missing_step("party", 3, 5), None);
    assert_eq!(registry.missing_step("party", 5, 5), None);
    assert_eq!(registry.missing_step("party", 8, 5), None);
    assert_eq!(registry.missing_step("party", 0, 5), Some(0));
    assert_eq!(registry.missing_step("item", 1, 5), Some(1));
}

#[test]
fn the_function_form_is_the_registry_method() {
    let registry = four_steps();
    let image = img(
        "party",
        1,
        json!({"legacy_name": "Acme", "credit_limit": "7"}),
    );
    let by_method = registry.upcast(image.clone(), 5).expect("upcast");
    let by_function = upcast(&registry, image, 5).expect("upcast");
    assert_eq!(by_method, by_function);
}
