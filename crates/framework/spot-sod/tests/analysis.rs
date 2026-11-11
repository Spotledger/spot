//! The seeded analysis: every realistic conflict of tests/rules.toml detected, with the
//! right `held` reasons (direct and through a role), the scope behaviour of both scope
//! modes, and the deterministic order.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_sod::{Duty, Role, ScopeMode, Severity, analyze};

#[test]
fn the_seeded_rules_load() {
    let rules = common::rules();
    assert_eq!(rules.as_slice().len(), 8, "all seeded rules load");
    let rule = rules.rule("sod.vendor-master.payments").expect("rule");
    assert_eq!(rule.severity, Severity::Critical);
    assert_eq!(rule.scope, ScopeMode::SameScope);
    assert_eq!(rule.sides.len(), 2);
    assert!(rule.sides[0].names_duty(&Duty::new("Maintain supplier master").expect("duty")));
}

#[test]
fn direct_duty_conflicts_are_all_detected_with_reasons() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let assignments = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
        common::duty("rohit", "Create purchase order", None),
        common::duty("rohit", "Approve purchase order", None),
    ];
    let conflicts = analyze(&rules, &catalogue, &assignments, &[]);
    assert_eq!(conflicts.len(), 2, "{conflicts:#?}");
    let first = &conflicts[0];
    assert_eq!(first.user, "jane");
    assert_eq!(first.rule_id, "sod.vendor-master.payments");
    assert_eq!(first.severity, Severity::Critical);
    assert_eq!(first.scope.as_deref(), Some("Company A"));
    assert!(!first.overridden);
    // One `held` entry per side, in side order, both direct.
    assert_eq!(first.held.len(), 2);
    let h0 = &first.held[0];
    assert_eq!(h0.side, 0);
    assert_eq!(h0.scope.as_deref(), Some("Company A"));
    assert!(
        matches!(&h0.hold, spot_sod::Hold::Duty { duty } if duty.as_str() == "Maintain supplier master")
    );
    assert!(matches!(&h0.via, spot_sod::Via::Direct));
    // rohit's purchase-order conflict comes second (user order).
    assert_eq!(conflicts[1].user, "rohit");
    assert_eq!(conflicts[1].rule_id, "sod.purchase-order.approval");
    assert_eq!(conflicts[1].severity, Severity::High);
    assert_eq!(conflicts[1].scope, None, "both hold it unscoped");
}

#[test]
fn a_duty_through_a_role_conflicts_with_the_reason_named() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // jane holds "Approve payment run" only through the Finance Approver role.
    let assignments = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::role("jane", "Finance Approver", Some("Company A")),
    ];
    let conflicts = analyze(&rules, &catalogue, &assignments, &[]);
    assert_eq!(conflicts.len(), 1, "{conflicts:#?}");
    let c = &conflicts[0];
    assert_eq!(c.rule_id, "sod.vendor-master.payments");
    let through = &c.held[1];
    assert_eq!(through.side, 1);
    assert!(
        matches!(
            (&through.hold, &through.via),
            (spot_sod::Hold::Duty { duty }, spot_sod::Via::ThroughRole { role })
                if duty.as_str() == "Approve payment run" && role.as_str() == "Finance Approver"
        ),
        "the reason names the duty and the role it came through: {through:?}"
    );
}

#[test]
fn same_scope_different_companies_is_not_a_conflict_same_company_is() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // The same two duties in **different** companies: no conflict under the same_scope rule.
    let split = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company B")),
    ];
    assert!(analyze(&rules, &catalogue, &split, &[]).is_empty());
    // Unscoped anchor counts as its own scope: scoped duty vs unscoped duty of the same
    // rule do not match either.
    let anchor_less = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", None),
    ];
    assert!(analyze(&rules, &catalogue, &anchor_less, &[]).is_empty());
    // In the **same** company: one conflict, scoped to it.
    let same = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
    ];
    let conflicts = analyze(&rules, &catalogue, &same, &[]);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].scope.as_deref(), Some("Company A"));
    // And the unscoped-with-unscoped pair does conflict (the shared anchor is `None`).
    let both_unscoped = vec![
        common::duty("jane", "Maintain supplier master", None),
        common::duty("jane", "Approve payment run", None),
    ];
    let conflicts = analyze(&rules, &catalogue, &both_unscoped, &[]);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].scope, None);
}

#[test]
fn any_scope_flags_every_combination() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // The bank-details/release rule is any_scope: different scopes still conflict.
    let split = vec![
        common::duty("jane", "Maintain bank details", Some("Company A")),
        common::duty("jane", "Release payment", Some("Company B")),
    ];
    let conflicts = analyze(&rules, &catalogue, &split, &[]);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].rule_id, "sod.bank-details.release");
    assert_eq!(conflicts[0].scope, None, "any_scope conflicts are unscoped");
    // Unscoped vs unscoped conflict too.
    let unscoped = vec![
        common::duty("kofi", "Maintain bank details", None),
        common::duty("kofi", "Release payment", None),
    ];
    assert_eq!(analyze(&rules, &catalogue, &unscoped, &[]).len(), 1);
}

#[test]
fn a_rule_over_roles_counts_the_held_role_itself() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // The expense rule names the two roles for convenience; mei holds both roles and the
    // catalogue expands them to the side duties too. The first side's `held` entry is the
    // direct side **duty** ("Submit expense" through the role); the role is ALSO on the side,
    // so the same conflict is reached through the hold on the role — the canonical holdings
    // order puts the duty hold first.
    let assignments = vec![
        common::role("mei", "Expense Submitter", Some("Company A")),
        common::role("mei", "Expense Approver", Some("Company A")),
    ];
    let conflicts = analyze(&rules, &catalogue, &assignments, &[]);
    assert_eq!(conflicts.len(), 1, "{conflicts:#?}");
    let c = &conflicts[0];
    assert_eq!(c.rule_id, "sod.expense.roles");
    // Side 0's first canonical hold: the side duty reached through the role.
    assert!(
        matches!(
            (&c.held[0].hold, &c.held[0].via),
            (spot_sod::Hold::Duty { duty }, spot_sod::Via::ThroughRole { role })
                if duty.as_str() == "Submit expense" && role.as_str() == "Expense Submitter"
        ),
        "side 0 is held through the Expense Submitter role: {c:#?}"
    );
    assert!(
        matches!(
            (&c.held[1].hold, &c.held[1].via),
            (spot_sod::Hold::Duty { duty }, spot_sod::Via::ThroughRole { role })
                if duty.as_str() == "Approve expense" && role.as_str() == "Expense Approver"
        ),
        "side 1 is held through the Expense Approver role: {c:#?}"
    );
}

#[test]
fn a_role_on_one_side_conflicts_with_a_direct_duty_on_the_other() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    // The payroll rule: duty "Run payroll" vs role "Payroll Approver", any_scope.
    let assignments = vec![
        common::duty("sam", "Run payroll", None),
        common::role("sam", "Payroll Approver", Some("Company B")),
    ];
    let conflicts = analyze(&rules, &catalogue, &assignments, &[]);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0].rule_id, "sod.payroll.mixed");
}

#[test]
fn the_order_of_the_output_is_total_and_input_order_free() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let a = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
        common::duty("rohit", "Create purchase order", Some("Company A")),
        common::duty("rohit", "Approve purchase order", Some("Company A")),
    ];
    let mut b = a.clone();
    b.reverse();
    let ca = analyze(&rules, &catalogue, &a, &[]);
    let cb = analyze(&rules, &catalogue, &b, &[]);
    assert_eq!(ca, cb, "input order must not matter");
    // The documented sort: user, severity descending, rule id, scope.
    let mut expected = ca.clone();
    expected.sort_by(|x, y| {
        x.user
            .cmp(&y.user)
            .then(spot_sod::Severity::cmp(&y.severity, &x.severity))
            .then(x.rule_id.cmp(&y.rule_id))
            .then(x.scope.cmp(&y.scope))
    });
    assert_eq!(ca, expected);
    // Users ascending, and the critical vendor rule before the high purchase rule for the
    // same user.
    assert_eq!(ca[0].user, "jane");
    assert_eq!(ca[0].rule_id, "sod.vendor-master.payments");
    assert_eq!(ca[1].user, "rohit");
    assert_eq!(ca[1].rule_id, "sod.purchase-order.approval");
}

#[test]
fn duplicate_holdings_do_not_duplicate_conflicts() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let assignments = vec![
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Maintain supplier master", Some("Company A")),
        common::duty("jane", "Approve payment run", Some("Company A")),
    ];
    assert_eq!(analyze(&rules, &catalogue, &assignments, &[]).len(), 1);
}

#[test]
fn a_user_without_conflicts_is_absent_from_the_output() {
    let (rules, catalogue) = (common::rules(), common::clean_catalogue());
    let assignments = vec![
        common::role("ana", "Ledger Accountant", None),
        common::duty("bob", "Administer users", None),
    ];
    assert!(analyze(&rules, &catalogue, &assignments, &[]).is_empty());
}

#[test]
fn the_catalogue_toml_loads_and_the_holdings_expand() {
    let catalogue = common::clean_catalogue();
    let role = Role::new("Accounts Payable Clerk").expect("role");
    let duties = catalogue.duties_of(&role);
    assert_eq!(
        duties,
        vec![
            Duty::new("Create purchase order").expect("duty"),
            Duty::new("Maintain supplier master").expect("duty"),
            Duty::new("Post journal entry").expect("duty"),
        ],
        "duties come back sorted"
    );
    // An unknown role expands to nothing but is nameable.
    let ghost = Role::new("Ghost Role").expect("role");
    assert!(catalogue.duties_of(&ghost).is_empty());
    assert!(!catalogue.knows(&ghost));
    // The holdings of a user with one role assignment.
    let holdings = spot_sod::holdings_of(
        &[common::role(
            "jane",
            "Accounts Payable Clerk",
            Some("Company A"),
        )],
        "jane",
        &catalogue,
    );
    assert_eq!(
        holdings.len(),
        3,
        "all three duties of the role: {holdings:?}"
    );
}
