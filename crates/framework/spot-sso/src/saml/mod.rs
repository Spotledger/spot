//! SAML 2.0 Web Browser SSO (SP-initiated) as a sans-I/O, clock-free state machine
//! (WP-IAM-05.T2, ADR-070 sections 5 and 7).
//!
//! This is the second half of `spot-sso`'s single sign-on support. Where [`crate::oidc`] carries
//! the OpenID Connect authorization-code flow, this module carries **SAML 2.0**: SP metadata,
//! the HTTP-Redirect `AuthnRequest`, the HTTP-POST `Response` and `Assertion`, XML-signature
//! verification that refuses the whole signature-wrapping family, replay protection, SP-initiated
//! logout and the same [`SsoIdentity`] that [`crate::resolve`] turns into a sign-in.
//!
//! # The one rule that makes SAML safe
//!
//! An XML signature signs a *reference to an element*, not the element a reader later reads.
//! Everywhere in this module the verified element **is** the element read: the signature must be
//! enveloped on the `Response` or `Assertion`, its single `Reference` must name its own parent,
//! the referenced `ID` must be unique, and identity is extracted only from the verified subtree
//! through typed accessors. There is no XPath and no document-wide search for a `NameID`.
//!
//! # Purity
//!
//! Nothing here reads the clock, the network, the filesystem or an entropy source. Random bytes,
//! the current time, the pinned certificates and the GET/POST bodies are all supplied by the
//! caller; the metadata document is fetched by the caller through `spot-egress`.

pub mod c14n;
mod keys;
pub mod logout;
pub mod metadata;
pub mod request;
pub mod response;
pub(crate) mod signature;
pub mod slo;
pub(crate) mod xml;

pub use crate::replay;
pub use crate::replay::{Claim, MemoryReplayStore, ReplayError, ReplayStore};
pub use c14n::{canonicalize, canonicalize_by_id};
pub use logout::{LogoutRequest, build_logout_request, validate_logout_response};
pub use metadata::{IdpMetadata, MetadataTrust, SamlEndpoint, parse_idp_metadata, sp_metadata};
pub use request::{AuthnRequest, BeginOptions, RedirectSignature, SamlPendingAuth, begin};
pub use response::{SamlSignIn, complete};
pub use signature::SignaturePolicy;
pub use slo::{LogoutStatus, build_logout_response, validate_logout_request};

use serde::{Deserialize, Serialize};

use crate::error::{SsoError, SsoErrors};
use crate::provider::{Jwa, ProviderConfig, ProviderId};

pub(crate) use xml::NS_PROTOCOL;

/// The SAML 2.0 HTTP-Redirect binding.
pub const BINDING_REDIRECT: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-Redirect";
/// The SAML 2.0 HTTP-POST binding.
pub const BINDING_POST: &str = "urn:oasis:names:tc:SAML:2.0:bindings:HTTP-POST";
/// The persistent `NameID` format.
pub const NAMEID_PERSISTENT: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:persistent";
/// The transient `NameID` format; never usable for linking.
pub const NAMEID_TRANSIENT: &str = "urn:oasis:names:tc:SAML:2.0:nameid-format:transient";
/// The unspecified `NameID` format.
pub const NAMEID_UNSPECIFIED: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:unspecified";
/// The e-mail-address `NameID` format.
pub const NAMEID_EMAIL: &str = "urn:oasis:names:tc:SAML:1.1:nameid-format:emailAddress";
/// The bearer subject-confirmation method.
pub const CONFIRMATION_BEARER: &str = "urn:oasis:names:tc:SAML:2.0:cm:bearer";

/// How an IdP endpoint is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamlBinding {
    /// HTTP-Redirect (query string, raw DEFLATE).
    Redirect,
    /// HTTP-POST (form field, Base64).
    Post,
}

impl SamlBinding {
    /// The SAML binding URI.
    #[must_use]
    pub fn uri(self) -> &'static str {
        match self {
            Self::Redirect => BINDING_REDIRECT,
            Self::Post => BINDING_POST,
        }
    }

    /// The binding named by a SAML binding URI, when it is one this crate supports.
    #[must_use]
    pub fn parse(uri: &str) -> Option<Self> {
        match uri {
            BINDING_REDIRECT => Some(Self::Redirect),
            BINDING_POST => Some(Self::Post),
            _ => None,
        }
    }
}

/// The service provider's configuration: what this tenant is to an IdP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpConfig {
    /// The SP entity id, an absolute URI and the audience assertions must name.
    pub entity_id: String,
    /// The assertion consumer service URL (HTTP-POST).
    pub acs_url: String,
    /// The single-logout service URL, when the SP offers one.
    pub slo_url: Option<String>,
    /// The SP signing certificate, DER-encoded, when the SP signs requests.
    pub signing_certificate: Option<Vec<u8>>,
    /// The SP encryption certificate, DER-encoded, when the SP can decrypt assertions.
    pub encryption_certificate: Option<Vec<u8>>,
    /// Whether the SP signs `AuthnRequest`s (requires [`SpConfig::signing_key_ref`]).
    pub sign_authn_requests: bool,
    /// The name under which `spot-secrets` holds the SP signing key; never the key itself.
    pub signing_key_ref: Option<String>,
    /// The SP signing algorithm, when signing is configured.
    pub signing_algorithm: Option<Jwa>,
    /// The tolerated clock skew, in seconds.
    pub clock_skew_seconds: i64,
    /// The furthest ahead of now a `LogoutRequest`'s `NotOnOrAfter` may lie, in seconds.
    pub max_logout_lifetime_seconds: i64,
    /// Whether IdP-initiated (unsolicited) responses are accepted (off by default).
    pub allow_idp_initiated: bool,
    /// Which element the policy requires to be signed.
    pub signature_policy: SignaturePolicy,
    /// The size cap on a POSTed response, in bytes.
    pub max_response_bytes: usize,
}

impl Default for SpConfig {
    fn default() -> Self {
        Self {
            entity_id: String::new(),
            acs_url: String::new(),
            slo_url: None,
            signing_certificate: None,
            encryption_certificate: None,
            sign_authn_requests: false,
            signing_key_ref: None,
            signing_algorithm: None,
            clock_skew_seconds: 120,
            max_logout_lifetime_seconds: 600,
            allow_idp_initiated: false,
            signature_policy: SignaturePolicy::Assertion,
            max_response_bytes: 256 * 1024,
        }
    }
}

impl SpConfig {
    /// The longest a `LogoutRequest` may live, in seconds: the ceiling the OIDC back-channel
    /// logout token shares ([`ProviderConfig::MAX_LOGOUT_TOKEN_LIFETIME_SECONDS`]).
    pub const MAX_LOGOUT_LIFETIME_SECONDS: i64 = ProviderConfig::MAX_LOGOUT_TOKEN_LIFETIME_SECONDS;

    /// Checks the settings that bound a replay record, reporting every problem at once.
    ///
    /// The logout lifetime must be at least 1 and at most
    /// [`Self::MAX_LOGOUT_LIFETIME_SECONDS`]: a non-positive value would refuse every request
    /// and a huge one would remove the bound that keeps a replay record short.
    ///
    /// # Errors
    ///
    /// [`SsoErrors`] with [`crate::codes::LOGOUT_LIFETIME_INVALID`] when it is out of bounds.
    pub fn validate(&self) -> std::result::Result<(), SsoErrors> {
        let mut problems = Vec::new();
        if !(1..=Self::MAX_LOGOUT_LIFETIME_SECONDS).contains(&self.max_logout_lifetime_seconds) {
            problems.push(SsoError::new(
                crate::error::codes::LOGOUT_LIFETIME_INVALID,
                "The identity provider is not configured completely.",
                "Set the longest logout token or LogoutRequest lifetime to between 1 and 600 seconds.",
            ));
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(SsoErrors::new(problems))
        }
    }
}

/// Which assertion attribute supplies each field of an [`crate::SsoIdentity`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SamlClaimsMap {
    /// The attribute whose value is the subject, or `None` to take the `NameID`.
    #[serde(default)]
    pub subject_attribute: Option<String>,
    /// The attribute whose value is the e-mail.
    pub email: String,
    /// The attribute whose value is the display name.
    pub name: String,
    /// The attribute whose values are the group memberships.
    pub groups: String,
}

impl Default for SamlClaimsMap {
    fn default() -> Self {
        Self {
            subject_attribute: None,
            email: "email".to_owned(),
            name: "displayName".to_owned(),
            groups: "groups".to_owned(),
        }
    }
}

/// An identity provider's configuration, as the tenant administrator pins it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdpConfig {
    /// The tenant's name for this provider.
    pub provider: ProviderId,
    /// The IdP entity id; every `Issuer` must equal it.
    pub entity_id: String,
    /// The single-sign-on endpoint the SP sends the browser to.
    pub sso_url: String,
    /// The binding of [`IdpConfig::sso_url`].
    pub sso_binding: SamlBinding,
    /// The single-logout endpoint, when the IdP offers one.
    pub slo_url: Option<String>,
    /// The pinned signing certificates (DER), the only keys a signature is verified with.
    pub signing_certificates: Vec<Vec<u8>>,
    /// The claim mapping.
    pub claims_map: SamlClaimsMap,
    /// The `AuthnContextClassRef`s the tenant requires, if any.
    pub acr_values: Vec<String>,
}

impl IdpConfig {
    /// A configuration with the standard claim mapping and no required authentication context.
    #[must_use]
    pub fn new(
        provider: ProviderId,
        entity_id: impl Into<String>,
        sso_url: impl Into<String>,
        signing_certificates: Vec<Vec<u8>>,
    ) -> Self {
        Self {
            provider,
            entity_id: entity_id.into(),
            sso_url: sso_url.into(),
            sso_binding: SamlBinding::Redirect,
            slo_url: None,
            signing_certificates,
            claims_map: SamlClaimsMap::default(),
            acr_values: Vec::new(),
        }
    }
}
