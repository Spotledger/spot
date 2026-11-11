//! Property tests: the grant-time check is exactly the set difference of the analyses, and
//! conflict results do not depend on the order of the assignments.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use proptest::prelude::*;
use spot_sod::{analyze, check_grant};

/// The seeded rules are the rule set of every property: eight realistic rules over duties
/// and roles, two scope modes.
fn rules() -> spot_sod::RuleSet {
    common::rules()
}

/// The clean reference catalogue of every property.
fn catalogue() -> spot_sod::RoleCatalogue {
    common::clean_catalogue()
}

/// Every duty and role name the tests can assign, with the roles of the catalogue.
const DUTIES: &[&str] = &[
    "Maintain supplier master",
    "Approve payment run",
    "Create purchase order",
    "Approve purchase order",
    "Maintain bank details",
    "Release payment",
    "Administer users",
    "Administer security",
    "Post journal entry",
    "Reconcile ledger",
    "Maintain item master",
    "Approve stock count",
];

const ROLES: &[&str] = &[
    "Accounts Payable Clerk",
    "Finance Approver",
    "Ledger Accountant",
    "Stock Controller",
    "Expense Submitter",
    "Expense Approver",
    "Payroll Approver",
    "Platform Administrator",
];

const USERS: &[&str] = &["jane", "rohit", "mei", "sam", "ana"];
const SCOPES: &[Option<&str>] = &[Some("Company A"), Some("Company B"), None];

/// A strategy for one assignment: any duty or role, any user, any scope.
fn assignment() -> impl Strategy<Value = spot_sod::Assignment> {
    (
        0usize..USERS.len(),
        prop::option::of(0usize..DUTIES.len()),
        prop::option::of(0usize..ROLES.len()),
        0usize..SCOPES.len(),
    )
        .prop_map(|(u, duty, role, scope)| {
            let user = USERS[u];
            let scope = SCOPES[scope];
            match (duty, role) {
                (Some(d), _) => spot_sod::Assignment::duty(user, DUTIES[d], scope),
                (None, Some(r)) => spot_sod::Assignment::role(user, ROLES[r], scope),
                (None, None) => spot_sod::Assignment::duty(user, DUTIES[u], scope),
            }
        })
}

/// A small list of assignments (0 to 5), built from the strategy above.
fn assignments() -> impl Strategy<Value = Vec<spot_sod::Assignment>> {
    prop::collection::vec(assignment(), 0..=5)
}

/// A proposed grant (duty or role, any user, any scope).
fn proposed() -> impl Strategy<Value = spot_sod::Assignment> {
    (
        0usize..USERS.len(),
        0usize..2,
        0usize..DUTIES.len(),
        0usize..ROLES.len(),
        0usize..SCOPES.len(),
    )
        .prop_map(|(u, kind, d, r, scope)| {
            let user = USERS[u];
            let scope = SCOPES[scope];
            if kind == 0 {
                spot_sod::Assignment::duty(user, DUTIES[d], scope)
            } else {
                spot_sod::Assignment::role(user, ROLES[r], scope)
            }
        })
}

proptest! {
    /// `check_grant` is exactly `analyze(existing + proposed) − analyze(existing)` under the
    /// (rule, user, scope) identity: every conflict in the combined analysis the existing
    /// assignments do not already carry, and nothing else. A conflict already present is
    /// never blamed on the grant.
    #[test]
    fn check_grant_is_the_difference(
        existing in assignments(),
        proposed in proposed(),
    ) {
        let rules = rules();
        let catalogue = catalogue();
        let mut with = existing.clone();
        with.push(proposed.clone());
        let before = analyze(&rules, &catalogue, &existing, &[]);
        let after = analyze(&rules, &catalogue, &with, &[]);
        let expected: Vec<_> = after
            .iter()
            .filter(|c| !before.iter().any(|b| b.rule_id == c.rule_id && b.user == c.user && b.scope == c.scope))
            .cloned()
            .collect();
        let check = check_grant(&rules, &catalogue, &existing, &proposed, &[]);
        match check {
            spot_sod::GrantCheck::Clear => prop_assert!(expected.is_empty(),
                "Clear but the difference has {}: {expected:#?} (existing {existing:?}, proposed {proposed:?})",
                expected.len()),
            spot_sod::GrantCheck::Conflicts(created) => {
                prop_assert_eq!(created, expected);
            }
        }
    }

    /// The analysis does not depend on the order of the assignments: any permutation of the
    /// same list produces the same conflicts.
    #[test]
    fn analysis_is_order_independent(
        assignments in assignments(),
        rotate in 0usize..8,
    ) {
        let rules = rules();
        let catalogue = catalogue();
        let plain = analyze(&rules, &catalogue, &assignments, &[]);
        let mut permuted = assignments.clone();
        if !permuted.is_empty() {
            let by = rotate % permuted.len();
            permuted.rotate_left(by);
            permuted.reverse();
        }
        let same = analyze(&rules, &catalogue, &permuted, &[]);
        prop_assert_eq!(plain, same);
    }

    /// Adding assignments never removes a conflict: the analysis is monotone. (With the same
    /// (rule, user, scope) keying, extra holdings can add conflicts but never take one away.)
    #[test]
    fn analysis_is_monotone(
        base in assignments(),
        extra in assignments(),
    ) {
        let rules = rules();
        let catalogue = catalogue();
        let mut grown = base.clone();
        grown.extend(extra.iter().cloned());
        let before: std::collections::BTreeSet<(String, String, Option<String>)> =
            analyze(&rules, &catalogue, &base, &[])
                .into_iter()
                .map(|c| (c.rule_id, c.user, c.scope))
                .collect();
        let after: std::collections::BTreeSet<(String, String, Option<String>)> =
            analyze(&rules, &catalogue, &grown, &[])
                .into_iter()
                .map(|c| (c.rule_id, c.user, c.scope))
                .collect();
        prop_assert!(before.is_subset(&after), "growing the assignments lost {before:?} from {after:?}");
    }

    /// The output is always sorted by the documented key (user, severity descending, rule
    /// id, scope), for any input.
    #[test]
    fn output_is_sorted_by_the_documented_key(assignments in assignments()) {
        let rules = rules();
        let catalogue = catalogue();
        let out = analyze(&rules, &catalogue, &assignments, &[]);
        let mut expected = out.clone();
        expected.sort_by(|x, y| {
            x.user
                .cmp(&y.user)
                .then(spot_sod::Severity::cmp(&y.severity, &x.severity))
                .then(x.rule_id.cmp(&y.rule_id))
                .then(x.scope.cmp(&y.scope))
        });
        prop_assert_eq!(out, expected);
    }
}
