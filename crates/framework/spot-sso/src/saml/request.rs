//! SP-initiated `AuthnRequest`: building it and encoding it for the HTTP-Redirect binding.
//!
//! [`begin`] produces the redirect URL and the [`SamlPendingAuth`] the caller stores. The URL's
//! `SAMLRequest` is raw-DEFLATE, Base64, then percent-encoded, exactly as the SAML 2.0 bindings
//! require. When the SP signs requests, the query string (including `SigAlg`) is returned as a
//! [`RedirectSignature`] for the caller to sign with the key named by
//! [`crate::saml::SpConfig::signing_key_ref`]; this crate never holds a private key.

use std::io::Write;

use flate2::Compression;
use flate2::write::DeflateEncoder;
use serde::{Deserialize, Serialize};
use url::form_urlencoded;

use crate::error::{self as e, Result, SsoError};
use crate::jose;
use crate::pending::PENDING_TTL_SECONDS;
use crate::provider::{Jwa, ProviderId};
use crate::saml::signature::{SIG_ECDSA_SHA256, SIG_RSA_SHA256};
use crate::saml::xml::{self, NS_ASSERTION, NS_PROTOCOL};
use crate::saml::{BINDING_POST, IdpConfig, NAMEID_PERSISTENT, SpConfig};

/// Options for [`begin`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BeginOptions {
    /// Force the IdP to re-authenticate the user (`ForceAuthn`), as step-up requires.
    pub force_authn: bool,
}

/// The pending SAML sign-in the caller stores between `begin` and `complete`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamlPendingAuth {
    /// The provider the sign-in is with.
    pub provider: ProviderId,
    /// The `AuthnRequest` `ID` the response's `InResponseTo` must echo.
    pub request_id: String,
    /// The opaque `RelayState` echoed back by the IdP on the POST.
    pub relay_state: String,
    /// The same-site relative path to return the browser to after sign-in.
    pub return_to: String,
    /// When the sign-in began, in unix seconds.
    pub created_at: i64,
    /// When it stops being valid, in unix seconds.
    pub expires_at: i64,
    /// Whether it has already been used (single use).
    pub used: bool,
}

impl std::fmt::Debug for SamlPendingAuth {
    /// Redacts the `RelayState`: it is a single-use value that should not reach a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SamlPendingAuth")
            .field("provider", &self.provider)
            .field("request_id", &self.request_id)
            .field("relay_state", &"<redacted>")
            .field("return_to", &self.return_to)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("used", &self.used)
            .finish()
    }
}

impl SamlPendingAuth {
    /// Whether the pending sign-in has expired at `now`.
    #[must_use]
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    /// Consumes the pending sign-in once, refusing a replay or an expiry.
    ///
    /// # Errors
    ///
    /// [`e::codes::STATE_REPLAYED`] when it was already used; [`e::codes::STATE_EXPIRED`] when
    /// its time has passed.
    pub fn consume(&self, now: i64) -> Result<SamlPendingAuth> {
        if self.used {
            return Err(SsoError::new(
                e::codes::STATE_REPLAYED,
                "This sign-in attempt has already been completed.",
                "Start the sign-in again from the beginning.",
            ));
        }
        if self.is_expired(now) {
            return Err(SsoError::new(
                e::codes::STATE_EXPIRED,
                "This sign-in attempt has taken too long.",
                "Start the sign-in again; the attempt expires after ten minutes.",
            ));
        }
        Ok(SamlPendingAuth {
            used: true,
            ..self.clone()
        })
    }
}

/// The signing the caller must apply to a redirect query string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedirectSignature {
    /// The algorithm the SP signs with.
    pub algorithm: Jwa,
    /// The `spot-secrets` name of the SP signing key; never the key.
    pub key_ref: String,
    /// The exact bytes to sign: the percent-encoded query string including `SigAlg`.
    pub signing_input: String,
}

/// A prepared `AuthnRequest`: the redirect URL and any signature to append.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthnRequest {
    /// The full redirect URL, without the `Signature` parameter.
    pub url: String,
    /// The signature the caller must compute and append, when configured.
    pub signing: Option<RedirectSignature>,
}

/// Begins a SAML sign-in: builds the `AuthnRequest` and the pending state to store.
///
/// `rng` must be at least 32 bytes from the caller's CSPRNG; the request id and `RelayState` are
/// derived from it with domain separation.
///
/// # Errors
///
/// [`e::codes::BAD_RETURN_TO`] for a `return_to` that is not a same-site relative path;
/// [`e::codes::RNG_INSUFFICIENT`] when fewer than 32 random bytes are supplied;
/// [`e::codes::SAML_SIGNING_KEY_MISSING`] when signing is requested without a key reference.
pub fn begin(
    sp: &SpConfig,
    idp: &IdpConfig,
    rng: &[u8],
    now: i64,
    return_to: &str,
    options: BeginOptions,
) -> Result<(AuthnRequest, SamlPendingAuth)> {
    if !is_safe_return_to(return_to) {
        return Err(SsoError::new(
            e::codes::BAD_RETURN_TO,
            "The page to return to is not allowed.",
            "Return to a path inside this application, not another site.",
        ));
    }
    if rng.len() < 32 {
        return Err(SsoError::new(
            e::codes::RNG_INSUFFICIENT,
            "The sign-in could not be prepared.",
            "Supply at least 32 random bytes to begin a sign-in.",
        ));
    }
    let request_id = format!(
        "_{}",
        jose::encode(&jose::sha256(&[b"request:".as_slice(), rng].concat()))
    );
    let relay_state = jose::encode(&jose::sha256(&[b"relay:".as_slice(), rng].concat()));

    let request_xml = build_authn_request_xml(sp, idp, &request_id, now, options);
    let encoded = encode_redirect(&request_xml)?;

    let mut query = format!(
        "SAMLRequest={}",
        form_urlencoded::byte_serialize(encoded.as_bytes()).collect::<String>()
    );
    query.push_str("&RelayState=");
    query.push_str(&form_urlencoded::byte_serialize(relay_state.as_bytes()).collect::<String>());

    let signing = redirect_signature(sp, &mut query)?;
    let url = format!("{}?{query}", idp.sso_url);

    let pending = SamlPendingAuth {
        provider: idp.provider.clone(),
        request_id,
        relay_state,
        return_to: return_to.to_owned(),
        created_at: now,
        expires_at: now + PENDING_TTL_SECONDS,
        used: false,
    };
    Ok((AuthnRequest { url, signing }, pending))
}

/// Appends `SigAlg` and returns the signature the caller must compute, when the SP signs.
pub(crate) fn redirect_signature(
    sp: &SpConfig,
    query: &mut String,
) -> Result<Option<RedirectSignature>> {
    if !sp.sign_authn_requests {
        return Ok(None);
    }
    let Some(key_ref) = sp.signing_key_ref.clone() else {
        return Err(SsoError::new(
            e::codes::SAML_SIGNING_KEY_MISSING,
            "The sign-in could not be prepared.",
            "Name the SP signing key in spot-secrets, or turn off request signing.",
        ));
    };
    let algorithm = sp.signing_algorithm.unwrap_or(Jwa::Rs256);
    let algorithm_uri = match algorithm {
        Jwa::Rs256 => SIG_RSA_SHA256,
        Jwa::Es256 => SIG_ECDSA_SHA256,
    };
    query.push_str("&SigAlg=");
    query.push_str(&form_urlencoded::byte_serialize(algorithm_uri.as_bytes()).collect::<String>());
    Ok(Some(RedirectSignature {
        algorithm,
        key_ref,
        signing_input: query.clone(),
    }))
}

/// Builds the `AuthnRequest` XML.
fn build_authn_request_xml(
    sp: &SpConfig,
    idp: &IdpConfig,
    request_id: &str,
    now: i64,
    options: BeginOptions,
) -> String {
    let mut xml = String::new();
    xml.push_str("<samlp:AuthnRequest xmlns:samlp=\"");
    xml.push_str(NS_PROTOCOL);
    xml.push_str("\" xmlns:saml=\"");
    xml.push_str(NS_ASSERTION);
    xml.push_str("\" ID=\"");
    xml.push_str(&xml::escape_attribute(request_id));
    xml.push_str("\" Version=\"2.0\" IssueInstant=\"");
    xml.push_str(&xml::format_instant(now));
    xml.push_str("\" Destination=\"");
    xml.push_str(&xml::escape_attribute(&idp.sso_url));
    xml.push_str("\" AssertionConsumerServiceURL=\"");
    xml.push_str(&xml::escape_attribute(&sp.acs_url));
    xml.push_str("\" ProtocolBinding=\"");
    xml.push_str(BINDING_POST);
    xml.push('"');
    if options.force_authn {
        xml.push_str(" ForceAuthn=\"true\"");
    }
    xml.push_str("><saml:Issuer>");
    xml.push_str(&xml::escape_text(&sp.entity_id));
    xml.push_str("</saml:Issuer><samlp:NameIDPolicy Format=\"");
    xml.push_str(NAMEID_PERSISTENT);
    xml.push_str("\" AllowCreate=\"false\"/>");
    if !idp.acr_values.is_empty() {
        xml.push_str("<samlp:RequestedAuthnContext Comparison=\"exact\">");
        for acr in &idp.acr_values {
            xml.push_str("<saml:AuthnContextClassRef>");
            xml.push_str(&xml::escape_text(acr));
            xml.push_str("</saml:AuthnContextClassRef>");
        }
        xml.push_str("</samlp:RequestedAuthnContext>");
    }
    xml.push_str("</samlp:AuthnRequest>");
    xml
}

/// Raw-DEFLATEs then Base64-encodes a message for the HTTP-Redirect binding.
///
/// # Errors
///
/// [`e::codes::SAML_XML_INVALID`] when compression fails (it does not for in-memory input).
pub(crate) fn encode_redirect(message: &str) -> Result<String> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(message.as_bytes())
        .map_err(|_| deflate_failed())?;
    let compressed = encoder.finish().map_err(|_| deflate_failed())?;
    Ok(xml::encode_base64(&compressed))
}

/// Inflates a raw-DEFLATE, Base64 message from the HTTP-Redirect binding.
///
/// # Errors
///
/// [`e::codes::SAML_XML_INVALID`] when the bytes are not valid raw DEFLATE.
pub fn decode_redirect(encoded: &str) -> Result<String> {
    let compressed = xml::decode_base64(encoded)?;
    let mut decoder = flate2::read::DeflateDecoder::new(compressed.as_slice());
    let mut out = String::new();
    std::io::Read::read_to_string(&mut decoder, &mut out).map_err(|_| deflate_failed())?;
    Ok(out)
}

/// Whether `return_to` is a same-site relative path (blocks open redirects).
fn is_safe_return_to(return_to: &str) -> bool {
    return_to.starts_with('/')
        && !return_to.starts_with("//")
        && !return_to.starts_with("/\\")
        && !return_to.chars().any(char::is_control)
}

fn deflate_failed() -> SsoError {
    SsoError::new(
        e::codes::SAML_XML_INVALID,
        "The sign-in response could not be read.",
        "Check the SAML message is raw-DEFLATE compressed and Base64-encoded.",
    )
}
