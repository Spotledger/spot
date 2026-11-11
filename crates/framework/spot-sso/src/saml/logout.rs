//! SP-initiated SAML logout: building a `LogoutRequest` and validating a `LogoutResponse`.
//!
//! Only SP-initiated single logout is in scope. The `LogoutRequest` travels on the HTTP-Redirect
//! binding and is signed the same way an `AuthnRequest` is: this crate returns the bytes to sign
//! and the key reference, never a private key. IdP-initiated single logout is out of scope.

use crate::error::{self as e, Result, SsoError};
use crate::jose;
use crate::saml::request::{RedirectSignature, encode_redirect};
use crate::saml::xml::{self, NS_ASSERTION, NS_PROTOCOL};
use crate::saml::{IdpConfig, SpConfig};

/// The `samlp:StatusCode` value for success.
const STATUS_SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";

/// A prepared `LogoutRequest`: the redirect URL and any signature to append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogoutRequest {
    /// The full redirect URL, without the `Signature` parameter.
    pub url: String,
    /// The signature the caller must compute and append, when configured.
    pub signing: Option<RedirectSignature>,
    /// The request id a subsequent `LogoutResponse` must reference.
    pub request_id: String,
}

/// Builds an SP-initiated `LogoutRequest` for the HTTP-Redirect binding.
///
/// # Errors
///
/// [`e::codes::SAML_NO_SSO_ENDPOINT`] when the IdP publishes no single-logout endpoint;
/// [`e::codes::RNG_INSUFFICIENT`] when fewer than 32 random bytes are supplied;
/// [`e::codes::SAML_SIGNING_KEY_MISSING`] when signing is requested without a key reference.
pub fn build_logout_request(
    sp: &SpConfig,
    idp: &IdpConfig,
    rng: &[u8],
    now: i64,
    name_id: &str,
    name_id_format: &str,
    session_index: Option<&str>,
) -> Result<LogoutRequest> {
    let slo_url = idp.slo_url.as_deref().ok_or_else(|| {
        SsoError::new(
            e::codes::SAML_NO_SSO_ENDPOINT,
            "This sign-in method cannot sign you out at the provider.",
            "The IdP metadata publishes no SingleLogoutService.",
        )
    })?;
    if rng.len() < 32 {
        return Err(SsoError::new(
            e::codes::RNG_INSUFFICIENT,
            "The sign-out could not be prepared.",
            "Supply at least 32 random bytes to begin a sign-out.",
        ));
    }
    let request_id = format!(
        "_{}",
        jose::encode(&jose::sha256(&[b"logout:".as_slice(), rng].concat()))
    );
    let mut message = String::new();
    message.push_str("<samlp:LogoutRequest xmlns:samlp=\"");
    message.push_str(NS_PROTOCOL);
    message.push_str("\" xmlns:saml=\"");
    message.push_str(NS_ASSERTION);
    message.push_str("\" ID=\"");
    message.push_str(&xml::escape_attribute(&request_id));
    message.push_str("\" Version=\"2.0\" IssueInstant=\"");
    message.push_str(&xml::format_instant(now));
    message.push_str("\" Destination=\"");
    message.push_str(&xml::escape_attribute(slo_url));
    message.push_str("\"><saml:Issuer>");
    message.push_str(&xml::escape_text(&sp.entity_id));
    message.push_str("</saml:Issuer><saml:NameID Format=\"");
    message.push_str(&xml::escape_attribute(name_id_format));
    message.push_str("\">");
    message.push_str(&xml::escape_text(name_id));
    message.push_str("</saml:NameID>");
    if let Some(index) = session_index {
        message.push_str("<samlp:SessionIndex>");
        message.push_str(&xml::escape_text(index));
        message.push_str("</samlp:SessionIndex>");
    }
    message.push_str("</samlp:LogoutRequest>");

    let encoded = encode_redirect(&message)?;
    let mut query = format!(
        "SAMLRequest={}",
        url::form_urlencoded::byte_serialize(encoded.as_bytes()).collect::<String>()
    );
    let signing = if sp.sign_authn_requests {
        crate::saml::request::redirect_signature(sp, &mut query)?
    } else {
        None
    };
    Ok(LogoutRequest {
        url: format!("{slo_url}?{query}"),
        signing,
        request_id,
    })
}

/// Validates a POSTed `LogoutResponse`.
///
/// # Errors
///
/// [`e::codes::SAML_NOT_A_RESPONSE`] for a non-`LogoutResponse` root;
/// [`e::codes::SAML_STATUS_NOT_SUCCESS`] for a non-success status;
/// [`e::codes::SAML_IN_RESPONSE_TO_MISMATCH`] when it does not name the pending logout request.
pub fn validate_logout_response(pending_request_id: &str, saml_response: &str) -> Result<()> {
    let decoded = xml::decode_base64(saml_response)?;
    let text = std::str::from_utf8(&decoded).map_err(|_| {
        SsoError::new(
            e::codes::SAML_NOT_UTF8,
            "The sign-out response could not be read.",
            "Check the identity provider sends UTF-8 SAML XML.",
        )
    })?;
    let document = xml::parse_document(text)?;
    let root = document.root_element();
    if !xml::is_tag(root, NS_PROTOCOL, "LogoutResponse") {
        return Err(SsoError::new(
            e::codes::SAML_NOT_A_RESPONSE,
            "The sign-out response was refused.",
            "The response root must be a samlp:LogoutResponse.",
        ));
    }
    let status = root
        .descendants()
        .find(|node| xml::is_tag(*node, NS_PROTOCOL, "StatusCode"))
        .and_then(|node| node.attribute("Value"))
        .unwrap_or_default();
    if status != STATUS_SUCCESS {
        return Err(SsoError::new(
            e::codes::SAML_STATUS_NOT_SUCCESS,
            "The identity provider refused the sign-out.",
            "The IdP returned a non-success logout status.",
        ));
    }
    if let Some(value) = root.attribute("InResponseTo").filter(|v| !v.is_empty())
        && value != pending_request_id
    {
        return Err(SsoError::new(
            e::codes::SAML_IN_RESPONSE_TO_MISMATCH,
            "The sign-out response did not match this attempt.",
            "The LogoutResponse InResponseTo does not name the pending request.",
        ));
    }
    Ok(())
}
