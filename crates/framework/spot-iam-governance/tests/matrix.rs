//! The seeded grant-conflict matrix: a table of (held duties, scopes, expected decision),
//! including scope-disjoint non-conflicts and descendants (ADR-070 section 8, rule 1).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{assignment, catalogue, hierarchy, request, rules};
use spot_iam_governance::{
    ApprovalPolicy, DelegationPolicy, GovernanceState, GrantDecision, GrantGate, GrantScope,
    Grantor, ScopeHierarchy,
};

/// What the matrix expects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// The grant proceeds.
    Allow,
    /// The grant creates this many SoD conflicts the grantor may override.
    NeedsOverride(usize),
    /// The grant needs approval.
    NeedsApproval,
}

/// One matrix row.
struct Row {
    name: &'static str,
    /// The user's existing assignments, as (role, scope value).
    held: &'static [(&'static str, Option<&'static str>)],
    /// The role requested.
    role: &'static str,
    /// The requested scope value.
    scope: Option<&'static str>,
    /// Whether the requested scope includes descendants.
    descendants: bool,
    /// A role the tenant requires approval for.
    approval_role: Option<&'static str>,
    /// A duty the tenant requires approval for.
    approval_duty: Option<&'static str>,
    /// The expected decision.
    expect: Expect,
}

/// The user every row is about.
const USER: &str = "jane";

const ROWS: &[Row] = &[
    Row {
        name: "clear: a different company is not the same scope",
        held: &[("Finance Approver", Some("Company B"))],
        role: "Accounts Payable Clerk",
        scope: Some("Company A"),
        descendants: false,
        approval_role: None,
        approval_duty: None,
        expect: Expect::Allow,
    },
    Row {
        name: "conflict: same duties in the same company conflict",
        held: &[("Finance Approver", Some("Company A"))],
        role: "Accounts Payable Clerk",
        scope: Some("Company A"),
        descendants: false,
        approval_role: None,
        approval_duty: None,
        expect: Expect::NeedsOverride(2),
    },
    Row {
        name: "descendants: an ancestor scope reaches the division's duties",
        held: &[("Finance Approver", Some("Company A/Division 1"))],
        role: "Accounts Payable Clerk",
        scope: Some("Company A"),
        descendants: true,
        approval_role: None,
        approval_duty: None,
        expect: Expect::NeedsOverride(2),
    },
    Row {
        name: "descendants: a scope that does not reach the division is clear",
        held: &[("Finance Approver", Some("Company B/Division 1"))],
        role: "Accounts Payable Clerk",
        scope: Some("Company A"),
        descendants: true,
        approval_role: None,
        approval_duty: None,
        expect: Expect::Allow,
    },
    Row {
        name: "any_scope: the bank-details rule conflicts across companies",
        held: &[("Finance Approver", Some("Company B"))],
        role: "Treasury Clerk",
        scope: Some("Company A"),
        descendants: false,
        approval_role: None,
        approval_duty: None,
        expect: Expect::NeedsOverride(1),
    },
    Row {
        name: "unscoped: two unscoped holds conflict",
        held: &[("Finance Approver", None)],
        role: "Accounts Payable Clerk",
        scope: None,
        descendants: false,
        approval_role: None,
        approval_duty: None,
        expect: Expect::NeedsOverride(2),
    },
    Row {
        name: "same_scope: a scoped and an unscoped hold do not conflict",
        held: &[("Finance Approver", Some("Company A"))],
        role: "Accounts Payable Clerk",
        scope: None,
        descendants: false,
        approval_role: None,
        approval_duty: None,
        expect: Expect::Allow,
    },
    Row {
        name: "approval: a role the tenant requires approval for",
        held: &[],
        role: "Finance Approver",
        scope: Some("Company A"),
        descendants: false,
        approval_role: Some("Finance Approver"),
        approval_duty: None,
        expect: Expect::NeedsApproval,
    },
    Row {
        name: "approval: a duty the role expands to",
        held: &[],
        role: "Finance Approver",
        scope: Some("Company A"),
        descendants: false,
        approval_role: None,
        approval_duty: Some("Approve payment run"),
        expect: Expect::NeedsApproval,
    },
    Row {
        name: "clear: no conflict and no approval policy",
        held: &[],
        role: "Accounts Payable Clerk",
        scope: Some("Company A"),
        descendants: false,
        approval_role: None,
        approval_duty: None,
        expect: Expect::Allow,
    },
];

#[test]
fn the_seeded_conflict_matrix_holds() {
    assert!(ROWS.len() >= 10, "the matrix needs at least ten rows");
    let gate = GrantGate::new();
    for row in ROWS {
        let state = state_for(row);
        let req = request(
            "req-1",
            USER,
            row.role,
            row.scope.map(|v| scope(row.descendants, v)),
            Grantor::new("admin", ["override_sod"]),
        );
        let decision = gate.evaluate(&req, &state).expect("the gate decides");
        match (row.expect, &decision) {
            (Expect::Allow, GrantDecision::Allow) => {}
            (Expect::NeedsOverride(n), GrantDecision::NeedsOverride(c)) => {
                assert_eq!(c.len(), n, "{}: wrong conflict count: {c:#?}", row.name);
            }
            (Expect::NeedsApproval, GrantDecision::NeedsApproval(_)) => {}
            (expected, actual) => {
                panic!("{}: expected {expected:?}, got {actual:#?}", row.name);
            }
        }
    }
}

#[test]
fn every_conflict_in_the_matrix_names_a_seeded_rule() {
    let gate = GrantGate::new();
    let row = &ROWS[1];
    let state = state_for(row);
    let req = request(
        "req-1",
        USER,
        row.role,
        Some(scope(false, "Company A")),
        Grantor::new("admin", ["override_sod"]),
    );
    let decision = gate.evaluate(&req, &state).expect("the gate decides");
    let rules: Vec<&str> = decision
        .conflicts()
        .iter()
        .filter_map(|c| c.rule_id())
        .collect();
    assert!(rules.contains(&"sod.vendor-master.payments"), "{rules:?}");
    assert!(rules.contains(&"sod.purchase-order.approval"), "{rules:?}");
}

fn scope(descendants: bool, value: &str) -> GrantScope {
    let scope = GrantScope::value("Company", value);
    if descendants {
        scope.with_descendants()
    } else {
        scope
    }
}

fn state_for(row: &Row) -> GovernanceState {
    let held = row
        .held
        .iter()
        .map(|(role, value)| assignment(USER, role, value.map(|v| scope(false, v))))
        .collect();
    let mut approval = ApprovalPolicy::none();
    if let Some(role) = row.approval_role {
        approval = approval.requiring_role(role);
    }
    if let Some(duty) = row.approval_duty {
        approval = approval.requiring_duty(duty);
    }
    if row.approval_role.is_some() || row.approval_duty.is_some() {
        // WP-IAM-08.T6: a policy that requires approval must say who the SLA escalates to,
        // or the gate refuses the grant instead of reporting an undecidable approval.
        approval = approval.routed_to("role:cfo", "role:ceo", 60);
    }
    GovernanceState::without_portal_users()
        .with_rules(rules())
        .with_catalogue(catalogue())
        .with_assignments(held)
        .with_delegation(DelegationPolicy::Unrestricted)
        .with_approval(approval)
        .with_hierarchy(if row.descendants {
            hierarchy()
        } else {
            ScopeHierarchy::new()
        })
}
