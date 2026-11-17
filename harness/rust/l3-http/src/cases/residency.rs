//! Residency enforced where data is placed (WP-SEC-05.T5). The handle's behaviour (a policy for
//! one region refuses a replica, a backup destination, a search store and a clone elsewhere,
//! an undeclared destination is refused, a move needs the owner approval, no policy changes
//! nothing) is covered by the module's tests; the placements are not reachable over HTTP yet, so
//! the cases here are blocked entries that name the owner.

use super::CaseDef;

/// After `retention`.
pub const ORDER: u32 = 33;

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "residency: a_site_with_a_backup_target_outside_the_policy_refuses_to_start",
            "Residency",
            "a configured backup target in a forbidden or undeclared region stops the site at start-up with the clause named and no region list",
            "WP-SEC-05.T5",
            "the start-up check is covered by unit tests; the shared fixture has no residency policy or backup target to reload onto",
        ),
        CaseDef::blocked(
            "residency: a_replica_a_search_store_and_a_clone_in_a_forbidden_region_are_refused",
            "Residency",
            "the modules that place a replica, an analytics mirror, a search store or a rehearsal clone ask the residency port before placing",
            "WP-PRD-04.T5",
            "the placing modules do not call the ResidencyCheck port yet (hook sites are listed in the task report)",
        ),
        CaseDef::blocked(
            "residency: a_tenant_move_without_the_owner_approval_is_refused",
            "Residency",
            "a cross-region tenant move with no approval, or approved by the requester, is refused and audited",
            "WP-SEC-05.T5",
            "no HTTP route starts a tenant move on the node",
        ),
    ]
}
