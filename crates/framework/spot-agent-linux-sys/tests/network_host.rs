//! The host entry points for the persisted revert (`run_revert`, `recover_network`) and the
//! complete previous configuration (address, gateway and DNS) a static to static revert
//! restores. The machine is a recording runner and an in-memory filesystem; the real host
//! network is never touched.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use spot_agent_linux_sys::error::{INVALID_ARGUMENT, NETWORK_BACKUP_INCOMPLETE};
use spot_agent_linux_sys::revert::{BACKUP_FILE, TICKET_FILE};
use spot_agent_linux_sys::testing::{FakeFs, FakeRunner, ok_output};
use spot_agent_linux_sys::{
    Addressing, LinuxSysHandler, NetworkChange, NetworkModule, Recovery, RevertOutcome,
    SystemdRunScheduler, revert_command_for, run_revert,
};
use spot_core::{Clock, ManualClock};
use time::OffsetDateTime;

const STATE: &str = "/state";
const WINDOW: u64 = 30;
const EXE: &str = "/usr/libexec/spot-agent";

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap(),
    ))
}

fn paths() -> (String, String) {
    (
        format!("{STATE}/{TICKET_FILE}"),
        format!("{STATE}/{BACKUP_FILE}"),
    )
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
    LinuxSysHandler::new(runner.clone(), fs.clone(), clock)
        .with_state_dir(STATE)
        .with_revert_command(revert_command_for(Path::new(EXE)))
}

/// Applies `change` through a module wired like the handler's (state directory and the
/// systemd timer), returning the ticket id.
fn apply(
    runner: &Arc<FakeRunner>,
    fs: &Arc<FakeFs>,
    clock: &Arc<ManualClock>,
    change: &NetworkChange,
) -> u64 {
    let scheduler =
        SystemdRunScheduler::new(runner.as_ref(), revert_command_for(Path::new(EXE))).unwrap();
    let module = NetworkModule::new(runner.as_ref(), fs.as_ref(), clock.clone())
        .with_state_dir(STATE)
        .with_scheduler(&scheduler);
    module.apply(change, WINDOW).unwrap().ticket.id
}

fn static_change(address: &str, gateway: &str, dns: &str) -> NetworkChange {
    NetworkChange::new(
        "eth0",
        Addressing::Static {
            address: address.to_owned(),
            prefix: 24,
            gateway: Some(gateway.to_owned()),
            dns: vec![dns.to_owned()],
        },
    )
    .unwrap()
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
    let runner = FakeRunner::new()
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
        );
    Arc::new(runner)
}

fn count(runner: &FakeRunner, key: &str) -> usize {
    runner
        .call_keys()
        .iter()
        .filter(|k| k.as_str() == key)
        .count()
}

#[test]
fn run_revert_refuses_anything_but_one_decimal_id_and_runs_nothing() {
    let (runner, fs, clock) = (
        Arc::new(FakeRunner::new()),
        Arc::new(FakeFs::new()),
        clock(),
    );
    let handler = handler(&runner, &fs, &clock);
    for bad in [
        args(&[]),
        args(&[""]),
        args(&["-1"]),
        args(&["1;2"]),
        args(&["0x10"]),
        args(&["99999999999999999999999"]),
        args(&["1", "2"]),
    ] {
        let error = run_revert(&bad, &handler).unwrap_err();
        assert_eq!(error.code, INVALID_ARGUMENT, "{bad:?}");
    }
    assert!(
        runner.calls().is_empty(),
        "a bad id never reaches the machine"
    );
}

#[test]
fn an_id_that_is_not_the_pending_ticket_changes_nothing() {
    let id = 1_800_000_000;
    let (runner, fs, clock) = (nm_static_runner(id), Arc::new(FakeFs::new()), clock());
    assert_eq!(
        apply(
            &runner,
            &fs,
            &clock,
            &static_change("10.0.9.9", "10.0.9.1", "10.0.9.53")
        ),
        id
    );
    let calls_before = runner.calls().len();
    let (ticket, backup) = paths();
    let (ticket_text, backup_text) = (fs.file(&ticket).unwrap(), fs.file(&backup).unwrap());

    let outcome = run_revert(
        &args(&[&(id + 1).to_string()]),
        &handler(&runner, &fs, &clock),
    );
    assert_eq!(outcome.unwrap(), RevertOutcome::NotThisTicket);
    assert_eq!(runner.calls().len(), calls_before, "no command ran");
    assert_eq!(fs.file(&ticket).unwrap(), ticket_text);
    assert_eq!(fs.file(&backup).unwrap(), backup_text);
}

#[test]
fn a_static_to_static_revert_restores_address_gateway_and_dns() {
    let id = 1_800_000_000;
    let (runner, fs, clock) = (nm_static_runner(id), Arc::new(FakeFs::new()), clock());
    apply(
        &runner,
        &fs,
        &clock,
        &static_change("10.0.9.9", "10.0.9.1", "10.0.9.53"),
    );
    let (ticket, backup) = paths();
    let saved = fs.file(&backup).unwrap();
    assert!(saved.contains("\"gateway\":\"10.0.0.1\""), "{saved}");
    assert!(
        saved.contains("\"dns\":[\"10.0.0.1\",\"10.0.0.2\"]"),
        "{saved}"
    );
    assert_eq!(count(&runner, NM_NEW), 1);
    assert_eq!(count(&runner, NM_OLD), 0);

    // The timer fires: the host's `network-revert <id>`.
    let handler = handler(&runner, &fs, &clock);
    let outcome = run_revert(&args(&[&id.to_string()]), &handler).unwrap();
    assert_eq!(outcome, RevertOutcome::Reverted);
    assert_eq!(
        count(&runner, NM_OLD),
        1,
        "the old address, gateway and DNS came back"
    );
    assert!(!fs.has_file(&ticket) && !fs.has_file(&backup));

    // Run again (the timer and the in-agent sweep can both fire): nothing more happens.
    let again = run_revert(&args(&[&id.to_string()]), &handler).unwrap();
    assert_eq!(again, RevertOutcome::AlreadyDone);
    assert_eq!(count(&runner, NM_OLD), 1);
}

#[test]
fn a_change_whose_previous_gateway_cannot_be_read_is_refused_before_anything_is_touched() {
    let id = 1_800_000_000;
    for broken in [
        // The connection cannot be shown at all.
        None,
        // It is shown but has no gateway line.
        Some("connection.id:eth0\nipv4.method:manual\nipv4.dns:10.0.0.1\n"),
        // The gateway is not an address.
        Some("connection.id:eth0\nipv4.method:manual\nipv4.gateway:gw.example\n"),
    ] {
        let (runner, fs, clock) = (nm_static_runner(id), Arc::new(FakeFs::new()), clock());
        match broken {
            Some(text) => runner.set_response("nmcli -t connection show eth0", ok_output(text)),
            None => runner.set_failure("nmcli -t connection show eth0", 10, "no such connection"),
        }
        let scheduler =
            SystemdRunScheduler::new(runner.as_ref(), revert_command_for(Path::new(EXE))).unwrap();
        let module = NetworkModule::new(runner.as_ref(), fs.as_ref(), clock.clone())
            .with_state_dir(STATE)
            .with_scheduler(&scheduler);
        let error = module
            .apply(&static_change("10.0.9.9", "10.0.9.1", "10.0.9.53"), WINDOW)
            .unwrap_err();
        assert_eq!(error.code, NETWORK_BACKUP_INCOMPLETE, "{broken:?}");
        let (ticket, backup) = paths();
        assert!(!fs.has_file(&ticket) && !fs.has_file(&backup));
        let keys = runner.call_keys();
        assert!(
            !keys
                .iter()
                .any(|k| k.starts_with("nmcli connection") || k.starts_with("systemd-run")),
            "nothing was changed or scheduled: {keys:?}"
        );
    }
}

#[test]
fn an_address_handed_out_by_dhcp_is_restored_as_dhcp() {
    let id = 1_800_000_000;
    let (runner, fs, clock) = (nm_static_runner(id), Arc::new(FakeFs::new()), clock());
    runner.set_response(
        "nmcli -t connection show eth0",
        ok_output("connection.id:eth0\nipv4.method:auto\nipv4.gateway:\nipv4.dns:\n"),
    );
    let dhcp =
        "nmcli connection modify eth0 ipv4.method auto ipv4.addresses  ipv4.gateway  ipv4.dns ";
    runner.set_response(dhcp, ok_output(""));
    apply(
        &runner,
        &fs,
        &clock,
        &static_change("10.0.9.9", "10.0.9.1", "10.0.9.53"),
    );
    let outcome = run_revert(&args(&[&id.to_string()]), &handler(&runner, &fs, &clock));
    assert_eq!(outcome.unwrap(), RevertOutcome::Reverted);
    assert_eq!(count(&runner, dhcp), 1);
}

#[test]
fn networkd_records_the_previous_gateway_and_dns_and_restores_its_file() {
    let id = 1_800_000_000;
    let file = "/etc/systemd/network/10-spotledger-eth0.network";
    let previous =
        "[Match]\nName=eth0\n\n[Network]\nAddress=10.0.0.5/24\nGateway=10.0.0.1\nDNS=10.0.0.1\n";
    let runner = Arc::new(
        FakeRunner::new()
            .with_response(
                "ip -j address show",
                ok_output(
                    r#"[{"ifname":"eth0","operstate":"UP","addr_info":[{"family":"inet","local":"10.0.0.5","prefixlen":24}]}]"#,
                ),
            )
            .with_response("ip -j route show", ok_output(r#"[{"dst":"default","gateway":"10.0.0.1","dev":"eth0","metric":100}]"#))
            .with_response("resolvectl status", ok_output("Global\n         DNS Servers: 10.0.0.1\n"))
            .with_response("systemctl is-active NetworkManager", ok_output("inactive\n"))
            .with_response("systemctl is-active systemd-networkd", ok_output("active\n"))
            .with_response("networkctl reload", ok_output(""))
            .with_response("networkctl reconfigure eth0", ok_output(""))
            .with_response(&systemd_run(id), ok_output(""))
            .with_response(&format!("systemctl stop spot-net-revert-{id}.timer"), ok_output("")),
    );
    let (fs, clock) = (Arc::new(FakeFs::new().with_file(file, previous)), clock());
    apply(
        &runner,
        &fs,
        &clock,
        &static_change("10.0.9.9", "10.0.9.1", "10.0.9.53"),
    );
    let (_, backup) = paths();
    let saved = fs.file(&backup).unwrap();
    assert!(saved.contains("\"gateway\":\"10.0.0.1\"") && saved.contains("\"dns\":[\"10.0.0.1\"]"));
    assert!(fs.file(file).unwrap().contains("Address=10.0.9.9/24"));

    let outcome = run_revert(&args(&[&id.to_string()]), &handler(&runner, &fs, &clock));
    assert_eq!(outcome.unwrap(), RevertOutcome::Reverted);
    assert_eq!(fs.file(file).as_deref(), Some(previous));

    // Unreadable route facts refuse the next change before the file is touched.
    runner.set_failure("ip -j route show", 1, "boom");
    let scheduler =
        SystemdRunScheduler::new(runner.as_ref(), revert_command_for(Path::new(EXE))).unwrap();
    let module = NetworkModule::new(runner.as_ref(), fs.as_ref(), clock.clone())
        .with_state_dir(STATE)
        .with_scheduler(&scheduler);
    let error = module
        .apply(&static_change("10.0.7.7", "10.0.7.1", "10.0.7.53"), WINDOW)
        .unwrap_err();
    assert_eq!(error.code, NETWORK_BACKUP_INCOMPLETE);
    assert_eq!(fs.file(file).as_deref(), Some(previous));
}

#[test]
fn recover_is_idempotent_and_reports_what_it_did() {
    let id = 1_800_000_000;
    let (runner, fs, clock) = (nm_static_runner(id), Arc::new(FakeFs::new()), clock());
    apply(
        &runner,
        &fs,
        &clock,
        &static_change("10.0.9.9", "10.0.9.1", "10.0.9.53"),
    );
    let handler = handler(&runner, &fs, &clock);
    runner.set_response(
        &systemd_run(id).replace("--on-active=30s", "--on-active=20s"),
        ok_output(""),
    );

    // The agent restarts inside the window: re-armed, and re-arming twice is the same.
    clock.advance(Duration::from_secs(10));
    let first = handler.recover_network().unwrap();
    assert!(
        matches!(
            first,
            Recovery::Rearmed {
                remaining_secs: 20,
                ..
            }
        ),
        "{first:?}"
    );
    assert!(first.summary().contains("re-armed"), "{}", first.summary());
    let again = handler.recover_network().unwrap();
    assert_eq!(again, first);

    // After the deadline: reverted once, then nothing is pending.
    clock.advance(Duration::from_secs(WINDOW));
    let reverted = handler.recover_network().unwrap();
    assert!(matches!(reverted, Recovery::Reverted(_)), "{reverted:?}");
    assert!(reverted.summary().contains("reverted"));
    assert_eq!(handler.recover_network().unwrap(), Recovery::Nothing);
    assert_eq!(count(&runner, NM_OLD), 1, "the revert ran exactly once");
}

/// All three distributions: the recorded transcripts give the previous gateway and DNS,
/// and the persisted backup carries them.
#[test]
fn every_distribution_backs_up_the_gateway_and_dns_it_will_restore() {
    for (distro, gateway, dns) in [
        ("fedora", "192.0.2.1", "\"dns\":[\"192.0.2.1\"]"),
        (
            "rhel",
            "198.51.100.1",
            "\"dns\":[\"198.51.100.1\",\"198.51.100.2\"]",
        ),
        ("ubuntu", "203.0.113.1", "\"dns\":[\"203.0.113.1\"]"),
    ] {
        let runner = common::runner(distro);
        let fs = common::fs(distro);
        let clock = clock();
        let facts = NetworkModule::new(&runner, &fs, clock.clone())
            .facts()
            .unwrap();
        let interface = facts.interfaces[0].name.clone();
        runner.set_response(&format!("nmcli connection up {interface}"), ok_output(""));
        runner.set_response("networkctl reload", ok_output(""));
        runner.set_response(
            &format!("networkctl reconfigure {interface}"),
            ok_output(""),
        );
        let address = facts.interfaces[0].addresses[0].local.clone();
        let wanted = NetworkChange::new(
            interface.clone(),
            Addressing::Static {
                address: "10.9.9.9".to_owned(),
                prefix: 24,
                gateway: Some("10.9.9.1".to_owned()),
                dns: vec!["10.9.9.53".to_owned()],
            },
        )
        .unwrap();
        let modify = format!(
            "nmcli connection modify {interface} ipv4.method manual ipv4.addresses 10.9.9.9/24 ipv4.gateway 10.9.9.1 ipv4.dns 10.9.9.53"
        );
        runner.set_response(&modify, ok_output(""));
        NetworkModule::new(&runner, &fs, clock.clone())
            .with_state_dir(STATE)
            .apply(&wanted, WINDOW)
            .unwrap();
        let (_, backup) = paths();
        let saved = fs.file(&backup).unwrap();
        assert!(
            saved.contains(&format!("\"address\":\"{address}\"")),
            "{distro}: {saved}"
        );
        assert!(
            saved.contains(&format!("\"gateway\":\"{gateway}\"")),
            "{distro}: {saved}"
        );
        assert!(saved.contains(dns), "{distro}: {saved}");
    }
}
