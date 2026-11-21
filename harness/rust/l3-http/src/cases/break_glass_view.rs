//! The customer's view of break-glass over HTTP (WP-ADM-11.T2): the session list, the recording
//! read, the revoke and the automatic kill at expiry.
//!
//! Every case here is blocked. The behaviour is proven by `spot-cli`'s own tests over fakes (the
//! module `services/break_glass`, `cargo test -p spot-cli break_glass`), but over HTTP it needs
//! the support-access routes of WP-SEC-07.T3 mounted by the host on top of the service of
//! WP-SEC-07.T2 (which provides the audit chain, object store, sealer and duty ports this module
//! reads) and a fake agent bridge the harness can make unreachable; neither exists in the
//! fixture today. When they land, each blocked entry becomes a `CaseDef::run`.

use super::CaseDef;

/// After the support-access cases of the registry.
pub const ORDER: u32 = 331;

const SECTION: &str = "Break-glass (customer side)";
const OWNER: &str = "WP-SEC-07.T2";
const REASON: &str = "the host mounts no support-access routes and the fixture has no agent bridge to make unreachable";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "break_glass: a_session_appears_for_the_customer_within_a_second_of_approval",
            SECTION,
            "the security administrator's session list shows an approved break-glass session with who, why and window within a second of approval",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "break_glass: a_revoke_ends_the_session_and_later_actions_are_refused",
            SECTION,
            "revoking an active session answers at once and every later `authorise` is refused, even when the agent bridge is unreachable",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "break_glass: the_expiry_sweep_kills_a_session_the_operator_never_ended",
            SECTION,
            "a session past its window is killed and marked `expired` on the customer's chain without any report from the operator side",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "break_glass: the_recording_hash_matches_the_audit_chain_and_tamper_is_detected",
            SECTION,
            "a recording served to the administrator hashes to the digest on the audit chain; a changed archive object is refused",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "break_glass: an_operator_cannot_delete_or_rewrite_an_archive_object",
            SECTION,
            "a vendor-side delete or a second write under the same session id is refused and the object is unchanged",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "break_glass: a_non_administrator_cannot_read_a_recording_and_the_read_is_audited",
            SECTION,
            "a user without the security administrator duty is refused (never an empty recording); an allowed read is itself an event on the chain",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "break_glass: a_session_of_another_tenant_is_not_found",
            SECTION,
            "another tenant's session id answers exactly like a missing one on every operation",
            OWNER,
            REASON,
        ),
    ]
}
