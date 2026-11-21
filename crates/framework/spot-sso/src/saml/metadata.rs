//! SAML 2.0 metadata: generating the SP's `EntityDescriptor` and parsing the IdP's.
//!
//! [`sp_metadata`] is deterministic: fixed element and attribute order, no timestamps. The
//! caller serves it or hands it to the IdP administrator.
//!
//! [`parse_idp_metadata`] reads the IdP's `EntityDescriptor`: its entity id, its single-sign-on
//! endpoints by binding, its signing certificates (several, each with a `use`), and its
//! `validUntil`/`cacheDuration`. A signature on the metadata is verified when the caller pins a
//! certificate; unsigned metadata is accepted only through the explicit
//! [`MetadataTrust::trust_unsigned`], which an administrator sets after receiving the document
//! out of band, never from an untrusted fetch.

use roxmltree::Node;

use crate::error::{self as e, Result, SsoError};
use crate::provider::ProviderId;
use crate::saml::signature;
use crate::saml::xml::{self, NS_DS, NS_METADATA};
use crate::saml::{IdpConfig, SamlBinding, SamlClaimsMap, SpConfig};

/// How much a metadata document is trusted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MetadataTrust {
    /// The certificate (DER) the metadata signature must verify with, when one is pinned.
    pub pinned_certificate: Option<Vec<u8>>,
    /// Whether unsigned (or unverifiable) metadata is accepted, having arrived out of band.
    pub trust_unsigned: bool,
}

/// One IdP single-sign-on endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SamlEndpoint {
    /// The binding the endpoint speaks.
    pub binding: SamlBinding,
    /// The endpoint URL.
    pub location: String,
}

/// The parts of an IdP metadata document this crate relies on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdpMetadata {
    /// The IdP entity id.
    pub entity_id: String,
    /// The single-sign-on endpoints, by binding.
    pub sso_endpoints: Vec<SamlEndpoint>,
    /// The single-logout endpoint, when published.
    pub slo_url: Option<String>,
    /// The signing certificates, DER-encoded.
    pub signing_certificates: Vec<Vec<u8>>,
    /// When the document stops being valid, in unix seconds.
    pub valid_until: Option<i64>,
    /// The `cacheDuration`, in seconds, when present.
    pub cache_duration: Option<i64>,
}

impl IdpMetadata {
    /// Builds an [`IdpConfig`] from the metadata, preferring the HTTP-Redirect endpoint.
    ///
    /// # Errors
    ///
    /// [`e::codes::SAML_NO_SSO_ENDPOINT`] when the metadata publishes no usable endpoint.
    pub fn into_config(
        &self,
        provider: ProviderId,
        claims_map: SamlClaimsMap,
        acr_values: Vec<String>,
    ) -> Result<IdpConfig> {
        let redirect = self
            .sso_endpoints
            .iter()
            .find(|endpoint| endpoint.binding == SamlBinding::Redirect)
            .or_else(|| self.sso_endpoints.first())
            .ok_or_else(|| {
                SsoError::new(
                    e::codes::SAML_NO_SSO_ENDPOINT,
                    "The identity provider could not be used.",
                    "The metadata publishes no single-sign-on endpoint.",
                )
            })?;
        Ok(IdpConfig {
            provider,
            entity_id: self.entity_id.clone(),
            sso_url: redirect.location.clone(),
            sso_binding: redirect.binding,
            slo_url: self.slo_url.clone(),
            signing_certificates: self.signing_certificates.clone(),
            claims_map,
            acr_values,
        })
    }
}

/// Builds the SP's metadata document.
///
/// The output is deterministic: the same [`SpConfig`] always produces the same bytes.
#[must_use]
pub fn sp_metadata(sp: &SpConfig) -> String {
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
         <md:EntityDescriptor xmlns:md=\"",
    );
    out.push_str(NS_METADATA);
    out.push_str("\" xmlns:ds=\"");
    out.push_str(NS_DS);
    out.push_str("\" entityID=\"");
    out.push_str(&xml::escape_attribute(&sp.entity_id));
    out.push_str("\"><md:SPSSODescriptor protocolSupportEnumeration=\"");
    out.push_str(crate::saml::NS_PROTOCOL);
    out.push_str("\" AuthnRequestsSigned=\"");
    out.push_str(if sp.sign_authn_requests {
        "true"
    } else {
        "false"
    });
    out.push_str("\" WantAssertionsSigned=\"true\">");
    if let Some(certificate) = &sp.signing_certificate {
        push_key_descriptor(&mut out, "signing", certificate);
    }
    if let Some(certificate) = &sp.encryption_certificate {
        push_key_descriptor(&mut out, "encryption", certificate);
    }
    out.push_str("<md:AssertionConsumerService Binding=\"");
    out.push_str(crate::saml::BINDING_POST);
    out.push_str("\" Location=\"");
    out.push_str(&xml::escape_attribute(&sp.acs_url));
    out.push_str("\" index=\"0\" isDefault=\"true\"/>");
    if let Some(slo) = &sp.slo_url {
        out.push_str("<md:SingleLogoutService Binding=\"");
        out.push_str(crate::saml::BINDING_POST);
        out.push_str("\" Location=\"");
        out.push_str(&xml::escape_attribute(slo));
        out.push_str("\"/>");
    }
    out.push_str("</md:SPSSODescriptor></md:EntityDescriptor>");
    out
}

/// Writes a `KeyDescriptor` carrying a Base64 X.509 certificate.
fn push_key_descriptor(out: &mut String, use_: &str, certificate_der: &[u8]) {
    out.push_str("<md:KeyDescriptor use=\"");
    out.push_str(use_);
    out.push_str("\"><ds:KeyInfo><ds:X509Data><ds:X509Certificate>");
    out.push_str(&xml::encode_base64(certificate_der));
    out.push_str("</ds:X509Certificate></ds:X509Data></ds:KeyInfo></md:KeyDescriptor>");
}

/// Parses an IdP metadata document.
///
/// # Errors
///
/// [`e::codes::SAML_METADATA_INVALID`] for malformed metadata missing its entity id;
/// [`e::codes::SAML_METADATA_UNSIGNED`] when the document is unsigned or unverifiable and
/// [`MetadataTrust::trust_unsigned`] is not set;
/// [`e::codes::SAML_METADATA_SIGNATURE_INVALID`] when a pinned certificate is supplied and the
/// signature does not verify; [`e::codes::SAML_METADATA_EXPIRED`] when `validUntil` has passed;
/// [`e::codes::SAML_NO_SIGNING_CERTIFICATE`] when no signing certificate is published.
pub fn parse_idp_metadata(bytes: &[u8], trust: &MetadataTrust, now: i64) -> Result<IdpMetadata> {
    let text = std::str::from_utf8(bytes).map_err(|_| metadata_invalid("not UTF-8"))?;
    let document = xml::parse_document(text)?;
    let root = document.root_element();
    if !xml::is_tag(root, NS_METADATA, "EntityDescriptor") {
        return Err(metadata_invalid("root is not an EntityDescriptor"));
    }
    let entity_id = root
        .attribute("entityID")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| metadata_invalid("no entityID"))?
        .to_owned();

    match &trust.pinned_certificate {
        Some(certificate) => {
            match signature::verify_metadata(&document, std::slice::from_ref(certificate)) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    if !trust.trust_unsigned {
                        return Err(metadata_unsigned());
                    }
                }
                Err(error) => {
                    return Err(metadata_signature_invalid(error.code()));
                }
            }
        }
        None => {
            // Without a pinned certificate a signature cannot be verified, so only an explicit
            // out-of-band trust decision accepts the document.
            if !trust.trust_unsigned {
                return Err(metadata_unsigned());
            }
        }
    }

    let valid_until = root.attribute("validUntil").and_then(xml::parse_instant);
    if let Some(valid_until) = valid_until
        && now >= valid_until
    {
        return Err(SsoError::new(
            e::codes::SAML_METADATA_EXPIRED,
            "The identity provider's metadata is out of date.",
            "Fetch the identity provider's metadata again.",
        ));
    }
    let cache_duration = root.attribute("cacheDuration").and_then(parse_duration);

    let mut sso_endpoints = Vec::new();
    let mut slo_url = None;
    for child in root.children().filter(Node::is_element) {
        if xml::is_tag(child, NS_METADATA, "IDPSSODescriptor") {
            for service in child.children().filter(Node::is_element) {
                if xml::is_tag(service, NS_METADATA, "SingleSignOnService")
                    && let (Some(binding), Some(location)) =
                        (service.attribute("Binding"), service.attribute("Location"))
                    && let Some(binding) = SamlBinding::parse(binding)
                {
                    sso_endpoints.push(SamlEndpoint {
                        binding,
                        location: location.to_owned(),
                    });
                }
                if slo_url.is_none()
                    && xml::is_tag(service, NS_METADATA, "SingleLogoutService")
                    && let Some(location) = service.attribute("Location")
                {
                    slo_url = Some(location.to_owned());
                }
            }
        }
    }

    let mut signing_certificates = Vec::new();
    for descriptor in root
        .children()
        .filter(|child| xml::is_tag(*child, NS_METADATA, "IDPSSODescriptor"))
    {
        for key in descriptor
            .children()
            .filter(|child| xml::is_tag(*child, NS_METADATA, "KeyDescriptor"))
        {
            let use_ = key.attribute("use").unwrap_or("signing");
            if use_ != "signing" {
                continue;
            }
            for certificate in key
                .descendants()
                .filter(|node| xml::is_tag(*node, NS_DS, "X509Certificate"))
            {
                if let Ok(der) = xml::decode_base64(&xml::concatenated_text(certificate)) {
                    signing_certificates.push(der);
                }
            }
        }
    }
    if signing_certificates.is_empty() {
        return Err(SsoError::new(
            e::codes::SAML_NO_SIGNING_CERTIFICATE,
            "The identity provider could not be used.",
            "Publish a signing certificate in the IdP metadata.",
        ));
    }

    Ok(IdpMetadata {
        entity_id,
        sso_endpoints,
        slo_url,
        signing_certificates,
        valid_until,
        cache_duration,
    })
}

/// Parses a subset of ISO 8601 durations (`PnDTnHnMnS`) into seconds.
fn parse_duration(text: &str) -> Option<i64> {
    let rest = text.strip_prefix('P')?;
    let mut seconds = 0i64;
    let mut in_time = false;
    let mut number = String::new();
    for ch in rest.chars() {
        match ch {
            'T' => in_time = true,
            '0'..='9' => number.push(ch),
            'D' => {
                seconds += number.parse::<i64>().ok()? * 86_400;
                number.clear();
            }
            'H' if in_time => {
                seconds += number.parse::<i64>().ok()? * 3_600;
                number.clear();
            }
            'M' if in_time => {
                seconds += number.parse::<i64>().ok()? * 60;
                number.clear();
            }
            'S' if in_time => {
                seconds += number.parse::<i64>().ok()?;
                number.clear();
            }
            _ => return None,
        }
    }
    if number.is_empty() {
        Some(seconds)
    } else {
        None
    }
}

fn metadata_invalid(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_METADATA_INVALID,
        "The identity provider's metadata could not be read.",
        "Check the identity provider publishes SAML 2.0 metadata.",
    )
    .with_detail(detail)
}

fn metadata_unsigned() -> SsoError {
    SsoError::new(
        e::codes::SAML_METADATA_UNSIGNED,
        "The identity provider's metadata is not trusted.",
        "Pin the metadata signing certificate, or mark the document trusted out of band.",
    )
}

fn metadata_signature_invalid(detail: &str) -> SsoError {
    SsoError::new(
        e::codes::SAML_METADATA_SIGNATURE_INVALID,
        "The identity provider's metadata is not trusted.",
        "Check the pinned metadata certificate and the provider's signature.",
    )
    .with_detail(detail)
}
