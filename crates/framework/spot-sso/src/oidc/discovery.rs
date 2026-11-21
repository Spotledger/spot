//! The OpenID Connect discovery document (RFC 8414, OpenID Connect Discovery 1.0).
//!
//! The caller fetches the document from [`crate::ProviderConfig::discovery_url`] through the
//! egress connector and hands the bytes here; this crate parses and checks them against the
//! configuration.

use serde::Deserialize;

use crate::error::{self as e, Result, SsoError};
use crate::provider::ProviderConfig;

/// The subset of the discovery document this crate relies on.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Discovery {
    /// The issuer identifier; must equal the configured issuer.
    pub issuer: String,
    /// The authorization endpoint.
    pub authorization_endpoint: String,
    /// The token endpoint.
    pub token_endpoint: String,
    /// The JWKS URI.
    pub jwks_uri: String,
    /// The RP-initiated logout endpoint, when the provider offers one.
    #[serde(default)]
    pub end_session_endpoint: Option<String>,
    /// The ID-token signing algorithms the provider advertises.
    #[serde(default)]
    pub id_token_signing_alg_values_supported: Vec<String>,
    /// The PKCE challenge methods the provider supports.
    #[serde(default)]
    pub code_challenge_methods_supported: Vec<String>,
    /// The response types the provider supports.
    #[serde(default)]
    pub response_types_supported: Vec<String>,
}

impl Discovery {
    /// Parses and checks a discovery document against `config`.
    ///
    /// # Errors
    ///
    /// [`e::codes::DISCOVERY_MISMATCH`] when a required field is missing, the issuer does not
    /// match, the provider does not support `S256`, or it does not offer the code flow.
    pub fn parse(bytes: &[u8], config: &ProviderConfig) -> Result<Self> {
        let document: Self = serde_json::from_slice(bytes).map_err(|_| mismatch())?;
        document.check(config)?;
        Ok(document)
    }

    /// Checks the document against `config`.
    ///
    /// # Errors
    ///
    /// [`e::codes::DISCOVERY_MISMATCH`] on any disagreement.
    pub fn check(&self, config: &ProviderConfig) -> Result<()> {
        if self.issuer != config.issuer
            || self.authorization_endpoint.is_empty()
            || self.token_endpoint.is_empty()
            || self.jwks_uri.is_empty()
        {
            return Err(mismatch());
        }
        if !self
            .code_challenge_methods_supported
            .iter()
            .any(|m| m == "S256")
        {
            return Err(mismatch().with_detail("code_challenge_methods_supported"));
        }
        if !self.response_types_supported.iter().any(|t| t == "code") {
            return Err(mismatch().with_detail("response_types_supported"));
        }
        if !self.id_token_signing_alg_values_supported.is_empty()
            && !self
                .id_token_signing_alg_values_supported
                .iter()
                .any(|alg| config.allowed_algs.iter().any(|a| a.as_str() == alg))
        {
            return Err(mismatch().with_detail("id_token_signing_alg_values_supported"));
        }
        Ok(())
    }
}

fn mismatch() -> SsoError {
    SsoError::new(
        e::codes::DISCOVERY_MISMATCH,
        "The identity provider could not be used.",
        "Check the discovery URL, the issuer and that the provider supports the code flow with S256.",
    )
}
