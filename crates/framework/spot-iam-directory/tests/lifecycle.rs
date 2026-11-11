//! Enable and disable: the state machine and the leaver actions.
#![allow(missing_docs)]

mod common;

use common::{admin_actor, at, id, internal, user};
use spot_events_contract::Actor;
use spot_iam_directory::{LeaverAction as A, LifecycleChange, UserType, codes, normalise};

const ORDER: [&str; 6] = [
    "end_sessions",
    "revoke_refresh_tokens",
    "disable_api_keys",
    "cancel_invitations",
    "reassign_open_tasks",
    "write_audit",
];

fn name(a: &A) -> &'static str {
    match a {
        A::EndSessions => ORDER[0],
        A::RevokeRefreshTokens => ORDER[1],
        A::DisableApiKeys => ORDER[2],
        A::CancelInvitations => ORDER[3],
        A::ReassignOpenTasks { .. } => ORDER[4],
        A::WriteAudit => ORDER[5],
    }
}

#[test]
fn disabling_returns_the_new_state_and_the_actions_in_the_fixed_order() {
    let manager = id();
    let mut u = internal();
    u.manager = Some(manager);
    let t = u
        .disable(&admin_actor(), "left the company", at(100))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(!t.new_state.enabled);
    assert_eq!(t.new_state.deactivated_at, Some(at(100)));
    assert_eq!(t.new_state.id, u.id);
    assert_eq!(
        t.actions.iter().map(name).collect::<Vec<_>>(),
        ORDER.to_vec()
    );
    assert_eq!(
        t.actions[4],
        A::ReassignOpenTasks { to: Some(manager) },
        "open tasks go to the manager"
    );
    let audit = t.audit.unwrap_or_else(|| panic!("an audit note"));
    assert_eq!(audit.change, LifecycleChange::Disabled);
    assert_eq!(audit.subject, u.id);
    assert_eq!(audit.reason, "left the company");
    assert_eq!(audit.at, at(100));
    assert!(t.new_state.validate().is_ok());
}

#[test]
fn a_nominated_delegate_wins_over_the_manager_and_nobody_means_none() {
    let (manager, delegate) = (id(), id());
    let mut u = internal();
    u.manager = Some(manager);
    let t = u
        .disable_to(&admin_actor(), "leaver", at(1), Some(delegate))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(t.actions[4], A::ReassignOpenTasks { to: Some(delegate) });
    let no_manager = internal();
    let t = no_manager
        .disable(&admin_actor(), "leaver", at(1))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(t.actions[4], A::ReassignOpenTasks { to: None });
    let e = u
        .disable_to(&admin_actor(), "leaver", at(1), Some(u.id))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert!(e.has(codes::MANAGER_SELF));
}

#[test]
fn disabling_twice_is_idempotent_and_an_empty_action_list() {
    let u = internal();
    let first = u
        .disable(&admin_actor(), "leaver", at(10))
        .unwrap_or_else(|e| panic!("{e}"));
    let again = first
        .new_state
        .disable(&admin_actor(), "scim retry", at(99))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(again.is_noop());
    assert!(again.audit.is_none());
    assert_eq!(again.new_state, first.new_state, "the first time stays");
}

#[test]
fn enabling_restores_the_user_and_enabling_twice_is_a_noop() {
    let u = internal();
    let off = u
        .disable(&admin_actor(), "leaver", at(10))
        .unwrap_or_else(|e| panic!("{e}"))
        .new_state;
    let on = off
        .enable(&admin_actor(), "came back", at(20))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(on.new_state.enabled);
    assert_eq!(on.new_state.deactivated_at, None);
    assert_eq!(on.actions, vec![A::WriteAudit]);
    assert_eq!(on.audit.map(|a| a.change), Some(LifecycleChange::Enabled));
    let again = on
        .new_state
        .enable(&admin_actor(), "again", at(30))
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(again.is_noop());
}

#[test]
fn a_reason_is_required() {
    for bad in ["", "   ", "\n"] {
        let e = internal()
            .disable(&admin_actor(), bad, at(1))
            .err()
            .unwrap_or_else(|| panic!("`{bad:?}` must fail"));
        assert_eq!(e.codes(), [codes::REASON_REQUIRED]);
    }
    let long = "x".repeat(501);
    assert!(internal().disable(&admin_actor(), &long, at(1)).is_err());
    let off = internal()
        .disable(&admin_actor(), "r", at(1))
        .unwrap_or_else(|e| panic!("{e}"))
        .new_state;
    assert!(off.enable(&admin_actor(), "", at(2)).is_err());
}

#[test]
fn a_user_cannot_be_deactivated_before_they_were_created() {
    let e = internal()
        .disable(&admin_actor(), "r", at(-5))
        .err()
        .unwrap_or_else(|| panic!("must fail"));
    assert_eq!(e.codes(), [codes::INVALID_STATE]);
}

#[test]
fn every_user_type_gets_the_same_leaver_actions() {
    for ty in [
        UserType::Internal,
        UserType::Portal,
        UserType::Service,
        UserType::Agent,
    ] {
        let t = user(ty)
            .disable(
                &Actor::System {
                    component: spot_storage_contract::Name::new("scim")
                        .unwrap_or_else(|e| panic!("{e}")),
                },
                "scim active=false",
                at(1),
            )
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(
            t.actions.iter().map(name).collect::<Vec<_>>(),
            ORDER.to_vec()
        );
    }
}

#[test]
fn the_action_order_is_stable_under_shuffled_input() {
    let all = || {
        vec![
            A::EndSessions,
            A::RevokeRefreshTokens,
            A::DisableApiKeys,
            A::CancelInvitations,
            A::ReassignOpenTasks { to: None },
            A::WriteAudit,
        ]
    };
    let want = all();
    // Every rotation and the reverse, and a doubled list: the same ordered result.
    let n = want.len();
    for shift in 0..n {
        let mut v = all();
        v.rotate_left(shift);
        assert_eq!(normalise(v), want, "rotation {shift}");
    }
    let mut reversed = all();
    reversed.reverse();
    assert_eq!(normalise(reversed), want);
    let mut doubled = all();
    doubled.extend(all());
    assert_eq!(normalise(doubled), want, "repeats are dropped");
}
