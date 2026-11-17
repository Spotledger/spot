//! Compliance evidence (WP-SEC-08.T3): the controls the server can evidence by itself are
//! collected from the hosted services, appended to the audit chain, and served read-only to the
//! auditor role.
//!
//! The service and its port (`CompliancePort`) are hosted by `spot-cli`
//! (`services/compliance.rs`), with unit tests over its fact sources. What these cases pin is the
//! HTTP surface, and no `compliance` route is mounted yet: the route file is a port file and a
//! route file in `spot-http` (WP-PRD-01.T28), which this task does not own. Each case is therefore
//! **blocked** with the observation that proves it, and becomes a `CaseDef::run` in this same file
//! (same name) when the route lands.

use super::CaseDef;

/// Runs after the other registered groups.
pub const ORDER: u32 = 60;

const SECTION: &str = "Compliance evidence";
const OWNER: &str = "WP-SEC-08.T3 (compliance routes)";
const REASON: &str = "no `/api/v1/compliance/*` route is mounted: the port exists in spot-cli, the route file is outside this task's owned paths";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "compliance: each_automatic_control_produces_evidence_from_a_seeded_state",
            SECTION,
            "`GET /compliance/controls` lists every automatic control with a latest result taken from the seeded subsystem state (audit chain, access review, SoD, retention, keys, backups, MFA, vulnerabilities)",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "compliance: a_disabled_retention_run_shows_as_a_gap",
            SECTION,
            "turning the retention run off makes the next collection report `ctl.retention` as failing",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "compliance: the_auditor_reads_the_pack_and_nothing_else",
            SECTION,
            "the auditor role reads the controls and the evidence pack (200) and is `403 permission.denied` on a customer document list; any other role is 403 on the pack",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "compliance: evidence_cannot_be_edited",
            SECTION,
            "there is no write route for evidence, and the chain proves an item changed after sealing no longer verifies",
            OWNER,
            REASON,
        ),
    ]
}
