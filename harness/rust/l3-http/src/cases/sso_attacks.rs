//! The end-to-end single sign-on gate (WP-IAM-05.T14).
//!
//! The deliverable is a stub OIDC identity provider (discovery, rotating JWKS, authorisation
//! redirect, token endpoint) and a stub SAML IdP (metadata, signed responses) on a loopback port
//! the test adds to the server's egress allow-list (`[[services.sso.private_providers]]`), the happy
//! paths for both protocols, and the attack cases, each asserting the exact stable code and that no
//! session exists afterwards: tampered, replayed, missing or foreign `state` and binding cookie;
//! replayed code; wrong nonce; `alg: none`; an HMAC token keyed with the public key; wrong audience;
//! expired; every SAML signature-wrapping variant; assertion replay; `InResponseTo` mismatch;
//! open-redirect `return_to`; tenant B's identity at tenant A; a consumer provider resolving a
//! staff user; an SSRF attempt through a discovery URL; a downgrade to password after SSO; and the
//! policy cases (`sso_required` with a password, a break-glass administrator, a third administrator).
//!
//! Blocked, so the missing surface is visible in every run rather than faked: a signed-in flow
//! needs the production `SsoIdentityPort` (directory, invitation, just-in-time creation, hand-over
//! and session with `method = sso`), `SsoSecrets` (the client secret by name), a SAML provider
//! store, the policy source and the admin routes (`SsoAdminPort`). None exists in `spot serve`
//! yet (see the reports of WP-IAM-05.T11, T12 and T13). The library cases the gate would repeat
//! (wrapping suite, replay, nonce, algorithm) already run in `spot-sso`'s own tests, and the host
//! behaviours that need no identity provider run in `sso_logout.rs`.

use super::CaseDef;

/// After the sign-in policy case.
pub const ORDER: u32 = 36;

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::blocked(
            "sso_attacks: oidc_and_saml_sign_in_work_end_to_end_against_a_stub_identity_provider",
            "Single sign-on",
            "a stub OIDC and a stub SAML identity provider sign a person in end to end, including linking by invitation and just-in-time creation without roles",
            "WP-IAM-05.T11",
            "spot serve has no production SsoIdentityPort, SsoSecrets or SAML provider store, so a sign-in cannot complete",
        ),
        CaseDef::blocked(
            "sso_attacks: every_attack_case_is_refused_with_its_exact_code_and_leaves_no_session",
            "Single sign-on",
            "tampered or replayed state, foreign binding, replayed code, wrong nonce, alg none, HMAC with the public key, wrong audience, expiry, SAML wrapping, assertion replay, InResponseTo mismatch, open redirect, cross-tenant identity, consumer provider on a staff user, SSRF through discovery, and a downgrade to password are each refused",
            "WP-IAM-05.T11",
            "needs the same production ports as the happy path; the library-level cases run in spot-sso's tests",
        ),
        CaseDef::blocked(
            "sso_attacks: sso_required_refuses_passwords_except_the_two_break_glass_administrators",
            "Single sign-on",
            "with the policy required a password attempt is refused, a break-glass administrator signs in, a third administrator is refused",
            "WP-IAM-05.T12",
            "needs the policy source, the admin route and the host wiring of the policy port into the login service",
        ),
    ]
}
