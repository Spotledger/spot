//! The portal rule (ADR-083 Barrier 3) applied by `build` from `AccessModel::portal_users`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Fixture, assignment, duty, grant, role, seeded, t};
use spot_iam_questions::{NoAudit, Page, Questions, build, build_guarded};
use spot_iam_roles::{OutsideGuard, RoleKind};
use spot_permissions::Right;

const AT: i64 = 200;

/// The seeded matrix plus a portal role (one outside-exposed duty) and an assignment of it
/// and of the desk role "Customer Reader" to `p01`.
fn with_portal_user() -> Fixture {
    let mut f = seeded();
    let mut exposed = duty(
        "Portal Invoices",
        vec![grant(
            "sales_invoice",
            "Portal Invoices",
            &[Right::Read],
            false,
        )],
    );
    exposed.outside_exposed = true;
    let mut portal = role("Portal Viewer", vec!["Portal Invoices"], vec![]);
    portal.kind = RoleKind::Portal;
    let mut duties: Vec<_> = f.registry.duties().cloned().collect();
    duties.push(exposed);
    let mut roles: Vec<_> = f.registry.roles().cloned().collect();
    roles.push(portal);
    f.registry = spot_iam_roles::Registry::new(duties, roles).expect("registry is valid");
    f.assignments.push(assignment(
        "a-p01-desk",
        "p01",
        "Customer Reader",
        spot_iam_roles::Source::Manual,
        None,
        None,
        None,
    ));
    f.assignments.push(assignment(
        "a-p01-portal",
        "p01",
        "Portal Viewer",
        spot_iam_roles::Source::Manual,
        None,
        None,
        None,
    ));
    f
}

fn doctypes_of(f: &Fixture, user: &str) -> Vec<String> {
    let auditor = NoAudit;
    let q = Questions::new(f, &auditor);
    q.what_can_user_do(user, user, t(AT), Page::default())
        .expect("self query")
        .items
        .into_iter()
        .map(|r| r.doctype)
        .collect()
}

#[test]
fn a_portal_user_with_a_desk_role_is_refused_and_grants_nothing() {
    let mut f = with_portal_user();
    f.set_portal_users(&["p01"]);
    let world = build(&f, t(AT)).unwrap();
    let refused: Vec<_> = world
        .input
        .refused
        .iter()
        .map(|r| r.assignment.as_str())
        .collect();
    assert_eq!(refused, vec!["a-p01-desk"]);
    let docs = doctypes_of(&f, "p01");
    assert!(!docs.iter().any(|d| d == "customer"), "{docs:?}");
}

#[test]
fn the_same_model_without_the_portal_mark_behaves_as_before() {
    let f = with_portal_user();
    let world = build(&f, t(AT)).unwrap();
    assert!(world.input.refused.is_empty());
    let docs = doctypes_of(&f, "p01");
    assert!(docs.iter().any(|d| d == "customer"), "{docs:?}");
}

#[test]
fn a_portal_user_with_a_portal_role_keeps_its_rights() {
    let mut f = with_portal_user();
    f.set_portal_users(&["p01"]);
    let docs = doctypes_of(&f, "p01");
    assert!(docs.iter().any(|d| d == "sales_invoice"), "{docs:?}");
}

#[test]
fn build_guarded_with_an_explicit_guard_is_reachable_and_works() {
    let f = with_portal_user();
    let guard = OutsideGuard::new(&f.registry, ["p01"]);
    let world = build_guarded(&f, t(AT), &guard).unwrap();
    assert_eq!(world.input.refused.len(), 1);
    let none = build_guarded(&f, t(AT), &OutsideGuard::none()).unwrap();
    assert!(none.input.refused.is_empty());
}
