//! Assignment rules run on a server (WP-IAM-04.T3). The end-to-end cases need the host module's
//! `RuleStore` adapter over the stored records and a two-node fixture, so each is **blocked**
//! with its owner; the rules of the reconciler (one transaction, gate, lease, guard, expiry
//! marker, digest) are pinned by the module's unit tests.

use super::CaseDef;

/// After the roles snapshot cases.
pub const ORDER: u32 = 19;

const SECTION: &str = "Assignment rules";
const OWNER: &str =
    "WP-IAM-04.T3 follow-up: the PostgreSQL RuleStore adapter and the two-node fixture";

pub fn cases() -> Vec<CaseDef> {
    let rows: [(&str, &str, &str); 9] = [
        (
            "a_rule_grants_on_a_field_change_and_revokes_when_it_reverts",
            "a rule-sourced assignment appears when the user's field changes and ends when it reverts",
            "needs the RuleStore adapter",
        ),
        (
            "a_manual_assignment_of_the_same_role_is_untouched",
            "ending the rule's assignment leaves a manual assignment of the same role in force",
            "needs the RuleStore adapter",
        ),
        (
            "an_idp_managed_role_refuses_a_manual_edit",
            "a manual edit of an IdP-managed role is refused",
            "needs the identity group (WP-IAM-12.T2) and the adapter",
        ),
        (
            "expiry_fires_once_across_a_restart",
            "an assignment's end event is raised once, also after the node restarts",
            "needs the adapter and a restartable host",
        ),
        (
            "two_nodes_one_reconciler",
            "with two nodes only the lease holder reconciles; the other stops at once when it loses the lease",
            "needs a two-node fixture",
        ),
        (
            "a_bad_rule_is_isolated",
            "a rule that fails validation is reported and the other rules keep running",
            "needs the adapter",
        ),
        (
            "the_bulk_guard_holds_a_large_removal",
            "a rule edit that would end more than 25 percent of a role's assignments is held until its previewed digest is run",
            "needs the adapter",
        ),
        (
            "the_sod_gate_blocks_a_conflicting_rule_grant",
            "a rule that would create a segregation-of-duties conflict is reported and applies nothing",
            "needs the governance host",
        ),
        (
            "a_cross_tenant_rule_run_is_refused",
            "preview and run for another tenant answer like a missing resource",
            "needs the identity group mounted",
        ),
    ];
    rows.iter()
        .map(|(name, pins, reason)| {
            CaseDef::blocked(
                leak(&format!("assignment_rules: {name}")),
                SECTION,
                pins,
                OWNER,
                reason,
            )
        })
        .collect()
}

/// The case table is built once per run; the strings are `'static` by construction.
fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}
