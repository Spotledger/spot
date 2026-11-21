//! Real Service Control Manager conformance (WP-ADM-05.T3, `services` in `matrix.toml`).
//!
//! The `WinApi` seam deliberately has no create/delete-service call — the agent never
//! installs software — so the harness creates its throw-away service with `sc.exe` and
//! then exercises the module's read, start-type, recovery and control operations against
//! it through the real `#[cfg(windows)]` implementation. The service carries this run's
//! prefix and is deleted on every path.

use spot_agent_windows::api::{RecoveryAction, RecoveryConfig, ServiceState, StartType};
use spot_agent_windows::catalogue::{
    SERVICE_SET_RECOVERY, SERVICE_SET_START_TYPE, SERVICE_STOP, SERVICE_STATUS,
};
use spot_agent_windows::{Services, WindowsError};
use spot_agent_windows_real::{Cleanup, args, assert_ours, config, flag, name, prefix, sc, text};

/// The placeholder service binary: it exits at once and never connects to the Service
/// Control Manager, so a start attempt exercises the real start path and its timeout
/// without leaking a process or touching any real service.
const PLACEHOLDER: &str = r"C:\Windows\System32\cmd.exe /c exit 0";

#[test]
fn throwaway_service_lifecycle() {
    let service_name = name("svc");
    let api = spot_agent_windows_real::api();

    // Never touch an existing object: this run's name must be free.
    match api.service_status(&service_name) {
        Err(WindowsError::UnknownService { .. }) => {}
        other => panic!("the throw-away service {service_name:?} already exists: {other:?}"),
    }

    let created = sc(&[
        "create",
        &service_name,
        "binPath=",
        PLACEHOLDER,
        "start=",
        "demand",
        "DisplayName=",
        &service_name,
    ]);
    assert!(
        created.status.success(),
        "sc create failed: {}",
        String::from_utf8_lossy(&created.stderr)
    );

    let mut cleanup = Cleanup::new();
    {
        let doomed = service_name.clone();
        cleanup.push(move || {
            assert_ours(&doomed);
            let _ = sc(&["delete", &doomed]);
        });
    }

    // Read through the real SCM.
    let status = api
        .service_status(&service_name)
        .expect("the new service must be readable");
    assert_eq!(status.name, service_name);
    assert_eq!(status.state, ServiceState::Stopped);
    assert_eq!(status.start_type, StartType::Manual);

    let services = Services::new(api.clone(), config());

    // The module lists it and sorts by name.
    let listed = services.list().expect("listing the SCM must succeed");
    assert!(listed.iter().any(|service| service.name == service_name));
    assert!(listed.windows(2).all(|pair| pair[0].name <= pair[1].name));

    // Start type: demand -> automatic -> disabled -> demand, each verified by a read.
    for wanted in ["automatic", "disabled", "manual"] {
        services
            .execute(&args(
                SERVICE_SET_START_TYPE,
                vec![("name", text(&service_name)), ("start_type", text(wanted))],
            ))
            .expect("setting the start type must succeed");
        let expected = match wanted {
            "automatic" => StartType::Automatic,
            "disabled" => StartType::Disabled,
            _ => StartType::Manual,
        };
        assert_eq!(
            api.service_status(&service_name)
                .expect("reading the start type")
                .start_type,
            expected
        );
    }

    // Recovery policy round-trips through the real SCM.
    let recovery = RecoveryConfig {
        actions: vec![
            RecoveryAction::Restart,
            RecoveryAction::Restart,
            RecoveryAction::None,
        ],
        restart_delay_seconds: 30,
        reset_period_seconds: 3600,
    };
    api.set_service_recovery(&service_name, &recovery)
        .expect("setting recovery must succeed");
    let read_back = api
        .service_recovery(&service_name)
        .expect("reading recovery must succeed");
    assert_eq!(read_back.actions, recovery.actions);
    assert_eq!(read_back.restart_delay_seconds, recovery.restart_delay_seconds);
    assert_eq!(read_back.reset_period_seconds, recovery.reset_period_seconds);

    // The real start path: the placeholder never connects to the SCM, so the call must
    // return the typed timeout rather than hanging or panicking.
    let started = api.start_service(&service_name, 5);
    assert!(
        matches!(started, Err(WindowsError::ServiceTimeout { .. })),
        "a placeholder service host must time out with a typed error: {started:?}"
    );

    // The protected-service guard is a pure function; it refuses the agent's own service
    // without the critical flag and never touches the machine.
    let refused = services
        .plan(&args(SERVICE_STOP, vec![("name", text("spot-runtime"))]))
        .expect_err("stopping the runtime without the flag must be refused");
    assert_eq!(refused.code(), spot_agent_windows::error::SERVICE_PROTECTED);
    assert!(
        services
            .plan(&args(
                SERVICE_STOP,
                vec![("name", text("spot-runtime")), ("allow_critical", flag(true))],
            ))
            .is_ok()
    );

    // Verification reads the real machine.
    let check = services
        .verify(&args(
            SERVICE_STATUS,
            vec![("name", text(&service_name))],
        ))
        .expect("verifying a service status must succeed");
    assert!(check.satisfied);

    // Only our own prefixed service may exist: the suite touched nothing else.
    let ours: Vec<String> = api
        .list_services()
        .expect("enumerating services must succeed")
        .into_iter()
        .filter(|service| service.name.starts_with(&prefix()))
        .map(|service| service.name)
        .collect();
    assert_eq!(ours, vec![service_name.clone()]);

    // Delete now; the cleanup guard repeats it harmlessly if this is reached on a panic.
    let deleted = sc(&["delete", &service_name]);
    assert!(
        deleted.status.success(),
        "sc delete failed: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
}

#[test]
fn an_unknown_service_is_a_typed_error() {
    let api = spot_agent_windows_real::api();
    let missing = name("absent");
    assert!(matches!(
        api.service_status(&missing),
        Err(WindowsError::UnknownService { .. })
    ));
    assert!(!missing.is_empty());
}
