//! The SAML `Response`: parsing it, verifying the signature over it and the identity it proves.
//!
//! [`complete`] runs the checks in a fixed order, each with its own refusal code, and extracts
//! identity **only** from the element the signature covers. The order is deliberate: cheap
//! shape checks first, then the signature, then the conditions that only make sense once the
//! bytes are authentic, then replay, then the identity.

use std::collections::BTreeMap;

use roxmltree::Node;

use crate::error::{self as e, Result, SsoError};
use crate::identity::SsoIdentity;
use crate::jose;
use crate::replay::{Claim, ReplayStore, namespaced};
use crate::saml::request::SamlPendingAuth;
use crate::saml::signature::{self, SignaturePolicy};
use crate::saml::xml::{self, NS_ASSERTION, NS_DS, NS_PROTOCOL};
use crate::saml::{IdpConfig, NAMEID_TRANSIENT, SpConfig};

/// A validated SAML sign-in: the shared identity and the session bounds the assertion set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamlSignIn {
    /// The identity, identical in shape to an OIDC one, so [`crate::resolve`] is shared.
    pub identity: SsoIdentity,
    /// The assertion's `SessionNotOnOrAfter`, when it caps the session.
    pub session_not_on_or_after: Option<i64>,
}

/// The `samlp:StatusCode` value for success.
pub(crate) const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";

/// Validates a POSTed `SAMLResponse` and returns the identity it proves.
///
/// # Errors
///
/// One of the `sso.saml.*` codes, in the order the checks run: size, Base64, UTF-8, DTD, XML,
/// root, encrypted assertion, assertion count, wrapper elements, destination, `InResponseTo`,
/// relay state, status, signature, issuer, conditions, subject confirmation, authentication
/// statement and context, replay, then identity. `sso.replay_store_unavailable` when the
/// replay store cannot answer (the response is refused).
pub async fn complete(
    sp: &SpConfig,
    idp: &IdpConfig,
    pending: &SamlPendingAuth,
    saml_response: &str,
    relay_state: Option<&str>,
    now: i64,
    replay: &impl ReplayStore,
) -> Result<SamlSignIn> {
    // 1. Shape: size, Base64, UTF-8, DTD, XML, root.
    if saml_response.len() > sp.max_response_bytes {
        return Err(SsoError::new(
            e::codes::SAML_TOO_LARGE,
            "The sign-in response was refused.",
            "Raise the response size cap only if a legitimate provider exceeds it.",
        ));
    }
    let decoded = xml::decode_base64(saml_response)?;
    let text = std::str::from_utf8(&decoded).map_err(|_| {
        SsoError::new(
            e::codes::SAML_NOT_UTF8,
            "The sign-in response could not be read.",
            "Check the identity provider sends UTF-8 SAML XML.",
        )
    })?;
    let document = xml::parse_document(text)?;
    if !xml::is_tag(document.root_element(), NS_PROTOCOL, "Response") {
        return Err(SsoError::new(
            e::codes::SAML_NOT_A_RESPONSE,
            "The sign-in response was refused.",
            "The response root must be a samlp:Response.",
        ));
    }
    let response = document.root_element();

    if response
        .descendants()
        .any(|node| xml::is_tag(node, NS_ASSERTION, "EncryptedAssertion"))
    {
        return Err(SsoError::new(
            e::codes::SAML_ENCRYPTED_UNSUPPORTED,
            "The sign-in response was refused.",
            "Encrypted assertions need a decryption key; none is configured.",
        ));
    }

    // 2. Exactly one assertion anywhere in the document.
    let assertions: Vec<Node<'_, '_>> = document
        .descendants()
        .filter(|node| xml::is_tag(*node, NS_ASSERTION, "Assertion"))
        .collect();
    let assertion = match assertions.as_slice() {
        [] => {
            return Err(SsoError::new(
                e::codes::SAML_ASSERTION_MISSING,
                "The sign-in response was incomplete.",
                "The response must carry exactly one assertion.",
            ));
        }
        [only] => *only,
        _ => {
            return Err(SsoError::new(
                e::codes::SAML_MULTIPLE_ASSERTIONS,
                "The sign-in response was refused.",
                "The response carries more than one assertion; refuse it.",
            ));
        }
    };

    // 3. No wrapper element that could hide a second copy of what was verified.
    if response
        .descendants()
        .chain(std::iter::once(response))
        .any(|node| {
            xml::is_tag(node, NS_ASSERTION, "Extensions")
                || xml::is_tag(node, NS_PROTOCOL, "Extensions")
                || xml::is_tag(node, NS_DS, "Object")
        })
    {
        return Err(SsoError::new(
            e::codes::SAML_WRAPPER_FORBIDDEN,
            "The sign-in response was refused.",
            "The message carries an Extensions or Object element; refuse it.",
        ));
    }

    // 4. Destination, InResponseTo, relay state and status.
    let destination = response.attribute("Destination").unwrap_or_default();
    if destination != sp.acs_url {
        return Err(SsoError::new(
            e::codes::SAML_DESTINATION_MISMATCH,
            "The sign-in response was refused.",
            "The response Destination must be this ACS URL.",
        ));
    }
    let in_response_to = response.attribute("InResponseTo").filter(|v| !v.is_empty());
    match in_response_to {
        Some(value) if value == pending.request_id => {}
        Some(_) => {
            return Err(SsoError::new(
                e::codes::SAML_IN_RESPONSE_TO_MISMATCH,
                "The sign-in response did not match this attempt.",
                "The InResponseTo does not name the pending AuthnRequest.",
            ));
        }
        None => {
            if !sp.allow_idp_initiated {
                return Err(SsoError::new(
                    e::codes::SAML_UNSOLICITED,
                    "The sign-in response was refused.",
                    "An unsolicited response arrived and IdP-initiated sign-in is off.",
                ));
            }
        }
    }
    if let Some(relay_state) = relay_state
        && !jose::ct_eq(relay_state.as_bytes(), pending.relay_state.as_bytes())
    {
        return Err(SsoError::new(
            e::codes::STATE_MISMATCH,
            "The sign-in response did not match this attempt.",
            "Start the sign-in again from the beginning.",
        ));
    }
    pending.consume(now)?;
    let status = response
        .descendants()
        .find(|node| xml::is_tag(*node, NS_PROTOCOL, "StatusCode"))
        .and_then(|node| node.attribute("Value"))
        .unwrap_or_default();
    if status != STATUS_SUCCESS {
        return Err(SsoError::new(
            e::codes::SAML_STATUS_NOT_SUCCESS,
            "The identity provider refused the sign-in.",
            "The IdP returned a non-success status; check the user's account there.",
        ));
    }

    // 5. The signature, and the element it covers.
    let verified = signature::verify(&document, &idp.signing_certificates, sp.signature_policy)?;
    if sp.signature_policy == SignaturePolicy::Assertion
        && !xml::is_tag(verified, NS_ASSERTION, "Assertion")
    {
        // Defensive: `verify` already enforces this, but the identity must never come from an
        // assertion the policy did not require to be signed.
        return Err(SsoError::new(
            e::codes::SAML_UNSIGNED_ASSERTION,
            "The sign-in response was refused.",
            "The asserted identity is not covered by the verified signature; refuse it.",
        ));
    }
    let signed_assertion = if xml::is_tag(verified, NS_ASSERTION, "Assertion") {
        verified
    } else {
        // The Response is signed; the single assertion it carries is covered by it.
        xml::child(verified, NS_ASSERTION, "Assertion").unwrap_or(assertion)
    };

    // 6. Issuer checks: the Response's (if present) and the assertion's.
    if let Some(issuer) = xml::child(response, NS_ASSERTION, "Issuer") {
        let value = xml::simple_text(issuer)?;
        if value != idp.entity_id {
            return Err(SsoError::new(
                e::codes::SAML_ISSUER_MISMATCH,
                "The sign-in response was not trusted.",
                "The response Issuer does not match the configured IdP entity id.",
            ));
        }
    }
    let assertion_issuer = xml::child(signed_assertion, NS_ASSERTION, "Issuer")
        .ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_ISSUER_MISMATCH,
                "The sign-in response was not trusted.",
                "The assertion must carry a saml:Issuer.",
            )
        })
        .and_then(|issuer| xml::simple_text(issuer))?;
    if assertion_issuer != idp.entity_id {
        return Err(SsoError::new(
            e::codes::SAML_PROVIDER_NOT_CONFIGURED,
            "That sign-in method is not available.",
            "The assertion's Issuer is not a configured identity provider.",
        ));
    }

    // 7. Conditions.
    let conditions = xml::child(signed_assertion, NS_ASSERTION, "Conditions");
    let mut not_on_or_after = None;
    if let Some(conditions) = conditions {
        if let Some(not_before) = conditions
            .attribute("NotBefore")
            .and_then(xml::parse_instant)
            && now + sp.clock_skew_seconds < not_before
        {
            return Err(conditions_invalid("the assertion is not valid yet"));
        }
        if let Some(expiry) = conditions
            .attribute("NotOnOrAfter")
            .and_then(xml::parse_instant)
        {
            if now >= expiry + sp.clock_skew_seconds {
                return Err(conditions_invalid("the assertion has expired"));
            }
            not_on_or_after = Some(expiry);
        }
        let audiences: Vec<Node<'_, '_>> = conditions
            .descendants()
            .filter(|node| xml::is_tag(*node, NS_ASSERTION, "Audience"))
            .collect();
        let mut matched = false;
        for audience in audiences {
            if xml::simple_text(audience)? == sp.entity_id {
                matched = true;
            }
        }
        if !matched {
            return Err(SsoError::new(
                e::codes::SAML_AUDIENCE_MISMATCH,
                "The sign-in response was not for this application.",
                "The AudienceRestriction must contain this SP entity id.",
            ));
        }
    } else {
        return Err(SsoError::new(
            e::codes::SAML_AUDIENCE_MISMATCH,
            "The sign-in response was not for this application.",
            "The assertion must carry Conditions naming this SP as an audience.",
        ));
    }

    // 8. Subject and its bearer confirmation.
    let subject = xml::child(signed_assertion, NS_ASSERTION, "Subject").ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_SUBJECT_CONFIRMATION,
            "The sign-in response was refused.",
            "The assertion must carry a Subject with a bearer confirmation.",
        )
    })?;
    let confirmation = xml::children(subject, NS_ASSERTION, "SubjectConfirmation")
        .into_iter()
        .find(|node| node.attribute("Method") == Some(crate::saml::CONFIRMATION_BEARER));
    let confirmation = confirmation.ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_SUBJECT_CONFIRMATION,
            "The sign-in response was refused.",
            "The Subject must carry a bearer SubjectConfirmation.",
        )
    })?;
    let data =
        xml::child(confirmation, NS_ASSERTION, "SubjectConfirmationData").ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_SUBJECT_CONFIRMATION,
                "The sign-in response was refused.",
                "The bearer confirmation must carry SubjectConfirmationData.",
            )
        })?;
    if data.attribute("Recipient") != Some(sp.acs_url.as_str()) {
        return Err(SsoError::new(
            e::codes::SAML_SUBJECT_CONFIRMATION,
            "The sign-in response was refused.",
            "The SubjectConfirmationData Recipient must be this ACS URL.",
        ));
    }
    if let Some(not_before) = data.attribute("NotBefore").and_then(xml::parse_instant)
        && now + sp.clock_skew_seconds < not_before
    {
        return Err(SsoError::new(
            e::codes::SAML_SUBJECT_CONFIRMATION,
            "The sign-in response was refused.",
            "The bearer confirmation is not valid yet.",
        ));
    }
    if let Some(expiry) = data.attribute("NotOnOrAfter").and_then(xml::parse_instant)
        && now >= expiry + sp.clock_skew_seconds
    {
        return Err(SsoError::new(
            e::codes::SAML_SUBJECT_CONFIRMATION,
            "The sign-in response was refused.",
            "The bearer confirmation has expired.",
        ));
    }
    if let Some(value) = data.attribute("InResponseTo").filter(|v| !v.is_empty())
        && value != pending.request_id
    {
        return Err(SsoError::new(
            e::codes::SAML_IN_RESPONSE_TO_MISMATCH,
            "The sign-in response did not match this attempt.",
            "The SubjectConfirmationData InResponseTo does not name the pending request.",
        ));
    }

    // 9. Authentication statement and context.
    let authn_statement =
        xml::child(signed_assertion, NS_ASSERTION, "AuthnStatement").ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_NO_AUTHN_STATEMENT,
                "The sign-in response was incomplete.",
                "The assertion must carry an AuthnStatement.",
            )
        })?;
    let class_ref = xml::child(authn_statement, NS_ASSERTION, "AuthnContext")
        .and_then(|context| xml::child(context, NS_ASSERTION, "AuthnContextClassRef"))
        .map(xml::simple_text)
        .transpose()?;
    if !idp.acr_values.is_empty()
        && !class_ref
            .as_deref()
            .is_some_and(|value| idp.acr_values.iter().any(|wanted| wanted == value))
    {
        return Err(SsoError::new(
            e::codes::SAML_AUTHN_CONTEXT,
            "The sign-in did not meet this tenant's requirements.",
            "Ask the IdP to authenticate with the required AuthnContextClassRef.",
        ));
    }
    let session_not_on_or_after = authn_statement
        .attribute("SessionNotOnOrAfter")
        .and_then(xml::parse_instant);
    let auth_time = authn_statement
        .attribute("AuthnInstant")
        .and_then(xml::parse_instant);

    // 10. Replay, keyed by the assertion id, kept until the conditions expire.
    let assertion_id = signed_assertion
        .attribute("ID")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_REFERENCE_INVALID,
                "The sign-in response was refused.",
                "The assertion must carry an ID.",
            )
        })?;
    let replay_expiry = not_on_or_after
        .or(session_not_on_or_after)
        .unwrap_or(now + 3600)
        + sp.clock_skew_seconds;
    // A store failure is a refusal (fail closed), never a fresh assertion.
    let key = namespaced("saml-assertion", idp.provider.as_str(), assertion_id);
    if replay.claim(&key, now, replay_expiry).await? == Claim::Replayed {
        return Err(SsoError::new(
            e::codes::SAML_REPLAY,
            "This sign-in response has already been used.",
            "Refuse a replayed assertion; the assertion id is single use.",
        ));
    }

    // 11. Identity, read only from the verified subtree.
    let name_id = xml::child(subject, NS_ASSERTION, "NameID");
    if let Some(name_id) = name_id
        && name_id.attribute("Format") == Some(NAMEID_TRANSIENT)
    {
        return Err(SsoError::new(
            e::codes::SAML_NAMEID_INVALID,
            "This sign-in account cannot be linked.",
            "A transient NameID cannot be used to link an identity.",
        ));
    }
    let attributes = attribute_map(signed_assertion)?;
    let subject_value = if let Some(attribute_name) = &idp.claims_map.subject_attribute {
        first_attribute(&attributes, attribute_name).ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_ATTRIBUTE_MISSING,
                "The sign-in response was incomplete.",
                "The configured subject attribute is missing from the assertion.",
            )
        })?
    } else {
        name_id
            .map(xml::simple_text)
            .transpose()?
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                SsoError::new(
                    e::codes::SAML_NAMEID_INVALID,
                    "The sign-in response was incomplete.",
                    "The assertion must carry a NameID or a configured subject attribute.",
                )
            })?
    };
    let email = first_attribute(&attributes, &idp.claims_map.email);
    let display_name = first_attribute(&attributes, &idp.claims_map.name);
    let groups = attributes
        .get(&idp.claims_map.groups)
        .cloned()
        .unwrap_or_default();
    let raw_claims_hash = jose::encode(&jose::sha256(
        crate::saml::c14n::canonicalize_subtree(signed_assertion, None, false).as_bytes(),
    ));

    let identity = SsoIdentity {
        provider: idp.provider.clone(),
        issuer: assertion_issuer,
        subject: subject_value,
        email_verified: email.is_some(),
        email,
        display_name,
        groups,
        acr: class_ref.clone(),
        amr: class_ref.into_iter().collect(),
        auth_time,
        raw_claims_hash,
    };
    Ok(SamlSignIn {
        identity,
        session_not_on_or_after,
    })
}

/// Collects `AttributeStatement/Attribute` values by name, reading only the verified subtree.
fn attribute_map(assertion: Node<'_, '_>) -> Result<BTreeMap<String, Vec<String>>> {
    let mut attributes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Some(statement) = xml::child(assertion, NS_ASSERTION, "AttributeStatement") else {
        return Ok(attributes);
    };
    for attribute in xml::children(statement, NS_ASSERTION, "Attribute") {
        let Some(name) = attribute.attribute("Name") else {
            continue;
        };
        let mut values = Vec::new();
        for value in xml::children(attribute, NS_ASSERTION, "AttributeValue") {
            values.push(xml::simple_text(value)?);
        }
        attributes
            .entry(name.to_owned())
            .or_default()
            .extend(values);
    }
    Ok(attributes)
}

/// The first value of a named attribute.
fn first_attribute(attributes: &BTreeMap<String, Vec<String>>, name: &str) -> Option<String> {
    attributes
        .get(name)
        .and_then(|values| values.first())
        .cloned()
}

fn conditions_invalid(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_CONDITIONS_INVALID,
        "The sign-in response was refused.",
        "The assertion's Conditions are outside the accepted time window.",
    )
    .with_detail(detail)
}
