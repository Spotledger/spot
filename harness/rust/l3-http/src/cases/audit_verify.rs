//! The hosted audit verification (WP-SEC-06.T8) over the real stack: a clean chain verifies, an
//! entry altered through a superuser connection is flagged at exactly its index, a deleted
//! segment, a reordered pair and a rolled-back head (caught by the signed anchor) are found, a
//! read by a non-auditor is refused, masked fields stay masked, the nightly pass resumes after a
//! restart and a second tenant's entries are unreachable.
//!
//! Every case is **blocked** today, with the observation that proves it: the host serves no
//! `/api/v1/security/audit/*` route (the three operations are in the contract and listed in the
//! conformance table as not served), and `spot-cli` provides no `AuditKeys` or `AuditAccess` port,
//! so `services/audit_verify.rs` cannot be turned on in a running site. The behaviours themselves
//! are proven at unit level against the real chain verifier in that module's tests (clean, altered
//! entry, deleted entry, reorder, rolled-back head, forged anchor, exactly once, resume, refusal,
//! masking, bounds, tenancy). When the routes and ports land, each blocked entry below becomes a
//! `CaseDef::run` against a PostgreSQL site, tampering through a superuser connection.

use super::CaseDef;

/// After the webhook secret case; independent of the other groups' data.
pub const ORDER: u32 = 41;

const SECTION: &str = "Audit verification";
const OWNER: &str = "WP-SEC-06.T8";
const NO_ROUTE: &str =
    "the host mounts no `/api/v1/security/audit/*` route and provides no audit keys or access port";

/// The registered cases.
pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "audit_verify: a_clean_chain_verifies_and_the_head_is_anchored",
            SECTION,
            "a clean chain verifies, the verified head advances and a signed head anchor is appended outside the database role's reach",
            OWNER,
            NO_ROUTE,
        ),
        CaseDef::blocked(
            "audit_verify: an_altered_entry_is_flagged_at_exactly_its_index",
            SECTION,
            "an entry altered through a superuser connection degrades the audit service, names the lane, the index and the kind and never the content, and the verified head stops advancing",
            OWNER,
            NO_ROUTE,
        ),
        CaseDef::blocked(
            "audit_verify: a_deleted_segment_a_reorder_and_a_rolled_back_head_are_found",
            SECTION,
            "a deleted entry (sequence gap), two reordered entries (broken link) and a head rolled back in the database (the anchor catches it) are each reported",
            OWNER,
            NO_ROUTE,
        ),
        CaseDef::blocked(
            "audit_verify: a_non_auditor_is_refused_and_entries_stay_masked",
            SECTION,
            "a caller without `security.audit.read` is refused (and the refusal audited), an export needs the step-up, and no entry view carries a payload value",
            OWNER,
            NO_ROUTE,
        ),
        CaseDef::blocked(
            "audit_verify: the_nightly_pass_resumes_after_a_restart_and_tenants_are_isolated",
            SECTION,
            "the nightly full pass continues from its saved cursor after a restart, and a second tenant's audit entries are unreachable (a cursor into another lane answers like an empty page)",
            OWNER,
            NO_ROUTE,
        ),
    ]
}
