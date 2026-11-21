//! Algorithm, transform, key, payload and comment-injection attacks. Each starts from a valid,
//! signed document and attacks the signature machinery or the message shape.

use super::support::*;
use super::*;

/// A SHA-1 `DigestMethod` is refused.
#[test]
fn sha1_digest_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(DIGEST_SHA256, DIGEST_SHA1, 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.alg_unsupported");
}

/// A `rsa-sha1` `SignatureMethod` is refused.
#[test]
fn rsa_sha1_signature_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(SIG_RSA_SHA256, SIG_RSA_SHA1, 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.alg_unsupported");
}

/// An XPath transform is refused (this crate implements no XPath).
#[test]
fn xpath_transform_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mutated = signed.replacen(
        "<ds:Transforms>",
        &format!("<ds:Transforms><ds:Transform Algorithm=\"{TRANSFORM_XPATH}\"/>"),
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.transform_unsupported");
}

/// A certificate in `KeyInfo` that is not the pinned one is refused, even though the signature
/// actually verifies with the pinned key.
#[test]
fn attacker_keyinfo_certificate_is_refused() {
    let h = Harness::new();
    let attacker = TestIdp::new();
    let signed = h.signed(Jwa::Rs256);
    // Replace the (matching) RSA certificate in KeyInfo with the attacker's.
    let honest = encode_base64(&h.idp.certificate(Jwa::Rs256));
    let evil = encode_base64(&attacker.certificate(Jwa::Es256));
    assert_ne!(honest, evil);
    let mutated = signed.replacen(&honest, &evil, 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.signature_invalid");
}

/// A DTD in the response is refused before parsing.
#[test]
fn dtd_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let payload =
        format!("<?xml version=\"1.0\"?><!DOCTYPE samlp:Response [<!ENTITY x \"y\">]>{signed}");
    assert_eq!(h.refusal(&payload), "sso.saml.dtd_forbidden");
}

/// An external-entity (`SYSTEM`) reference is refused.
#[test]
fn external_entity_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let payload = format!("<!DOCTYPE samlp:Response SYSTEM \"http://evil.example/e.dtd\">{signed}");
    assert_eq!(h.refusal(&payload), "sso.saml.dtd_forbidden");
}

/// A billion-laughs expansion is refused at the DTD check, before any entity is expanded.
#[test]
fn billion_laughs_is_refused_at_the_dtd_check() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let dtd = concat!(
        "<!DOCTYPE samlp:Response [",
        "<!ENTITY lol \"lol\">",
        "<!ENTITY lol1 \"&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;&lol;\">",
        "<!ENTITY lol2 \"&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;&lol1;\">",
        "<!ENTITY lol3 \"&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;&lol2;\">",
        "<!ENTITY lol4 \"&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;&lol3;\">",
        "]>"
    );
    let payload = format!("{dtd}{signed}");
    assert_eq!(h.refusal(&payload), "sso.saml.dtd_forbidden");
}

/// A response over the size cap is refused.
#[test]
fn oversized_response_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let mut sp = sp_config();
    sp.max_response_bytes = 64;
    assert_eq!(h.refusal_with(&sp, &signed), "sso.saml.too_large");
}

/// Comment injection inside `NameID`: the signed text is split by a comment, so it is refused.
#[test]
fn comment_injection_in_nameid_is_refused() {
    let h = Harness::new();
    let assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    let unsigned = render_assertion(&assertion, h.now);
    let mutated = unsigned.replacen(
        ">ada@example.com<",
        ">ada@example.com<!--x-->@evil.example<",
        1,
    );
    let document = h.wrap_signed_assertion(&mutated, &assertion.id);
    assert_eq!(h.refusal(&document), "sso.saml.comment_in_value");
}

/// Comment injection inside an attribute value is refused.
#[test]
fn comment_injection_in_an_attribute_is_refused() {
    let h = Harness::new();
    let assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    let unsigned = render_assertion(&assertion, h.now);
    let mutated = unsigned.replacen(
        ">ada@example.com</saml:AttributeValue>",
        ">ada@example.com<!--x-->@evil.example</saml:AttributeValue>",
        1,
    );
    let document = h.wrap_signed_assertion(&mutated, &assertion.id);
    assert_eq!(h.refusal(&document), "sso.saml.comment_in_value");
}

/// An assertion whose issuer is not the configured IdP is refused.
#[test]
fn assertion_from_an_unconfigured_idp_is_refused() {
    let h = Harness::new();
    let mut assertion = Assertion::valid("_assertion-1", h.now, Some(REQUEST_ID));
    assertion.issuer = OTHER_ISSUER.to_owned();
    let document = h.signed_for(Jwa::Rs256, &assertion);
    // The signature is over the assertion, so it verifies; the issuer check then refuses it.
    assert_eq!(h.refusal(&document), "sso.saml.provider_not_configured");
}

/// A message with no signature at all is refused.
#[test]
fn unsigned_message_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let signature = signature_block(&signed).to_owned();
    let mutated = signed.replacen(&signature, "", 1);
    assert_eq!(h.refusal(&mutated), "sso.saml.signature_missing");
}

/// An encrypted assertion without a decryption key is refused with a clear code.
#[test]
fn encrypted_assertion_is_refused() {
    let h = Harness::new();
    let signed = h.signed(Jwa::Rs256);
    let block = assertion_block(&signed, "saml").to_owned();
    let mutated = signed.replacen(
        &block,
        &format!(
            "<saml:EncryptedAssertion xmlns:saml=\"{NS_ASSERTION}\">{block}</saml:EncryptedAssertion>"
        ),
        1,
    );
    assert_eq!(h.refusal(&mutated), "sso.saml.encrypted_unsupported");
}
