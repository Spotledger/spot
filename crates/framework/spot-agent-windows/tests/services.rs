//! Services: start/stop/restart, start type, recovery and the protected-service guard,
//! driven through the handler registry exactly as the executor would.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{boolean, duration, run, succeeded, text};
use spot_agent_windows::catalogue::{
    SERVICE_LIST, SERVICE_RESTART, SERVICE_SET_RECOVERY, SERVICE_SET_START_TYPE, SERVICE_START,
    SERVICE_STATUS, SERVICE_STOP,
};
use spot_agent_windows::{FakeWin, ServiceState, StartType, WinApi};

#[test]
fn start_stop_and_restart_move_the_state() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();

    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_START,
        vec![("name", text("spooler"))]
    )));
    assert_eq!(
        api.service_status("spooler").unwrap().state,
        ServiceState::Running
    );

    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_RESTART,
        vec![("name", text("spooler"))]
    )));
    assert_eq!(
        api.service_status("spooler").unwrap().state,
        ServiceState::Running
    );

    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_STOP,
        vec![("name", text("spooler"))]
    )));
    assert_eq!(
        api.service_status("spooler").unwrap().state,
        ServiceState::Stopped
    );
}

#[test]
fn starting_an_already_running_service_is_idempotent() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    // wuauserv starts running in the stock fake, so both runs change nothing.
    for _ in 0..2 {
        assert!(succeeded(&run(
            &registry,
            &catalogue,
            SERVICE_START,
            vec![("name", text("wuauserv"))]
        )));
    }
    assert_eq!(
        api.service_status("wuauserv").unwrap().state,
        ServiceState::Running
    );
}

#[test]
fn stopping_a_protected_service_is_refused_without_the_flag() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    let outcome = run(
        &registry,
        &catalogue,
        SERVICE_STOP,
        vec![("name", text("spot-runtime"))],
    );
    match outcome {
        spot_agent_core::HandlerOutcome::Failed { detail } => {
            assert!(
                detail.contains("agent_windows.service_protected"),
                "{detail}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // The service is untouched.
    assert_eq!(
        api.service_status("spot-runtime").unwrap().state,
        ServiceState::Running
    );
}

#[test]
fn a_protected_service_stops_with_the_flag() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_STOP,
        vec![
            ("name", text("spot-runtime")),
            ("allow_critical", boolean(true)),
        ]
    )));
    assert_eq!(
        api.service_status("spot-runtime").unwrap().state,
        ServiceState::Stopped
    );
}

#[test]
fn set_start_type_is_verified() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_SET_START_TYPE,
        vec![("name", text("spooler")), ("start_type", text("disabled"))]
    )));
    assert_eq!(
        api.service_status("spooler").unwrap().start_type,
        StartType::Disabled
    );
    // The second run is a no-op that still verifies.
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_SET_START_TYPE,
        vec![("name", text("spooler")), ("start_type", text("disabled"))]
    )));
}

#[test]
fn set_recovery_is_verified() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_SET_RECOVERY,
        vec![
            ("name", text("wuauserv")),
            ("first_action", text("restart")),
            ("second_action", text("reboot")),
            ("third_action", text("none")),
            ("restart_delay", duration(30)),
            ("reset_period", duration(3600)),
        ]
    )));
    let recovery = api.service_recovery("wuauserv").unwrap();
    assert_eq!(recovery.reset_period_seconds, 3600);
    assert_eq!(recovery.restart_delay_seconds, 30);
}

#[test]
fn list_and_status_read_the_machine() {
    let api = Arc::new(FakeWin::with_stock_services());
    let registry = common::registry(api.clone(), Arc::new(common::RecordingChannel::default()));
    let catalogue = common::catalogue();
    assert!(succeeded(&run(&registry, &catalogue, SERVICE_LIST, vec![])));
    assert!(succeeded(&run(
        &registry,
        &catalogue,
        SERVICE_STATUS,
        vec![("name", text("wuauserv"))]
    )));
    // An unknown service fails with the typed code.
    let outcome = run(
        &registry,
        &catalogue,
        SERVICE_STATUS,
        vec![("name", text("nothing-here"))],
    );
    match outcome {
        spot_agent_core::HandlerOutcome::Failed { detail } => {
            assert!(detail.contains("agent_windows.unknown_service"), "{detail}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
}
