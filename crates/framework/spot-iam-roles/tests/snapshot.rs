//! The snapshot input, proved against the permission engine.
#![allow(missing_docs)]

mod common;

use common::{action, at, customers, engine, invoice, only_customers, registry};
use spot_iam_roles::{Assignment, Scope, Source, build_snapshot_input, codes, expiry_notices};

fn scoped(
    id: &str,
    user: &str,
    role: &str,
    customers: &[spot_app_api::prelude::RecordId],
) -> Assignment {
    Assignment {
        scope: Scope {
            restrictions: vec![only_customers(user, customers)],
        },
        ..Assignment::new(id, user, role)
    }
}

#[test]
fn manager_for_company_a_and_viewer_for_company_b_are_two_entries_with_their_own_scope() {
    let c = customers();
    let (a, b) = (c["c1"], c["c2"]);
    let assignments = [
        scoped("a-mgr", "u1", "Accounting Manager", &[a]),
        scoped("b-view", "u1", "Accounting Viewer", &[b]),
    ];
    let input =
        build_snapshot_input(&registry(), &assignments, at(0)).unwrap_or_else(|e| panic!("{e}"));
    let contexts = &input.assignments["u1"];
    assert_eq!(contexts.len(), 2);
    assert_eq!(contexts[0].id, "a-mgr");
    assert_eq!(contexts[0].roles[0].as_str(), "Accounting Manager");
    assert_eq!(contexts[0].scope[0].allowed, [a.to_string()]);
    assert_eq!(contexts[1].id, "b-view");
    assert_eq!(contexts[1].roles[0].as_str(), "Accounting Viewer");
    assert_eq!(contexts[1].scope[0].allowed, [b.to_string()]);

    let engine = engine(&input);
    let user = input.principal("u1");
    let can = |act: &str, customer| {
        engine
            .can(&user, action(act), &invoice(customer, "draft"))
            .allowed
    };
    // Company A: manager rights.
    assert!(can("read", a));
    assert!(can("update", a));
    assert!(can("post", a));
    // Company B: viewer rights only.
    assert!(can("read", b));
    assert!(!can("update", b));
    assert!(!can("post", b));
    // Company C: neither assignment covers it.
    assert!(!can("read", c["c3"]));
}

#[test]
fn no_assignment_means_no_rights() {
    let c = customers();
    let input = build_snapshot_input(&registry(), &[], at(0)).unwrap_or_else(|e| panic!("{e}"));
    let engine = engine(&input);
    assert!(
        !engine
            .can(
                &input.principal("nobody"),
                action("read"),
                &invoice(c["c1"], "draft")
            )
            .allowed
    );
}

#[test]
fn the_window_is_inclusive_at_the_start_and_exclusive_at_the_end() {
    let mut a = Assignment::new("a1", "u1", "Clerk");
    a.valid_from = Some(at(100));
    a.valid_to = Some(at(200));
    let active = |s: i64| {
        build_snapshot_input(&registry(), std::slice::from_ref(&a), at(s))
            .unwrap_or_else(|e| panic!("{e}"))
            .assignments
            .contains_key("u1")
    };
    assert!(!active(99));
    assert!(active(100));
    assert!(active(199));
    assert!(!active(200));
    assert!(!active(1000));
}

#[test]
fn expired_assignments_are_listed_once_ended() {
    let mut a = Assignment::new("a1", "u1", "Clerk");
    a.valid_to = Some(at(200));
    a.source = Source::Emergency;
    let before = build_snapshot_input(&registry(), std::slice::from_ref(&a), at(199))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(before.expired.is_empty());
    let after = build_snapshot_input(&registry(), std::slice::from_ref(&a), at(200))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(after.expired.len(), 1);
    assert_eq!(after.expired[0].assignment, "a1");
    assert_eq!(after.expired[0].valid_to, at(200));
}

#[test]
fn expiry_notices_cover_a_half_open_interval_in_order() {
    let make = |id: &str, to: i64| {
        let mut a = Assignment::new(id, "u1", "Clerk");
        a.valid_to = Some(at(to));
        a
    };
    let list = [
        make("c", 300),
        make("a", 100),
        make("b", 200),
        make("never", 0),
    ];
    let mut never = list[3].clone();
    never.valid_to = None;
    let list = [list[0].clone(), list[1].clone(), list[2].clone(), never];
    let ids = |after: i64, up_to: i64| -> Vec<String> {
        expiry_notices(&list, at(after), at(up_to))
            .into_iter()
            .map(|n| n.assignment)
            .collect()
    };
    assert_eq!(ids(0, 300), ["a", "b", "c"]);
    assert_eq!(ids(100, 300), ["b", "c"], "the start is exclusive");
    assert_eq!(ids(0, 199), ["a"]);
    assert!(ids(300, 400).is_empty());
}

#[test]
fn an_unknown_role_is_an_error_not_a_silent_skip() {
    let e = build_snapshot_input(&registry(), &[Assignment::new("a1", "u1", "Ghost")], at(0))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::UNKNOWN_ROLE]);
}

#[test]
fn the_output_does_not_depend_on_the_input_order() {
    let c = customers();
    let list = [
        scoped("a3", "u2", "Clerk", &[c["c1"]]),
        scoped("a1", "u1", "Accounting Manager", &[c["c1"]]),
        scoped("a2", "u1", "Accounting Viewer", &[c["c2"]]),
        Assignment::new("a4", "u3", "Own Invoices"),
    ];
    let forward = build_snapshot_input(&registry(), &list, at(0)).unwrap_or_else(|e| panic!("{e}"));
    let mut reversed = list.clone();
    reversed.reverse();
    let backward =
        build_snapshot_input(&registry(), &reversed, at(0)).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(forward, backward);
    assert_eq!(
        forward.assignments["u1"]
            .iter()
            .map(|a| a.id.as_str())
            .collect::<Vec<_>>(),
        ["a1", "a2"]
    );
}

#[test]
fn a_field_deny_reaches_the_engine_as_a_deny() {
    let c = customers();
    let input = build_snapshot_input(
        &registry(),
        &[Assignment::new("a1", "u1", "Redacted Viewer")],
        at(0),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    assert!(
        input
            .field_grants
            .iter()
            .any(|g| g.deny && g.role.as_str() == "Redacted Viewer")
    );
    let engine = engine(&input);
    assert!(
        engine
            .can(
                &input.principal("u1"),
                action("read"),
                &invoice(c["c1"], "draft")
            )
            .allowed
    );
}

#[test]
fn the_resolved_roles_explain_where_a_right_came_from() {
    let input = build_snapshot_input(
        &registry(),
        &[Assignment::new("a1", "u1", "Senior Accountant")],
        at(0),
    )
    .unwrap_or_else(|e| panic!("{e}"));
    let context = &input.assignments["u1"][0];
    let role = spot_iam_roles::RoleName::from(context.roles[0].as_str());
    let sources = input.resolved[&role].sources_of("sales_invoice", false);
    assert!(sources.iter().any(
        |s| s.duty.as_ref().map(|d| d.as_str()) == Some("cancel_invoices")
            && s.role_path.len() == 1
    ));
}
