//! Support access (WP-SEC-07.T2): the customer approves, scopes and revokes support access, and
//! nothing is issued without a verified customer signature.
//!
//! The service is hosted by `spot-cli` (`services/support_access.rs`) and implements the
//! `SupportAccessPort` that the security routes of `spot-http` (WP-SEC-07.T3) serve. Those routes
//! are not in the route registry or the conformance list yet (they are mounted through their own
//! `registration()`), and the operator's intake and issuance are in-process ports, not HTTP. Each
//! case is therefore **blocked** with the observation that proves it; it becomes a `CaseDef::run`
//! in this file (same name) when the route group is registered.

use super::CaseDef;

/// Runs after the other registered groups.
pub const ORDER: u32 = 70;

const SECTION: &str = "Support access";
const OWNER: &str = "WP-SEC-07.T3 (route registration)";
const REASON: &str = "the `/api/v1/security/support-access/*` routes are not mounted by the host: the route group is not in the registry yet";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "support_access: a_request_without_consent_cannot_become_access",
            SECTION,
            "a pending request lists with zero approvals; issuing from it is refused; a consent by the wrong key, an expired consent, a wider scope and another tenant's request are `security.consent_invalid` or `security.not_found`",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: a_write_request_needs_two_administrators",
            SECTION,
            "the first approval leaves the request pending (2 needed); the same administrator again is refused; a second administrator approves it",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: revocation_takes_effect_on_the_next_action",
            SECTION,
            "a revoked request ends its sessions at once (`state` is `revoked`) and the audit chain holds the revocation",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: the_customer_sees_every_action_of_a_session",
            SECTION,
            "`GET .../sessions/{id}/recording` lists `at`, `action` and `target` from the audit chain, and a user who is not a security administrator gets `permission.denied`",
            OWNER,
            REASON,
        ),
    ]
}
