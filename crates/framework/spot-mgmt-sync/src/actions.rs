//! How the operator actions the conditions catalogue names map onto operations.
//!
//! `spot-sync-conditions` names, per condition, the commands an operator may run (the
//! `actions` of a `ConditionDef`: `notify_contact`, `suspend_node`, `resend`, ...). Each of them
//! is either exactly one operation of this crate ([`OPERATOR_ACTIONS`]) or is deliberately
//! not one ([`NOT_AN_OPERATION`], with the reason). A test in `tests/conditions.rs` walks the
//! catalogue and fails when a condition names an action that is in neither table, so the
//! catalogue and the operations cannot drift apart.

use crate::ids;

/// Operator actions that are exactly one operation here: `(action, operation id)`.
pub const OPERATOR_ACTIONS: [(&str, &str); 4] = [
    ("notify_contact", ids::NODE_NOTIFY_CONTACT),
    ("suspend_node", ids::NODE_SUSPEND),
    ("resume_node", ids::NODE_RESUME),
    ("resend", ids::NODE_RESEND),
];

/// Operator actions that are not an operation of this crate, with the reason.
///
/// None of them is purely automatic: each is a person's step that another surface owns (the
/// obligation ledger, the journal module, the bootstrap host, the incident workflow). They are
/// listed so a new action in the conditions catalogue cannot slip in unmapped, and so the
/// lead sees which surfaces still need an operation (reported under Open questions).
pub const NOT_AN_OPERATION: [(&str, &str); 6] = [
    (
        "re_verify",
        "journal verify and audit are operations of the journal module (verify and audit run)",
    ),
    (
        "release",
        "releasing a held item belongs to the quarantine and obligation surfaces, not the node verbs",
    ),
    (
        "mark_reviewed",
        "marking a finding reviewed is a step of the findings workflow, not a sync operation",
    ),
    (
        "approve_recovery",
        "approving a recovery is a decision recorded by the decision workflow (sys_sync_decision)",
    ),
    (
        "notify_role",
        "notifying a role is an automatic response step of the conditions engine; an operator reaches a person through notify_contact",
    ),
    (
        "re_bootstrap_range",
        "re-bootstrapping a range is run by the bootstrap host (WP-SYN bootstrap); the bootstrap status operation only reads its progress",
    ),
];

/// The operation an operator action maps to, if it is one.
#[must_use]
pub fn operation_for(action: &str) -> Option<&'static str> {
    OPERATOR_ACTIONS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, op)| *op)
}

/// The reason an operator action is not an operation, if it is listed as such.
#[must_use]
pub fn not_an_operation(action: &str) -> Option<&'static str> {
    NOT_AN_OPERATION
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, reason)| *reason)
}
