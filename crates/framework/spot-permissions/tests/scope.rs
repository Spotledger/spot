//! Per-assignment scope: link checks, allowed values, the explanation and the phase table.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;

use common::*;
use spot_app_api::prelude::Value;
use spot_events_contract::lifecycle::Operation;
use spot_permissions::engine::{Action, Allowed};
use spot_permissions::model::{AppliesTo, AssignmentContext, Principal, Role, UserPermission};
use spot_permissions::snapshot::DocumentData;
use spot_permissions::{DocFacts, phase_requirements};

fn id_text(alias: &str) -> String {
    match resolve_value(alias) {
        Value::Id(id) => id.to_string(),
        other => panic!("{other:?}"),
    }
}

fn up(path: &str, allowed: &[&str]) -> UserPermission {
    UserPermission {
        user: String::new(),
        doctype: None,
        field_or_link_doctype: path.to_owned(),
        allowed: allowed.iter().map(|a| id_text(a)).collect(),
        applies_to: AppliesTo::AllDoctypes,
        include_descendants: false,
    }
}

fn assignment(id: &str, roles: &[&str], scope: Vec<UserPermission>) -> AssignmentContext {
    AssignmentContext {
        id: id.to_owned(),
        roles: roles.iter().map(|r| Role((*r).to_owned())).collect(),
        scope,
    }
}

/// Company A Manager and Company B Viewer: two assignments, each with its own scope.
fn a_and_b() -> Principal {
    Principal::user(
        "dave",
        vec![
            assignment(
                "a",
                &["Manager"],
                vec![up("project.customer", &["customer_1"])],
            ),
            assignment(
                "b",
                &["Viewer"],
                vec![up("project.customer", &["customer_2"])],
            ),
        ],
    )
}

fn project_data(customer: &str) -> DocumentData {
    DocumentData {
        fields: BTreeMap::from([("customer".to_owned(), resolve_value(customer))]),
        ..DocumentData::default()
    }
}

fn project_facts() -> DocFacts {
    DocFacts {
        doctype: "project".into(),
        state: Some("draft".into()),
        ..DocFacts::default()
    }
}

#[test]
fn a_link_outside_the_granting_assignments_scope_is_a_violation() {
    let engine = engine();
    let p = a_and_b();
    // Writing a project of company 2 needs Manager, which only company 1's assignment holds.
    let v = engine.check_links(
        &p,
        Action::Op(Operation::Update),
        &project_facts(),
        &project_data("customer_2"),
    );
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].path, "customer");
    assert_eq!(v[0].value, id_text("customer_2"));
    // Inside the Manager's scope: nothing to report.
    assert!(
        engine
            .check_links(
                &p,
                Action::Op(Operation::Update),
                &project_facts(),
                &project_data("customer_1")
            )
            .is_empty()
    );
    // Reading company 2 is granted by the Viewer assignment, whose scope covers it.
    assert!(
        engine
            .check_links(
                &p,
                Action::Read,
                &project_facts(),
                &project_data("customer_2")
            )
            .is_empty()
    );
    // An action no assignment grants has no link violation: `can` is what refuses it.
    assert!(
        engine
            .check_links(
                &user("zed", &[]),
                Action::Read,
                &project_facts(),
                &project_data("customer_3")
            )
            .is_empty()
    );
    assert!(
        engine
            .check_links(
                &Principal::system(),
                Action::Read,
                &project_facts(),
                &project_data("customer_3")
            )
            .is_empty()
    );
}

#[test]
fn child_rows_carry_their_own_path() {
    let engine = engine();
    let p = Principal::user(
        "dave",
        vec![assignment(
            "a",
            &["Writer"],
            vec![up("sales_invoice_item.item", &["customer_1"])],
        )],
    );
    let row = |item: &str| BTreeMap::from([("item".to_owned(), resolve_value(item))]);
    let data = DocumentData {
        tables: BTreeMap::from([(
            "sales_invoice_item".to_owned(),
            vec![row("customer_1"), row("customer_2")],
        )]),
        ..DocumentData::default()
    };
    let facts = DocFacts {
        doctype: "sales_invoice".into(),
        state: Some("draft".into()),
        ..DocFacts::default()
    };
    let v = engine.check_links(&p, Action::Op(Operation::Update), &facts, &data);
    assert_eq!(v.len(), 1);
    assert_eq!(v[0].path, "sales_invoice_item[2].item");
}

#[test]
fn allowed_values_union_the_assignments_that_can_read() {
    let engine = engine();
    let only = |a: Allowed| match a {
        Allowed::Only(mut v) => {
            v.sort();
            v
        }
        Allowed::All => panic!("expected a restriction"),
    };
    let mut both = vec![id_text("customer_1"), id_text("customer_2")];
    both.sort();
    assert_eq!(
        only(engine.allowed_values(&a_and_b(), "project", "customer")),
        both
    );
    // An unscoped assignment that can read lifts the restriction.
    let mut with_open = a_and_b();
    with_open
        .assignments
        .push(assignment("open", &["Reader"], vec![]));
    assert_eq!(
        engine.allowed_values(&with_open, "project", "customer"),
        Allowed::All
    );
    // An unscoped assignment that cannot read the DocType does not widen the list; one scoped
    // on another DocType's field that can read it is unrestricted here.
    let mut with_other = a_and_b();
    with_other
        .assignments
        .push(assignment("other", &["Creator"], vec![]));
    assert_eq!(
        only(engine.allowed_values(&with_other, "project", "customer")),
        both,
        "Creator cannot read projects"
    );
    assert_eq!(
        engine.allowed_values(&Principal::system(), "project", "customer"),
        Allowed::All
    );
    // No assignments, no read: nothing may be entered.
    assert_eq!(
        engine.allowed_values(&user("zed", &[]), "project", "customer"),
        Allowed::Only(vec![])
    );
}

#[test]
fn the_explanation_names_every_assignment_and_role() {
    let engine = engine();
    let facts = DocFacts {
        values: BTreeMap::from([("customer".to_owned(), resolve_value("customer_2"))]),
        ..project_facts()
    };
    let text = engine.explain(&a_and_b(), Action::Read, &facts);
    assert!(text.contains("ALLOWED"), "{text}");
    assert!(text.contains("assignment `a` does not cover"), "{text}");
    assert!(text.contains("assignment `b` covers"), "{text}");
    assert!(
        text.contains("assignment `b`: role `Viewer` grants read"),
        "{text}"
    );
    let machine = engine.explain_machine(&a_and_b(), Action::Read, &facts);
    assert!(machine.starts_with("read project allow"), "{machine}");
    assert!(machine.contains("role_grant(b, Viewer)"), "{machine}");
    let denied = engine.explain(&user("zed", &[]), Action::Read, &facts);
    assert!(
        denied.contains("DENIED") && denied.contains("no_grant"),
        "{denied}"
    );
}

#[test]
fn phases_run_as_the_user_until_the_committed_step() {
    use spot_app_api::lifecycle::Phase;
    for op in [
        Operation::Create,
        Operation::Update,
        Operation::Post,
        Operation::Cancel,
    ] {
        let phases = phase_requirements(op);
        assert_eq!(phases.len(), 4);
        assert_eq!(phases[0].0, Phase::Before);
        assert_eq!(phases[3].0, Phase::Committed);
        assert_eq!(phases[3].1.as_str(), "as_system");
        assert!(phases[..3].iter().all(|(_, c)| c.as_str() == "as_user"));
    }
}
