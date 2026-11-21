//! Local users and groups: create, disable, reset a password (never returned), membership
//! and the protected-account guard.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{boolean, run, succeeded, text, text_list};
use spot_agent_windows::catalogue::{
    GROUP_ADD_MEMBER, GROUP_LIST, GROUP_REMOVE_MEMBER, USER_CREATE, USER_DISABLE, USER_LIST,
    USER_RESET_PASSWORD, USER_SET_GROUPS,
};
use spot_agent_windows::{FakeWin, WinApi};

#[test]
fn creating_a_user_sets_and_delivers_a_password() {
    let api = Arc::new(FakeWin::with_stock_users());
    let channel = Arc::new(common::RecordingChannel::default());
    let registry = common::registry(api.clone(), channel.clone());
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        USER_CREATE,
        vec![
            ("user_name", text("ana2")),
            ("groups", text_list(&["Users"])),
        ]
    )));
    assert!(api.recorded_password_len("ana2").is_some());
    let delivered = channel.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, "ana2");
    assert!(delivered[0].1 >= 20, "a real password length is delivered");
}

#[test]
fn creating_the_same_user_twice_delivers_one_password() {
    let api = Arc::new(FakeWin::with_stock_users());
    let channel = Arc::new(common::RecordingChannel::default());
    let registry = common::registry(api.clone(), channel.clone());
    let catalogue = common::catalogue();
    for _ in 0..2 {
        assert!(succeeded(&run(
            &registry,
            &catalogue,
            USER_CREATE,
            vec![("user_name", text("ana3"))]
        )));
    }
    assert_eq!(channel.delivered().len(), 1);
}

#[test]
fn disabling_a_protected_account_needs_the_flag() {
    let api = Arc::new(FakeWin::with_stock_users());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    match run(
        &registry,
        &catalogue,
        USER_DISABLE,
        vec![("user_name", text("Administrator"))],
    ) {
        spot_agent_core::HandlerOutcome::Failed { detail } => {
            assert!(detail.contains("agent_windows.user_protected"), "{detail}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(api.local_user("Administrator").unwrap().enabled);
}

#[test]
fn disabling_a_normal_account_works_and_is_idempotent() {
    let api = Arc::new(FakeWin::with_stock_users());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    for _ in 0..2 {
        assert!(succeeded(&run(
            &registry,
            &catalogue,
            USER_DISABLE,
            vec![("user_name", text("ana"))]
        )));
    }
    assert!(!api.local_user("ana").unwrap().enabled);
}

#[test]
fn resetting_a_password_delivers_a_new_secret() {
    let api = Arc::new(FakeWin::with_stock_users());
    let channel = Arc::new(common::RecordingChannel::default());
    let registry = common::registry(api.clone(), channel.clone());
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        USER_RESET_PASSWORD,
        vec![("user_name", text("ana"))]
    )));
    let delivered = channel.delivered();
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].0, "ana");
    assert!(api.recorded_password_len("ana").is_some());
}

#[test]
fn setting_groups_honours_the_allow_list() {
    let api = Arc::new(FakeWin::with_stock_users());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    // Adding ana to the protected Administrators group needs the flag.
    match run(
        &registry,
        &catalogue,
        USER_SET_GROUPS,
        vec![
            ("user_name", text("ana")),
            ("groups", text_list(&["Administrators"])),
        ],
    ) {
        spot_agent_core::HandlerOutcome::Failed { detail } => {
            assert!(detail.contains("agent_windows.user_protected"), "{detail}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        USER_SET_GROUPS,
        vec![
            ("user_name", text("ana")),
            ("groups", text_list(&["Administrators"])),
            ("allow_protected", boolean(true)),
        ]
    )));
    assert_eq!(
        api.local_user("ana").unwrap().groups,
        vec!["Administrators"]
    );
}

#[test]
fn group_membership_round_trips() {
    let api = Arc::new(FakeWin::with_stock_users());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        GROUP_ADD_MEMBER,
        vec![
            ("group", text("Administrators")),
            ("user_name", text("ana")),
            ("allow_protected", boolean(true)),
        ]
    )));
    assert!(
        api.list_local_groups()
            .unwrap()
            .iter()
            .find(|g| g.name == "Administrators")
            .unwrap()
            .members
            .iter()
            .any(|m| m == "ana")
    );
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        GROUP_REMOVE_MEMBER,
        vec![
            ("group", text("Administrators")),
            ("user_name", text("ana")),
            ("allow_protected", boolean(true)),
        ]
    )));
    assert!(
        !api.list_local_groups()
            .unwrap()
            .iter()
            .find(|g| g.name == "Administrators")
            .unwrap()
            .members
            .iter()
            .any(|m| m == "ana")
    );
}

#[test]
fn list_reads_users_and_groups() {
    let api = Arc::new(FakeWin::with_stock_users());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(&registry, &catalogue, USER_LIST, vec![])));
    assert!(succeeded(&run(&registry, &catalogue, GROUP_LIST, vec![])));
}
