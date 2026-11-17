//! WP-ADM-05.T6 tests. The pure parts (definitions, `sc.exe` command lists, idempotent
//! installation, the binary-path safety check, the control state machine, the grammar) run
//! on Linux; the SCM test is `#[cfg(windows)]` and runs on the Windows runner/VM of the
//! self-hosted CI (installs, starts, stops and uninstalls the service). The PR must state
//! the runner used.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use spot_agent::winsvc::{
    ControlEvent, DRAIN_DEADLINE, Existing, PRIV_SERVICE_NAME, PathRefused, RecoveryAction,
    SERVICE_ACCOUNT, SERVICE_NAME, ServiceCommand, ServiceDef, State, check_binary_path,
    failure_actions, on_control, parse_sc_qc, parse_service_args, plan_install, plan_uninstall,
};

const BIN: &str = r"C:\Program Files\Spotledger\Agent\spot-agent.exe";

#[test]
fn the_agent_service_definition_follows_the_brief() {
    let def = ServiceDef::agent(BIN);
    assert_eq!(def.name, "SpotledgerAgent");
    assert_eq!(def.account, r"NT SERVICE\SpotledgerAgent");
    assert_eq!(SERVICE_ACCOUNT, def.account);
    assert!(def.delayed_auto_start);
    assert_eq!(
        def.recovery,
        vec![
            RecoveryAction::Restart(Duration::from_secs(5)),
            RecoveryAction::Restart(Duration::from_secs(30)),
            RecoveryAction::Restart(Duration::from_secs(60)),
        ]
    );
    assert_eq!(
        failure_actions(&def),
        "restart/5000/restart/30000/restart/60000"
    );
    assert_eq!(def.bin_path_value(), format!("\"{BIN}\" service run"));
    let helper = ServiceDef::helper(BIN);
    assert_eq!(
        (helper.name, helper.account),
        (PRIV_SERVICE_NAME, "LocalSystem")
    );
    assert!(helper.bin_path_value().ends_with("service run --helper"));
}

#[test]
fn installation_is_silent_argv_only_and_idempotent() {
    let def = ServiceDef::agent(BIN);
    let fresh = plan_install(None, &def);
    assert_eq!(fresh.len(), 4);
    assert_eq!(fresh[0][0], "create");
    assert!(fresh[0].windows(2).any(|w| w == ["start=", "delayed-auto"]));
    assert!(
        fresh[0]
            .windows(2)
            .any(|w| w == ["obj=", r"NT SERVICE\SpotledgerAgent"])
    );
    assert_eq!(fresh[1][0], "description");
    assert_eq!(fresh[2][0], "failure");
    assert!(fresh[2].contains(&"restart/5000/restart/30000/restart/60000".to_owned()));
    // No shell: no command contains `cmd`, `&`, or a pipe.
    for command in &fresh {
        assert!(
            command
                .iter()
                .all(|a| !a.contains('&') && !a.contains('|') && a != "cmd" && a != "/c")
        );
    }
    // A second install of the same definition is a no-op.
    let existing = Existing {
        bin_path: def.bin_path_value(),
        account: def.account.to_owned(),
        delayed_auto_start: true,
    };
    assert!(plan_install(Some(&existing), &def).is_empty());
    // A changed binary path updates the service instead of creating it.
    let moved = ServiceDef::agent(r"C:\Program Files\Spotledger\Agent2\spot-agent.exe");
    let update = plan_install(Some(&existing), &moved);
    assert_eq!(update[0][0], "config");
    assert!(update[0].contains(&moved.bin_path_value()));
    // Uninstall: nothing when absent, stop then delete when present.
    assert!(plan_uninstall(None, SERVICE_NAME).is_empty());
    let removal = plan_uninstall(Some(&existing), SERVICE_NAME);
    assert_eq!(
        removal,
        vec![
            vec!["stop".to_owned(), SERVICE_NAME.to_owned()],
            vec!["delete".to_owned(), SERVICE_NAME.to_owned()]
        ]
    );
}

#[test]
fn sc_qc_output_is_parsed() {
    let out = "[SC] QueryServiceConfig SUCCESS\n\nSERVICE_NAME: SpotledgerAgent\n        TYPE               : 10  WIN32_OWN_PROCESS\n        START_TYPE         : 2   AUTO_START  (DELAYED)\n        ERROR_CONTROL      : 1   NORMAL\n        BINARY_PATH_NAME   : \"C:\\Program Files\\Spotledger\\Agent\\spot-agent.exe\" service run\n        SERVICE_START_NAME : NT SERVICE\\SpotledgerAgent\n";
    let e = parse_sc_qc(out).unwrap();
    assert_eq!(e.bin_path, format!("\"{BIN}\" service run"));
    assert_eq!(e.account, r"NT SERVICE\SpotledgerAgent");
    assert!(e.delayed_auto_start);
    assert_eq!(
        plan_install(Some(&e), &ServiceDef::agent(BIN)),
        Vec::<Vec<String>>::new()
    );
    assert!(parse_sc_qc("[SC] OpenService FAILED 1060").is_none());
}

#[test]
fn a_binary_in_a_user_writable_directory_is_refused() {
    assert_eq!(check_binary_path(BIN), Ok(()));
    assert_eq!(
        check_binary_path(r"D:\Program Files (x86)\Spotledger\spot-agent.exe"),
        Ok(())
    );
    assert_eq!(
        check_binary_path(r"C:\Windows\System32\spot-agent.exe"),
        Ok(())
    );
    for bad in [
        r"C:\Users\alice\Downloads\spot-agent.exe",
        r"C:\Users\bob\Desktop\spot-agent.exe",
        r"C:\ProgramData\Spotledger\spot-agent.exe",
        r"C:\Temp\spot-agent.exe",
        r"C:\Program Files\Spotledger\AppData\x.exe",
        r"C:\tools\spot-agent.exe",
        r"E:\spot-agent.exe",
    ] {
        assert_eq!(
            check_binary_path(bad),
            Err(PathRefused::UserWritable),
            "{bad}"
        );
    }
    assert_eq!(
        check_binary_path("spot-agent.exe"),
        Err(PathRefused::NotAbsolute)
    );
    assert_eq!(
        check_binary_path(r"\\server\share\spot-agent.exe"),
        Err(PathRefused::NotAbsolute)
    );
    assert_eq!(
        check_binary_path(r"C:\Program Files\..\Users\x.exe"),
        Err(PathRefused::Suspicious)
    );
    assert_eq!(
        check_binary_path("C:\\Program Files\\a\"b.exe"),
        Err(PathRefused::Suspicious)
    );
    assert_eq!(
        check_binary_path(r"C:\Program Files\a.exe:stream"),
        Err(PathRefused::Suspicious)
    );
}

#[test]
fn stop_and_shutdown_drain_with_a_deadline() {
    for event in [ControlEvent::Stop, ControlEvent::Shutdown] {
        let r = on_control(State::Running, event);
        assert!(r.trigger_shutdown && r.handled);
        assert_eq!(
            r.state,
            State::StopPending {
                wait_hint_secs: DRAIN_DEADLINE.as_secs() + 5
            }
        );
        // A second stop while draining does not trigger again.
        let again = on_control(r.state, event);
        assert!(!again.trigger_shutdown && again.handled);
    }
    assert_eq!(DRAIN_DEADLINE, Duration::from_secs(30));
    let interrogate = on_control(State::Running, ControlEvent::Interrogate);
    assert_eq!(
        (
            interrogate.state,
            interrogate.trigger_shutdown,
            interrogate.handled
        ),
        (State::Running, false, true)
    );
    assert!(!on_control(State::Running, ControlEvent::Other).handled);
}

#[test]
fn the_service_grammar() {
    let args = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    assert_eq!(
        parse_service_args(&args("install")).unwrap(),
        ServiceCommand::Install { binary: None }
    );
    assert_eq!(
        parse_service_args(&args("install --binary C:\\x.exe")).unwrap(),
        ServiceCommand::Install {
            binary: Some("C:\\x.exe".to_owned())
        }
    );
    assert_eq!(
        parse_service_args(&args("uninstall")).unwrap(),
        ServiceCommand::Uninstall
    );
    assert_eq!(
        parse_service_args(&args("run")).unwrap(),
        ServiceCommand::Run { helper: false }
    );
    assert_eq!(
        parse_service_args(&args("run --helper")).unwrap(),
        ServiceCommand::Run { helper: true }
    );
    assert!(parse_service_args(&args("install --password x")).is_err());
    assert!(parse_service_args(&[]).is_err());
}

#[test]
fn the_helper_pipe_acl_admits_only_the_agent_account() {
    let sddl = spot_agent::privilege::protocol::pipe_sddl("S-1-5-80-1-2-3-4-5").unwrap();
    assert!(
        sddl.starts_with("D:P(D;;GA;;;NU)"),
        "network logons are denied first"
    );
    assert!(sddl.contains("(A;;GRGW;;;S-1-5-80-1-2-3-4-5)"));
    assert!(!sddl.contains(";WD)") && !sddl.contains(";BU)") && !sddl.contains(";AU)"));
}

/// Installs, starts, stops and uninstalls the real service. Needs an elevated Windows
/// runner; skipped (with a message) when not elevated.
#[cfg(windows)]
#[test]
fn the_service_installs_starts_stops_and_uninstalls_on_windows() {
    use std::process::Command;
    let elevated = Command::new("net")
        .arg("session")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !elevated {
        eprintln!("skipped: not elevated");
        return;
    }
    let source = std::env::current_exe().unwrap();
    let target_dir = std::path::PathBuf::from(r"C:\Program Files\SpotledgerAgentTest");
    std::fs::create_dir_all(&target_dir).unwrap();
    let target = target_dir.join("spot-agent.exe");
    std::fs::copy(&source, &target).unwrap();
    let binary = target.display().to_string();
    assert_eq!(spot_agent::winsvc::install(&binary), 0);
    assert_eq!(
        spot_agent::winsvc::install(&binary),
        0,
        "a second install is a no-op"
    );
    let query = Command::new("sc.exe")
        .args(["qc", SERVICE_NAME])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&query.stdout).contains("DELAYED"));
    assert_eq!(spot_agent::winsvc::uninstall(), 0);
    assert_eq!(
        spot_agent::winsvc::install(r"C:\Users\alice\spot-agent.exe"),
        spot_agent::winsvc::EXIT_UNSAFE_PATH
    );
    let _ = std::fs::remove_dir_all(&target_dir);
}
