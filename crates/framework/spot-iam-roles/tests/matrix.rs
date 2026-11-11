//! The role and assignment matrix, read from `tests/matrix.toml`.
#![allow(missing_docs)]

mod common;

use std::collections::BTreeMap;

use common::{action, at, customers, engine, invoice, only_customers, registry};
use serde::Deserialize;
use spot_iam_roles::{Assignment, RoleName, Scope, build_snapshot_input, effective};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    scenario: Vec<Scenario>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    kind: String,
    role: Option<String>,
    doctype: Option<String>,
    rights: Option<Vec<String>>,
    at: Option<i64>,
    #[serde(default)]
    assignments: Vec<Row>,
    active: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    can: Vec<Check>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Row {
    id: String,
    user: String,
    role: String,
    #[serde(default)]
    scope: Vec<String>,
    valid_from: Option<i64>,
    valid_to: Option<i64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    action: String,
    customer: String,
    state: String,
    allowed: bool,
}

fn assignment(row: &Row) -> Assignment {
    let c = customers();
    let ids: Vec<_> = row.scope.iter().map(|n| c[n.as_str()]).collect();
    Assignment {
        scope: Scope {
            restrictions: if ids.is_empty() {
                vec![]
            } else {
                vec![only_customers(&row.user, &ids)]
            },
        },
        valid_from: row.valid_from.map(at),
        valid_to: row.valid_to.map(at),
        ..Assignment::new(&row.id, &row.user, &row.role)
    }
}

#[test]
fn the_matrix_holds() {
    let file: File =
        toml::from_str(include_str!("matrix.toml")).unwrap_or_else(|e| panic!("matrix.toml: {e}"));
    assert!(
        file.scenario.len() >= 25,
        "the matrix needs at least 25 scenarios"
    );
    let reg = registry();
    let c = customers();
    for s in &file.scenario {
        let list: Vec<Assignment> = s.assignments.iter().map(assignment).collect();
        match s.kind.as_str() {
            "rights" => {
                let role = reg
                    .role(&RoleName::from(s.role.as_deref().unwrap_or_default()))
                    .cloned()
                    .unwrap_or_else(|| panic!("{}: unknown role", s.name));
                let doctype = s.doctype.as_deref().unwrap_or_default();
                let got: Vec<String> = effective(&role, &reg)
                    .grants
                    .iter()
                    .find(|g| g.grant.doctype == doctype && !g.grant.if_owner)
                    .map(|g| {
                        g.grant
                            .rights
                            .as_slice()
                            .iter()
                            .map(|r| r.as_str().to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                let mut want = s.rights.clone().unwrap_or_default();
                want.sort();
                let mut got_sorted = got.clone();
                got_sorted.sort();
                assert_eq!(got_sorted, want, "{}", s.name);
            }
            "active" => {
                let input = build_snapshot_input(&reg, &list, at(s.at.unwrap_or(0)))
                    .unwrap_or_else(|e| panic!("{}: {e}", s.name));
                let got: BTreeMap<String, Vec<String>> = input
                    .assignments
                    .iter()
                    .map(|(u, v)| (u.clone(), v.iter().map(|a| a.id.clone()).collect()))
                    .collect();
                assert_eq!(Some(&got), s.active.as_ref(), "{}", s.name);
            }
            "can" => {
                let input = build_snapshot_input(&reg, &list, at(0))
                    .unwrap_or_else(|e| panic!("{}: {e}", s.name));
                let engine = engine(&input);
                let user = s.assignments.first().map_or("u", |r| r.user.as_str());
                let principal = input.principal(user);
                for check in &s.can {
                    let got = engine
                        .can(
                            &principal,
                            action(&check.action),
                            &invoice(c[check.customer.as_str()], &check.state),
                        )
                        .allowed;
                    assert_eq!(
                        got, check.allowed,
                        "{}: {} on {}",
                        s.name, check.action, check.customer
                    );
                }
            }
            other => panic!("{}: unknown kind `{other}`", s.name),
        }
    }
}
