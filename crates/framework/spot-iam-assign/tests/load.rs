//! Loading rules and mappings: every problem at once.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{fixture, roles};
use spot_iam_assign::{GroupRole, GroupRoleMap, RoleName, RuleSet, RuleSpec, codes};

fn spec(id: &str, role: &str, when: &str) -> RuleSpec {
    toml::from_str(&format!(
        "id = '{id}'\nname = 'n'\nrole = '{role}'\nwhen = \"{when}\"\n"
    ))
    .unwrap()
}

#[test]
fn the_fixture_rules_load_in_priority_order() {
    let set = RuleSet::load(fixture().rule, &roles()).unwrap();
    let ids: Vec<&str> = set.rules().iter().map(|r| r.id()).collect();
    assert_eq!(ids, ["sales", "accountants", "auditors"]);
}

#[test]
fn every_problem_is_reported_at_once() {
    let mut scoped = spec("e", "Manager", "true");
    scoped.scope = toml::from_str::<RuleSpec>(
        "id='x'\nname='n'\nrole='Manager'\nwhen='true'\n[[scope]]\nlink='Company'\nallowed='42'\n",
    )
    .unwrap()
    .scope;
    let errors = RuleSet::load(
        vec![
            spec("a", "No Such Role", "facts.x == 1"),
            spec("b", "Manager", "facts.x + 1"),
            spec("c", "Break Glass", "true"),
            spec("d", "Manager", "facts.x =="),
            scoped,
            spec("a", "Manager", "true"),
        ],
        &roles(),
    )
    .unwrap_err();
    for code in [
        codes::UNKNOWN_ROLE,
        codes::CONDITION_NOT_BOOLEAN,
        codes::EMERGENCY_ROLE,
        codes::BAD_EXPRESSION,
        codes::SCOPE_NOT_LINK,
        codes::DUPLICATE_RULE,
    ] {
        assert!(errors.has(code), "missing {code} in {errors}");
    }
}

#[test]
fn an_emergency_only_role_in_a_rule_is_refused() {
    let errors = RuleSet::load(vec![spec("a", "Break Glass", "true")], &roles()).unwrap_err();
    assert_eq!(errors.codes(), [codes::EMERGENCY_ROLE]);
    assert!(errors.to_string().contains("Break Glass"));
}

#[test]
fn a_mapping_to_an_unknown_or_emergency_role_is_refused() {
    let rows: Vec<GroupRole> = vec![
        GroupRole {
            group: "g1".into(),
            role: RoleName::from("Nope"),
            scope: vec![],
        },
        GroupRole {
            group: "g2".into(),
            role: RoleName::from("Break Glass"),
            scope: vec![],
        },
    ];
    let errors = GroupRoleMap::load(rows, &roles()).unwrap_err();
    assert!(errors.has(codes::UNKNOWN_ROLE) && errors.has(codes::EMERGENCY_ROLE));
}

#[test]
fn facts_inside_a_text_literal_are_left_alone() {
    // `'facts.x'` is text, not a path; only the bare path is rewritten.
    let set = RuleSet::load(vec![spec("a", "Manager", "facts.n == 'facts.x'")], &roles());
    assert!(set.is_ok());
}
