//! The support-access gate over HTTP with a hostile operator and a hostile tenant user
//! (WP-SEC-07.T4): no credential without a verified customer signature, revoke takes effect on
//! the next action, every session is visible to the security administrator only, the audit chain
//! is complete and the operator cannot delete an archive object or alter a chain entry.
//!
//! Every case is blocked today: the host mounts no support-access routes (WP-SEC-07.T3 ships them
//! behind ports, not registered in the route registry) and no module provides the customer side
//! (WP-SEC-07.T2, WP-ADM-11.T2 ports). Each entry becomes a `CaseDef::run` when they land; anything
//! that then unexpectedly passes is a security finding for Known security issues.

use super::CaseDef;

/// After the break-glass view cases.
pub const ORDER: u32 = 332;

const SECTION: &str = "Support access (attacks)";
const OWNER: &str = "WP-SEC-07.T2";
const REASON: &str = "the host mounts no support-access routes and no module provides the customer side of support access";

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "support_access: a_consent_signed_by_the_wrong_key_is_refused_and_issues_no_credential",
            SECTION,
            "a consent signed with a key that is not the tenant's registered customer key answers the stable code and no credential exists afterwards",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: a_replayed_receipt_nonce_is_refused",
            SECTION,
            "the same receipt nonce presented twice (also from a second node) is refused the second time",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: an_expired_consent_a_consent_for_another_scope_and_another_tenants_request_are_refused",
            SECTION,
            "each refusal carries its stable code, writes an audit record and leaves no credential; the other tenant's request answers like a missing one",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: a_two_person_consent_cannot_be_confirmed_by_its_own_requester",
            SECTION,
            "when the policy asks for a second administrator, the user who gave the consent cannot also confirm it",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: a_revoked_credential_is_refused_on_the_next_action_and_the_chain_verifies",
            SECTION,
            "request, consent, issue, use and revoke appear in order on the audit chain, the chain verifies, and the next action under the credential is refused",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: a_normal_user_sees_no_support_sessions_and_a_recording_read_is_audited",
            SECTION,
            "a user without the security administrator duty sees nothing and is refused (never an empty list); the administrator sees sessions and recordings and each recording read is itself on the chain",
            OWNER,
            REASON,
        ),
        CaseDef::blocked(
            "support_access: an_operator_cannot_delete_an_archive_object_or_alter_a_chain_entry",
            SECTION,
            "a delete or rewrite of an archive object, or an edit of a chain entry, is refused and the chain still verifies",
            OWNER,
            REASON,
        ),
    ]
}
