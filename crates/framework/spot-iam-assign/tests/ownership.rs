//! One source cannot remove another's; manual edits of IdP-managed roles are refused.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{T0, counting_ids, live_roles, roles, rule_set, settle, users};
use spot_iam_assign::{
    Actor, Assignment, ChangeRequest, GroupRole, GroupRoleMap, ManagedRoles, RoleName, Source,
    authorise_change, codes, plan_idp_guarded, plan_rules_guarded,
};

fn with(id: &str, role: &str, source: Source) -> Assignment {
    let mut a = Assignment::new(id, "ben", role);
    a.source = source;
    a
}

fn managed() -> ManagedRoles {
    let mut m = ManagedRoles::new();
    m.declare(RoleName::from("Controller"), "Entra ID");
    m
}

const SOURCES: [Source; 4] = [Source::Manual, Source::Rule, Source::Idp, Source::Emergency];

#[test]
fn a_source_ends_only_its_own_assignments() {
    for owner in SOURCES {
        for actor in SOURCES {
            let verdict = authorise_change(
                &ChangeRequest::End(with("a", "Manager", owner)),
                Actor::Source(actor),
                &ManagedRoles::new(),
            );
            if owner == actor {
                assert!(verdict.is_ok(), "{owner:?} should end its own");
            } else if owner == Source::Emergency {
                assert_eq!(verdict.unwrap_err().code, codes::EMERGENCY_END);
            } else {
                assert_eq!(
                    verdict.unwrap_err().code,
                    codes::NOT_OWNER,
                    "{actor:?} on {owner:?}"
                );
            }
        }
    }
}

#[test]
fn expiry_may_end_any_source() {
    for owner in SOURCES {
        let r = authorise_change(
            &ChangeRequest::End(with("a", "Manager", owner)),
            Actor::Expiry,
            &ManagedRoles::new(),
        );
        assert!(r.is_ok());
    }
}

#[test]
fn a_source_edits_only_its_own_and_never_the_source() {
    let before = with("a", "Manager", Source::Rule);
    let mut after = before.clone();
    after.reason = Some("changed".into());
    let none = ManagedRoles::new();
    let edit = ChangeRequest::Edit {
        before: before.clone(),
        after: after.clone(),
    };
    assert!(authorise_change(&edit, Actor::Source(Source::Rule), &none).is_ok());
    assert_eq!(
        authorise_change(&edit, Actor::Source(Source::Manual), &none)
            .unwrap_err()
            .code,
        codes::NOT_OWNER
    );
    after.source = Source::Manual;
    let edit = ChangeRequest::Edit { before, after };
    assert_eq!(
        authorise_change(&edit, Actor::Source(Source::Rule), &none)
            .unwrap_err()
            .code,
        codes::SOURCE_EDIT
    );
}

#[test]
fn a_source_adds_only_its_own() {
    let add = ChangeRequest::Add(with("a", "Manager", Source::Idp));
    let none = ManagedRoles::new();
    assert!(authorise_change(&add, Actor::Source(Source::Idp), &none).is_ok());
    assert_eq!(
        authorise_change(&add, Actor::Source(Source::Manual), &none)
            .unwrap_err()
            .code,
        codes::WRONG_SOURCE
    );
}

#[test]
fn a_manual_change_to_an_idp_managed_role_is_refused_and_names_it() {
    let m = managed();
    for request in [
        ChangeRequest::Add(with("a", "Controller", Source::Manual)),
        ChangeRequest::End(with("a", "Controller", Source::Idp)),
        ChangeRequest::Edit {
            before: with("a", "Controller", Source::Idp),
            after: with("a", "Controller", Source::Idp),
        },
    ] {
        let err = authorise_change(&request, Actor::Source(Source::Manual), &m).unwrap_err();
        assert_eq!(err.code, codes::IDP_MANAGED);
        assert!(err.message.contains("Controller"), "{err}");
        assert!(err.message.contains("Entra ID"), "{err}");
    }
    // The IdP itself, and a role nobody manages, are fine.
    assert!(
        authorise_change(
            &ChangeRequest::End(with("a", "Controller", Source::Idp)),
            Actor::Source(Source::Idp),
            &m
        )
        .is_ok()
    );
    assert!(
        authorise_change(
            &ChangeRequest::Add(with("a", "Manager", Source::Manual)),
            Actor::Source(Source::Manual),
            &m
        )
        .is_ok()
    );
}

#[test]
fn a_mapping_declares_its_roles_idp_managed() {
    let map = GroupRoleMap::load(
        vec![GroupRole {
            group: "fin".into(),
            role: RoleName::from("Controller"),
            scope: vec![],
        }],
        &roles(),
    )
    .unwrap();
    let m = map.managed_roles("Entra ID");
    assert_eq!(m.provider(&RoleName::from("Controller")), Some("Entra ID"));
}

#[test]
fn the_same_role_from_two_sources_is_two_assignments_and_ends_only_when_all_end() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut manual = Assignment::new("m1", "ben", "Accountant");
    manual.source = Source::Manual;
    let state = vec![manual];
    let state = settle(
        &plan_rules_guarded(
            &set,
            &mut ids,
            &state,
            &users(),
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &state,
    );
    let ben: Vec<&Assignment> = state.iter().filter(|a| a.user == "ben").collect();
    assert_eq!(ben.len(), 2);
    // The condition ends: only the rule one goes; the manual one keeps the role.
    let mut all = users();
    all[1].1 = spot_iam_assign::Facts::new().text("department", "Legal");
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &all,
        common::later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert!(plan.changes.iter().all(|c| c.id() != "m1"));
    let state = settle(&plan, &state);
    assert_eq!(live_roles(&state, "ben", common::later(1)), ["Accountant"]);
}

#[test]
fn the_reconcilers_never_touch_another_sources_assignments() {
    let mut ids = counting_ids();
    let set = rule_set();
    let mut idp_given = Assignment::new("i1", "ben", "Controller");
    idp_given.source = Source::Idp;
    idp_given.reason = Some("idp:fin".into());
    let mut emergency = Assignment::new("e1", "ben", "Break Glass");
    emergency.source = Source::Emergency;
    let state = vec![idp_given, emergency];
    // Rules plan: none of the foreign assignments is ended.
    let plan = plan_rules_guarded(
        &set,
        &mut ids,
        &state,
        &users(),
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert!(
        plan.changes
            .iter()
            .all(|c| c.id() != "i1" && c.id() != "e1")
    );
    // Idp plan with a complete, empty listing ends the idp one only.
    let map = GroupRoleMap::load(vec![], &roles()).unwrap();
    let plan = plan_idp_guarded(
        &map,
        &mut ids,
        &[],
        true,
        &state,
        T0,
        &spot_iam_assign::OutsideGuard::none(),
    );
    let ids: Vec<&str> = plan.changes.iter().map(|c| c.id()).collect();
    assert_eq!(ids, ["i1"]);
}
