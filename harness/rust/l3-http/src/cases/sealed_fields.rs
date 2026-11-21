//! Sealed and encrypted protected fields on a node (WP-SEC-10.T6), end to end on two nodes.
//!
//! Every case here is **blocked**: it names the surface that is missing. The harness has one host
//! and no second (branch) node, and the engine has no seam for the field transform or for masking a
//! sealed value on its read path (see `docs/running-spot/sealed_fields.md`). The module's own
//! behaviour (rules, the box and its binding, the plaintext refusal, reveal rate) is covered by the
//! unit tests of `services/sealed_fields.rs`; these cases pin what only the whole stack can show.
//! A blocked case is visible in every run and in `CASES.md` until its owner lands.

use super::CaseDef;

/// After the other registered groups; owns no fixture.
pub const ORDER: u32 = 50;

const SECTION: &str = "Sealed fields";
const OWNER: &str = "WP-SEC-10.T6 follow-up";
const NO_SEAMS: &str = "the engine has no write-transform seam and no read-mask seam for sealed values, and the harness starts no second node";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "sealed_fields: a_sealed_field_written_on_the_branch_is_unreadable_there_on_every_channel",
            SECTION,
            "a sealed value written on the branch node is not returned by a read, list, report, export, print, journal view, search or realtime payload there",
            OWNER,
            NO_SEAMS,
        ),
        CaseDef::blocked(
            "sealed_fields: the_hub_reveals_a_sealed_value_to_an_authorised_user_online_and_audits_it",
            SECTION,
            "the reveal route returns the value once, to a user holding the reveal right, and writes one audit record naming the field and document but not the value",
            OWNER,
            "no reveal route is mounted by the host and no hub is reachable from a second node",
        ),
        CaseDef::blocked(
            "sealed_fields: an_offline_reveal_answers_reveal_offline_and_serves_no_cached_value",
            SECTION,
            "with the hub unreachable the reveal answers `sealed_fields.reveal_offline` and never a stored or cached plaintext",
            OWNER,
            NO_SEAMS,
        ),
        CaseDef::blocked(
            "sealed_fields: a_write_from_a_revoked_node_is_quarantined",
            SECTION,
            "a node revoked at the hub has its later writes quarantined, and its key release is refused",
            OWNER,
            "the sync apply path has no quarantine for a revoked node's writes in the harness",
        ),
        CaseDef::blocked(
            "sealed_fields: a_group_with_no_class_refuses_the_write",
            SECTION,
            "a protected field of a group tagged secret with no sync class is refused with `sealed_fields.class_unset`",
            OWNER,
            "the shared fixture's metadata declares no protection group",
        ),
        CaseDef::blocked(
            "sealed_fields: an_encrypted_field_round_trips_under_a_live_lease",
            SECTION,
            "an encrypted field is written under the held group key and read back while the lease is live, and fails closed with `sealed_fields.lease_expired` after it",
            OWNER,
            NO_SEAMS,
        ),
        CaseDef::blocked(
            "sealed_fields: a_sealed_value_cannot_be_moved_to_another_tenant_or_record",
            SECTION,
            "a ciphertext copied to another record or tenant fails to open at the hub, and a cross-tenant id answers like a missing one",
            OWNER,
            "the harness has one tenant and no hub unseal call",
        ),
        CaseDef::blocked(
            "sealed_fields: the_database_dump_holds_no_plaintext_of_a_sealed_or_encrypted_field",
            SECTION,
            "a dump of the node's database searched for the planted values finds none",
            OWNER,
            NO_SEAMS,
        ),
    ]
}
