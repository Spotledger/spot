//! Overrides: refused without the right, refused with a blank reason or a blank control,
//! refused when the override covers a different rule (or scope), accepted when complete —
//! and then `analyze` still lists the conflict, flagged `overridden: true`.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_sod::{
    GrantCheck, OVERRIDE_SOD_RIGHT, OverrideDecision, Rights, SodOverride, Uncovered, analyze,
    apply_override, check_grant,
};

/// The grant the override tests revolve around: jane, vendor master vs payment run, Company A.
fn conflicted_check() -> GrantCheck {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let existing = vec![common::duty(
        "jane",
        "Maintain supplier master",
        Some("Company A"),
    )];
    let proposed = common::duty("jane", "Approve payment run", Some("Company A"));
    check_grant(&rules, &catalogue, &existing, &proposed, &[])
}

/// A complete override of the conflicted rule for jane in Company A.
fn full_override() -> SodOverride {
    SodOverride {
        rule_id: "sod.vendor-master.payments".to_owned(),
        user: "jane".to_owned(),
        scope: Some("Company A".to_owned()),
        reason: "month-end cover while the controller is on leave".to_owned(),
        mitigating_control: "every payment she approves is re-reviewed by the controller on return"
            .to_owned(),
        granted_by: "controller".to_owned(),
        granted_at: 0,
    }
}

/// The grantor's rights with `override_sod`.
fn with_right() -> Rights {
    Rights::new(["grant_roles", OVERRIDE_SOD_RIGHT])
}

#[test]
fn a_complete_override_on_a_complete_grantor_is_allowed() {
    let check = conflicted_check();
    let decision = apply_override(&check, &[full_override()], &with_right());
    assert_eq!(decision, OverrideDecision::Allowed);
}

#[test]
fn refused_without_the_right_and_the_refusal_says_so() {
    let check = conflicted_check();
    let without = Rights::new(["grant_roles"]);
    let decision = apply_override(&check, &[full_override()], &without);
    let OverrideDecision::Refused(refusal) = decision else {
        panic!("a grantor without the right is refused");
    };
    assert!(refusal.missing_right);
    assert!(refusal.uncovered_rules.is_empty());
    assert!(!refusal.blank_reason);
    assert!(!refusal.blank_mitigating_control);
    assert!(
        refusal.missing_right || !refusal.uncovered_rules.is_empty(),
        "a refusal is non-empty by construction"
    );
}

#[test]
fn refused_with_a_blank_reason_or_a_blank_control() {
    let check = conflicted_check();
    let mut blank_reason = full_override();
    blank_reason.reason = "   ".to_owned();
    let OverrideDecision::Refused(refusal) = apply_override(&check, &[blank_reason], &with_right())
    else {
        panic!("a blank reason is refused");
    };
    assert!(refusal.blank_reason);
    assert!(!refusal.blank_mitigating_control);

    let mut blank_control = full_override();
    blank_control.mitigating_control = "".to_owned();
    let OverrideDecision::Refused(refusal) =
        apply_override(&check, &[blank_control], &with_right())
    else {
        panic!("a blank mitigating control is refused");
    };
    assert!(refusal.blank_mitigating_control);
    assert!(!refusal.blank_reason);
}

#[test]
fn an_override_does_not_silently_cover_a_different_rule() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // A proposed grant conflicting under a DIFFERENT rule than the override names.
    let existing = vec![common::duty(
        "rohit",
        "Post journal entry",
        Some("Company A"),
    )];
    let proposed = common::duty("rohit", "Reconcile ledger", Some("Company A"));
    let check = check_grant(&rules, &catalogue, &existing, &proposed, &[]);
    let GrantCheck::Conflicts(conflicts) = &check else {
        panic!("the reconciliation pair conflicts");
    };
    let rule = conflicts[0].rule_id.clone();
    // The override covers jane's rule, not rohit's.
    let decision = apply_override(&check, &[full_override()], &with_right());
    let OverrideDecision::Refused(refusal) = decision else {
        panic!("an override of another rule does not cover this one");
    };
    assert_eq!(
        refusal.uncovered_rules,
        vec![(rule, Uncovered::NoOverride)],
        "exactly the uncovered rule is named"
    );
}

#[test]
fn an_override_does_not_silently_cover_a_different_scope() {
    let check = conflicted_check();
    let mut other_scope = full_override();
    other_scope.scope = Some("Company B".to_owned());
    let decision = apply_override(&check, &[other_scope], &with_right());
    let OverrideDecision::Refused(refusal) = decision else {
        panic!("an override of another scope does not cover this one");
    };
    assert_eq!(
        refusal.uncovered_rules,
        vec![(
            "sod.vendor-master.payments".to_owned(),
            Uncovered::ScopeMismatch {
                recorded: Some("Company B".to_owned()),
            },
        )],
        "the recorded scope is named in the refusal"
    );
    // And the unscoped override does not cover the scoped conflict either.
    let mut unscoped = full_override();
    unscoped.scope = None;
    let OverrideDecision::Refused(refusal) = apply_override(&check, &[unscoped], &with_right())
    else {
        panic!("a None scope covers only the unscoped conflict");
    };
    assert_eq!(
        refusal.uncovered_rules[0].1,
        Uncovered::ScopeMismatch { recorded: None }
    );
}

#[test]
fn every_gap_is_listed_at_once_not_just_the_first() {
    let check = conflicted_check();
    let mut shabby = full_override();
    shabby.reason = "  ".to_owned();
    shabby.mitigating_control = "".to_owned();
    let no_right = Rights::new(Vec::<String>::new());
    let OverrideDecision::Refused(refusal) = apply_override(&check, &[shabby], &no_right) else {
        panic!("everything missing is refused");
    };
    assert!(refusal.missing_right, "the right is missing");
    assert!(refusal.blank_reason, "the reason is blank");
    assert!(refusal.blank_mitigating_control, "the control is blank");
}

#[test]
fn a_clear_check_needs_no_override() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let existing = vec![common::duty(
        "jane",
        "Maintain supplier master",
        Some("Company A"),
    )];
    let proposed = common::duty("jane", "Create purchase order", Some("Company A"));
    let check = check_grant(&rules, &catalogue, &existing, &proposed, &[]);
    assert_eq!(check, GrantCheck::Clear);
    // Nothing to override: allowed whatever the rights.
    assert_eq!(
        apply_override(&check, &[], &Rights::new(Vec::<String>::new())),
        OverrideDecision::Allowed
    );
}

#[test]
fn an_overridden_conflict_is_still_reported_but_flagged() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let assignments = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
    ];
    // Without the override: reported, not flagged.
    let plain = analyze(&rules, &catalogue, &assignments, &[]);
    assert_eq!(plain.len(), 1);
    assert!(!plain[0].overridden);
    // With the active override: STILL reported, flagged.
    let overrides = vec![full_override()];
    let covered = analyze(&rules, &catalogue, &assignments, &overrides);
    assert_eq!(covered.len(), 1, "the access review must still show it");
    assert!(covered[0].overridden);
    // A different user's conflict is not flagged by jane's override.
    let others = vec![
        common::duty("kofi", "Maintain supplier master", Some("Company A")),
        common::duty("kofi", "Approve payment run", Some("Company A")),
    ];
    let not_flagged = analyze(&rules, &catalogue, &others, &overrides);
    assert_eq!(not_flagged.len(), 1);
    assert!(!not_flagged[0].overridden);
    // Neither is jane's conflict in another scope.
    let jane_b = vec![
        common::duty("jane", "Maintain supplier master", Some("Company B")),
        common::duty("jane", "Approve payment run", Some("Company B")),
    ];
    let other_scope = analyze(&rules, &catalogue, &jane_b, &overrides);
    assert_eq!(other_scope.len(), 1);
    assert!(!other_scope[0].overridden);
}

#[test]
fn the_rights_type_round_trips_through_toml() {
    let rights = with_right();
    // A `#[serde(transparent)]` newtype of Vec<String> serialises as a TOML array, not a
    // table, so the round trip goes through `toml::Value`.
    let value = toml::Value::try_from(&rights).expect("rights convert to a TOML value");
    let back: Rights = value.try_into().expect("rights load back");
    assert_eq!(back, rights);
    assert!(back.has_override_sod());
    // A blank right is dropped, duplicates collapse, and the sorted set compares equal.
    let messy = Rights::new(["grant_roles", OVERRIDE_SOD_RIGHT, " grant_roles ", ""]);
    assert_eq!(messy, rights);
    assert!(!Rights::new(Vec::<String>::new()).has_override_sod());
}
