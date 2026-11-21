//! The grant gate (WP-IAM-08.T7) over HTTP: what a stranger can see of it. The gate is a port
//! (`GrantGuard`) that the grant paths call in process; no HTTP route reaches it yet, so the
//! end-to-end behaviours are blocked entries naming the task that adds the route.

use super::CaseDef;

/// After the key case.
pub const ORDER: u32 = 44;

const SECTION: &str = "Grant governance";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "governance: a_conflicting_grant_is_refused_an_override_needs_a_reason_and_a_control",
            SECTION,
            "a grant that creates a segregation-of-duties conflict is refused with the rule ids only; an override with a reason and a mitigating control is accepted and listed, one without a control is refused, an expired override stops applying",
            "WP-IAM-12.T4",
            "the identity assignment route (`identity_assignment_create`) is served by a module that does not exist yet and calls no `GrantGuard`; the gate is reachable only in process",
        ),
        CaseDef::blocked(
            "governance: an_approval_grant_fails_closed_without_a_launcher_and_the_requester_cannot_approve",
            SECTION,
            "a grant that needs approval with no process launcher is refused (`iam_governance.approval_unavailable`), never granted; the requester and the beneficiary cannot approve; a decision is made once",
            "WP-IAM-08.T11",
            "no route decides an approval and no launcher is provided until the approval process is hosted",
        ),
    ]
}
