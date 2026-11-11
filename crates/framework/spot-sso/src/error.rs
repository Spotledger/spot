//! The one failure type of this crate, with a stable `sso.*` code and **two texts**.
//!
//! Every failure carries a `code` (for logs, metrics and the error catalogue), a `public`
//! message that is safe to show an end user — it never says *which* check failed, so a
//! passer-by cannot probe the tenant's policy or a token's contents — and an `admin` message
//! that tells a tenant administrator what to look at. A short, non-secret `detail` may name
//! the field or provider involved; it is never a token, code, verifier or secret.
//!
//! The catalogue file `catalogue/sso.toml` in `spot-errors` maps `public` to its `summary`
//! and `admin` to its `remedy`; the two-text split lives on this type because the catalogue
//! schema carries one summary and one remedy per code.
//!
//! [`SsoErrors`] collects every problem of a load or a policy check, so callers report them
//! all at once instead of one at a time.

use std::fmt;

/// The stable `sso.*` codes this crate raises, as listed in the error catalogue.
pub mod codes {
    /// Local sign-in is refused while single sign-on is required.
    pub const REQUIRED: &str = "sso.required";
    /// Fewer than two break-glass administrators would remain while SSO is required.
    pub const BREAK_GLASS_TOO_FEW: &str = "sso.break_glass_too_few";
    /// A break-glass entry is not an enabled tenant administrator.
    pub const BREAK_GLASS_NOT_ADMIN: &str = "sso.break_glass_not_admin";
    /// A break-glass administrator has no local password.
    pub const BREAK_GLASS_NO_PASSWORD: &str = "sso.break_glass_no_password";
    /// A break-glass administrator has no TOTP or hardware second factor.
    pub const BREAK_GLASS_NO_SECOND_FACTOR: &str = "sso.break_glass_no_second_factor";
    /// No configured provider is healthy while SSO is required.
    pub const NO_PROVIDER: &str = "sso.no_provider";
    /// A policy change would lock the tenant out.
    pub const POLICY_LOCKOUT: &str = "sso.policy_lockout";
    /// Single sign-on is refused under a local-only policy.
    pub const SSO_NOT_ALLOWED: &str = "sso.sso_not_allowed";
    /// The provider is not trusted by the tenant's policy.
    pub const PROVIDER_NOT_CONFIGURED: &str = "sso.provider_not_configured";
    /// A user id the directory does not know.
    pub const UNKNOWN_USER: &str = "sso.unknown_user";

    /// The configured issuer is not a secure, canonical URL.
    pub const ISSUER_INVALID: &str = "sso.issuer_invalid";
    /// The client id is missing.
    pub const CLIENT_ID_MISSING: &str = "sso.client_id_missing";
    /// A client credential reference is missing or looks like a pasted secret.
    pub const SECRET_REF_INVALID: &str = "sso.secret_ref_invalid";
    /// The scopes do not include `openid`.
    pub const SCOPES_MISSING_OPENID: &str = "sso.scopes_missing_openid";
    /// The redirect URI is insecure or has a wildcard or fragment.
    pub const REDIRECT_URI_INVALID: &str = "sso.redirect_uri_invalid";
    /// The discovery URL is not a secure URL.
    pub const DISCOVERY_URL_INVALID: &str = "sso.discovery_url_invalid";
    /// An algorithm outside RS256/ES256 was configured.
    pub const ALG_UNSUPPORTED: &str = "sso.alg_unsupported";
    /// The tolerated clock skew is outside 0 to 120 seconds.
    pub const CLOCK_SKEW_EXCESSIVE: &str = "sso.clock_skew_excessive";
    /// A claim mapping names no claim.
    pub const CLAIM_MAP_INVALID: &str = "sso.claim_map_invalid";

    /// The `return_to` path is not a same-site relative path.
    pub const BAD_RETURN_TO: &str = "sso.bad_return_to";
    /// Fewer than 32 random bytes were supplied to begin a sign-in.
    pub const RNG_INSUFFICIENT: &str = "sso.rng_insufficient";

    /// The callback `state` did not match the pending sign-in.
    pub const STATE_MISMATCH: &str = "sso.state_mismatch";
    /// The pending sign-in has expired.
    pub const STATE_EXPIRED: &str = "sso.state_expired";
    /// The pending sign-in was already used.
    pub const STATE_REPLAYED: &str = "sso.state_replayed";
    /// The identity provider returned an error.
    pub const IDP_ERROR: &str = "sso.idp_error";
    /// A required parameter is missing from a response.
    pub const MISSING_PARAM: &str = "sso.missing_param";

    /// The discovery document disagrees with the configuration.
    pub const DISCOVERY_MISMATCH: &str = "sso.discovery_mismatch";
    /// The JWKS document could not be read.
    pub const JWKS_INVALID: &str = "sso.jwks_invalid";
    /// An RSA key is smaller than 2048 bits.
    pub const KEY_TOO_SMALL: &str = "sso.key_too_small";
    /// The token's key id is not in the key set; a refresh may help.
    pub const JWKS_STALE: &str = "sso.jwks_stale";

    /// The token is not a readable compact JWS.
    pub const TOKEN_MALFORMED: &str = "sso.token_malformed";
    /// The header carries `jku`, `x5u` or an embedded `jwk`.
    pub const HEADER_UNTRUSTED: &str = "sso.header_untrusted";
    /// The header algorithm does not agree with the key.
    pub const ALG_MISMATCH: &str = "sso.alg_mismatch";
    /// The signature did not verify.
    pub const SIGNATURE_INVALID: &str = "sso.signature_invalid";
    /// The issuer claim does not match the configuration.
    pub const ISSUER_MISMATCH: &str = "sso.issuer_mismatch";
    /// The audience does not contain the client id.
    pub const AUDIENCE_MISMATCH: &str = "sso.audience_mismatch";
    /// With several audiences, `azp` does not name the client id.
    pub const AZP_MISMATCH: &str = "sso.azp_mismatch";
    /// The token has expired.
    pub const TOKEN_EXPIRED: &str = "sso.token_expired";
    /// The token is not valid yet.
    pub const TOKEN_NOT_YET_VALID: &str = "sso.token_not_yet_valid";
    /// The `iat` claim is in the future beyond skew.
    pub const ISSUED_AT_INVALID: &str = "sso.issued_at_invalid";
    /// The nonce does not match the pending sign-in.
    pub const NONCE_MISMATCH: &str = "sso.nonce_mismatch";
    /// The `at_hash` does not match the access token.
    pub const AT_HASH_MISMATCH: &str = "sso.at_hash_mismatch";
    /// The `acr` does not meet the tenant's required values.
    pub const ACR_INSUFFICIENT: &str = "sso.acr_insufficient";
    /// A verified e-mail is required and the token lacks one.
    pub const EMAIL_NOT_VERIFIED: &str = "sso.email_not_verified";
    /// The PKCE verifier no longer matches its challenge.
    pub const PKCE_MISMATCH: &str = "sso.pkce_mismatch";
    /// The configured subject claim is missing from the token.
    pub const CLAIM_MISSING: &str = "sso.claim_missing";

    /// The identity is not linked, invited or allowed for just-in-time creation.
    pub const UNKNOWN_IDENTITY: &str = "sso.unknown_identity";
    /// The linked user is disabled.
    pub const USER_DISABLED: &str = "sso.user_disabled";
    /// The identity is already linked to another user.
    pub const IDENTITY_OTHER_USER: &str = "sso.identity_other_user";
    /// Linking needs the user to prove control of an existing local account.
    pub const LINK_REQUIRES_PROOF: &str = "sso.link_requires_proof";
    /// Just-in-time creation is off.
    pub const JIT_NOT_ALLOWED: &str = "sso.jit_not_allowed";

    /// The provider publishes no end-session endpoint.
    pub const NO_END_SESSION: &str = "sso.no_end_session";
    /// The post-logout redirect does not match the registered URI.
    pub const LOGOUT_REDIRECT: &str = "sso.logout_redirect";

    // Back-channel logout (WP-IAM-05.T3).
    /// A back-channel logout token carries no `jti`.
    pub const LOGOUT_TOKEN_MISSING_JTI: &str = "sso.logout_token_missing_jti";
    /// The logout token's `jti` was already used.
    pub const LOGOUT_TOKEN_REPLAYED: &str = "sso.logout_token_replayed";
    /// The logout token carries no back-channel-logout `events` member.
    pub const LOGOUT_TOKEN_EVENTS_MISSING: &str = "sso.logout_token_events_missing";
    /// The token carries a `nonce`, so it is an ID token, not a logout token.
    pub const LOGOUT_TOKEN_NONCE_PRESENT: &str = "sso.logout_token_nonce_present";
    /// The logout token names neither a subject nor a session.
    pub const LOGOUT_TOKEN_NO_TARGET: &str = "sso.logout_token_no_target";
    /// The logout token carries no usable integer `exp`.
    pub const LOGOUT_TOKEN_NO_EXP: &str = "sso.logout_token_no_exp";
    /// The logout token's `exp` has passed.
    pub const LOGOUT_TOKEN_EXPIRED: &str = "sso.logout_token_expired";
    /// The logout token lives longer than the provider's configured maximum.
    pub const LOGOUT_TOKEN_LIFETIME_EXCESSIVE: &str = "sso.logout_token_lifetime_excessive";
    /// A logout token's `exp` is not after its `iat`: its times contradict each other.
    pub const LOGOUT_TOKEN_MALFORMED_TIMES: &str = "sso.logout_token_malformed_times";
    /// The configured maximum logout-token lifetime is outside its bounds.
    pub const LOGOUT_LIFETIME_INVALID: &str = "sso.logout_lifetime_invalid";

    // Replay store (WP-IAM-05.T5).
    /// The replay store could not answer, so the message was refused (fail closed).
    pub const REPLAY_STORE_UNAVAILABLE: &str = "sso.replay_store_unavailable";

    // SAML 2.0 (WP-IAM-05.T2). The catalogue file is `sso.toml`'s prefix is `sso`, so the
    // codes carry the `sso.saml.` prefix and are grouped here for readability.
    /// The IdP metadata document could not be read.
    pub const SAML_METADATA_INVALID: &str = "sso.saml.metadata_invalid";
    /// The IdP metadata is unsigned and not explicitly trusted out of band.
    pub const SAML_METADATA_UNSIGNED: &str = "sso.saml.metadata_unsigned";
    /// The signature on the IdP metadata did not verify against the pinned certificate.
    pub const SAML_METADATA_SIGNATURE_INVALID: &str = "sso.saml.metadata_signature_invalid";
    /// The IdP metadata has expired.
    pub const SAML_METADATA_EXPIRED: &str = "sso.saml.metadata_expired";
    /// The IdP metadata carries no usable signing certificate.
    pub const SAML_NO_SIGNING_CERTIFICATE: &str = "sso.saml.no_signing_certificate";
    /// The IdP metadata publishes no single-sign-on endpoint.
    pub const SAML_NO_SSO_ENDPOINT: &str = "sso.saml.no_sso_endpoint";
    /// A size cap on a signed relay (the POSTed response) was exceeded.
    pub const SAML_TOO_LARGE: &str = "sso.saml.too_large";
    /// The `SAMLResponse` is not Base64.
    pub const SAML_NOT_BASE64: &str = "sso.saml.not_base64";
    /// The decoded `SAMLResponse` is not UTF-8.
    pub const SAML_NOT_UTF8: &str = "sso.saml.not_utf8";
    /// The response carries a DTD, an entity declaration or an external reference.
    pub const SAML_DTD_FORBIDDEN: &str = "sso.saml.dtd_forbidden";
    /// The response is not well-formed XML.
    pub const SAML_XML_INVALID: &str = "sso.saml.xml_invalid";
    /// The XML root is not a `samlp:Response`.
    pub const SAML_NOT_A_RESPONSE: &str = "sso.saml.not_a_response";
    /// The response carries more than one `Assertion`.
    pub const SAML_MULTIPLE_ASSERTIONS: &str = "sso.saml.multiple_assertions";
    /// The response carries no `Assertion`.
    pub const SAML_ASSERTION_MISSING: &str = "sso.saml.assertion_missing";
    /// An encrypted assertion was sent without a configured decryption key.
    pub const SAML_ENCRYPTED_UNSUPPORTED: &str = "sso.saml.encrypted_unsupported";
    /// The document carries an `Extensions`, `Object` or wrapper element that could hide a
    /// second copy of the data that was verified.
    pub const SAML_WRAPPER_FORBIDDEN: &str = "sso.saml.wrapper_forbidden";
    /// The `Destination` is not this service provider's ACS URL.
    pub const SAML_DESTINATION_MISMATCH: &str = "sso.saml.destination_mismatch";
    /// `InResponseTo` does not name the pending request.
    pub const SAML_IN_RESPONSE_TO_MISMATCH: &str = "sso.saml.in_response_to_mismatch";
    /// An unsolicited response arrived and IdP-initiated sign-in is off.
    pub const SAML_UNSOLICITED: &str = "sso.saml.unsolicited";
    /// The response status is not success.
    pub const SAML_STATUS_NOT_SUCCESS: &str = "sso.saml.status_not_success";
    /// No signature was found where the policy requires one.
    pub const SAML_SIGNATURE_MISSING: &str = "sso.saml.signature_missing";
    /// More than one `ds:Signature` element was found.
    pub const SAML_SIGNATURE_MULTIPLE: &str = "sso.saml.signature_multiple";
    /// The signature is not an enveloped signature over the element it is attached to.
    pub const SAML_SIGNATURE_NOT_ENVELOPED: &str = "sso.saml.signature_not_enveloped";
    /// The signature did not verify against the pinned IdP certificates.
    pub const SAML_SIGNATURE_INVALID: &str = "sso.saml.signature_invalid";
    /// The signature reference is not exactly one `URI="#id"` naming the signed element.
    pub const SAML_REFERENCE_INVALID: &str = "sso.saml.reference_invalid";
    /// An XML `ID` value appears more than once in the document.
    pub const SAML_DUPLICATE_ID: &str = "sso.saml.duplicate_id";
    /// A signature, digest or canonicalisation algorithm outside the allow-list was used.
    pub const SAML_ALG_UNSUPPORTED: &str = "sso.saml.alg_unsupported";
    /// A transform outside enveloped-signature and exclusive canonicalisation was used.
    pub const SAML_TRANSFORM_UNSUPPORTED: &str = "sso.saml.transform_unsupported";
    /// The recomputed digest does not match the signed `DigestValue`.
    pub const SAML_DIGEST_MISMATCH: &str = "sso.saml.digest_mismatch";
    /// The document contains an assertion that the verified signature does not cover.
    pub const SAML_UNSIGNED_ASSERTION: &str = "sso.saml.unsigned_assertion";
    /// A value's text is split by a comment or processing instruction.
    pub const SAML_COMMENT_IN_VALUE: &str = "sso.saml.comment_in_value";
    /// The `Issuer` does not equal the configured IdP entity id.
    pub const SAML_ISSUER_MISMATCH: &str = "sso.saml.issuer_mismatch";
    /// The assertion is expired or not yet valid.
    pub const SAML_CONDITIONS_INVALID: &str = "sso.saml.conditions_invalid";
    /// The audience does not contain this service provider's entity id.
    pub const SAML_AUDIENCE_MISMATCH: &str = "sso.saml.audience_mismatch";
    /// The `SubjectConfirmation` is not a valid bearer confirmation for this ACS URL.
    pub const SAML_SUBJECT_CONFIRMATION: &str = "sso.saml.subject_confirmation";
    /// The assertion carries no `AuthnStatement`.
    pub const SAML_NO_AUTHN_STATEMENT: &str = "sso.saml.no_authn_statement";
    /// The authentication context does not meet the tenant's requirement.
    pub const SAML_AUTHN_CONTEXT: &str = "sso.saml.authn_context";
    /// The `NameID` is missing, malformed, or transient (which cannot be linked).
    pub const SAML_NAMEID_INVALID: &str = "sso.saml.nameid_invalid";
    /// A configured identity attribute is missing from the assertion.
    pub const SAML_ATTRIBUTE_MISSING: &str = "sso.saml.attribute_missing";
    /// The assertion was already used (replay).
    pub const SAML_REPLAY: &str = "sso.saml.replay";
    /// The assertion's issuer is not a configured identity provider.
    pub const SAML_PROVIDER_NOT_CONFIGURED: &str = "sso.saml.provider_not_configured";
    /// No SP signing key is configured where the caller asked for a signed redirect.
    pub const SAML_SIGNING_KEY_MISSING: &str = "sso.saml.signing_key_missing";

    // SAML IdP-initiated single logout (WP-IAM-05.T3).
    /// The XML root is not a `samlp:LogoutRequest`.
    pub const SAML_SLO_NOT_A_REQUEST: &str = "sso.saml.slo_not_a_request";
    /// The `LogoutRequest` `Destination` is not this SP's single-logout URL.
    pub const SAML_SLO_DESTINATION_MISMATCH: &str = "sso.saml.slo_destination_mismatch";
    /// The `LogoutRequest` has expired.
    pub const SAML_SLO_EXPIRED: &str = "sso.saml.slo_expired";
    /// The LogoutRequest's `NotOnOrAfter` lies further ahead than the configured maximum.
    pub const SAML_SLO_LIFETIME_EXCESSIVE: &str = "sso.saml.slo_lifetime_excessive";
    /// The `LogoutRequest` `ID` was already used (replay).
    pub const SAML_SLO_REPLAY: &str = "sso.saml.slo_replay";
    /// The `LogoutRequest` carries no usable `NameID`.
    pub const SAML_SLO_NAME_ID_MISSING: &str = "sso.saml.slo_name_id_missing";
    /// The SP or IdP has no single-logout endpoint configured.
    pub const SAML_SLO_ENDPOINT_MISSING: &str = "sso.saml.slo_endpoint_missing";
}

/// One single sign-on failure: a stable code, a public text and an administrator text.
///
/// The public text is deliberately generic — it never names the check that failed, so it is
/// safe to return to whoever started the sign-in. The administrator text names what to fix.
#[derive(Clone, PartialEq, Eq)]
pub struct SsoError {
    code: &'static str,
    public: &'static str,
    admin: &'static str,
    detail: Option<String>,
}

impl SsoError {
    /// Builds an error from its code and its two texts.
    #[must_use]
    pub const fn new(code: &'static str, public: &'static str, admin: &'static str) -> Self {
        Self {
            code,
            public,
            admin,
            detail: None,
        }
    }

    /// The same error with a short, non-secret note (a field name, a provider id).
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The message safe to show an end user.
    #[must_use]
    pub fn public_message(&self) -> &'static str {
        self.public
    }

    /// The message for a tenant administrator.
    #[must_use]
    pub fn admin_message(&self) -> &'static str {
        self.admin
    }

    /// The short, non-secret note naming the field or provider involved, if any.
    #[must_use]
    pub fn detail(&self) -> Option<&str> {
        self.detail.as_deref()
    }
}

impl fmt::Display for SsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Only the public text is ever formatted. The code (which names the failed check) and
        // the admin text stay out, so a stray `{}` shown to an end user leaks neither which
        // check failed nor what an administrator would do about it.
        f.write_str(self.public)
    }
}

impl fmt::Debug for SsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The detail is deliberate and non-secret; echo it for operators.
        match &self.detail {
            Some(detail) => write!(f, "SsoError({}, detail={detail:?})", self.code),
            None => write!(f, "SsoError({})", self.code),
        }
    }
}

impl std::error::Error for SsoError {}

/// Several [`SsoError`]s at once: every problem of a load, a config or a policy check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SsoErrors {
    problems: Vec<SsoError>,
}

impl SsoErrors {
    /// Collects the problems; an empty list is a programming error, so it is kept as given.
    #[must_use]
    pub fn new(problems: Vec<SsoError>) -> Self {
        Self { problems }
    }

    /// One problem as a list.
    #[must_use]
    pub fn one(problem: SsoError) -> Self {
        Self {
            problems: vec![problem],
        }
    }

    /// The problems.
    #[must_use]
    pub fn problems(&self) -> &[SsoError] {
        &self.problems
    }

    /// The stable codes, in order.
    pub fn codes(&self) -> impl Iterator<Item = &'static str> + '_ {
        self.problems.iter().map(SsoError::code)
    }

    /// Whether any problem carries `code`.
    #[must_use]
    pub fn has_code(&self, code: &str) -> bool {
        self.problems.iter().any(|p| p.code == code)
    }
}

impl fmt::Display for SsoErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let codes: Vec<&str> = self.codes().collect();
        write!(
            f,
            "{} single sign-on problem(s): {}",
            codes.len(),
            codes.join(", ")
        )
    }
}

impl std::error::Error for SsoErrors {}

/// The crate's single-failure result type.
pub type Result<T> = core::result::Result<T, SsoError>;

/// The crate's multi-failure result type (loads, configs and policy checks).
pub type ResultList<T> = core::result::Result<T, SsoErrors>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_stable() {
        let cases: Vec<(SsoError, &str)> = vec![
            (required(), codes::REQUIRED),
            (
                SsoError::new(codes::BREAK_GLASS_TOO_FEW, "a", "b"),
                "sso.break_glass_too_few",
            ),
            (SsoError::new(codes::JWKS_STALE, "a", "b"), "sso.jwks_stale"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "code drifted for {error:?}");
        }
    }

    #[test]
    fn display_shows_only_the_public_text() {
        let error =
            SsoError::new(codes::REQUIRED, "public text", "admin text").with_detail("field");
        let shown = format!("{error}");
        assert_eq!(shown, "public text");
        assert!(!shown.contains("admin text"));
        assert!(!shown.contains("sso.required"));
        assert!(!shown.contains("field"));
        assert_eq!(error.detail(), Some("field"));
        assert_eq!(error.code(), codes::REQUIRED);
    }

    #[test]
    fn list_reports_every_code() {
        let list = SsoErrors::new(vec![
            required(),
            SsoError::new(codes::NO_PROVIDER, "a", "b"),
        ]);
        assert!(list.has_code(codes::REQUIRED));
        assert!(list.has_code(codes::NO_PROVIDER));
        assert_eq!(list.codes().count(), 2);
    }

    /// The one constructor reused above.
    fn required() -> SsoError {
        SsoError::new(
            codes::REQUIRED,
            "Local sign-in is not available for this tenant.",
            "Sign in with single sign-on, or use a break-glass administrator account.",
        )
    }
}
