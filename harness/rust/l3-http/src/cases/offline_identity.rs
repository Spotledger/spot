//! Offline identity on a hub and an edge (WP-IAM-09.T4): a snapshot built from a seeded role matrix, a
//! reduction that raises the epoch and shows the stale node, an offline write by a user revoked after issue
//! quarantined on arrival, replay that does not duplicate, every entry point honouring an expired lease, and
//! cross-tenant isolation.
//!
//! Every case here is **blocked**: they need a hub and an edge over the platform's stored records, the
//! `HubFacts` provider (node registry and user directory), the issuer and carrier of the sync modules and the
//! entry points calling `decide_at`. The rules themselves (issue, epochs, revocations, the sort and the table)
//! are pinned by the unit tests of `spot-cli` `services/offline_identity.rs`.

use super::CaseDef;

/// After the roles snapshot cases.
pub const ORDER: u32 = 19;

const SECTION: &str = "Offline identity";
const OWNER: &str =
    "WP-IAM-09.T4 follow-up: the HubFacts provider, the issuer and the entry-point wiring";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "offline_identity: a_snapshot_built_from_a_seeded_role_matrix_matches_the_live_decisions",
            SECTION,
            "the permission snapshot issued to a node says exactly what the live role snapshot decides for each user the node serves",
            OWNER,
            "needs a hub and an edge and the HubFacts provider",
        ),
        CaseDef::blocked(
            "offline_identity: a_reduction_raises_the_epoch_and_the_stale_node_is_reported",
            SECTION,
            "reducing a user's access raises the epoch of the next snapshot; a node still holding the old epoch shows in readiness and in the needs-you list",
            OWNER,
            "needs a hub and an edge",
        ),
        CaseDef::blocked(
            "offline_identity: an_offline_write_by_a_user_revoked_after_issue_is_quarantined_on_arrival",
            SECTION,
            "a write made offline under epoch N by a user revoked at epoch N+1 is quarantined when it reaches the hub, visible in the quarantine list, never dropped",
            OWNER,
            "needs the sync carrier and the quarantine queue on the hub",
        ),
        CaseDef::blocked(
            "offline_identity: a_replayed_journal_does_not_duplicate_quarantine_items",
            SECTION,
            "the same journal position arriving twice leaves one quarantine item",
            OWNER,
            "needs the sync carrier",
        ),
        CaseDef::blocked(
            "offline_identity: every_entry_point_honours_an_expired_lease",
            SECTION,
            "with an expired lease the API, realtime, jobs, scripts, controllers and management entry points all refuse the offline decision",
            OWNER,
            "needs the entry points to call decide_at (their files are owned by other tasks)",
        ),
        CaseDef::blocked(
            "offline_identity: a_snapshot_of_another_tenant_answers_like_a_missing_one",
            SECTION,
            "a node of tenant A asking for a snapshot or reporting an epoch for tenant B is refused exactly like an unknown node",
            OWNER,
            "needs two tenants on one hub",
        ),
    ]
}
