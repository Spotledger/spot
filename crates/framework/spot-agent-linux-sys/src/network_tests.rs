//! Unit tests for the network module: facts parsing, the nmcli argv, and the
//! revert guard driven by a manual clock.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use crate::testing::{FakeFs, FakeRunner, ok_output};
use spot_core::ManualClock;

fn manual() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH))
}

#[test]
fn interfaces_and_routes_parse() {
    let json = r#"[{"ifname":"eth0","operstate":"UP","address":"52:54:00:00:00:01","addr_info":[{"family":"inet","local":"10.0.0.5","prefixlen":24}]}]"#;
    let interfaces = parse_interfaces(json).unwrap();
    assert_eq!(interfaces[0].name, "eth0");
    assert_eq!(interfaces[0].addresses[0].prefix, 24);
    let routes =
        parse_routes(r#"[{"dst":"default","gateway":"10.0.0.1","dev":"eth0","metric":100}]"#)
            .unwrap();
    assert_eq!(routes[0].gateway.as_deref(), Some("10.0.0.1"));
}

#[test]
fn dns_servers_parse_from_resolvectl() {
    let text = "Global\n         DNS Servers: 10.0.0.1 10.0.0.2\nLink 2 (eth0)\n         DNS Servers: fe80::1\n";
    assert_eq!(
        parse_resolvectl_dns(text),
        vec![
            "10.0.0.1".to_owned(),
            "10.0.0.2".to_owned(),
            "fe80::1".to_owned()
        ]
    );
}

#[test]
fn addressing_validates_its_addresses() {
    assert!(Addressing::Dhcp.validate().is_ok());
    let good = Addressing::Static {
        address: "10.0.0.5".to_owned(),
        prefix: 24,
        gateway: Some("10.0.0.1".to_owned()),
        dns: vec!["10.0.0.1".to_owned()],
    };
    assert!(good.validate().is_ok());
    let bad = Addressing::Static {
        address: "not-an-ip".to_owned(),
        prefix: 24,
        gateway: None,
        dns: Vec::new(),
    };
    assert!(bad.validate().is_err());
}

#[test]
fn applying_then_sweeping_reverts_after_the_window() {
    let runner = FakeRunner::new()
        .with_response(
            "ip -j address show",
            ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
        )
        .with_response("systemctl is-active NetworkManager", ok_output("active\n"))
        .with_response(
            "nmcli connection modify eth0 ipv4.method manual ipv4.addresses 10.0.0.5/24 ipv4.gateway 10.0.0.1 ipv4.dns 10.0.0.1",
            ok_output(""),
        )
        .with_response("nmcli connection up eth0", ok_output(""))
        .with_response(
            "nmcli connection modify eth0 ipv4.method auto ipv4.addresses  ipv4.gateway  ipv4.dns ",
            ok_output(""),
        );
    let fs = FakeFs::new();
    let clock = manual();
    let module = NetworkModule::new(&runner, &fs, clock.clone());
    let change = NetworkChange::new(
        "eth0",
        Addressing::Static {
            address: "10.0.0.5".to_owned(),
            prefix: 24,
            gateway: Some("10.0.0.1".to_owned()),
            dns: vec!["10.0.0.1".to_owned()],
        },
    )
    .unwrap();
    let applied = module.apply(&change, 30).unwrap();
    assert_eq!(applied.ticket.interface, "eth0");
    // Before the window: no revert.
    assert!(module.sweep().unwrap().is_none());
    clock.advance(Duration::from_secs(31));
    let reverted = module.sweep().unwrap().expect("the window passed");
    assert!(reverted.changed);
    assert!(module.pending().unwrap().is_none());
}

#[test]
fn confirming_inside_the_window_cancels_the_revert() {
    let runner = FakeRunner::new()
        .with_response(
            "ip -j address show",
            ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
        )
        .with_response("systemctl is-active NetworkManager", ok_output("active\n"))
        .with_response(
            "nmcli connection modify eth0 ipv4.method auto ipv4.addresses  ipv4.gateway  ipv4.dns ",
            ok_output(""),
        )
        .with_response("nmcli connection up eth0", ok_output(""));
    let fs = FakeFs::new();
    let clock = manual();
    let module = NetworkModule::new(&runner, &fs, clock.clone());
    let change = NetworkChange::new("eth0", Addressing::Dhcp).unwrap();
    let applied = module.apply(&change, 30).unwrap();
    assert!(!module.confirm(applied.ticket.id).unwrap().changed);
    assert!(module.sweep().unwrap().is_none());
}

#[test]
fn a_second_change_while_pending_is_refused() {
    let runner = FakeRunner::new()
        .with_response(
            "ip -j address show",
            ok_output(r#"[{"ifname":"eth0","operstate":"UP","addr_info":[]}]"#),
        )
        .with_response("systemctl is-active NetworkManager", ok_output("active\n"))
        .with_response(
            "nmcli connection modify eth0 ipv4.method auto ipv4.addresses  ipv4.gateway  ipv4.dns ",
            ok_output(""),
        )
        .with_response("nmcli connection up eth0", ok_output(""));
    let fs = FakeFs::new();
    let clock = manual();
    let module = NetworkModule::new(&runner, &fs, clock.clone());
    let change = NetworkChange::new("eth0", Addressing::Dhcp).unwrap();
    module.apply(&change, 30).unwrap();
    let error = module.apply(&change, 30).unwrap_err();
    assert_eq!(error.code, NETWORK_REVERT_REQUIRED);
}

#[test]
fn no_backend_is_reported() {
    let runner = FakeRunner::new()
        .with_response(
            "systemctl is-active NetworkManager",
            ok_output("inactive\n"),
        )
        .with_response(
            "systemctl is-active systemd-networkd",
            ok_output("inactive\n"),
        );
    let fs = FakeFs::new();
    let clock = manual();
    let module = NetworkModule::new(&runner, &fs, clock.clone());
    let change = NetworkChange::new("eth0", Addressing::Dhcp).unwrap();
    assert_eq!(module.plan(&change).unwrap_err().code, NETWORK_NO_BACKEND);
}
