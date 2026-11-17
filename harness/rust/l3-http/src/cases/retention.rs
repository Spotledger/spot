//! Retention enforced on a server (WP-SEC-05.T4). The job's behaviour (dry run changes nothing,
//! the confirmed digest, holds, the kill switch, resume, the proof) is covered by the module's
//! tests over the in-memory store; the cases here need the production retention store and the
//! administrator routes, which the host does not have yet, so they are blocked entries that
//! name the owner.

use super::CaseDef;

/// After `ai_audit`; the cases own their fixtures.
pub const ORDER: u32 = 32;

pub fn cases() -> Vec<CaseDef> {
    let reason = "no module provides the production `StoreProvider` (PostgreSQL ledger plus a document-lifecycle record source), so `[services.data_policy]` starts degraded";
    vec![
        CaseDef::blocked(
            "retention: a_dry_run_changes_nothing_and_a_confirmed_run_deletes_exactly_the_plan",
            "Retention",
            "a dry run leaves every record; a run with the confirmed digest deletes exactly the planned items and each has a verifiable proof",
            "WP-SEC-05.T4",
            reason,
        ),
        CaseDef::blocked(
            "retention: a_hold_added_mid_run_protects_the_held_records",
            "Retention",
            "a legal hold added between batches protects its records from the next batch on",
            "WP-SEC-05.T4",
            reason,
        ),
        CaseDef::blocked(
            "retention: a_statutory_floor_beats_a_shorter_rule",
            "Retention",
            "a record inside its statutory period is skipped whatever the rule says",
            "WP-SEC-05.T4",
            reason,
        ),
        CaseDef::blocked(
            "retention: a_run_killed_mid_way_resumes_without_double_deletion",
            "Retention",
            "a killed run continues from its checkpoint and no record is deleted twice",
            "WP-SEC-05.T4",
            reason,
        ),
        CaseDef::blocked(
            "retention: an_unconfirmed_or_modified_plan_is_refused_and_tenants_stay_apart",
            "Retention",
            "a run without the confirmed digest or with another plan's digest is refused, and tenant B's rule never touches tenant A",
            "WP-SEC-05.T4",
            reason,
        ),
        CaseDef::blocked(
            "retention: a_posted_ledger_document_is_never_deleted_by_a_raw_path",
            "Retention",
            "a posted document is deleted only through the document lifecycle",
            "WP-SEC-05.T4",
            reason,
        ),
    ]
}
