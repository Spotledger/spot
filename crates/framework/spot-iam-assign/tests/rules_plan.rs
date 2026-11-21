//! Rule-sourced assignments follow the condition.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{T0, counting_ids, later, live_roles, roles, rule_set, settle, users};
use spot_iam_assign::{
    Assignment, Change, EndCause, EventPayload, Facts, RuleSet, RuleSpec, Source,
    plan_rules_guarded,
};

fn by_user(plan: &spot_iam_assign::Plan, user: &str) -> Vec<String> {
    plan.changes
        .iter()
        .filter(|c| c.user() == user)
        .map(|c| match c {
            Change::Grant(a) => format!("grant {}", a.role.as_str()),
            Change::End { role, .. } => format!("end {}", role.as_str()),
            Change::Replace { with, .. } => format!("replace {}", with.role.as_str()),
        })
        .collect()
}

#[test]
fn a_rule_starts_and_ends_with_its_condition() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut all = users();
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &[],
        &all,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert!(plan.errors.is_empty());
    assert_eq!(by_user(&plan, "ben"), ["grant Accountant"]);
    let state = settle(&plan, &[]);
    let ben = state.iter().find(|a| a.user == "ben").unwrap();
    assert_eq!(ben.source, Source::Rule);
    assert_eq!(ben.reason.as_deref(), Some("rule:accountants"));
    assert_eq!(live_roles(&state, "ben", T0), ["Accountant"]);

    // Ben leaves Finance: the assignment ends at once.
    all[1].1 = Facts::new()
        .text("department", "Legal")
        .text("company", "C1");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(by_user(&plan, "ben"), ["end Accountant"]);
    let state = settle(&plan, &state);
    assert!(live_roles(&state, "ben", later(1)).is_empty());
    assert!(matches!(
        plan.events
            .iter()
            .find(|e| matches!(e, EventPayload::Ended { user, .. } if user == "ben")),
        Some(EventPayload::Ended {
            cause: EndCause::RuleEnded,
            source: Source::Rule,
            ..
        })
    ));
}

#[test]
fn a_department_change_moves_a_user_between_two_rules() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut all = users();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    assert_eq!(live_roles(&state, "ben", T0), ["Accountant"]);
    all[1].1 = Facts::new()
        .text("department", "Sales")
        .text("company", "C1");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(
        by_user(&plan, "ben"),
        ["end Accountant", "grant Sales User"]
    );
    let state = settle(&plan, &state);
    assert_eq!(live_roles(&state, "ben", later(1)), ["Sales User"]);
}

#[test]
fn the_scope_template_gives_each_user_their_own_company() {
    let mut ids = counting_ids();
    let state = settle(
        &plan_rules_guarded(
            &rule_set(),
            &mut ids,
            &[],
            &users(),
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let allowed = |user: &str| {
        let a = state
            .iter()
            .find(|a| a.user == user && a.role.as_str() == "Accountant")
            .unwrap();
        assert_eq!(a.scope.restrictions.len(), 1);
        let r = &a.scope.restrictions[0];
        assert!(r.include_descendants && r.user == user);
        (r.field_or_link_doctype.clone(), r.allowed.clone())
    };
    assert_eq!(
        allowed("ben"),
        ("Company".to_owned(), vec!["C1".to_owned()])
    );
    assert_eq!(allowed("cy"), ("Company".to_owned(), vec!["C2".to_owned()]));
}

#[test]
fn a_changed_scope_is_replaced_in_place_without_a_gap() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut all = users();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    all[1].1 = Facts::new()
        .text("department", "Finance")
        .text("position", "Accountant")
        .text("company", "C9");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(by_user(&plan, "ben"), ["replace Accountant"]);
    let Change::Replace { id, with } = &plan.changes[0] else {
        panic!("expected a replace")
    };
    assert_eq!(id, &with.id);
    assert_eq!(with.scope.restrictions[0].allowed, ["C9"]);
    assert!(matches!(plan.events[0], EventPayload::Changed { .. }));
    let state = settle(&plan, &state);
    assert_eq!(live_roles(&state, "ben", later(1)), ["Accountant"]);
}

#[test]
fn a_multi_valued_fact_gives_one_assignment_with_all_values() {
    let mut ids = counting_ids();
    let spec: RuleSpec = toml::from_str(
        "id='multi'\nname='n'\nrole='Manager'\nwhen='true'\n[[scope]]\nlink='Company'\nallowed='facts.companies'\n",
    )
    .unwrap();
    let set = RuleSet::load(vec![spec], &roles()).unwrap();
    let users = vec![(
        "u".to_owned(),
        Facts::new().texts("companies", &["B", "A", "B"]),
    )];
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &[],
        &users,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let a = plan.grants().next().unwrap();
    assert_eq!(a.scope.restrictions[0].allowed, ["A", "B"]);
}

#[test]
fn a_disabled_or_deleted_rule_ends_what_it_gave() {
    let mut ids = counting_ids();
    let set = rule_set();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &users(),
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let mut specs = common::fixture().rule;
    specs[0].enabled = false; // sales disabled
    specs.retain(|r| r.id != "accountants"); // accountants deleted
    let smaller = RuleSet::load(specs, &roles()).unwrap();
    let plan = plan_rules_guarded(
        &smaller,
        &mut ids,
        &state,
        &users(),
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(by_user(&plan, "ana"), ["end Sales User"]);
    assert_eq!(by_user(&plan, "ben"), ["end Accountant"]);
    let why: Vec<&str> = plan
        .changes
        .iter()
        .filter_map(|c| match c {
            Change::End { reason, .. } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    assert!(why.iter().any(|w| w.contains("disabled")));
    assert!(why.iter().any(|w| w.contains("no longer exists")));
}

#[test]
fn an_evaluation_error_leaves_that_users_assignments_alone() {
    let mut ids = counting_ids();
    let spec: RuleSpec =
        toml::from_str("id='lvl'\nname='n'\nrole='Manager'\nwhen='facts.level > 3'\n").unwrap();
    let mut specs = common::fixture().rule;
    specs.push(spec);
    let set = RuleSet::load(specs, &roles()).unwrap();
    let mut all = users();
    all[0].1 = all[0]
        .1
        .clone()
        .set_value("level", spot_iam_assign::FactValue::Int(5));
    all[1].1 = all[1]
        .1
        .clone()
        .set_value("level", spot_iam_assign::FactValue::Int(5));
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    assert!(live_roles(&state, "ben", T0).contains(&"Manager".to_owned()));

    // Ben's `level` becomes text: `>` cannot compare it. His access must survive, and he
    // must not be changed in any way; the others are planned normally.
    all[1].1 = all[1]
        .1
        .clone()
        .set_value("level", spot_iam_assign::FactValue::Text("high".into()));
    all[1].1 = all[1].1.clone().text("department", "Legal");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(plan.errors.len(), 1);
    assert_eq!(plan.errors[0].user, "ben");
    assert!(plan.changes.iter().all(|c| c.user() != "ben"));
    let state = settle(&plan, &state);
    assert!(live_roles(&state, "ben", later(1)).contains(&"Accountant".to_owned()));
    // Cy, evaluated without trouble, still got planned (and is unchanged here).
    assert!(plan.errors.iter().all(|e| e.user != "cy"));
}

#[test]
fn a_missing_scope_value_never_becomes_an_unrestricted_grant() {
    let mut ids = counting_ids();
    let mut all = users();
    all[1].1 = Facts::new()
        .text("department", "Finance")
        .text("position", "Accountant"); // no company
    let plan = plan_rules_guarded(
        &rule_set(),
        &mut ids,
        &[],
        &all,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(plan.errors.len(), 1);
    assert!(plan.changes.iter().all(|c| c.user() != "ben"));
}

#[test]
fn a_time_boxed_rule_grants_for_its_span_and_is_not_renewed() {
    let mut ids = counting_ids();
    let set = rule_set();
    let all = users();
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let audit = state
        .iter()
        .find(|a| a.role.as_str() == "Auditor")
        .unwrap()
        .clone();
    assert_eq!(audit.valid_to, Some(later(24)));
    // While it runs, the plan is quiet.
    assert!(
        plan_rules_guarded(
            &set,
            &mut ids,
            &state,
            &all,
            later(5),
            &spot_iam_assign::OutsideGuard::none()
        )
        .changes
        .is_empty()
    );
    // After it ran its course the condition still holds, but it is not given again.
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        later(30),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert!(plan.changes.is_empty(), "{:?}", plan.changes);
}

#[test]
fn a_rule_assignment_reusing_an_ended_id_gets_a_fresh_id() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut all = users();
    let s1 = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &[],
            &all,
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let saved = all[0].1.clone();
    all[0].1 = Facts::new().text("department", "Legal");
    let s2 = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &s1,
            &all,
            later(1),
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &s1,
    );
    all[0].1 = saved;
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &s2,
        &all,
        later(2),
        &spot_iam_assign::OutsideGuard::none(),
    );
    let fresh: Vec<&Assignment> = plan.grants().collect();
    assert!(fresh.iter().all(|a| s2.iter().all(|old| old.id != a.id)));
}
