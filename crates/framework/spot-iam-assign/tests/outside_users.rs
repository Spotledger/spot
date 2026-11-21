//! Barrier 3 of ADR-083 on the assignment sources: a rule or an IdP group never gives a portal
//! user a role an outside user may not hold, and ends one it already gave.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{T0, counting_ids, later};
use spot_iam_assign::{
    Assignment, Change, Facts, GroupRole, GroupRoleMap, OutsideGuard, RoleDirectory, RuleSet,
    RuleSpec, Source, plan_idp_guarded, plan_rules_guarded,
};
use spot_iam_roles::Registry;

const FIXTURE: &str = include_str!("../../spot-iam-roles/tests/fixtures/outside.toml");

fn registry() -> Registry {
    Registry::from_toml(FIXTURE).unwrap_or_else(|e| panic!("{e}"))
}

fn rule_set(role: &str) -> RuleSet {
    let spec = RuleSpec {
        id: "r1".to_owned(),
        name: "everyone".to_owned(),
        role: role.into(),
        when: "true".to_owned(),
        scope: vec![],
        valid_for_secs: None,
        enabled: true,
        priority: 0,
    };
    RuleSet::load(vec![spec], &RoleDirectory::from_registry(&registry())).unwrap()
}

fn users() -> Vec<(String, Facts)> {
    vec![
        ("portal-1".to_owned(), Facts::new()),
        ("staff-1".to_owned(), Facts::new()),
    ]
}

fn granted_to(plan: &spot_iam_assign::Plan) -> Vec<String> {
    plan.changes
        .iter()
        .filter_map(|c| match c {
            Change::Grant(a) => Some(a.user.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_rule_gives_a_portal_user_only_what_the_allow_list_lets_through() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    // (role, expected grants, expected errors)
    let table: [(&str, &[&str], usize); 5] = [
        ("Desk Clerk", &["staff-1"], 1),
        ("Portal Leaky", &["staff-1"], 1),
        ("Portal Inherits Leaky", &["staff-1"], 1),
        ("Portal Viewer", &["portal-1", "staff-1"], 0),
        ("Portal Direct", &["staff-1"], 1),
    ];
    for (role, grants, errors) in table {
        let plan = plan_rules_guarded(
            &rule_set(role),
            &mut counting_ids(),
            &[],
            &users(),
            T0,
            &guard,
        );
        assert_eq!(granted_to(&plan), grants, "{role}");
        assert_eq!(plan.errors.len(), errors, "{role}");
        if let Some(e) = plan.errors.first() {
            assert_eq!(e.user, "portal-1");
            assert!(e.message.contains(role), "{}", e.message);
            assert!(e.message.starts_with("iam_outside."), "{}", e.message);
        }
    }
}

#[test]
fn a_rule_assignment_a_portal_user_already_holds_ends_when_it_is_refused() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    let mut existing = Assignment::new("old", "portal-1", "Desk Clerk");
    existing.source = Source::Rule;
    existing.reason = Some("rule:r1".to_owned());
    let plan = plan_rules_guarded(
        &rule_set("Desk Clerk"),
        &mut counting_ids(),
        &[existing],
        &users(),
        later(1),
        &guard,
    );
    assert!(
        plan.changes
            .iter()
            .any(|c| matches!(c, Change::End { id, .. } if id == "old")),
        "{plan:?}"
    );
}

#[test]
fn the_empty_guard_changes_nothing() {
    let plan = plan_rules_guarded(
        &rule_set("Desk Clerk"),
        &mut counting_ids(),
        &[],
        &users(),
        T0,
        &OutsideGuard::none(),
    );
    assert_eq!(granted_to(&plan), ["portal-1", "staff-1"]);
    assert!(plan.errors.is_empty());
}

fn mapping(role: &str) -> GroupRoleMap {
    GroupRoleMap::load(
        vec![GroupRole {
            group: "g".to_owned(),
            role: role.into(),
            scope: vec![],
        }],
        &RoleDirectory::from_registry(&registry()),
    )
    .unwrap()
}

#[test]
fn an_idp_group_gives_a_portal_user_only_what_the_allow_list_lets_through() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    let groups = vec![
        ("portal-1".to_owned(), vec!["g".to_owned()]),
        ("staff-1".to_owned(), vec!["g".to_owned()]),
    ];
    for (role, grants, errors) in [
        ("Desk Clerk", vec!["staff-1"], 1),
        ("Portal Leaky", vec!["staff-1"], 1),
        ("Portal Viewer", vec!["portal-1", "staff-1"], 0),
    ] {
        let plan = plan_idp_guarded(
            &mapping(role),
            &mut counting_ids(),
            &groups,
            true,
            &[],
            T0,
            &guard,
        );
        assert_eq!(granted_to(&plan), grants, "{role}");
        assert_eq!(plan.errors.len(), errors, "{role}");
        if let Some(e) = plan.errors.first() {
            assert_eq!(e.subject, "idp:g");
            assert!(e.message.contains(role));
        }
    }
}

#[test]
fn an_idp_assignment_a_portal_user_already_holds_ends_when_it_is_refused() {
    let r = registry();
    let guard = OutsideGuard::new(&r, ["portal-1"]);
    let mut existing = Assignment::new("old", "portal-1", "Desk Clerk");
    existing.source = Source::Idp;
    existing.reason = Some("idp:g".to_owned());
    let groups = vec![("portal-1".to_owned(), vec!["g".to_owned()])];
    let plan = plan_idp_guarded(
        &mapping("Desk Clerk"),
        &mut counting_ids(),
        &groups,
        true,
        &[existing],
        later(1),
        &guard,
    );
    assert!(
        plan.changes
            .iter()
            .any(|c| matches!(c, Change::End { id, .. } if id == "old"))
    );
    assert!(granted_to(&plan).is_empty());
}
