//! Loading the model: every problem at once, and nothing about `permlevel`.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_permissions::{ModelTexts, load};

#[test]
fn the_fixture_model_loads_and_is_sorted() {
    let model = common::model();
    assert!(model.grants.len() >= 15);
    assert!(model.field_grants.len() >= 20);
    assert_eq!(model.groups.len(), 5);
    assert_eq!(model.shares.len(), 2);
    let mut sorted = model.grants.clone();
    sorted.sort();
    assert_eq!(sorted, model.grants);
}

#[test]
fn permlevel_is_an_error_not_an_alias() {
    let texts = ModelTexts {
        grants: r#"
[[entries]]
role = "Writer"
doctype = "project"
rights = ["read"]
permlevel = 1
"#
        .to_owned(),
        ..ModelTexts::default()
    };
    let errors = load(&texts).err().unwrap();
    assert_eq!(errors.as_slice().len(), 1);
    assert_eq!(errors.as_slice()[0].code(), "perm.parse_failed");
    assert!(errors.as_slice()[0].to_string().contains("permlevel"));
}

#[test]
fn every_problem_is_reported_at_once() {
    let texts = ModelTexts {
        grants: r#"
[[entries]]
role = "A"
doctype = "project"
rights = []
[[entries]]
role = "A"
doctype = "project"
rights = ["read"]
[[entries]]
role = "A"
doctype = "project"
rights = ["read"]
"#
        .to_owned(),
        field_grants: r#"
[[entries]]
role = "A"
doctype = "project"
target = { group = "" }
access = ["read"]
[[entries]]
role = "B"
doctype = "project"
target = { field = "x" }
"#
        .to_owned(),
        groups: r#"
[[entries]]
name = "g"
[[entries]]
name = "g"
"#
        .to_owned(),
        shares: r#"
[[entries]]
doc = "not-an-id"
user = "sam"
rights = ["read"]
"#
        .to_owned(),
    };
    let errors = load(&texts).err().unwrap();
    let codes: Vec<&str> = errors.as_slice().iter().map(|e| e.code()).collect();
    // empty rights, two duplicates of the first grant's key, empty target, no rights on a
    // non-deny field grant, duplicate group, a bad share id.
    assert_eq!(codes.len(), 7, "{:?}", errors.as_slice());
    assert!(codes.contains(&"perm.missing"));
    assert!(codes.contains(&"perm.inconsistent"));
    assert!(codes.contains(&"perm.value_invalid"));
}

#[test]
fn the_snapshot_refuses_what_the_metadata_does_not_know() {
    let texts = ModelTexts {
        grants: r#"
[[entries]]
role = "A"
doctype = "ghost"
rights = ["read"]
"#
        .to_owned(),
        field_grants: r#"
[[entries]]
role = "A"
doctype = "project"
target = { field = "nope" }
access = ["read"]
[[entries]]
role = "A"
doctype = "project"
target = { group = "undeclared" }
access = ["read"]
"#
        .to_owned(),
        ..ModelTexts::default()
    };
    let model = load(&texts).unwrap();
    let errors = spot_permissions::Snapshot::build(&common::meta(), &model)
        .err()
        .unwrap();
    assert_eq!(errors.as_slice().len(), 3, "{:?}", errors.as_slice());
}

#[test]
fn field_facts_must_name_declared_groups_and_real_fields() {
    use spot_permissions::{FieldFacts, GroupName};
    let snap = common::snapshot();
    let bad_group = snap.clone().with_field_facts(
        "project",
        vec![FieldFacts {
            path: "project_name".into(),
            groups: vec![GroupName("nope".into())],
            ..FieldFacts::default()
        }],
    );
    assert!(bad_group.is_err());
    let bad_field = snap.with_field_facts("project", vec![FieldFacts::plain("ghost")]);
    assert!(bad_field.is_err());
}
