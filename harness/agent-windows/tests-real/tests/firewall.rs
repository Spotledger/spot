//! Real Windows Firewall conformance (WP-ADM-05.T3, `firewall` in `matrix.toml`).
//!
//! Reads the real firewall profiles and adds, reads back and removes a throw-away rule
//! through `INetFwPolicy2`. The rule carries this run's prefix; the test enumerates the
//! firewall and asserts that only its own rule carries the prefix, so it never touches a
//! rule it did not create. Removal runs on every path through the `Cleanup` guard.

use spot_agent_windows::api::{Direction, Profile, Protocol, RuleAction};
use spot_agent_windows::catalogue::{FIREWALL_ADD_RULE, FIREWALL_REMOVE_RULE};
use spot_agent_windows::{Firewall, WindowsError};
use spot_agent_windows_real::{Cleanup, args, assert_ours, config, list, name, prefix, text};

#[test]
fn throwaway_rule_lifecycle() {
    let rule_name = name("rule");
    let api = spot_agent_windows_real::api();
    let firewall = Firewall::new(api.clone(), config());

    // Never touch an existing object: this run's rule name must be free.
    assert!(
        firewall
            .list(Some(&rule_name))
            .expect("listing the firewall must succeed")
            .is_empty(),
        "the throw-away rule {rule_name:?} already exists"
    );

    let mut cleanup = Cleanup::new();
    {
        let doomed = rule_name.clone();
        let api = api.clone();
        cleanup.push(move || {
            assert_ours(&doomed);
            let _ = api.remove_firewall_rule(&doomed);
        });
    }

    // Add through the module: plan, apply, verify.
    let add = args(
        FIREWALL_ADD_RULE,
        vec![
            ("name", text(&rule_name)),
            ("direction", text("inbound")),
            ("protocol", text("tcp")),
            ("action", text("block")),
            ("profiles", list(&["public"])),
        ],
    );
    firewall
        .execute(&add)
        .expect("adding the throw-away rule must succeed");

    // Read it back through the real policy object.
    let rules = firewall
        .list(Some(&rule_name))
        .expect("listing the rule must succeed");
    assert_eq!(rules.len(), 1);
    let read_back = &rules[0];
    assert_eq!(read_back.name, rule_name);
    assert_eq!(read_back.direction, Direction::Inbound);
    assert_eq!(read_back.action, RuleAction::Block);
    assert_eq!(read_back.protocol, Protocol::Tcp);
    assert!(read_back.enabled);

    // Only our own prefixed rule may exist.
    let ours: Vec<String> = firewall
        .list(None)
        .expect("listing every rule must succeed")
        .into_iter()
        .filter(|rule| rule.name.starts_with(&prefix()))
        .map(|rule| rule.name)
        .collect();
    assert_eq!(ours, vec![rule_name.clone()]);

    // An identical add is idempotent: the module plans no change.
    assert!(
        firewall
            .plan(&add)
            .expect("planning an identical add must succeed")
            .is_no_change()
    );

    // Remove through the module and confirm it is gone.
    let remove = args(FIREWALL_REMOVE_RULE, vec![("name", text(&rule_name))]);
    firewall
        .execute(&remove)
        .expect("removing the throw-away rule must succeed");
    assert!(
        firewall
            .list(Some(&rule_name))
            .expect("listing the rule must succeed")
            .is_empty()
    );

    // The lock-out guard refuses a protected rule without the critical flag, touching
    // nothing.
    let refused = firewall
        .plan(&args(
            FIREWALL_REMOVE_RULE,
            vec![("name", text("Remote Desktop - User Mode (TCP-In)"))],
        ))
        .expect_err("removing an RDP rule without the flag must be refused");
    assert_eq!(refused.code(), spot_agent_windows::error::RULE_PROTECTED);
}

#[test]
fn firewall_profiles_are_readable() {
    let api = spot_agent_windows_real::api();
    for profile in Profile::all() {
        // Reading each profile exercises the real policy object; the value is the
        // machine's, so it is only asserted to be a typed answer.
        let _enabled: bool = api
            .profile_enabled(profile)
            .expect("reading a firewall profile must succeed");
    }
    assert!(matches!(
        api.remove_firewall_rule(&name("absent-rule")),
        Err(WindowsError::UnknownRule { .. })
    ));
}
