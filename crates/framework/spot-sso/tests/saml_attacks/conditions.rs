//! Condition, subject, shape and context attacks: everything that must be true of an
//! assertion's content before it can become a sign-in.

use super::support::*;
use super::*;

/// The response `Destination` must be this ACS URL.
#[test]
fn wrong_destination_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(
        &format!("Destination=\"{ACS_URL}\""),
        "Destination=\"https://evil.example/acs\"",
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.destination_mismatch");
}

/// The audience must contain this SP entity id.
#[test]
fn wrong_audience_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.audience = "https://other.example/saml".to_owned();
    let document = h.signed_for(Jwa::Rs256, &assertion);
    assert_eq!(h.refusal(&document), "sso.saml.audience_mismatch");
}

/// An expired assertion is refused.
#[test]
fn expired_assertion_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.conditions_not_on_or_after = Some(h.now - 1_000);
    let document = h.signed_for(Jwa::Rs256, &assertion);
    assert_eq!(h.refusal(&document), "sso.saml.conditions_invalid");
}

/// An assertion that is not yet valid is refused.
#[test]
fn not_yet_valid_assertion_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.conditions_not_before = Some(h.now + 1_000);
    let document = h.signed_for(Jwa::Rs256, &assertion);
    assert_eq!(h.refusal(&document), "sso.saml.conditions_invalid");
}

/// `InResponseTo` must name the pending request.
#[test]
fn in_response_to_mismatch_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(
        &format!("InResponseTo=\"{REQUEST_ID}\""),
        "InResponseTo=\"_wrong-request\"",
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.in_response_to_mismatch");
}

/// An unsolicited response is refused while IdP-initiated sign-in is off.
#[test]
fn unsolicited_response_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(&format!(" InResponseTo=\"{REQUEST_ID}\""), "", 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.unsolicited");
}

/// A non-success status is refused.
#[test]
fn non_success_status_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(
        STATUS_SUCCESS,
        "urn:oasis:names:tc:SAML:2.0:status:Requester",
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.status_not_success");
}

/// A bearer confirmation for a different recipient is refused.
#[test]
fn wrong_recipient_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.recipient = "https://evil.example/acs".to_owned();
    let document = h.signed_for(Jwa::Rs256, &assertion);
    assert_eq!(h.refusal(&document), "sso.saml.subject_confirmation");
}

/// An expired bearer confirmation is refused.
#[test]
fn expired_confirmation_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.confirmation_not_on_or_after = Some(h.now - 1_000);
    let document = h.signed_for(Jwa::Rs256, &assertion);
    assert_eq!(h.refusal(&document), "sso.saml.subject_confirmation");
}

/// An assertion with no `AuthnStatement` is refused.
#[test]
fn missing_authn_statement_is_refused() {
    let h = Harness::new();
    let assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    let unsigned = render_assertion(&assertion, h.now);
    let mutated = unsigned
        .replace("<saml:AuthnStatement", "<saml:NotAuthnStatement")
        .replace("</saml:AuthnStatement>", "</saml:NotAuthnStatement>");
    let document = h.wrap_signed_assertion(&mutated, &assertion.id);
    assert_eq!(h.refusal(&document), "sso.saml.no_authn_statement");
}

/// An authentication context the tenant does not allow is refused.
#[test]
fn insufficient_authn_context_is_refused() {
    let h = Harness::new();
    let mut idp = h.provider();
    idp.acr_values = vec!["urn:tenant:required:hardware".to_owned()];
    // The standard assertion reports only the silver context.
    let document = h.signed(Jwa::Rs256);
    assert_eq!(
        h.refusal_provider(&h.sp, &idp, &document),
        "sso.saml.authn_context"
    );
}

/// A transient `NameID` cannot link an identity and is refused.
#[test]
fn transient_nameid_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.name_id_format = NAMEID_TRANSIENT.to_owned();
    let document = h.signed_for(Jwa::Rs256, &assertion);
    assert_eq!(h.refusal(&document), "sso.saml.nameid_invalid");
}

/// A response that carries no assertion at all is refused.
#[test]
fn missing_assertion_is_refused() {
    let h = Harness::new();
    let response = render_response(
        "samlp",
        "_response-1",
        ACS_URL,
        REQUEST_ID,
        None,
        STATUS_SUCCESS,
        &[],
        None,
    );
    assert_eq!(h.refusal(&response), "sso.saml.assertion_missing");
}

/// A document that is not a response is refused.
#[test]
fn not_a_response_is_refused() {
    let h = Harness::new();
    // `post` of a plain element: the root is not a samlp:Response.
    assert_eq!(h.refusal("<foo/>"), "sso.saml.not_a_response");
}

/// Text that is not Base64 is refused.
#[test]
fn not_base64_is_refused() {
    let h = Harness::new();
    let replay = MemoryReplayStore::new();
    let error = err(block_on(complete(
        &h.sp,
        &h.provider(),
        &h.pending(),
        "!!!not base64!!!",
        Some(RELAY),
        h.now,
        &replay,
    )));
    assert_eq!(error.code(), "sso.saml.not_base64");
}

/// Bytes that are not UTF-8 are refused.
#[test]
fn not_utf8_is_refused() {
    let h = Harness::new();
    let replay = MemoryReplayStore::new();
    let error = err(block_on(complete(
        &h.sp,
        &h.provider(),
        &h.pending(),
        &encode_base64(&[0xff, 0xfe, 0xfd]),
        Some(RELAY),
        h.now,
        &replay,
    )));
    assert_eq!(error.code(), "sso.saml.not_utf8");
}

/// Malformed XML is refused.
#[test]
fn malformed_xml_is_refused() {
    let h = Harness::new();
    assert_eq!(h.refusal("<a><b></a>"), "sso.saml.xml_invalid");
}
