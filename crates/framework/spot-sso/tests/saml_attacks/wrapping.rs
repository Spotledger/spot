//! Signature-wrapping (XSW) and signature-structure attacks. In every case a valid, signed
//! assertion is present and an attacker copies, moves or duplicates content around it.

use super::support::*;
use super::*;

/// XSW: an unsigned evil assertion placed before the signed one.
#[test]
fn xsw_evil_assertion_before_the_signed_one() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let evil = render_assertion(&Assertion::valid("_evil", h.now, Some(REQUEST_ID)), h.now);
    let block = assertion_block(&signed, "saml").to_owned();
    let mutated = signed.replacen(&block, &format!("{evil}{block}"), 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.multiple_assertions");
}

/// XSW: an unsigned evil assertion placed after the signed one.
#[test]
fn xsw_evil_assertion_after_the_signed_one() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let evil = render_assertion(&Assertion::valid("_evil", h.now, Some(REQUEST_ID)), h.now);
    let block = assertion_block(&signed, "saml").to_owned();
    let mutated = signed.replacen(&block, &format!("{block}{evil}"), 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.multiple_assertions");
}

/// XSW: the signed assertion is moved into `Extensions` and an evil copy takes its place.
#[test]
fn xsw_signed_assertion_moved_into_extensions() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let evil = render_assertion(&Assertion::valid("_evil", h.now, Some(REQUEST_ID)), h.now);
    let block = assertion_block(&signed, "saml").to_owned();
    let wrapped =
        format!("<saml:Extensions xmlns:saml=\"{NS_ASSERTION}\">{block}</saml:Extensions>");
    let mutated = signed.replacen(&block, &format!("{wrapped}{evil}"), 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.multiple_assertions");
}

/// XSW: an `Extensions` element is added even though the assertion is untouched.
#[test]
fn xsw_extensions_wrapper_is_forbidden() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(
        "<samlp:Status>",
        &format!(
            "<saml:Extensions xmlns:saml=\"{NS_ASSERTION}\"><saml:ExtensionsValue/></saml:Extensions><samlp:Status>"
        ),
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.wrapper_forbidden");
}

/// XSW: a `ds:Object` could hide a second copy of the data; it is refused.
#[test]
fn xsw_object_wrapper_is_forbidden() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(
        "<samlp:Status>",
        &format!("<ds:Object xmlns:ds=\"{NS_DS}\"/><samlp:Status>"),
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.wrapper_forbidden");
}

/// Two `ds:Signature` elements: a reader could verify one and read the other.
#[test]
fn two_signatures_are_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let signature = signature_block(&signed).to_owned();
    let mutated = signed.replacen(&signature, &format!("{signature}{signature}"), 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.signature_multiple");
}

/// The signature is moved onto the `Response` while it still references the assertion.
#[test]
fn signature_moved_to_the_response_reference_elsewhere() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let signature = signature_block(&signed).to_owned();
    let without = signed.replacen(&signature, "", 1);
    // Insert the signature as the first child of the Response (after its opening tag).
    let mutated = without.replacen(">", &format!(">{signature}"), 1);
    assert_eq!(
        h.refusal_with(&response_policy(), &mutated),
        "sso.saml.reference_invalid"
    );
}

/// The signature is moved onto a non-signable element (the status).
#[test]
fn signature_moved_onto_a_non_signable_element() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let signature = signature_block(&signed).to_owned();
    let without = signed.replacen(&signature, "", 1);
    let mutated = without.replacen("<samlp:Status>", &format!("<samlp:Status>{signature}"), 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.signature_not_enveloped");
}

/// `Reference URI` pointing at an element elsewhere in the document.
#[test]
fn reference_uri_pointing_elsewhere() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen("URI=\"#_assertion-1\"", "URI=\"#_elsewhere\"", 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.reference_invalid");
}

/// An empty `URI=""` whole-document reference.
#[test]
fn empty_whole_document_reference() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen("URI=\"#_assertion-1\"", "URI=\"\"", 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.reference_invalid");
}

/// A duplicate `ID`: the `Response` reuses the assertion's id, so a reader could pick the wrong
/// element.
#[test]
fn duplicate_id_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen("ID=\"_response-1\"", "ID=\"_assertion-1\"", 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.duplicate_id");
}

/// An `ID` collision: a second element carries the same id as the signed assertion.
#[test]
fn id_collision_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let block = assertion_block(&signed, "saml").to_owned();
    let mutated = signed.replacen(
        &block,
        &format!("<saml:Advice xmlns:saml=\"{NS_ASSERTION}\" ID=\"_assertion-1\"/>{block}"),
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.duplicate_id");
}
