//! Roles served from one atomically published snapshot (WP-IAM-03.T2): the Company A / Company B
//! scenario, a grant on node 1 effective on node 2 within the stated bound, an in-flight request
//! finishing under the snapshot it started with, a failing build keeping the old snapshot and
//! degrading, an upgrade diff that changes no copy, time-boxed assignments ending at the second,
//! and cross-tenant isolation.
//!
//! Every case here is **blocked**: the cases need two running nodes over the platform's stored
//! `role` and `role_assignment` records and a `RoleSource` adapter over them, and the host module
//! (`spot-cli` `services/roles.rs`) is written against that seam, not against the storage. The
//! rules themselves (the swap, the kept snapshot on a failed build, the in-flight snapshot, the
//! second-exact boundaries, customise and the diff) are pinned by the module's own unit tests.

use super::CaseDef;

/// After the sessions and before the events cases.
pub const ORDER: u32 = 18;

const SECTION: &str = "Roles snapshot";
const OWNER: &str =
    "WP-IAM-03.T2 follow-up: the PostgreSQL RoleSource adapter and the two-node fixture";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "roles_snapshot: one_user_is_manager_for_company_a_and_viewer_for_company_b_and_sees_only_that",
            SECTION,
            "a user holding Manager scoped to Company A and Viewer scoped to Company B reads and writes Company A documents and only reads Company B ones",
            OWNER,
            "needs the stored role and role_assignment records behind a RoleSource",
        ),
        CaseDef::blocked(
            "roles_snapshot: a_grant_on_node_1_is_effective_on_node_2_within_the_refresh_bound",
            SECTION,
            "an assignment written through node 1 is in force on node 2 within `services.roles.refresh_seconds` (default 5 seconds)",
            OWNER,
            "needs a two-node fixture",
        ),
        CaseDef::blocked(
            "roles_snapshot: an_in_flight_request_keeps_the_snapshot_it_started_with",
            SECTION,
            "a request that started before a revoke finishes under the snapshot it started with; the next request sees the new one",
            OWNER,
            "needs the engine's authoriser to read the published snapshot (the engine reads stored assignments per request today)",
        ),
        CaseDef::blocked(
            "roles_snapshot: a_failing_build_keeps_the_old_snapshot_and_degrades_the_site",
            SECTION,
            "with the role records unreadable the node keeps serving under the previous snapshot, readiness reports degraded with `roles_host.source_unavailable`, and no request is allowed that the old snapshot denied",
            OWNER,
            "needs a fault point on the RoleSource adapter",
        ),
        CaseDef::blocked(
            "roles_snapshot: an_upgrade_shows_a_diff_and_changes_no_copy",
            SECTION,
            "`GET /identity/roles/{id}/upgrade-diff` lists what a newer reference role adds and removes and a customised copy is byte-identical afterwards",
            OWNER,
            "needs a reference pack upgrade fixture",
        ),
        CaseDef::blocked(
            "roles_snapshot: a_time_boxed_assignment_ends_at_the_second_on_every_node",
            SECTION,
            "an assignment with `valid_to` stops granting at that second on both nodes without a restart",
            OWNER,
            "needs a two-node fixture with a controllable clock",
        ),
        CaseDef::blocked(
            "roles_snapshot: a_role_of_another_tenant_answers_like_a_missing_one",
            SECTION,
            "`GET /identity/roles/{id}` with a role id of another tenant is the same 404 `identity.not_found` as an unknown id",
            OWNER,
            "needs the identity group mounted over this host module (WP-IAM-12.T2)",
        ),
    ]
}
