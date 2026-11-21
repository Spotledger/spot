//! The SAML flow end to end: metadata, the `AuthnRequest`, a positive sign-in with a signed
//! assertion and with a signed response, replay, logout, and the shape refusals.

#[path = "saml_support/mod.rs"]
mod support;

use spot_sso::contract::block_on;
use spot_sso::saml::{
    BeginOptions, build_logout_request, complete, parse_idp_metadata, sp_metadata,
    validate_logout_response,
};
use spot_sso::{
    Jwa, MemoryReplayStore, MetadataTrust, SamlBinding, SamlClaimsMap, SignaturePolicy,
};

use support::*;

const NOW: i64 = 1_700_000_000;
const REQUEST_ID: &str = "_request-1";
const RELAY: &str = "relay-state-1";

/// A positive sign-in with an RS256 assertion, read back through `complete`.
#[test]
fn happy_path_signed_assertion_rs256() {
    let idp = TestIdp::new();
    let assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    let signed = signed_assertion_response(&idp, Jwa::Rs256, NOW, REQUEST_ID, &assertion);
    let pending = pending(REQUEST_ID, RELAY, NOW);
    let replay = MemoryReplayStore::new();
    let outcome = must(
        block_on(complete(
            &sp_config(),
            &idp.idp_config("acme"),
            &pending,
            &post(&signed.xml),
            Some(RELAY),
            NOW,
            &replay,
        )),
        "complete",
    );
    assert_eq!(outcome.identity.issuer, ISSUER);
    assert_eq!(outcome.identity.subject, "ada@example.com");
    assert_eq!(outcome.identity.email.as_deref(), Some("ada@example.com"));
    assert_eq!(
        outcome.identity.display_name.as_deref(),
        Some("Ada Lovelace")
    );
    assert_eq!(outcome.identity.groups, vec!["engineering".to_owned()]);
    assert_eq!(outcome.identity.acr.as_deref(), Some(ACR_SILVER));
    assert_eq!(outcome.identity.amr, vec![ACR_SILVER.to_owned()]);
    assert_eq!(outcome.identity.auth_time, Some(NOW));
    assert_eq!(outcome.session_not_on_or_after, Some(NOW + 7200));
    assert!(!outcome.identity.raw_claims_hash.is_empty());
}

/// The same with an ES256 assertion.
#[test]
fn happy_path_signed_assertion_es256() {
    let idp = TestIdp::new();
    let assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    let signed = signed_assertion_response(&idp, Jwa::Es256, NOW, REQUEST_ID, &assertion);
    let replay = MemoryReplayStore::new();
    let outcome = must(
        block_on(complete(
            &sp_config(),
            &idp.idp_config("acme"),
            &pending(REQUEST_ID, RELAY, NOW),
            &post(&signed.xml),
            Some(RELAY),
            NOW,
            &replay,
        )),
        "complete",
    );
    assert_eq!(outcome.identity.subject, "ada@example.com");
}

/// A fake Entra-style IdP: it signs the whole `Response`, not the assertion, and the tenant
/// allows that explicitly.
#[test]
fn happy_path_signed_response_entra_style() {
    let idp = TestIdp::new();
    let mut sp = sp_config();
    sp.signature_policy = SignaturePolicy::AssertionOrResponse;
    let mut assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    assertion.prefix = "saml2".to_owned();
    let signed = signed_response(&idp, Jwa::Rs256, NOW, REQUEST_ID, &assertion);
    let replay = MemoryReplayStore::new();
    let outcome = must(
        block_on(complete(
            &sp,
            &idp.idp_config("entra"),
            &pending(REQUEST_ID, RELAY, NOW),
            &post(&signed.xml),
            Some(RELAY),
            NOW,
            &replay,
        )),
        "complete",
    );
    assert_eq!(outcome.identity.subject, "ada@example.com");
    assert_eq!(
        outcome.identity.display_name.as_deref(),
        Some("Ada Lovelace")
    );
}

/// A fake Okta-style IdP: the assertion uses the default namespace and a signed assertion.
#[test]
fn happy_path_okta_style_default_namespace() {
    let idp = TestIdp::new();
    let mut assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    assertion.prefix = String::new();
    let signed = signed_assertion_response(&idp, Jwa::Rs256, NOW, REQUEST_ID, &assertion);
    let replay = MemoryReplayStore::new();
    let outcome = must(
        block_on(complete(
            &sp_config(),
            &idp.idp_config("okta"),
            &pending(REQUEST_ID, RELAY, NOW),
            &post(&signed.xml),
            Some(RELAY),
            NOW,
            &replay,
        )),
        "complete",
    );
    assert_eq!(outcome.identity.subject, "ada@example.com");
}

/// A second use of the same assertion id is a replay.
#[test]
fn a_replayed_assertion_is_refused() {
    let idp = TestIdp::new();
    let assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    let signed = signed_assertion_response(&idp, Jwa::Rs256, NOW, REQUEST_ID, &assertion);
    let replay = MemoryReplayStore::new();
    must(
        block_on(complete(
            &sp_config(),
            &idp.idp_config("acme"),
            &pending(REQUEST_ID, RELAY, NOW),
            &post(&signed.xml),
            Some(RELAY),
            NOW,
            &replay,
        )),
        "the first use succeeds",
    );
    let error = err(block_on(complete(
        &sp_config(),
        &idp.idp_config("acme"),
        &pending(REQUEST_ID, RELAY, NOW),
        &post(&signed.xml),
        Some(RELAY),
        NOW,
        &replay,
    )));
    assert_eq!(error.code(), "sso.saml.replay");
}

/// A mismatched relay state is refused before the signature is even considered.
#[test]
fn relay_state_mismatch_is_refused() {
    let idp = TestIdp::new();
    let assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    let signed = signed_assertion_response(&idp, Jwa::Rs256, NOW, REQUEST_ID, &assertion);
    let replay = MemoryReplayStore::new();
    let error = err(block_on(complete(
        &sp_config(),
        &idp.idp_config("acme"),
        &pending(REQUEST_ID, RELAY, NOW),
        &post(&signed.xml),
        Some("not-the-relay"),
        NOW,
        &replay,
    )));
    assert_eq!(error.code(), "sso.state_mismatch");
}

/// `begin` builds a redirect whose `SAMLRequest` decodes to the expected `AuthnRequest`.
#[test]
fn begin_builds_a_redirect_authn_request() {
    let idp = TestIdp::new();
    let (request, pending) = must(
        spot_sso::saml::begin(
            &sp_config(),
            &idp.idp_config("acme"),
            &rng_bytes(7),
            NOW,
            "/app/invoices",
            BeginOptions::default(),
        ),
        "begin",
    );
    assert!(pending.request_id.starts_with('_'));
    assert!(!pending.relay_state.is_empty());
    let query = request
        .url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    let mut saml_request = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "SAMLRequest" {
            saml_request = Some(value.into_owned());
        }
    }
    let xml = must(
        spot_sso::saml::request::decode_redirect(&saml_request.unwrap_or_default()),
        "decode the redirect",
    );
    assert!(xml.contains("<samlp:AuthnRequest"));
    assert!(xml.contains(&format!("ID=\"{}\"", pending.request_id)));
    assert!(xml.contains(&format!("AssertionConsumerServiceURL=\"{ACS_URL}\"")));
    assert!(xml.contains("ProtocolBinding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\""));
    assert!(xml.contains("NameIDPolicy"));
    assert!(!xml.contains("ForceAuthn"));
    assert!(request.signing.is_none());
}

/// Step-up forces a fresh authentication.
#[test]
fn begin_step_up_forces_authn() {
    let idp = TestIdp::new();
    let (request, _pending) = must(
        spot_sso::saml::begin(
            &sp_config(),
            &idp.idp_config("acme"),
            &rng_bytes(9),
            NOW,
            "/app",
            BeginOptions { force_authn: true },
        ),
        "begin",
    );
    let query = request
        .url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    let mut saml_request = None;
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "SAMLRequest" {
            saml_request = Some(value.into_owned());
        }
    }
    let xml = must(
        spot_sso::saml::request::decode_redirect(&saml_request.unwrap_or_default()),
        "decode the redirect",
    );
    assert!(xml.contains("ForceAuthn=\"true\""));
}

/// A return path outside this site is refused.
#[test]
fn begin_refuses_an_open_redirect() {
    let idp = TestIdp::new();
    let error = err(spot_sso::saml::begin(
        &sp_config(),
        &idp.idp_config("acme"),
        &rng_bytes(1),
        NOW,
        "https://evil.example",
        BeginOptions::default(),
    ));
    assert_eq!(error.code(), "sso.bad_return_to");
}

/// SP metadata is deterministic and declares the ACS and signed-assertion requirement.
#[test]
fn sp_metadata_is_deterministic() {
    let sp = sp_config();
    let first = sp_metadata(&sp);
    let second = sp_metadata(&sp);
    assert_eq!(first, second);
    assert!(first.contains(&format!("entityID=\"{SP_ENTITY_ID}\"")));
    assert!(first.contains(&format!("Location=\"{ACS_URL}\"")));
    assert!(first.contains("WantAssertionsSigned=\"true\""));
    assert!(first.contains("HTTP-POST"));
}

/// Unsigned IdP metadata is refused unless explicitly trusted out of band.
#[test]
fn unsigned_metadata_needs_explicit_trust() {
    let idp = TestIdp::new();
    let metadata = metadata_xml(&idp, true, "_md-1");
    let refused = err(parse_idp_metadata(
        metadata.as_bytes(),
        &MetadataTrust::default(),
        NOW,
    ));
    assert_eq!(refused.code(), "sso.saml.metadata_unsigned");

    let parsed = must(
        parse_idp_metadata(
            metadata.as_bytes(),
            &MetadataTrust {
                pinned_certificate: None,
                trust_unsigned: true,
            },
            NOW,
        ),
        "trusted metadata parses",
    );
    assert_eq!(parsed.entity_id, ISSUER);
    assert_eq!(parsed.signing_certificates.len(), 1);
    assert_eq!(parsed.sso_endpoints[0].binding, SamlBinding::Redirect);
    let config = must(
        parsed.into_config(
            spot_sso::ProviderId::new("acme"),
            SamlClaimsMap::default(),
            Vec::new(),
        ),
        "into_config",
    );
    assert_eq!(config.entity_id, ISSUER);
}

/// A signed IdP metadata document verifies with the pinned certificate.
#[test]
fn signed_metadata_verifies() {
    let idp = TestIdp::new();
    let unsigned = metadata_xml(&idp, true, "_md-1");
    let signed = sign_metadata(&idp, Jwa::Rs256, &unsigned, "_md-1");
    let parsed = must(
        parse_idp_metadata(
            signed.as_bytes(),
            &MetadataTrust {
                pinned_certificate: Some(idp.certificate(Jwa::Rs256)),
                trust_unsigned: false,
            },
            NOW,
        ),
        "signed metadata verifies",
    );
    assert_eq!(parsed.entity_id, ISSUER);
}

/// The logout request builder and response validation.
#[test]
fn logout_round_trip() {
    let idp = TestIdp::new();
    let request = must(
        build_logout_request(
            &sp_config(),
            &idp.idp_config("acme"),
            &rng_bytes(3),
            NOW,
            "ada@example.com",
            spot_sso::saml::NAMEID_PERSISTENT,
            None,
        ),
        "build_logout_request",
    );
    assert!(request.request_id.starts_with('_'));
    let query = request
        .url
        .split_once('?')
        .map(|(_, query)| query)
        .unwrap_or("");
    assert!(query.contains("SAMLRequest="));
    let response = format!(
        "<samlp:LogoutResponse xmlns:samlp=\"{NS_PROTOCOL}\" ID=\"_lr\" InResponseTo=\"{}\"><samlp:Status><samlp:StatusCode Value=\"{STATUS_SUCCESS}\"/></samlp:Status></samlp:LogoutResponse>",
        request.request_id
    );
    must(
        validate_logout_response(&request.request_id, &post(&response)),
        "validate_logout_response",
    );
}

/// The unsigned-metadata helper.
fn metadata_xml(idp: &TestIdp, include_cert: bool, id: &str) -> String {
    let mut key = String::new();
    if include_cert {
        key.push_str(
            "<md:KeyDescriptor use=\"signing\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>",
        );
        key.push_str(&encode_base64(&idp.certificate(Jwa::Rs256)));
        key.push_str("</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>");
    }
    format!(
        "<md:EntityDescriptor xmlns:md=\"{NS_METADATA}\" xmlns:ds=\"{NS_DS}\" ID=\"{id}\" entityID=\"{ISSUER}\" validUntil=\"2030-01-01T00:00:00Z\"><md:IDPSSODescriptor protocolSupportEnumeration=\"{NS_PROTOCOL}\">{key}<md:SingleSignOnService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect\" Location=\"{ISSUER}/sso\"/><md:SingleLogoutService Binding=\"urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST\" Location=\"{ISSUER}/slo\"/></md:IDPSSODescriptor></md:EntityDescriptor>"
    )
}

/// A replay store that is down refuses a valid, never-seen assertion (fail closed), and the
/// future is `Send` so a service can hold it across tasks.
#[test]
fn a_failing_replay_store_refuses_the_assertion() {
    let idp = TestIdp::new();
    let assertion = Assertion::valid("_assertion-1", NOW, Some(REQUEST_ID));
    let signed = signed_assertion_response(&idp, Jwa::Rs256, NOW, REQUEST_ID, &assertion);
    let idp_config = idp.idp_config("acme");
    let sp = sp_config();
    let pending = pending(REQUEST_ID, RELAY, NOW);
    let xml = post(&signed.xml);
    let store = spot_sso::contract::FailingStore;
    let future = complete(&sp, &idp_config, &pending, &xml, Some(RELAY), NOW, &store);
    assert_send(&future);
    let error = err(block_on(future));
    assert_eq!(error.code(), "sso.replay_store_unavailable");
    // The same message is accepted by a working store: the failure alone caused the refusal.
    let working = MemoryReplayStore::new();
    must(
        block_on(complete(
            &sp,
            &idp_config,
            &pending,
            &xml,
            Some(RELAY),
            NOW,
            &working,
        )),
        "a working store accepts it",
    );
}

fn assert_send<T: Send>(_: &T) {}
