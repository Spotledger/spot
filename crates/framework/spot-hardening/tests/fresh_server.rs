//! The acceptance loop of WP-ADM-10: a fresh server with default facts is evaluated, its
//! remediable controls are applied to the fake, and the server is evaluated again. Every
//! auto-remediable control must then pass and every other deviation must be listed.
//!
//! The default facts are the state of a freshly installed host before hardening; the effects
//! are what each catalogue operation is modelled to change. Nothing here touches a real host.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_hardening::{
    Baseline, FactKey, FactValue, FakeServer, OsFamily, Policy, Report, Severity, evaluate,
    remediation_plan, suggested_plan,
};

fn key(text: &str) -> FactKey {
    FactKey::try_new(text).unwrap()
}

fn text(value: &str) -> FactValue {
    FactValue::Text(value.to_owned())
}

fn list(values: &[&str]) -> FactValue {
    FactValue::TextList(values.iter().map(|value| (*value).to_owned()).collect())
}

/// A fresh Linux server before any hardening, with the effects of the two operations its
/// baseline uses.
fn fresh_linux() -> FakeServer {
    FakeServer::new(OsFamily::Linux)
        .with(key("ssh.enabled"), FactValue::Bool(true))
        .with(key("ssh.permit_root_login"), text("yes"))
        .with(key("ssh.password_authentication"), text("yes"))
        .with(key("mac.mode"), text("permissive"))
        .with(key("firewall.default_inbound_deny"), FactValue::Bool(false))
        .with(
            key("firewall.open_inbound_ports"),
            list(&["22/tcp", "5432/tcp"]),
        )
        .with(key("firewall.declared_ports"), list(&["22/tcp"]))
        .with(
            key("updates.security_updates_automatic"),
            FactValue::Bool(false),
        )
        .with(
            key("services.running"),
            list(&["sshd", "chronyd", "telnet"]),
        )
        .with(key("services.allowed"), list(&["sshd", "chronyd"]))
        .with(key("time.synchronised"), FactValue::Bool(false))
        .with(key("kernel.hardening_sysctls"), FactValue::Bool(false))
        .with(key("files.spotledger_dirs_secure"), FactValue::Bool(false))
        .with_effect(
            "os.service.disable",
            vec![(key("ssh.enabled"), FactValue::Bool(false))],
        )
        .with_effect(
            "os.service.enable",
            vec![(key("time.synchronised"), FactValue::Bool(true))],
        )
}

/// A fresh Windows server before any hardening.
fn fresh_windows() -> FakeServer {
    FakeServer::new(OsFamily::Windows)
        .with(
            key("windows.firewall.domain_enabled"),
            FactValue::Bool(true),
        )
        .with(
            key("windows.firewall.private_enabled"),
            FactValue::Bool(false),
        )
        .with(
            key("windows.firewall.public_enabled"),
            FactValue::Bool(false),
        )
        .with(key("windows.smb1_enabled"), FactValue::Bool(true))
        .with(key("windows.rdp.nla_required"), FactValue::Bool(false))
        .with(
            key("windows.defender.realtime_enabled"),
            FactValue::Bool(false),
        )
        .with(key("windows.uac.enabled"), FactValue::Bool(false))
        .with_effect(
            "os.service.set_start_type",
            vec![(key("windows.smb1_enabled"), FactValue::Bool(false))],
        )
        .with_effect(
            "os.service.start",
            vec![(
                key("windows.defender.realtime_enabled"),
                FactValue::Bool(true),
            )],
        )
}

/// Applies every operation in `plan` to `server`, asserting each has a modelled effect.
fn apply_all(server: &mut FakeServer, plan: &[spot_hardening::OpRef]) {
    for op in plan {
        assert!(
            server.apply(op),
            "no modelled effect for {}",
            op.op.as_str()
        );
    }
}

/// Asserts the acceptance shape: every auto-remediable control passes, and every other control
/// is still listed as a deviation.
fn assert_acceptance(report: &Report, expected_remaining: usize) {
    let mut remaining = 0;
    for result in &report.results {
        if result.auto_remediable() {
            assert!(
                !result.outcome.is_fail(),
                "auto-remediable control {} still fails",
                result.id.as_str()
            );
        } else {
            assert!(
                result.outcome.is_fail(),
                "non-remediable control {} should still be listed",
                result.id.as_str()
            );
            remaining += 1;
        }
    }
    assert_eq!(remaining, expected_remaining);
}

#[test]
fn a_fresh_linux_server_reaches_the_baseline() {
    let baseline = Baseline::linux().unwrap();
    let mut server = fresh_linux();

    let before = evaluate(&baseline, &server);
    assert_eq!(
        before.score.passed, 0,
        "a fresh server starts non-compliant"
    );
    assert_eq!(before.score.failed, 10);

    let policy = Policy::auto(Severity::Low).with_lockout_guard(true);
    let plan = remediation_plan(&before, &policy);
    assert_eq!(plan.len(), 2, "one disable and one enable, deduplicated");
    assert!(suggested_plan(&before, &policy).is_empty());
    apply_all(&mut server, &plan);

    let after = evaluate(&baseline, &server);
    assert_eq!(after.score.passed, 3);
    assert_acceptance(&after, 7);
}

#[test]
fn a_fresh_windows_server_reaches_the_baseline() {
    let baseline = Baseline::windows().unwrap();
    let mut server = fresh_windows();

    let before = evaluate(&baseline, &server);
    assert_eq!(before.score.passed, 0);
    assert_eq!(before.score.failed, 5);

    let policy = Policy::auto(Severity::Low);
    let plan = remediation_plan(&before, &policy);
    assert_eq!(plan.len(), 2);
    apply_all(&mut server, &plan);

    let after = evaluate(&baseline, &server);
    assert_eq!(after.score.passed, 2);
    assert_acceptance(&after, 3);
}

#[test]
fn without_a_guard_the_ssh_change_is_only_suggested() {
    let baseline = Baseline::linux().unwrap();
    let server = fresh_linux();
    let before = evaluate(&baseline, &server);

    let policy = Policy::auto(Severity::Low);
    let plan = remediation_plan(&before, &policy);
    assert_eq!(plan.len(), 1);
    assert_eq!(plan[0].op.as_str(), "os.service.enable");

    let suggested = suggested_plan(&before, &policy);
    assert_eq!(suggested.len(), 1);
    assert_eq!(suggested[0].op.as_str(), "os.service.disable");
}

#[test]
fn a_suggest_only_policy_applies_nothing() {
    let baseline = Baseline::linux().unwrap();
    let server = fresh_linux();
    let before = evaluate(&baseline, &server);
    let policy = Policy::suggest();
    assert!(remediation_plan(&before, &policy).is_empty());
    assert_eq!(suggested_plan(&before, &policy).len(), 2);
}

#[test]
fn the_baseline_diff_shows_the_remediation() {
    let baseline = Baseline::linux().unwrap();
    let mut server = fresh_linux();
    let before = evaluate(&baseline, &server);
    let policy = Policy::auto(Severity::Low).with_lockout_guard(true);
    apply_all(&mut server, &remediation_plan(&before, &policy));
    let after = evaluate(&baseline, &server);

    let delta = spot_hardening::diff(&before, &after);
    assert_eq!(delta.changes.len(), 3);
    assert!(
        delta
            .changes
            .iter()
            .all(|change| { change.direction == spot_hardening::Direction::Improved })
    );
}
