//! The Windows Firewall: list, add/remove rules, profile state and the lock-out guard.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{boolean, int_list, run, succeeded, text, text_list};
use spot_agent_windows::catalogue::{
    FIREWALL_ADD_RULE, FIREWALL_LIST, FIREWALL_REMOVE_RULE, FIREWALL_SET_PROFILE,
};
use spot_agent_windows::{FakeWin, Profile, WinApi};

#[test]
fn adding_a_rule_then_adding_it_again_is_idempotent() {
    let api = Arc::new(FakeWin::with_stock_firewall());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    let params = || {
        vec![
            ("name", text("App (TCP-In)")),
            ("direction", text("inbound")),
            ("protocol", text("tcp")),
            ("ports", int_list(&[8443])),
            ("profiles", text_list(&["domain", "private", "public"])),
        ]
    };
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_ADD_RULE,
        params()
    )));
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_ADD_RULE,
        params()
    )));
    let rules = api.list_firewall_rules().unwrap();
    assert_eq!(rules.iter().filter(|r| r.name == "App (TCP-In)").count(), 1);
}

#[test]
fn removing_the_rdp_rule_needs_the_flag() {
    let api = Arc::new(FakeWin::with_stock_firewall());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    let outcome = run(
        &registry,
        &catalogue,
        FIREWALL_REMOVE_RULE,
        vec![("name", text("Remote Desktop - User Mode (TCP-In)"))],
    );
    match outcome {
        spot_agent_core::HandlerOutcome::Failed { detail } => {
            assert!(detail.contains("agent_windows.rule_protected"), "{detail}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The rule is still there.
    assert!(
        api.list_firewall_rules()
            .unwrap()
            .iter()
            .any(|r| r.name == "Remote Desktop - User Mode (TCP-In)")
    );
}

#[test]
fn removing_the_rdp_rule_with_the_flag_succeeds() {
    let api = Arc::new(FakeWin::with_stock_firewall());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_REMOVE_RULE,
        vec![
            ("name", text("Remote Desktop - User Mode (TCP-In)")),
            ("allow_critical", boolean(true)),
        ]
    )));
    assert!(
        !api.list_firewall_rules()
            .unwrap()
            .iter()
            .any(|r| r.name == "Remote Desktop - User Mode (TCP-In)")
    );
}

#[test]
fn removing_an_absent_rule_is_a_no_op() {
    let api = Arc::new(FakeWin::with_stock_firewall());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_REMOVE_RULE,
        vec![("name", text("Never There"))]
    )));
}

#[test]
fn profile_state_round_trips() {
    let api = Arc::new(FakeWin::with_stock_firewall());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_SET_PROFILE,
        vec![("profile", text("public")), ("enabled", boolean(false))]
    )));
    assert!(!api.profile_enabled(Profile::Public).unwrap());
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_SET_PROFILE,
        vec![("profile", text("public")), ("enabled", boolean(true))]
    )));
    assert!(api.profile_enabled(Profile::Public).unwrap());
}

#[test]
fn list_reads_the_rules() {
    let api = Arc::new(FakeWin::with_stock_firewall());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_LIST,
        vec![]
    )));
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        FIREWALL_LIST,
        vec![("name", text("Web (HTTP-In)"))]
    )));
}
