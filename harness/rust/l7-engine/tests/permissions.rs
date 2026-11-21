//! The permission matrix: six principals crossed with six lifecycle operations, from a TOML
//! table, judged by the same decision core the engine's authorizer wraps.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use serde::Deserialize;
use spot_app_api::Operation;
use spot_app_api::prelude::Value;
use spot_l7_engine::world::{TIME_ENTRY, World};
use spot_permissions::{Action, DocFacts};

#[derive(Deserialize)]
struct Matrix {
    principals: Vec<String>,
    operations: Vec<String>,
    expected: Vec<Vec<bool>>,
}

fn matrix() -> Matrix {
    toml::from_str(include_str!("../permissions.toml")).expect("the matrix parses")
}

fn action(operation: &str) -> Action {
    match operation {
        "read" => Action::Read,
        "create" => Action::Op(Operation::Create),
        "update" => Action::Op(Operation::Update),
        "delete" => Action::Op(Operation::Delete),
        "post" => Action::Op(Operation::Post),
        "cancel" => Action::Op(Operation::Cancel),
        other => panic!("unknown operation {other}"),
    }
}

fn facts(operation: &str) -> DocFacts {
    let (state, created_by) = match operation {
        "create" => (None, None),
        "cancel" => (Some("posted"), Some("member")),
        _ => (Some("draft"), Some("member")),
    };
    DocFacts {
        doctype: TIME_ENTRY.to_owned(),
        id: None,
        created_by: created_by.map(str::to_owned),
        state: state.map(str::to_owned),
        values: BTreeMap::<String, Value>::new(),
    }
}

#[test]
fn the_permission_matrix_holds() {
    let world = World::new();
    let m = matrix();
    assert_eq!(m.principals.len(), 6);
    assert_eq!(m.operations.len(), 6);
    assert_eq!(m.expected.len(), 6);
    for (row, principal) in m.principals.iter().enumerate() {
        let p = world.principals.principal(principal);
        for (col, operation) in m.operations.iter().enumerate() {
            let expected = m.expected[row][col];
            let got = world
                .decision
                .can(p, action(operation), &facts(operation))
                .allowed;
            assert_eq!(
                got, expected,
                "principal `{principal}` operation `{operation}`: expected {expected}, got {got}"
            );
        }
    }
}

#[test]
fn the_six_principals_are_distinct_and_seeded() {
    let world = World::new();
    let names = world.principals.names();
    assert_eq!(names.len(), 6);
    let ids: Vec<_> = names.iter().map(|n| world.principals.id(n)).collect();
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(unique.len(), 6, "every principal has its own actor id");
    // `system` is not a user id.
    assert!(world.principals.principal("system").is_system);
    assert!(!world.principals.principal("ghost").is_system);
}
