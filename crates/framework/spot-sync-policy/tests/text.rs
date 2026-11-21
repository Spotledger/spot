//! The metadata text form: parsing a DocType property into a policy, key by key.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use spot_sync_policy::{
    Class, Direction, FieldName, PolicyProperty, ScopeKeyType, SyncPolicy, Trigger, codes,
};

#[test]
fn whole_policy_parses_from_inline_toml() {
    let policy = SyncPolicy::local();
    let property = PolicyProperty::parse(
        "sync_policy",
        r#"class = "master", direction = "down", trigger = "on_save""#,
    )
    .unwrap();
    let mut policy = policy;
    property.apply_to(&mut policy);
    assert_eq!(policy.class, Class::Master);
    assert_eq!(policy.direction, Direction::Down);
    assert_eq!(policy.trigger, Trigger::OnSave);
    assert_eq!(policy.owner_scope, None);
    assert!(policy.exclude_fields.is_empty());
}

#[test]
fn whole_policy_parses_from_multiline_toml_with_every_field() {
    let text = "class = \"shared\"\n\
                direction = \"both\"\n\
                trigger = \"manual\"\n\
                owner_scope = \"warehouse\"\n\
                exclude_fields = [\"cost_price\", \"internal_note\"]";
    let PolicyProperty::Policy(policy) = PolicyProperty::parse("sync_policy", text).unwrap() else {
        panic!("the whole policy form yields a Policy");
    };
    assert_eq!(policy.class, Class::Shared);
    assert_eq!(policy.direction, Direction::Both);
    assert_eq!(policy.trigger, Trigger::Manual);
    assert_eq!(policy.owner_scope.as_ref().unwrap().as_str(), "warehouse");
    let excluded: BTreeSet<&str> = policy.excluded().map(FieldName::as_str).collect();
    assert_eq!(excluded, BTreeSet::from(["cost_price", "internal_note"]));
}

#[test]
fn single_field_keys_parse_and_compose() {
    let mut policy = SyncPolicy::local();
    PolicyProperty::parse("sync_class", "transactional")
        .unwrap()
        .apply_to(&mut policy);
    PolicyProperty::parse("sync_direction", "up")
        .unwrap()
        .apply_to(&mut policy);
    PolicyProperty::parse("sync_owner_scope", "company")
        .unwrap()
        .apply_to(&mut policy);
    PolicyProperty::parse(
        "sync_exclude_fields",
        "cost_price, secret_note\ninternal_note",
    )
    .unwrap()
    .apply_to(&mut policy);

    assert_eq!(policy.class, Class::Transactional);
    assert_eq!(policy.direction, Direction::Up);
    assert_eq!(policy.owner_scope.as_ref().unwrap().as_str(), "company");
    assert_eq!(policy.exclude_fields.len(), 3);
    assert!(policy.excludes("cost_price"));
    assert!(policy.excludes("secret_note"));
    assert!(policy.excludes("internal_note"));
}

#[test]
fn empty_owner_scope_and_exclude_fields_clear() {
    let mut policy = SyncPolicy::new(
        Class::Master,
        Some(ScopeKeyType::new("company").unwrap()),
        Direction::Down,
        Trigger::OnSubmit,
        BTreeSet::from([FieldName::new("cost_price").unwrap()]),
    );
    PolicyProperty::parse("sync_owner_scope", "")
        .unwrap()
        .apply_to(&mut policy);
    PolicyProperty::parse("sync_exclude_fields", "")
        .unwrap()
        .apply_to(&mut policy);
    assert_eq!(policy.owner_scope, None);
    assert!(policy.exclude_fields.is_empty());
}

#[test]
fn keys_report_their_text_form() {
    assert_eq!(
        PolicyProperty::parse("sync_class", "local").unwrap().key(),
        "sync_class"
    );
    assert_eq!(
        PolicyProperty::parse("sync_policy", r#"class = "local""#)
            .unwrap()
            .key(),
        "sync_policy"
    );
}

#[test]
fn an_unknown_key_is_refused() {
    let error = PolicyProperty::parse("sync_something", "x").unwrap_err();
    assert_eq!(error.code(), codes::UNKNOWN_PROPERTY);
    assert!(error.detail().contains("sync_something"));
}

#[test]
fn a_bad_value_is_refused() {
    let error = PolicyProperty::parse("sync_class", "sideways").unwrap_err();
    assert_eq!(error.code(), codes::BAD_VALUE);
    assert!(error.detail().contains("sideways"));

    let error = PolicyProperty::parse("sync_exclude_fields", "Not A Field").unwrap_err();
    assert_eq!(error.code(), codes::BAD_VALUE);
}

#[test]
fn a_bad_toml_body_is_refused() {
    let error = PolicyProperty::parse("sync_policy", "class = ").unwrap_err();
    assert_eq!(error.code(), codes::BAD_VALUE);
}

#[test]
fn the_error_maps_onto_a_validation_app_error() {
    let error = PolicyProperty::parse("sync_class", "sideways").unwrap_err();
    let app = error.to_error();
    assert_eq!(app.code, codes::BAD_VALUE);
    assert_eq!(app.class, spot_app_api::ErrorClass::Validation);
}
