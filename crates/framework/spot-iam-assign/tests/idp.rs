//! IdP-sourced assignments from group lists.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{T0, counting_ids, later, live_roles, roles, settle};
use spot_iam_assign::{
    Assignment, EndCause, EventPayload, GroupRole, GroupRoleMap, RoleName, Source,
    StaticRestriction, plan_idp_guarded,
};

fn map() -> GroupRoleMap {
    GroupRoleMap::load(
        vec![
            GroupRole {
                group: "finance".into(),
                role: RoleName::from("Accountant"),
                scope: vec![],
            },
            GroupRole {
                group: "controllers-c1".into(),
                role: RoleName::from("Controller"),
                scope: vec![StaticRestriction {
                    doctype: None,
                    link: "Company".into(),
                    allowed: vec!["C1".into()],
                    include_descendants: false,
                }],
            },
        ],
        &roles(),
    )
    .unwrap()
}

fn groups(list: &[&str]) -> Vec<String> {
    list.iter().map(|g| (*g).to_owned()).collect()
}

#[test]
fn groups_become_idp_assignments_and_removal_ends_them() {
    let mut ids = counting_ids();
    let m = map();
    let listing = vec![(
        "ben".to_owned(),
        groups(&["finance", "controllers-c1", "unmapped"]),
    )];
    let state = settle(
        &plan_idp_guarded(
            &m,
            &mut ids,
            &listing,
            false,
            &[],
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    assert_eq!(live_roles(&state, "ben", T0), ["Accountant", "Controller"]);
    let c = state
        .iter()
        .find(|a| a.role.as_str() == "Controller")
        .unwrap();
    assert_eq!(c.source, Source::Idp);
    assert_eq!(c.reason.as_deref(), Some("idp:controllers-c1"));
    assert_eq!(c.scope.restrictions[0].allowed, ["C1"]);

    let listing = vec![("ben".to_owned(), groups(&["finance"]))];
    let plan = plan_idp_guarded(
        &m,
        &mut ids,
        &listing,
        false,
        &state,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(plan.changes.len(), 1);
    assert!(matches!(
        &plan.events[0],
        EventPayload::Ended {
            cause: EndCause::IdpRemoved,
            source: Source::Idp,
            ..
        }
    ));
    let state = settle(&plan, &state);
    assert_eq!(live_roles(&state, "ben", later(1)), ["Accountant"]);
}

#[test]
fn an_incomplete_listing_ends_nothing_for_absent_users() {
    let mut ids = counting_ids();
    let m = map();
    let listing = vec![("ben".to_owned(), groups(&["finance"]))];
    let state = settle(
        &plan_idp_guarded(
            &m,
            &mut ids,
            &listing,
            false,
            &[],
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    // Ben is not in the next (partial) listing at all.
    let plan = plan_idp_guarded(
        &m,
        &mut ids,
        &[],
        false,
        &state,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert!(plan.changes.is_empty());
    // The same, but the caller says the listing was complete: he has no groups.
    let plan = plan_idp_guarded(
        &m,
        &mut ids,
        &[],
        true,
        &state,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(plan.changes.len(), 1);
}

#[test]
fn a_present_user_with_no_groups_loses_their_idp_roles_even_when_incomplete() {
    let mut ids = counting_ids();
    let m = map();
    let listing = vec![("ben".to_owned(), groups(&["finance"]))];
    let state = settle(
        &plan_idp_guarded(
            &m,
            &mut ids,
            &listing,
            false,
            &[],
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    let listing = vec![("ben".to_owned(), vec![])];
    let plan = plan_idp_guarded(
        &m,
        &mut ids,
        &listing,
        false,
        &state,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert_eq!(plan.changes.len(), 1);
}

#[test]
fn a_manual_assignment_of_the_same_role_survives_the_idp_removal() {
    let mut ids = counting_ids();
    let m = map();
    let mut manual = Assignment::new("m1", "ben", "Accountant");
    manual.source = Source::Manual;
    let listing = vec![("ben".to_owned(), groups(&["finance"]))];
    let state = settle(
        &plan_idp_guarded(
            &m,
            &mut ids,
            &listing,
            false,
            std::slice::from_ref(&manual),
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[manual],
    );
    assert_eq!(state.len(), 2);
    let plan = plan_idp_guarded(
        &m,
        &mut ids,
        &[("ben".to_owned(), vec![])],
        false,
        &state,
        later(1),
        &spot_iam_assign::OutsideGuard::none(),
    );
    assert!(plan.changes.iter().all(|c| c.id() != "m1"));
    let state = settle(&plan, &state);
    assert_eq!(live_roles(&state, "ben", later(1)), ["Accountant"]);
}

#[test]
fn idp_planning_is_idempotent() {
    let mut ids = counting_ids();
    let m = map();
    let listing = vec![
        ("ben".to_owned(), groups(&["finance", "controllers-c1"])),
        ("cy".to_owned(), groups(&["finance"])),
    ];
    let state = settle(
        &plan_idp_guarded(
            &m,
            &mut ids,
            &listing,
            true,
            &[],
            T0,
            &spot_iam_assign::OutsideGuard::none(),
        ),
        &[],
    );
    assert!(
        plan_idp_guarded(
            &m,
            &mut ids,
            &listing,
            true,
            &state,
            later(1),
            &spot_iam_assign::OutsideGuard::none()
        )
        .is_empty()
    );
}
