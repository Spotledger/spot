//! XML Signature (XML-DSig Core) verification for SAML, with the wrapping family refused.
//!
//! SAML signs an element *by reference*: `ds:SignedInfo/ds:Reference` carries `URI="#id"`, and a
//! verifier that checks the signature but then reads a *different* element is how an attacker
//! becomes another user. This verifier therefore never lets a message choose what is verified:
//!
//! - exactly one enveloped `ds:Signature` is allowed, attached directly to a signable element
//!   (the `Response`, the `Assertion`, a `LogoutRequest`, or — for metadata — the
//!   `EntityDescriptor`);
//! - exactly one `ds:Reference`, whose `URI` names the signature's **own parent**, so the digest
//!   and the data that is later read are the same element;
//! - the referenced `ID` must be unique in the whole document (an ID collision is refused);
//! - transforms may only be enveloped-signature and exclusive canonicalisation, the digest only
//!   SHA-256, the signature only RSA-SHA256 or ECDSA-SHA256;
//! - the key is chosen **only** from the pinned IdP certificates; a `ds:KeyInfo` is never trusted,
//!   and an `X509Certificate` inside it is only compared against the pinned set.
//!
//! The digest is recomputed over the referenced element's exclusive-canonical form *after the
//! signature is removed*, and the `SignedInfo` is verified the same way.

use roxmltree::{Document, Node};

use crate::error::{self as e, Result, SsoError};
use crate::jose;
use crate::provider::Jwa;
use crate::saml::c14n;
use crate::saml::keys;
use crate::saml::xml::{self, NS_ASSERTION, NS_DS, NS_METADATA, NS_PROTOCOL};

/// The enveloped-signature transform.
pub(crate) const ENVELOPED_SIGNATURE: &str =
    "http://www.w3.org/2000/09/xmldsig#enveloped-signature";
/// Exclusive canonicalisation, without comments.
pub(crate) const C14N_EXCLUSIVE: &str = "http://www.w3.org/2001/10/xml-exc-c14n#";
/// Exclusive canonicalisation, with comments.
pub(crate) const C14N_EXCLUSIVE_COMMENTS: &str =
    "http://www.w3.org/2001/10/xml-exc-c14n#WithComments";
/// RSA with SHA-256 (XML Signature 1.1).
pub(crate) const SIG_RSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#rsa-sha256";
/// ECDSA with SHA-256 (XML Signature 1.1).
pub(crate) const SIG_ECDSA_SHA256: &str = "http://www.w3.org/2001/04/xmldsig-more#ecdsa-sha256";
/// SHA-256, the only digest algorithm accepted.
pub(crate) const DIGEST_SHA256: &str = "http://www.w3.org/2001/04/xmlenc#sha256";

/// Which element the tenant's policy requires to be signed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SignaturePolicy {
    /// The `Assertion` must carry the signature (the default, and the safest).
    #[default]
    Assertion,
    /// Either the `Assertion` or the whole `Response` may carry it.
    AssertionOrResponse,
}

/// Verifies the message's enveloped signature and returns the element the signature covers.
///
/// The returned node is the only element whose subtree may subsequently be read.
///
/// # Errors
///
/// One of the `sso.saml.*` signature codes.
pub(crate) fn verify<'a, 'input>(
    document: &'a Document<'input>,
    pinned: &[Vec<u8>],
    policy: SignaturePolicy,
) -> Result<Node<'a, 'input>> {
    let signature = single_signature(document)?;
    let parent = signature
        .parent()
        .ok_or_else(|| not_enveloped("no parent"))?;
    let parent_is_response = xml::is_tag(parent, NS_PROTOCOL, "Response");
    let parent_is_assertion = xml::is_tag(parent, NS_ASSERTION, "Assertion");
    if !parent_is_response && !parent_is_assertion {
        return Err(not_enveloped("attached to neither Response nor Assertion"));
    }
    if policy == SignaturePolicy::Assertion && !parent_is_assertion {
        return Err(signature_missing("the Assertion is not signed"));
    }
    verify_core(signature, pinned)
}

/// Verifies the signature over a SAML `LogoutRequest`'s root element.
///
/// The same wrapping checks as [`verify`] apply: exactly one enveloped `ds:Signature`, attached
/// directly to the `LogoutRequest`, whose single `Reference` names that element's own unique
/// `ID`. The returned node is the only element whose subtree may subsequently be read.
///
/// # Errors
///
/// One of the `sso.saml.*` signature codes.
pub(crate) fn verify_logout_request<'a, 'input>(
    document: &'a Document<'input>,
    pinned: &[Vec<u8>],
) -> Result<Node<'a, 'input>> {
    let signature = single_signature(document)?;
    let parent = signature
        .parent()
        .ok_or_else(|| not_enveloped("no parent"))?;
    if !xml::is_tag(parent, NS_PROTOCOL, "LogoutRequest") {
        return Err(not_enveloped(
            "the signature is not attached to a LogoutRequest",
        ));
    }
    verify_core(signature, pinned)
}

/// Verifies the signature over a metadata `EntityDescriptor`, when one is present.
///
/// Returns `Ok(None)` when the document is unsigned; the caller decides whether that is
/// acceptable ([`crate::saml::MetadataTrust::trust_unsigned`]).
///
/// # Errors
///
/// The signature codes, when a signature is present but does not verify.
pub(crate) fn verify_metadata<'a, 'input>(
    document: &'a Document<'input>,
    pinned: &[Vec<u8>],
) -> Result<Option<Node<'a, 'input>>> {
    let signatures: Vec<Node<'_, '_>> = document
        .descendants()
        .filter(|node| xml::is_tag(*node, NS_DS, "Signature"))
        .collect();
    let signature = match signatures.as_slice() {
        [] => return Ok(None),
        [only] => *only,
        _ => return Err(signature_multiple()),
    };
    let parent = signature
        .parent()
        .ok_or_else(|| not_enveloped("no parent"))?;
    if !xml::is_tag(parent, NS_METADATA, "EntityDescriptor")
        && !xml::is_tag(parent, NS_METADATA, "EntitiesDescriptor")
    {
        return Err(not_enveloped("metadata signature is not on the descriptor"));
    }
    verify_core(signature, pinned).map(Some)
}

/// The core: reference, algorithms, KeyInfo, signature verify and digest recompute.
fn verify_core<'a, 'input>(
    signature: Node<'a, 'input>,
    pinned: &[Vec<u8>],
) -> Result<Node<'a, 'input>> {
    let parent = signature
        .parent()
        .ok_or_else(|| not_enveloped("no parent"))?;
    let id = parent
        .attribute("ID")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| reference_invalid("the signed element has no ID"))?;
    ensure_unique_id(signature.document(), id)?;

    let signed_info = only_child(signature, "SignedInfo")?;
    let canonicalization = only_child(signed_info, "CanonicalizationMethod")?
        .attribute("Algorithm")
        .unwrap_or_default();
    let with_comments = match canonicalization {
        C14N_EXCLUSIVE => false,
        C14N_EXCLUSIVE_COMMENTS => true,
        _ => return Err(transform_unsupported("unsupported canonicalization method")),
    };
    let signature_algorithm = only_child(signed_info, "SignatureMethod")?
        .attribute("Algorithm")
        .unwrap_or_default();
    let alg = match signature_algorithm {
        SIG_RSA_SHA256 => Jwa::Rs256,
        SIG_ECDSA_SHA256 => Jwa::Es256,
        _ => return Err(alg_unsupported("unsupported signature algorithm")),
    };

    let references: Vec<Node<'_, '_>> = xml::children(signed_info, NS_DS, "Reference");
    let reference = match references.as_slice() {
        [only] => *only,
        _ => {
            return Err(reference_invalid(
                "the signature must have exactly one reference",
            ));
        }
    };
    let uri = reference.attribute("URI").unwrap_or_default();
    let referenced = uri.strip_prefix('#').unwrap_or("");
    if referenced.is_empty() || referenced != id {
        return Err(reference_invalid(
            "the reference does not name the signed element",
        ));
    }

    let reference_with_comments = check_transforms(reference)?;

    let digest_algorithm = only_child(reference, "DigestMethod")?
        .attribute("Algorithm")
        .unwrap_or_default();
    if digest_algorithm != DIGEST_SHA256 {
        return Err(alg_unsupported("unsupported digest algorithm"));
    }
    let digest_value = only_child(reference, "DigestValue")?;
    let digest = xml::decode_base64(&xml::concatenated_text(digest_value))?;

    check_key_info(signature, pinned)?;

    let signature_value = only_child(signature, "SignatureValue")?;
    let signature_bytes = xml::decode_base64(&xml::concatenated_text(signature_value))?;

    let signed_info_canonical = c14n::canonicalize_subtree(signed_info, None, with_comments);
    if !keys::verify_with_certificates(
        pinned,
        alg,
        signed_info_canonical.as_bytes(),
        &signature_bytes,
    ) {
        return Err(SsoError::new(
            e::codes::SAML_SIGNATURE_INVALID,
            "The sign-in response was not trusted.",
            "Check the identity provider's pinned certificates and signing algorithm.",
        ));
    }

    let canonical = c14n::canonicalize_subtree(parent, Some(signature), reference_with_comments);
    let computed = jose::sha256(canonical.as_bytes());
    if !jose::ct_eq(&computed, &digest) {
        return Err(SsoError::new(
            e::codes::SAML_DIGEST_MISMATCH,
            "The sign-in response was not trusted.",
            "The signed element was altered after signing; refuse the message.",
        ));
    }
    Ok(parent)
}

/// The single `ds:Signature` in the document, or a missing/multiple refusal.
fn single_signature<'a, 'input>(document: &'a Document<'input>) -> Result<Node<'a, 'input>> {
    let signatures: Vec<Node<'a, 'input>> = document
        .descendants()
        .filter(|node| xml::is_tag(*node, NS_DS, "Signature"))
        .collect();
    match signatures.as_slice() {
        [] => Err(signature_missing("")),
        [only] => Ok(*only),
        _ => Err(signature_multiple()),
    }
}

/// The single `ds:` child element named `local`, or a malformed-signature refusal.
fn only_child<'a, 'input>(parent: Node<'a, 'input>, local: &str) -> Result<Node<'a, 'input>> {
    let mut matches = xml::children(parent, NS_DS, local).into_iter();
    let first = matches.next().ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_SIGNATURE_INVALID,
            "The sign-in response was not trusted.",
            "The SAML signature is missing a required part.",
        )
    })?;
    if matches.next().is_some() {
        return Err(SsoError::new(
            e::codes::SAML_SIGNATURE_INVALID,
            "The sign-in response was not trusted.",
            "The SAML signature repeats a required part.",
        ));
    }
    Ok(first)
}

/// Whether any other element carries the same `ID`, and whether any ID is duplicated at all.
fn ensure_unique_id(document: &Document<'_>, id: &str) -> Result<()> {
    let mut values: Vec<&str> = Vec::new();
    let mut id_count = 0usize;
    for element in document.descendants().filter(Node::is_element) {
        if let Some(value) = element.attribute("ID").filter(|value| !value.is_empty()) {
            if value == id {
                id_count += 1;
            }
            if values.contains(&value) {
                return Err(SsoError::new(
                    e::codes::SAML_DUPLICATE_ID,
                    "The sign-in response was refused.",
                    "The SAML message reuses an XML ID; refuse it.",
                ));
            }
            values.push(value);
        }
    }
    if id_count > 1 {
        return Err(SsoError::new(
            e::codes::SAML_DUPLICATE_ID,
            "The sign-in response was refused.",
            "The SAML message reuses an XML ID; refuse it.",
        ));
    }
    Ok(())
}

/// Checks the `Transforms` allow-list and reports the reference's canonicalisation comment flag.
fn check_transforms(reference: Node<'_, '_>) -> Result<bool> {
    let Some(transforms) = xml::child(reference, NS_DS, "Transforms") else {
        return Ok(false);
    };
    let mut with_comments = false;
    let mut enveloped = 0usize;
    for transform in xml::children(transforms, NS_DS, "Transform") {
        match transform.attribute("Algorithm").unwrap_or_default() {
            ENVELOPED_SIGNATURE => enveloped += 1,
            C14N_EXCLUSIVE => {}
            C14N_EXCLUSIVE_COMMENTS => with_comments = true,
            _ => return Err(transform_unsupported("unsupported transform")),
        }
    }
    if enveloped > 1 {
        return Err(transform_unsupported(
            "repeated enveloped-signature transform",
        ));
    }
    Ok(with_comments)
}

/// Refuses a `KeyInfo` certificate that is not one of the pinned certificates.
fn check_key_info(signature: Node<'_, '_>, pinned: &[Vec<u8>]) -> Result<()> {
    let Some(key_info) = xml::child(signature, NS_DS, "KeyInfo") else {
        return Ok(());
    };
    for certificate in key_info
        .descendants()
        .filter(|node| xml::is_tag(*node, NS_DS, "X509Certificate"))
    {
        let der = xml::decode_base64(&xml::concatenated_text(certificate))?;
        if !keys::matches_pinned(&der, pinned) {
            return Err(SsoError::new(
                e::codes::SAML_SIGNATURE_INVALID,
                "The sign-in response was not trusted.",
                "A certificate in the message is not one of the pinned IdP certificates; refuse it.",
            ));
        }
    }
    Ok(())
}

fn signature_missing(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_SIGNATURE_MISSING,
        "The sign-in response was not trusted.",
        "The identity provider must sign the assertion with a pinned certificate.",
    )
    .with_detail(detail)
}

fn signature_multiple() -> SsoError {
    SsoError::new(
        e::codes::SAML_SIGNATURE_MULTIPLE,
        "The sign-in response was refused.",
        "The SAML message carries more than one signature; refuse it.",
    )
}

fn not_enveloped(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_SIGNATURE_NOT_ENVELOPED,
        "The sign-in response was refused.",
        "The SAML signature is not an enveloped signature over the assertion or response.",
    )
    .with_detail(detail)
}

fn reference_invalid(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_REFERENCE_INVALID,
        "The sign-in response was refused.",
        "The signature reference must name the element it is attached to.",
    )
    .with_detail(detail)
}

fn alg_unsupported(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_ALG_UNSUPPORTED,
        "The sign-in response used a method this tenant does not allow.",
        "Accept only SHA-256 digests and RSA-SHA256 or ECDSA-SHA256 signatures.",
    )
    .with_detail(detail)
}

fn transform_unsupported(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_TRANSFORM_UNSUPPORTED,
        "The sign-in response was refused.",
        "Accept only the enveloped-signature and exclusive-canonicalisation transforms.",
    )
    .with_detail(detail)
}
