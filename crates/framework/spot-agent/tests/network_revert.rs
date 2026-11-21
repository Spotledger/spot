//! WP-ADM-04.T8 tests: the `network-revert <id>` subcommand and the start-up network
//! recovery. The machine is a recording runner and an in-memory filesystem (no real
//! `systemd-run` or `nmcli`); the binary tests use only arguments that are refused before
//! anything is touched.
//!
//! The binary's own module `src/revert.rs` is compiled into this test crate, so the tests
//! drive exactly the code `main` calls.
#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

#[path = "../src/revert.rs"]
mod revert;

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_agent::audit::{LocalAuditLog, verify_file};
use spot_agent::config::LogLevel;
use spot_agent::logging::Logger;
use spot_agent::state::StateDir;
use spot_agent_linux_sys::revert::{BACKUP_FILE, TICKET_FILE};
use spot_agent_linux_sys::testing::{FakeFs, FakeRunner, ok_output};
use spot_agent_linux_sys::{
    Addressing, LinuxSysHandler, NetworkChange, NetworkModule, SystemdRunScheduler,
    revert_command_for,
};
use spot_core::{Clock, ManualClock};
use time::OffsetDateTime;

const ROOT: &str = "/state";
const NET: &str = "/state/network";
const WINDOW: u64 = 30;
const EXE: &str = "/usr/libexec/spot-agent";
const ID: u64 = 1_800_000_000;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Capture {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(i64::try_from(ID).unwrap()).unwrap(),
    ))
}

fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

fn handler(
    runner: &Arc<FakeRunner>,
    fs: &Arc<FakeFs>,
    clock: &Arc<ManualClock>,
) -> LinuxSysHandler {
    let clock: Arc<dyn Clock> = clock.clone();
    revert::build_handler(
        Path::new(ROOT),
        Path::new(EXE),
        runner.clone(),
        fs.clone(),
        clock,
    )
}

/// Applies a static change through a module wired like the handler's (state directory and
/// systemd timer), returning the ticket id.
fn apply(runner: &Arc<FakeRunner>, fs: &Arc<FakeFs>, clock: &Arc<ManualClock>) -> u64 {
    let scheduler =
        SystemdRunScheduler::new(runner.as_ref(), revert_command_for(Path::new(EXE))).unwrap();
    let module = NetworkModule::new(runner.as_ref(), fs.as_ref(), clock.clone())
        .with_state_dir(NET)
        .with_scheduler(&scheduler);
    let change = NetworkChange::new(
        "eth0",
        Addressing::Static {
            address: "10.0.9.9".to_owned(),
            prefix: 24,
            gateway: Some("10.0.9.1".to_owned()),
            dns: vec!["10.0.9.53".to_owned()],
        },
    )
    .unwrap();
    module.apply(&change, WINDOW).unwrap().ticket.id
}

const NM_NEW: &str = "nmcli connection modify eth0 ipv4.method manual ipv4.addresses 10.0.9.9/24 ipv4.gateway 10.0.9.1 ipv4.dns 10.0.9.53";
const NM_OLD: &str = "nmcli connection modify eth0 ipv4.method manual ipv4.addresses 10.0.0.5/24 ipv4.gateway 10.0.0.1 ipv4.dns 10.0.0.1,10.0.0.2";

fn systemd_run(id: u64) -> String {
    format!(
        "systemd-run --unit=spot-net-revert-{id} --on-active={WINDOW}s --timer-property=AccuracySec=1s -- {EXE} network-revert {id}"
    )
}

/// NetworkManager, eth0 statically 10.0.0.5/24 with a gateway and two DNS servers.
fn nm_static_runner(id: u64) -> Arc<FakeRunner> {
    Arc::new(
        FakeRunner::new()
            .with_response(
                "ip -j address show",
                ok_output(
                    r#"[{"ifname":"eth0","operstate":"UP","addr_info":[{"family":"inet","local":"10.0.0.5","prefixlen":24}]}]"#,
                ),
            )
            .with_response("systemctl is-active NetworkManager", ok_output("active\n"))
            .with_response(
                "nmcli -t connection show eth0",
                ok_output(
                    "connection.id:eth0\nipv4.method:manual\nipv4.gateway:10.0.0.1\nipv4.dns:10.0.0.1,10.0.0.2\n",
                ),
            )
            .with_response(NM_NEW, ok_output(""))
            .with_response(NM_OLD, ok_output(""))
            .with_response("nmcli connection up eth0", ok_output(""))
            .with_response(&systemd_run(id), ok_output(""))
            .with_response(
                &format!("systemctl stop spot-net-revert-{id}.timer"),
                ok_output(""),
            ),
    )
}

fn count(runner: &FakeRunner, key: &str) -> usize {
    runner
        .call_keys()
        .iter()
        .filter(|k| k.as_str() == key)
        .count()
}

fn position(runner: &FakeRunner, key: &str) -> Option<usize> {
    runner.call_keys().iter().position(|k| k.as_str() == key)
}

// ----- the subcommand --------------------------------------------------------------------

#[test]
fn the_pending_ticket_reverts_and_exits_zero_then_already_done_then_not_this_ticket() {
    let (runner, fs, clock) = (nm_static_runner(ID), Arc::new(FakeFs::new()), clock());
    assert_eq!(apply(&runner, &fs, &clock), ID);
    let handler = handler(&runner, &fs, &clock);

    // Another id first: nothing changes.
    let calls_before = runner.calls().len();
    let ticket = fs.file(&format!("{NET}/{TICKET_FILE}")).unwrap();
    let other = revert::run_network_revert(&args(&[&(ID + 1).to_string()]), &handler);
    assert_eq!(other.code, 0);
    assert_eq!(other.line, "network revert: not this ticket");
    assert_eq!(runner.calls().len(), calls_before, "no command ran");
    assert_eq!(fs.file(&format!("{NET}/{TICKET_FILE}")).unwrap(), ticket);

    let first = revert::run_network_revert(&args(&[&ID.to_string()]), &handler);
    assert_eq!(first.code, 0, "{}", first.line);
    assert_eq!(first.line, "network revert: reverted");
    assert_eq!(count(&runner, NM_OLD), 1);
    assert!(fs.file(&format!("{NET}/{BACKUP_FILE}")).is_none());

    let again = revert::run_network_revert(&args(&[&ID.to_string()]), &handler);
    assert_eq!(again.code, 0);
    assert_eq!(again.line, "network revert: already done");
    assert_eq!(count(&runner, NM_OLD), 1, "the second run changed nothing");
}

#[test]
fn a_missing_wrong_or_overflowing_id_exits_one_and_runs_nothing() {
    let (runner, fs, clock) = (
        Arc::new(FakeRunner::new()),
        Arc::new(FakeFs::new()),
        clock(),
    );
    let handler = handler(&runner, &fs, &clock);
    for bad in [
        args(&[]),
        args(&["-1"]),
        args(&["1;2"]),
        args(&["0x10"]),
        args(&["99999999999999999999999"]),
        args(&["1", "2"]),
    ] {
        let exit = revert::run_network_revert(&bad, &handler);
        assert_eq!(exit.code, 1, "{bad:?}");
        assert!(
            exit.line.starts_with("network revert failed: "),
            "{}",
            exit.line
        );
        assert!(exit.line.contains("agent_linux_sys."), "{}", exit.line);
    }
    assert!(
        runner.calls().is_empty(),
        "a bad id never reaches the machine"
    );
}

#[test]
fn the_subcommand_needs_no_enrolment_and_an_empty_state_directory() {
    let (runner, fs, clock) = (
        Arc::new(FakeRunner::new()),
        Arc::new(FakeFs::new()),
        clock(),
    );
    // No agent.toml, no identity, no ticket: still a clean "already done".
    assert!(fs.file("/state/agent.toml").is_none());
    let exit = revert::run_network_revert(&args(&["42"]), &handler(&runner, &fs, &clock));
    assert_eq!(exit.code, 0, "{}", exit.line);
    assert_eq!(exit.line, "network revert: already done");
    assert!(runner.calls().is_empty());
}

#[test]
fn the_command_vector_the_timer_runs_is_the_executable_and_the_subcommand() {
    let (runner, fs, clock) = (nm_static_runner(ID), Arc::new(FakeFs::new()), clock());
    apply(&runner, &fs, &clock);
    assert_eq!(count(&runner, &systemd_run(ID)), 1);
    assert_eq!(
        revert_command_for(Path::new(EXE)),
        [EXE, "network-revert"],
        "the scheduler and the binary agree on the subcommand name"
    );
}

// ----- start-up recovery -----------------------------------------------------------------

#[test]
fn a_pending_ticket_with_an_expired_deadline_is_reverted_before_the_first_plan() {
    let (runner, fs, clock) = (nm_static_runner(ID), Arc::new(FakeFs::new()), clock());
    apply(&runner, &fs, &clock);
    clock.advance(Duration::from_secs(WINDOW + 5));

    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path());
    let capture = Capture::default();
    let logger = Logger::new(LogLevel::Info, Box::new(capture.clone()));
    let handler = handler(&runner, &fs, &clock);

    let report = revert::startup_recovery(&handler, &state, None, "", &logger, 1_000);
    assert!(report.ok, "{}", report.summary);
    assert!(report.summary.contains("reverted"), "{}", report.summary);

    // The first plan is accepted only after recovery returned: replay the order. The
    // recorded transcript has the revert, and only then the change of the new plan.
    let revert_at = position(&runner, NM_OLD).expect("the revert ran");
    let calls_after_recovery = runner.calls().len();
    assert!(revert_at < calls_after_recovery);
    assert_eq!(
        count(&runner, NM_NEW),
        1,
        "only the original apply, no new plan yet"
    );
    assert!(position(&runner, NM_NEW).unwrap() < revert_at);

    // The structured log got one event with the summary.
    let log = capture.text();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains("network.recover"), "{log}");
    assert!(log.contains("reverted"), "{log}");
}

#[test]
fn a_second_start_is_idempotent() {
    let (runner, fs, clock) = (nm_static_runner(ID), Arc::new(FakeFs::new()), clock());
    apply(&runner, &fs, &clock);
    clock.advance(Duration::from_secs(WINDOW + 5));
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path());
    let logger = Logger::new(LogLevel::Info, Box::new(Capture::default()));
    let handler = handler(&runner, &fs, &clock);

    let first = revert::startup_recovery(&handler, &state, None, "", &logger, 1_000);
    let second = revert::startup_recovery(&handler, &state, None, "", &logger, 1_001);
    assert!(first.ok && second.ok);
    assert!(
        second.summary.contains("no network change"),
        "{}",
        second.summary
    );
    assert_eq!(count(&runner, NM_OLD), 1, "the revert ran exactly once");
}

#[test]
fn the_recovery_event_reaches_the_audit_log_and_holds_no_secret() {
    let (runner, fs, clock) = (nm_static_runner(ID), Arc::new(FakeFs::new()), clock());
    apply(&runner, &fs, &clock);
    clock.advance(Duration::from_secs(WINDOW + 5));
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path());
    let audit = LocalAuditLog::open(state.audit_path(), "agent-t8").unwrap();
    let capture = Capture::default();
    let logger = Logger::new(LogLevel::Info, Box::new(capture.clone()));

    let report = revert::startup_recovery(
        &handler(&runner, &fs, &clock),
        &state,
        Some(&audit),
        "agent-t8",
        &logger,
        1_000,
    );
    assert!(report.ok);
    assert_eq!(verify_file(&state.audit_path(), "agent-t8").unwrap(), 1);
    let text = std::fs::read_to_string(state.audit_path()).unwrap();
    assert!(text.contains("agent.network_recover"), "{text}");
    assert!(text.contains("recovered"), "{text}");
    for secret_word in ["token", "password", "secret", "BEGIN"] {
        assert!(
            !text.to_lowercase().contains(&secret_word.to_lowercase()),
            "{text}"
        );
        assert!(
            !capture
                .text()
                .to_lowercase()
                .contains(&secret_word.to_lowercase())
        );
    }
}

#[test]
fn a_failing_recovery_is_reported_by_status_until_the_next_success() {
    let (runner, fs, clock) = (nm_static_runner(ID), Arc::new(FakeFs::new()), clock());
    apply(&runner, &fs, &clock);
    clock.advance(Duration::from_secs(WINDOW + 5));
    runner.set_failure(NM_OLD, 1, "boom");
    let dir = tempfile::tempdir().unwrap();
    let state = StateDir::new(dir.path());
    let capture = Capture::default();
    let logger = Logger::new(LogLevel::Info, Box::new(capture.clone()));
    let handler = handler(&runner, &fs, &clock);

    let failed = revert::startup_recovery(&handler, &state, None, "", &logger, 1_000);
    assert!(!failed.ok);
    assert!(
        failed.summary.starts_with("network recovery failed"),
        "{}",
        failed.summary
    );
    assert!(capture.text().contains("warn") || capture.text().contains("WARN"));
    let lines = revert::recovery_status_lines(&state);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("FAILED"), "{lines:?}");

    // The machine is reachable again: the next start reverts, and the marker goes.
    runner.set_response(NM_OLD, ok_output(""));
    let healed = revert::startup_recovery(&handler, &state, None, "", &logger, 2_000);
    assert!(healed.ok, "{}", healed.summary);
    assert!(revert::recovery_status_lines(&state).is_empty());
}

// ----- the binary ------------------------------------------------------------------------

#[cfg(target_os = "linux")]
#[test]
fn the_binary_refuses_bad_arguments_with_exit_one_and_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    for bad in [
        vec!["network-revert"],
        vec!["network-revert", "-1"],
        vec!["network-revert", "1;2"],
        vec!["network-revert", "0x10"],
        vec!["network-revert", "99999999999999999999999"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_spot-agent"))
            .arg("--state-dir")
            .arg(dir.path())
            .args(&bad)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "{bad:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("network revert failed"), "{stderr}");
        assert!(output.stdout.is_empty());
    }
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        0,
        "the subcommand created no state and needed no enrolment"
    );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn off_linux_the_subcommand_is_a_stub_that_exits_one() {
    let exit = revert::network_revert_main(None, &args(&["1"]));
    assert_eq!(exit.code, 1);
    assert_eq!(exit.line, revert::UNSUPPORTED_MESSAGE);
}

#[test]
fn the_unsupported_message_is_the_documented_one() {
    assert_eq!(
        revert::UNSUPPORTED_MESSAGE,
        "network-revert is not supported on this platform"
    );
}
