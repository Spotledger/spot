//! The document permission matrix: one test reads `matrix.toml` and runs every scenario.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use spot_permissions::engine::Action;
use spot_permissions::model::{AssignmentContext, Principal, Role};
use spot_permissions::{DocFacts, Reason};

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Scenario {
    name: String,
    #[serde(default)]
    principal: PrincipalSpec,
    action: String,
    doctype: String,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    created_by: Option<String>,
    /// Either a canonical UUIDv7 text or a `doc_x` alias.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    values: BTreeMap<String, String>,
    allowed: bool,
    #[serde(default)]
    codes: Vec<String>,
    /// `id:covers` or `id:outside`, in assignment-id order.
    #[serde(default)]
    assignments: Option<Vec<String>>,
}

#[derive(Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PrincipalSpec {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    roles: Option<Vec<String>>,
    #[serde(default)]
    assignments: Vec<AssignmentContext>,
    #[serde(default)]
    is_system: bool,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Matrix {
    version: u32,
    #[serde(rename = "crate")]
    crate_: String,
    scenarios: Vec<Scenario>,
}

fn principal(spec: PrincipalSpec) -> Principal {
    if spec.is_system {
        return Principal::system();
    }
    let mut assignments = spec.assignments;
    if let Some(roles) = spec.roles {
        assignments.push(AssignmentContext {
            id: "default".to_owned(),
            roles: roles.into_iter().map(Role).collect(),
            scope: vec![],
        });
    }
    // `allowed` cells are aliases for record ids.
    for a in &mut assignments {
        for up in &mut a.scope {
            for cell in &mut up.allowed {
                if let spot_app_api::prelude::Value::Id(id) = resolve_value(cell) {
                    *cell = id.to_string();
                }
            }
        }
    }
    Principal::user(spec.user.as_deref().unwrap_or("dave"), assignments)
}

#[test]
fn the_document_matrix() {
    let text = include_str!("matrix.toml");
    let matrix: Matrix = toml::from_str(text).unwrap_or_else(|e| panic!("matrix.toml: {e}"));
    assert_eq!(matrix.version, 1);
    assert_eq!(matrix.crate_, "spot-permissions");
    assert!(
        matrix.scenarios.len() >= 40,
        "the brief asks for at least 40 scenarios, found {}",
        matrix.scenarios.len()
    );
    let engine = engine();
    let mut failures = Vec::new();
    for s in matrix.scenarios {
        let action = Action::parse(&s.action).unwrap_or_else(|| panic!("{}: action", s.name));
        let facts = DocFacts {
            doctype: s.doctype.clone(),
            id: s.id.as_deref().map(resolve_doc_id),
            created_by: s.created_by.clone(),
            state: s.state.clone(),
            values: s
                .values
                .iter()
                .map(|(k, v)| (k.clone(), resolve_value(v)))
                .collect(),
        };
        let p = principal(s.principal);
        let d = engine.can(&p, action, &facts);
        let kinds: Vec<&str> = d
            .reasons
            .iter()
            .filter(|r| r.kind() != "assignment")
            .map(Reason::kind)
            .collect();
        let want: Vec<&str> = s.codes.iter().map(String::as_str).collect();
        if d.allowed != s.allowed {
            failures.push(format!(
                "{}: allowed {} expected {}\n{}",
                s.name,
                d.allowed,
                s.allowed,
                engine.explain(&p, action, &facts)
            ));
        } else if kinds != want {
            failures.push(format!(
                "{}: reasons {kinds:?} expected {want:?}\n{}",
                s.name,
                engine.explain(&p, action, &facts)
            ));
        }
        if let Some(expected) = s.assignments {
            let got: Vec<String> = d
                .reasons
                .iter()
                .filter_map(|r| match r {
                    Reason::Assignment { id, covers } => Some(format!(
                        "{id}:{}",
                        if *covers { "covers" } else { "outside" }
                    )),
                    _ => None,
                })
                .collect();
            if got != expected {
                failures.push(format!(
                    "{}: assignments {got:?} expected {expected:?}",
                    s.name
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
}
