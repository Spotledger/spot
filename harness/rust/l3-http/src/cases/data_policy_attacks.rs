//! The hostile gate for data policy, residency and key revocation over HTTP (WP-SEC-05.T7).
//!
//! Every case here needs the `/api/v1/security/*` data-policy and protected-group operations to
//! be **served**. They are not yet: the handlers exist (WP-SEC-05.T6, `routes/data_policy.rs`)
//! but are mounted by hand and not registered, the retention job has no production store
//! (WP-SEC-05.T4), the placing modules do not ask the residency port (WP-SEC-05.T5) and the
//! sealed-field hosting has no engine write seam (WP-SEC-10.T6). Each case is therefore a
//! blocked entry that names its owner and the observation that proves the gap, and each is
//! written as the exact hostile scenario the brief lists, so the owner turns it into
//! `CaseDef::run` in the same file with the same name.
//!
//! The assertions each case must make, once served, are in its `pins` line. Anything that
//! unexpectedly passes is a security finding for the run's `Known security issues`.

use super::CaseDef;

/// After `residency`.
pub const ORDER: u32 = 34;

const NOT_SERVED: &str = "the data-policy operations are not registered in the route registry (WP-SEC-05.T6 mounts them by hand)";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "data_policy_attacks: a_real_retention_run_without_the_digest_is_refused_at_the_edge",
            "Data policy attacks",
            "POST security_retention_run_create with dry_run=false and no plan_digest answers 422 security.plan_required before the port, deletes nothing, and the refusal is audited",
            "WP-SEC-05.T6",
            NOT_SERVED,
        ),
        CaseDef::blocked(
            "data_policy_attacks: a_run_with_another_tenants_digest_or_a_modified_plan_is_refused",
            "Data policy attacks",
            "a digest minted for tenant B answers the same 404-shaped envelope as a missing plan, a digest of a modified plan is refused, and no record of either tenant is deleted",
            "WP-SEC-05.T4",
            "no production retention store provider exists, so no plan can be minted over HTTP",
        ),
        CaseDef::blocked(
            "data_policy_attacks: a_hold_created_between_preview_and_run_protects_the_record",
            "Data policy attacks",
            "a hold created after the dry run and before the confirmed run leaves its record in place, and the outcome row says legal_hold",
            "WP-SEC-05.T4",
            "no production retention store provider exists",
        ),
        CaseDef::blocked(
            "data_policy_attacks: only_an_administrator_starts_a_run_or_creates_and_releases_a_hold",
            "Data policy attacks",
            "a Team Member and a Viewer get 403 on run create, hold create and hold delete; the administrator succeeds; a released hold is audited with who released it",
            "WP-SEC-05.T6",
            NOT_SERVED,
        ),
        CaseDef::blocked(
            "data_policy_attacks: a_deletion_proof_verifies_against_the_chain_and_one_flipped_byte_fails",
            "Data policy attacks",
            "the proof from security_retention_proof_list matches the retention.proof chain entry; flipping one byte of the proof makes verification fail and the verifier names the entry",
            "WP-SEC-05.T4",
            "no production retention store provider exists, so no proof is produced over HTTP",
        ),
        CaseDef::blocked(
            "data_policy_attacks: every_placement_path_reachable_over_http_refuses_a_forbidden_region",
            "Data policy attacks",
            "with a policy allowing one region, security_residency_check and each reachable placement answer a refusal naming the clause and no region list; an undeclared destination is refused",
            "WP-SEC-05.T5",
            "security_residency_check is not registered and the placing modules do not call the ResidencyCheck port yet",
        ),
        CaseDef::blocked(
            "data_policy_attacks: a_revoked_node_is_refused_its_next_release_and_the_group_epoch_advances",
            "Data policy attacks",
            "after security_node_key_revoke the node's next key-release request is refused, the protected group's key epoch is higher, and the revocation is audited",
            "WP-SEC-10.T6",
            "no engine write/read seam: a protected field cannot be written on a node, so no key release exists to refuse",
        ),
        CaseDef::blocked(
            "data_policy_attacks: no_response_body_in_any_case_contains_key_material",
            "Data policy attacks",
            "every response body of every case above is scanned for key, secret, seed, private or wrapped-key fields and for the key text of the node's key file: none appears",
            "WP-SEC-05.T6",
            NOT_SERVED,
        ),
        CaseDef::blocked(
            "data_policy_attacks: a_retention_kill_switch_stops_a_run_within_one_batch_over_http",
            "Data policy attacks",
            "stopping the retention job mid-run leaves the remaining records and a checkpoint, and the next run continues without deleting a record twice",
            "WP-SEC-05.T4",
            "no production retention store provider exists, and the administrator kill operation is not routed",
        ),
    ]
}
