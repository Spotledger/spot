//! Windows Update: install by id or severity, the reboot-required report and the
//! scheduled reboot — and that the module never reboots on its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{integer, run, succeeded, text, text_list};
use spot_agent_windows::catalogue::{
    UPDATE_INSTALL, UPDATE_PENDING, UPDATE_REBOOT_STATUS, UPDATE_SCHEDULE_REBOOT, UPDATE_SEARCH,
};
use spot_agent_windows::{FakeWin, WinApi};

#[test]
fn install_by_severity_installs_only_that_severity() {
    let api = Arc::new(FakeWin::with_stock_updates());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        UPDATE_INSTALL,
        vec![("severity", text("critical"))]
    )));
    let updates = api.search_updates(true).unwrap();
    let critical = updates.iter().find(|u| u.id == "u-critical").unwrap();
    assert!(critical.installed);
    let moderate = updates.iter().find(|u| u.id == "u-moderate").unwrap();
    assert!(
        !moderate.installed,
        "only the selected severity is installed"
    );
    // Installing a reboot-requiring update reports the flag but never reboots.
    assert!(api.reboot_required().unwrap());
    assert_eq!(api.scheduled_reboot().unwrap(), None);
}

#[test]
fn install_by_id_installs_exactly_that_update() {
    let api = Arc::new(FakeWin::with_stock_updates());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        UPDATE_INSTALL,
        vec![("ids", text_list(&["u-moderate"]))]
    )));
    let updates = api.search_updates(true).unwrap();
    assert!(
        updates
            .iter()
            .find(|u| u.id == "u-moderate")
            .unwrap()
            .installed
    );
    assert!(
        !updates
            .iter()
            .find(|u| u.id == "u-critical")
            .unwrap()
            .installed
    );
}

#[test]
fn an_empty_or_ambiguous_selection_is_refused() {
    let api = Arc::new(FakeWin::with_stock_updates());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    for params in [
        vec![],
        vec![
            ("severity", text("critical")),
            ("ids", text_list(&["u-critical"])),
        ],
    ] {
        match run(&registry, &catalogue, UPDATE_INSTALL, params) {
            spot_agent_core::HandlerOutcome::Failed { detail } => {
                assert!(
                    detail.contains("agent_windows.update_selection"),
                    "{detail}"
                );
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }
}

#[test]
fn install_twice_is_idempotent() {
    let api = Arc::new(FakeWin::with_stock_updates());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    for _ in 0..2 {
        assert!(succeeded(&run(
            &registry,
            &catalogue,
            UPDATE_INSTALL,
            vec![("severity", text("moderate"))]
        )));
    }
    assert!(
        api.search_updates(true)
            .unwrap()
            .iter()
            .find(|u| u.id == "u-moderate")
            .unwrap()
            .installed
    );
}

#[test]
fn a_scheduled_reboot_uses_an_absolute_instant() {
    let api = Arc::new(FakeWin::with_stock_updates());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        UPDATE_SCHEDULE_REBOOT,
        vec![("at", integer(2_000_000_000))]
    )));
    assert_eq!(api.scheduled_reboot().unwrap(), Some(2_000_000_000));
    // A non-positive instant is refused.
    match run(
        &registry,
        &catalogue,
        UPDATE_SCHEDULE_REBOOT,
        vec![("at", integer(0))],
    ) {
        spot_agent_core::HandlerOutcome::Failed { detail } => {
            assert!(
                detail.contains("agent_windows.reboot_time_invalid"),
                "{detail}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn search_pending_and_reboot_status_read_the_agent() {
    let api = Arc::new(FakeWin::with_stock_updates());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        UPDATE_SEARCH,
        vec![("include_hidden", common::boolean(true))]
    )));
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        UPDATE_PENDING,
        vec![]
    )));
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        UPDATE_REBOOT_STATUS,
        vec![]
    )));
}
