//! The SAML attack suite: every known signature-wrapping, algorithm, transform, payload and
//! condition attack is built from a **valid, signed** document and must be refused with its
//! own code. A refusal for the wrong reason fails, because each test asserts the exact code.

#[path = "../saml_support/mod.rs"]
mod support;

mod conditions;
mod payloads;
mod wrapping;

use spot_sso::contract::block_on;
use spot_sso::saml::{SamlSignIn, complete};
use spot_sso::{IdpConfig, Jwa, MemoryReplayStore, SamlPendingAuth, SignaturePolicy, SpConfig};

use support::*;
/// The standard instant and pending values the suite shares.
pub const NOW: i64 = 1_700_000_000;
const REQUEST_ID: &str = "_request-1";
const RELAY: &str = "relay-state-1";

/// A test fixture: one fake IdP, one SP configuration, one instant.
pub struct Harness {
    idp: TestIdp,
    sp: SpConfig,
    now: i64,
}

impl Harness {
    /// A fresh provider and configuration.
    pub fn new() -> Self {
        Self {
            idp: TestIdp::new(),
            sp: sp_config(),
            now: NOW,
        }
    }

    /// The provider configuration that pins the fake IdP's certificates.
    pub fn provider(&self) -> IdpConfig {
        self.idp.idp_config("acme")
    }

    /// The pending sign-in a response must answer.
    pub fn pending(&self) -> SamlPendingAuth {
        pending(REQUEST_ID, RELAY, self.now)
    }

    /// Runs `complete` and returns the refusal code, panicking if the document was accepted.
    pub fn refusal(&self, xml: &str) -> &'static str {
        self.refusal_with(&self.sp, xml)
    }

    /// Runs `complete` under a specific SP configuration and returns the refusal code.
    pub fn refusal_with(&self, sp: &SpConfig, xml: &str) -> &'static str {
        self.refusal_provider(sp, &self.provider(), xml)
    }

    /// Runs `complete` with both a specific SP configuration and provider configuration.
    pub fn refusal_provider(&self, sp: &SpConfig, idp: &IdpConfig, xml: &str) -> &'static str {
        let replay = MemoryReplayStore::new();
        match block_on(complete(
            sp,
            idp,
            &self.pending(),
            &post(xml),
            Some(RELAY),
            self.now,
            &replay,
        )) {
            Ok(_) => panic!("the attack was accepted"),
            Err(error) => error.code(),
        }
    }

    /// Runs `complete` and unwraps the accepted sign-in.
    pub fn accept(&self, xml: &str) -> SamlSignIn {
        let replay = MemoryReplayStore::new();
        must(
            block_on(complete(
                &self.sp,
                &self.provider(),
                &self.pending(),
                &post(xml),
                Some(RELAY),
                self.now,
                &replay,
            )),
            "the positive case is accepted",
        )
    }

    /// A valid signed-assertion response for the standard assertion.
    pub fn signed(&self, alg: Jwa) -> String {
        let assertion = Assertion::valid("_assertion-1", self.now, Some(REQUEST_ID));
        signed_assertion_response(&self.idp, alg, self.now, REQUEST_ID, &assertion).xml
    }

    /// A valid signed-assertion response for a custom assertion.
    pub fn signed_for(&self, alg: Jwa, assertion: &Assertion) -> String {
        signed_assertion_response(&self.idp, alg, self.now, REQUEST_ID, assertion).xml
    }

    /// Signs an already-rendered (possibly mutated) assertion and wraps it in a `Response`.
    pub fn wrap_signed_assertion(&self, assertion_xml: &str, id: &str) -> String {
        let signed = sign_assertion_xml(&self.idp, Jwa::Rs256, assertion_xml, id);
        render_response(
            "samlp",
            "_response-1",
            ACS_URL,
            REQUEST_ID,
            None,
            STATUS_SUCCESS,
            &[signed],
            None,
        )
    }
}

impl Default for Harness {
    fn default() -> Self {
        Self::new()
    }
}

/// A signature policy the tests switch to when a response-level signature is under test.
pub fn response_policy() -> SpConfig {
    let mut sp = sp_config();
    sp.signature_policy = SignaturePolicy::AssertionOrResponse;
    sp
}

use proptest::prelude::*;

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Arbitrary input never panics the message entry points, and never verifies.
    #[test]
    fn arbitrary_responses_never_panic(text in ".{0,400}") {
        let h = Harness::new();
        let replay = MemoryReplayStore::new();
        let outcome = block_on(complete(
            &h.sp,
            &h.provider(),
            &h.pending(),
            &text,
            Some(RELAY),
            h.now,
            &replay,
        ));
        prop_assert!(outcome.is_err(), "an arbitrary response verified");
        let _ = spot_sso::saml::parse_idp_metadata(
            text.as_bytes(),
            &spot_sso::MetadataTrust::default(),
            h.now,
        );
        let _ = spot_sso::saml::request::decode_redirect(&text);
        let _ = spot_sso::saml::canonicalize(&text, false);
    }
}
