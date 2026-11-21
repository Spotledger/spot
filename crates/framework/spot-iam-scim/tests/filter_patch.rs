//! Filters and PATCH: the subset the providers send, and the clean refusals for the rest.
#![allow(missing_docs)]

use serde_json::json;
use spot_iam_scim::{Filter, PatchOpKind, PatchRequest, ScimGroup, ScimUser, Target};

fn user() -> ScimUser {
    serde_json::from_value(json!({
        "id": "0191a0b2-0000-7000-8000-000000000001",
        "userName": "Bob@Contoso.com",
        "displayName": "Bob Smith",
        "externalId": "e-1",
        "active": true,
        "name": {"givenName": "Bob", "familyName": "Smith"},
        "emails": [
            {"value": "bob@contoso.com", "type": "work", "primary": true},
            {"value": "bob@home.example", "type": "home"}
        ]
    }))
    .unwrap_or_else(|e| panic!("user: {e}"))
}

fn group() -> ScimGroup {
    serde_json::from_value(json!({
        "id": "0191a0b2-0000-7000-8000-000000000002",
        "displayName": "Engineering",
        "members": [
            {"value": "0191a0b2-0000-7000-8000-000000000001", "display": "Bob Smith"}
        ]
    }))
    .unwrap_or_else(|e| panic!("group: {e}"))
}

fn matches(filter: &str, target: &Target<'_>) -> bool {
    Filter::parse(filter)
        .unwrap_or_else(|e| panic!("{filter}: {e}"))
        .matches(target)
        .unwrap_or_else(|e| panic!("{filter}: {e}"))
}

#[test]
fn equality_is_case_insensitive_and_covers_the_provider_attributes() {
    let u = user();
    assert!(matches(
        "userName eq \"bob@contoso.com\"",
        &Target::User(&u)
    ));
    assert!(matches("externalId eq \"E-1\"", &Target::User(&u)));
    assert!(matches("displayName eq \"bob smith\"", &Target::User(&u)));
    assert!(matches("active eq true", &Target::User(&u)));
    assert!(matches(
        "emails.value eq \"BOB@CONTOSO.COM\"",
        &Target::User(&u)
    ));
    assert!(!matches(
        "userName eq \"nobody@contoso.com\"",
        &Target::User(&u)
    ));
    assert!(matches(
        "userName ne \"nobody@contoso.com\"",
        &Target::User(&u)
    ));
}

#[test]
fn the_string_operators_and_logical_operators_work() {
    let u = user();
    assert!(matches("displayName co \"Smith\"", &Target::User(&u)));
    assert!(matches("displayName sw \"Bob\"", &Target::User(&u)));
    assert!(matches("displayName ew \"Smith\"", &Target::User(&u)));
    assert!(matches("externalId pr", &Target::User(&u)));
    assert!(matches(
        "userName eq \"bob@contoso.com\" and active eq true",
        &Target::User(&u)
    ));
    assert!(matches(
        "(userName eq \"x\" or userName eq \"bob@contoso.com\") and active eq true",
        &Target::User(&u)
    ));
    assert!(matches("not (userName eq \"x\")", &Target::User(&u)));
}

#[test]
fn value_filters_select_rows_of_a_multi_valued_attribute() {
    let u = user();
    assert!(matches(
        "emails[type eq \"work\"].value eq \"bob@contoso.com\"",
        &Target::User(&u)
    ));
    assert!(!matches(
        "emails[type eq \"home\"].value eq \"bob@contoso.com\"",
        &Target::User(&u)
    ));
    let g = group();
    assert!(matches(
        "members[value eq \"0191a0b2-0000-7000-8000-000000000001\"]",
        &Target::Group(&g)
    ));
    assert!(matches(
        "displayName eq \"Engineering\"",
        &Target::Group(&g)
    ));
}

#[test]
fn an_unknown_attribute_is_refused_not_ignored() {
    let u = user();
    let error = Filter::parse("favouriteColour eq \"blue\"")
        .unwrap_or_else(|e| panic!("{e}"))
        .matches(&Target::User(&u))
        .err()
        .unwrap_or_else(|| panic!("should refuse"));
    assert_eq!(error.code, "iam_scim.invalid_filter");
}

#[test]
fn patch_replaces_a_single_attribute() {
    let patch = PatchRequest::parse(&json!({
        "Operations": [{"op": "replace", "path": "displayName", "value": "Robert Smith"}]
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    let next = patch
        .apply_to_user(&user())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(next.display_name.as_deref(), Some("Robert Smith"));
}

#[test]
fn patch_accepts_entras_string_false_for_active() {
    let patch = PatchRequest::parse(&json!({
        "Operations": [{"op": "Replace", "path": "active", "value": "False"}]
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    let next = patch
        .apply_to_user(&user())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!next.active);
}

#[test]
fn patch_accepts_a_no_path_replace_with_an_object() {
    let patch = PatchRequest::parse(&json!({
        "Operations": [{"op": "replace", "value": {"active": false, "displayName": "Gone"}}]
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    let next = patch
        .apply_to_user(&user())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!next.active);
    assert_eq!(next.display_name.as_deref(), Some("Gone"));
}

#[test]
fn patch_adds_and_removes_group_members() {
    let add = PatchRequest::parse(&json!({
        "Operations": [{"op": "add", "path": "members", "value": [{"value": "0191a0b2-0000-7000-8000-000000000009"}]}]
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    let next = add
        .apply_to_group(&group())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(next.members.len(), 2);

    let remove = PatchRequest::parse(&json!({
        "Operations": [{"op": "remove", "path": "members[value eq \"0191a0b2-0000-7000-8000-000000000001\"]"}]
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    let next = remove
        .apply_to_group(&group())
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(next.members.is_empty());
}

#[test]
fn patch_refuses_an_unknown_path_and_a_wrong_value() {
    let bad_path = PatchRequest::parse(&json!({
        "Operations": [{"op": "replace", "path": "nope", "value": "x"}]
    }))
    .unwrap_or_else(|e| panic!("{e}"))
    .apply_to_user(&user())
    .err()
    .unwrap_or_else(|| panic!("should refuse"));
    assert_eq!(bad_path.code, "iam_scim.invalid_path");

    let bad_value = PatchRequest::parse(&json!({
        "Operations": [{"op": "replace", "path": "active", "value": "maybe"}]
    }))
    .unwrap_or_else(|e| panic!("{e}"))
    .apply_to_user(&user())
    .err()
    .unwrap_or_else(|| panic!("should refuse"));
    assert_eq!(bad_value.code, "iam_scim.invalid_value");
}

#[test]
fn patch_op_kind_is_case_insensitive() {
    let request = PatchRequest::parse(&json!({
        "Operations": [
            {"op": "Add", "path": "displayName", "value": "A"},
            {"op": "REPLACE", "path": "displayName", "value": "B"},
            {"op": "Remove", "path": "externalId"}
        ]
    }))
    .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(request.operations[0].op, PatchOpKind::Add);
    assert_eq!(request.operations[1].op, PatchOpKind::Replace);
    assert_eq!(request.operations[2].op, PatchOpKind::Remove);
}
