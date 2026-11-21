//! SAML 2.0 IdP-initiated single logout: validating a `LogoutRequest` and answering it.
//!
//! SP-initiated logout ([`super::logout`]) is the other direction. Here the **identity provider**
//! starts: it sends a signed `samlp:LogoutRequest` naming a `NameID` and zero or more
//! `SessionIndex` values. This module verifies that request and returns the
//! [`LogoutTarget`] it names; it never touches a session. The caller resolves the subject to
//! linked identities ([`crate::logout::resolve_target`]) and ends the sessions, then answers
//! with [`build_logout_response`].
//!
//! # What a `LogoutRequest` must satisfy
//!
//! - a valid [`SpConfig`] ([`SpConfig::validate`]): a bad logout lifetime refuses every request;
//! - parse without a DTD or entity declaration, and have a `samlp:LogoutRequest` root;
//! - carry no `Assertion`, `Extensions` or `ds:Object` that could hide a second copy of the
//!   data that is read;
//! - an enveloped signature over **the root request itself**, verified against the IdP's pinned
//!   certificates with the same wrapping checks as a `Response` (single signature, single
//!   `Reference` naming the request's own unique `ID`, SHA-256 digest, RSA-SHA256/ECDSA-SHA256,
//!   `KeyInfo` never trusted);
//! - `Issuer` exactly the configured IdP entity id;
//! - `Destination` exactly this service provider's single-logout URL;
//! - `NotOnOrAfter`, when present, not past (with the configured skew) and no further ahead
//!   than the configured maximum lifetime; when absent, an `IssueInstant` no older than that same
//!   lifetime (plus skew), with the replay record kept for exactly that window;
//! - an `ID` recorded once through a [`crate::replay::ReplayStore`];
//! - a non-empty, non-transient `NameID`, and any number of `SessionIndex` values.

use crate::error::{self as e, Result, SsoError};
use crate::jose;
use crate::logout::LogoutTarget;
use crate::replay::{Claim, ReplayStore, namespaced};
use crate::saml::signature;
use crate::saml::xml::{self, NS_ASSERTION, NS_DS, NS_PROTOCOL};
use crate::saml::{IdpConfig, NAMEID_TRANSIENT, SpConfig};

/// The `samlp:StatusCode` value for full success.
pub const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
/// The `samlp:StatusCode` value for a partial logout.
pub const STATUS_PARTIAL: &str = "urn:oasis:names:tc:SAML:2.0:status:PartialLogout";

/// How much of a single logout succeeded, as the response reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogoutStatus {
    /// Every named session was ended.
    Success,
    /// Some named sessions were ended, or the provider gave no session to end.
    Partial,
}

impl LogoutStatus {
    /// The top-level `samlp:StatusCode` value.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Success => STATUS_SUCCESS,
            Self::Partial => STATUS_PARTIAL,
        }
    }
}

/// Validates an IdP-initiated `LogoutRequest` and returns what it names.
///
/// `xml` is the decoded SAML message (the caller inflates a Redirect binding with
/// [`super::request::decode_redirect`], or decodes the POST form field). `now` is the caller's
/// clock; nothing here reads one. `replay` records the request `ID` only after the signature and
/// every claim have passed, so an unauthenticated request cannot fill the store.
///
/// # Errors
///
/// One of the `sso.saml.*` codes, in the order the checks run: XML shape, root, wrapper
/// elements, signature, issuer, destination, expiry, replay, `NameID`; and
/// `sso.replay_store_unavailable` when the replay store cannot answer (the request is refused).
pub async fn validate_logout_request(
    sp: &SpConfig,
    idp: &IdpConfig,
    xml_text: &str,
    now: i64,
    replay: &impl ReplayStore,
) -> Result<LogoutTarget> {
    // 0. A bad lifetime setting refuses every request: a non-positive value would do so silently
    //    and a huge one would remove the bound on the replay record.
    if let Err(errors) = sp.validate()
        && let Some(problem) = errors.problems().first()
    {
        return Err(problem.clone());
    }
    // 1. Shape: XML without a DTD, and a LogoutRequest root.
    let document = xml::parse_document(xml_text)?;
    let root = document.root_element();
    if !xml::is_tag(root, NS_PROTOCOL, "LogoutRequest") {
        return Err(SsoError::new(
            e::codes::SAML_SLO_NOT_A_REQUEST,
            "The sign-out request was refused.",
            "The request root must be a samlp:LogoutRequest.",
        ));
    }
    // 2. No wrapper element that could hide a second copy of what was verified.
    if root.descendants().chain(std::iter::once(root)).any(|node| {
        xml::is_tag(node, NS_ASSERTION, "Assertion")
            || xml::is_tag(node, NS_ASSERTION, "Extensions")
            || xml::is_tag(node, NS_PROTOCOL, "Extensions")
            || xml::is_tag(node, NS_DS, "Object")
    }) {
        return Err(SsoError::new(
            e::codes::SAML_WRAPPER_FORBIDDEN,
            "The sign-out request was refused.",
            "The message carries an Assertion, Extensions or Object element; refuse it.",
        ));
    }
    // 3. The signature, and the rule that the element verified is the element read.
    let verified = signature::verify_logout_request(&document, &idp.signing_certificates)?;
    if verified != root {
        return Err(SsoError::new(
            e::codes::SAML_WRAPPER_FORBIDDEN,
            "The sign-out request was refused.",
            "The verified LogoutRequest is not the message root; refuse it.",
        ));
    }
    // 4. Issuer, exactly the configured IdP entity id.
    let issuer = xml::child(root, NS_ASSERTION, "Issuer")
        .map(xml::simple_text)
        .transpose()?
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_ISSUER_MISMATCH,
                "The sign-out request was not trusted.",
                "The LogoutRequest must carry a saml:Issuer.",
            )
        })?;
    if issuer != idp.entity_id {
        return Err(SsoError::new(
            e::codes::SAML_ISSUER_MISMATCH,
            "The sign-out request was not trusted.",
            "The LogoutRequest Issuer does not match the configured IdP entity id.",
        ));
    }
    // 5. Destination, exactly this service provider's single-logout URL.
    let slo_url = sp.slo_url.as_deref().ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_SLO_ENDPOINT_MISSING,
            "This sign-in method cannot sign you out here.",
            "Set the service provider's single-logout URL before accepting IdP-initiated logout.",
        )
    })?;
    if root.attribute("Destination") != Some(slo_url) {
        return Err(SsoError::new(
            e::codes::SAML_SLO_DESTINATION_MISMATCH,
            "The sign-out request was refused.",
            "The LogoutRequest Destination must be this service provider's SLO URL.",
        ));
    }
    // 6. NotOnOrAfter, when present, not past (with skew).
    let not_on_or_after = root.attribute("NotOnOrAfter").and_then(xml::parse_instant);
    if let Some(expiry) = not_on_or_after
        && now >= expiry + sp.clock_skew_seconds
    {
        return Err(SsoError::new(
            e::codes::SAML_SLO_EXPIRED,
            "The sign-out request has taken too long.",
            "The LogoutRequest NotOnOrAfter has passed; refuse it and let the provider retry.",
        ));
    }
    // A `NotOnOrAfter` far in the future would keep the replay record, and the request, alive
    // for as long as the IdP likes: bound it.
    if let Some(expiry) = not_on_or_after
        && expiry.saturating_sub(now) > sp.max_logout_lifetime_seconds
    {
        return Err(SsoError::new(
            e::codes::SAML_SLO_LIFETIME_EXCESSIVE,
            "The sign-out request was refused.",
            "The LogoutRequest NotOnOrAfter lies further ahead than this tenant allows; refuse it.",
        ));
    }
    // A request with no `NotOnOrAfter` is bounded to the same lifetime as an OIDC logout token:
    // its `IssueInstant` must be present, not ahead of now beyond the skew, and no older than the
    // configured lifetime (plus skew). The replay record below lives exactly that long.
    let window_end = if not_on_or_after.is_none() {
        let issued = root
            .attribute("IssueInstant")
            .and_then(xml::parse_instant)
            .filter(|issued| *issued <= now.saturating_add(sp.clock_skew_seconds));
        let end = issued.map(|issued| {
            issued
                .saturating_add(sp.max_logout_lifetime_seconds)
                .saturating_add(sp.clock_skew_seconds)
        });
        match end {
            Some(end) if now < end => Some(end),
            _ => {
                return Err(SsoError::new(
                    e::codes::SAML_SLO_EXPIRED,
                    "The sign-out request has taken too long.",
                    "A LogoutRequest without NotOnOrAfter needs a recent IssueInstant; refuse it.",
                ));
            }
        }
    } else {
        None
    };
    // 7. Replay: the request ID is single use, kept until the request could no longer pass.
    let request_id = root
        .attribute("ID")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SsoError::new(
                e::codes::SAML_REFERENCE_INVALID,
                "The sign-out request was refused.",
                "The LogoutRequest must carry an ID.",
            )
        })?;
    let replay_expiry = match (not_on_or_after, window_end) {
        (Some(expiry), _) => expiry.saturating_add(sp.clock_skew_seconds),
        (None, Some(end)) => end,
        // Unreachable: a request without `NotOnOrAfter` always has a window end. Fail closed.
        (None, None) => {
            return Err(SsoError::new(
                e::codes::SAML_SLO_EXPIRED,
                "The sign-out request has taken too long.",
                "A LogoutRequest without NotOnOrAfter needs a recent IssueInstant; refuse it.",
            ));
        }
    };
    // A store failure is a refusal (fail closed), never a fresh request.
    let key = namespaced("saml-logout", idp.provider.as_str(), request_id);
    if replay.claim(&key, now, replay_expiry).await? == Claim::Replayed {
        return Err(SsoError::new(
            e::codes::SAML_SLO_REPLAY,
            "This sign-out request has already been used.",
            "Refuse a replayed LogoutRequest; the request ID is single use.",
        ));
    }
    // 8. The subject: a persistent or unspecified NameID, never a transient one.
    let name_id_node = xml::child(root, NS_ASSERTION, "NameID").ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_SLO_NAME_ID_MISSING,
            "The sign-out request was incomplete.",
            "A LogoutRequest must carry a saml:NameID.",
        )
    })?;
    if name_id_node.attribute("Format") == Some(NAMEID_TRANSIENT) {
        return Err(SsoError::new(
            e::codes::SAML_NAMEID_INVALID,
            "This sign-out account cannot be matched.",
            "A transient NameID cannot be matched to a linked identity; end nothing.",
        ));
    }
    let name_id = xml::simple_text(name_id_node)?;
    if name_id.is_empty() {
        return Err(SsoError::new(
            e::codes::SAML_SLO_NAME_ID_MISSING,
            "The sign-out request was incomplete.",
            "A LogoutRequest must carry a non-empty saml:NameID.",
        ));
    }
    // 9. The sessions the provider names; zero is valid and means "all of the subject's".
    let session_indexes = xml::children(root, NS_PROTOCOL, "SessionIndex")
        .into_iter()
        .map(xml::simple_text)
        .collect::<Result<Vec<String>>>()?
        .into_iter()
        .filter(|index| !index.is_empty())
        .collect();
    Ok(LogoutTarget {
        provider: idp.provider.clone(),
        issuer: idp.entity_id.clone(),
        subject: Some(name_id),
        session_id: None,
        session_indexes,
    })
}

/// Builds the `LogoutResponse` the service provider sends back to the IdP.
///
/// The response names the request it answers (`InResponseTo`), this SP as `Issuer`, the IdP's
/// single-logout endpoint as `Destination`, and the outcome as its status. Its own id is derived
/// deterministically from the request id, so answering the same request twice produces the same
/// document and nothing here needs an entropy source.
///
/// # Errors
///
/// [`e::codes::SAML_SLO_ENDPOINT_MISSING`] when the IdP publishes no single-logout endpoint.
pub fn build_logout_response(
    sp: &SpConfig,
    idp: &IdpConfig,
    request_id: &str,
    status: LogoutStatus,
    now: i64,
) -> Result<String> {
    let slo_url = idp.slo_url.as_deref().ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_SLO_ENDPOINT_MISSING,
            "This sign-in method cannot answer the sign-out here.",
            "The IdP metadata publishes no SingleLogoutService.",
        )
    })?;
    let response_id = format!(
        "_{}",
        jose::encode(&jose::sha256(
            &[b"logout-response:".as_slice(), request_id.as_bytes()].concat()
        ))
    );
    let mut message = String::new();
    message.push_str("<samlp:LogoutResponse xmlns:samlp=\"");
    message.push_str(NS_PROTOCOL);
    message.push_str("\" xmlns:saml=\"");
    message.push_str(NS_ASSERTION);
    message.push_str("\" ID=\"");
    message.push_str(&xml::escape_attribute(&response_id));
    message.push_str("\" Version=\"2.0\" IssueInstant=\"");
    message.push_str(&xml::format_instant(now));
    message.push_str("\" Destination=\"");
    message.push_str(&xml::escape_attribute(slo_url));
    message.push_str("\" InResponseTo=\"");
    message.push_str(&xml::escape_attribute(request_id));
    message.push_str("\"><saml:Issuer>");
    message.push_str(&xml::escape_text(&sp.entity_id));
    message.push_str("</saml:Issuer><samlp:Status><samlp:StatusCode Value=\"");
    message.push_str(status.code());
    message.push_str("\"/></samlp:Status></samlp:LogoutResponse>");
    Ok(message)
}
