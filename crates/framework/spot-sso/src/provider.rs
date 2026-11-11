//! An OIDC provider's configuration, and the loader that refuses dangerous shapes at load.
//!
//! Everything a provider needs is data supplied by the tenant administrator: the issuer, the
//! client id, the **name** of the client secret in `spot-secrets` (never the secret itself),
//! the exact redirect URI, the discovery URL, the token algorithms and the claim mapping.
//!
//! The loader reports **every** problem at once, so an administrator fixes one screen rather
//! than one error at a time. `none` and the HMAC algorithms (`HS256`, ...) are refused at load:
//! this crate verifies signatures with a public key, and no configuration may turn that off.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{self as e, ResultList, SsoError, SsoErrors};

/// Identifies one configured identity provider.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProviderId(pub String);

impl ProviderId {
    /// The provider id from text.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The JOSE signature algorithms this crate accepts: RS256 and ES256 only (RFC 7518).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Jwa {
    /// RSASSA-PKCS1-v1_5 with SHA-256.
    #[serde(rename = "RS256")]
    Rs256,
    /// ECDSA over P-256 with SHA-256.
    #[serde(rename = "ES256")]
    Es256,
}

impl Jwa {
    /// Parses a JWA name, refusing `none` and every HMAC algorithm.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "RS256" => Some(Self::Rs256),
            "ES256" => Some(Self::Es256),
            _ => None,
        }
    }

    /// The JWA name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rs256 => "RS256",
            Self::Es256 => "ES256",
        }
    }

    /// Whether this algorithm needs an RSA key.
    #[must_use]
    pub fn needs_rsa(self) -> bool {
        matches!(self, Self::Rs256)
    }
}

/// How the client authenticates to the token endpoint.
///
/// The secret is never carried here: only the **name** under which `spot-secrets` holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ClientAuth {
    /// A confidential client with a shared secret held by `spot-secrets`.
    ClientSecretRef {
        /// The secret's name in `spot-secrets`.
        secret_ref: String,
    },
    /// `private_key_jwt` (RFC 9700); the key material is resolved later.
    PrivateKeyJwt {
        /// The signing key's name in `spot-secrets`.
        key_ref: String,
    },
}

impl ClientAuth {
    /// The reference name, whichever scheme is used.
    #[must_use]
    pub fn reference(&self) -> &str {
        match self {
            Self::ClientSecretRef { secret_ref } => secret_ref,
            Self::PrivateKeyJwt { key_ref } => key_ref,
        }
    }
}

/// Which ID-token claim supplies each field of an [`crate::SsoIdentity`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaimsMap {
    /// The stable subject claim (OIDC `sub`).
    pub subject: String,
    /// The e-mail claim.
    pub email: String,
    /// The display-name claim.
    pub name: String,
    /// The group-membership claim.
    pub groups: String,
}

impl Default for ClaimsMap {
    fn default() -> Self {
        Self {
            subject: "sub".to_owned(),
            email: "email".to_owned(),
            name: "name".to_owned(),
            groups: "groups".to_owned(),
        }
    }
}

/// A provider's full configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderConfig {
    /// The tenant's name for this provider.
    pub id: ProviderId,
    /// The expected `iss`; must equal the discovery document's issuer.
    pub issuer: String,
    /// The OAuth client id.
    pub client_id: String,
    /// How the client authenticates to the token endpoint.
    pub auth: ClientAuth,
    /// The requested scopes; must include `openid`.
    pub scopes: Vec<String>,
    /// The exact redirect URI registered with the provider.
    pub redirect_uri: String,
    /// The `/.well-known/openid-configuration` URL.
    pub discovery_url: String,
    /// The accepted ID-token algorithms; RS256/ES256 only.
    pub allowed_algs: Vec<Jwa>,
    /// The tolerated clock skew in seconds, 0 to [`ProviderConfig::MAX_CLOCK_SKEW_SECONDS`].
    pub clock_skew_seconds: i64,
    /// The longest a back-channel logout token may live (`exp` minus `iat`), in seconds,
    /// 1 to [`ProviderConfig::MAX_LOGOUT_TOKEN_LIFETIME_SECONDS`]; defaults to 120.
    #[serde(default = "default_logout_token_lifetime")]
    pub max_logout_token_lifetime_seconds: i64,
    /// The claim mapping.
    pub claims_map: ClaimsMap,
    /// Whether a verified e-mail is required for e-mail-based linking.
    pub require_email_verified: bool,
    /// The `acr` values the tenant requires, if any.
    pub acr_values: Vec<String>,
}

/// A provider configuration as an administrator writes it: algorithms are plain strings, so
/// the loader can refuse `none` and `HS*` and report them with the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawProviderConfig {
    /// The tenant's name for this provider.
    pub id: ProviderId,
    /// The expected issuer.
    pub issuer: String,
    /// The client id.
    pub client_id: String,
    /// The secret or key reference.
    pub auth: ClientAuth,
    /// The requested scopes.
    pub scopes: Vec<String>,
    /// The exact redirect URI.
    pub redirect_uri: String,
    /// The discovery URL.
    pub discovery_url: String,
    /// The accepted algorithms, as written.
    pub allowed_algs: Vec<String>,
    /// The tolerated clock skew in seconds; defaults to zero.
    #[serde(default)]
    pub clock_skew_seconds: i64,
    /// The longest a back-channel logout token may live, in seconds; defaults to 120.
    #[serde(default = "default_logout_token_lifetime")]
    pub max_logout_token_lifetime_seconds: i64,
    /// The claim map; defaults to the standard names.
    #[serde(default)]
    pub claims_map: ClaimsMap,
    /// Whether a verified e-mail is required.
    #[serde(default)]
    pub require_email_verified: bool,
    /// Required `acr` values.
    #[serde(default)]
    pub acr_values: Vec<String>,
}

/// The default longest lifetime of a back-channel logout token, in seconds.
fn default_logout_token_lifetime() -> i64 {
    ProviderConfig::DEFAULT_LOGOUT_TOKEN_LIFETIME_SECONDS
}

impl ProviderConfig {
    /// The largest clock skew a tenant may allow, in seconds (RFC 9700's guidance).
    pub const MAX_CLOCK_SKEW_SECONDS: i64 = 120;
    /// The default longest lifetime of a back-channel logout token, in seconds.
    pub const DEFAULT_LOGOUT_TOKEN_LIFETIME_SECONDS: i64 = 120;
    /// The longest lifetime a tenant may allow a back-channel logout token, in seconds.
    pub const MAX_LOGOUT_TOKEN_LIFETIME_SECONDS: i64 = 600;

    /// Checks every field, reporting all problems at once.
    ///
    /// # Errors
    ///
    /// [`SsoErrors`] with one [`SsoError`] per problem.
    pub fn validate(&self) -> ResultList<()> {
        let mut problems = Vec::new();
        check_issuer(&self.issuer, &mut problems);
        if self.client_id.trim().is_empty() {
            problems.push(SsoError::new(
                e::codes::CLIENT_ID_MISSING,
                "The identity provider is not configured completely.",
                "Set the client id issued by the identity provider.",
            ));
        }
        check_reference(self.auth.reference(), self.auth.code(), &mut problems);
        if !self.scopes.iter().any(|s| s == "openid") {
            problems.push(SsoError::new(
                e::codes::SCOPES_MISSING_OPENID,
                "The identity provider is not configured completely.",
                "Add the `openid` scope to the provider's scopes.",
            ));
        }
        check_redirect_uri(&self.redirect_uri, &mut problems);
        check_https_url(
            &self.discovery_url,
            e::codes::DISCOVERY_URL_INVALID,
            "The identity provider's discovery URL is not a secure URL.",
            "Set the provider's discovery URL to an `https://` address.",
            &mut problems,
        );
        if self.allowed_algs.is_empty() {
            problems.push(SsoError::new(
                e::codes::ALG_UNSUPPORTED,
                "The identity provider is not configured completely.",
                "Keep at least one of the RS256 and ES256 signing algorithms.",
            ));
        }
        if !(0..=Self::MAX_CLOCK_SKEW_SECONDS).contains(&self.clock_skew_seconds) {
            problems.push(SsoError::new(
                e::codes::CLOCK_SKEW_EXCESSIVE,
                "The identity provider is not configured completely.",
                "Set the tolerated clock skew to between 0 and 120 seconds.",
            ));
        }
        if !(1..=Self::MAX_LOGOUT_TOKEN_LIFETIME_SECONDS)
            .contains(&self.max_logout_token_lifetime_seconds)
        {
            problems.push(SsoError::new(
                e::codes::LOGOUT_LIFETIME_INVALID,
                "The identity provider is not configured completely.",
                "Set the longest logout token lifetime to between 1 and 600 seconds.",
            ));
        }
        for (field, value) in [
            ("subject", &self.claims_map.subject),
            ("email", &self.claims_map.email),
            ("name", &self.claims_map.name),
            ("groups", &self.claims_map.groups),
        ] {
            if value.trim().is_empty() {
                problems.push(
                    SsoError::new(
                        e::codes::CLAIM_MAP_INVALID,
                        "The identity provider is not configured completely.",
                        "Name a claim for every mapped field.",
                    )
                    .with_detail(field),
                );
            }
        }
        finish(problems)
    }

    /// Loads a raw configuration, refusing dangerous shapes and reporting every problem.
    ///
    /// # Errors
    ///
    /// [`SsoErrors`] with one [`SsoError`] per problem, including [`e::codes::ALG_UNSUPPORTED`]
    /// for `none` and every `HS*` algorithm.
    pub fn load(raw: RawProviderConfig) -> ResultList<Self> {
        let mut problems = Vec::new();
        let mut allowed_algs = Vec::new();
        for name in &raw.allowed_algs {
            match Jwa::parse(name) {
                Some(alg) => {
                    if !allowed_algs.contains(&alg) {
                        allowed_algs.push(alg);
                    }
                }
                None => problems.push(
                    SsoError::new(
                        e::codes::ALG_UNSUPPORTED,
                        "The identity provider is not configured completely.",
                        "Use only the RS256 and ES256 signing algorithms.",
                    )
                    .with_detail(name.clone()),
                ),
            }
        }
        let config = Self {
            id: raw.id,
            issuer: raw.issuer,
            client_id: raw.client_id,
            auth: raw.auth,
            scopes: raw.scopes,
            redirect_uri: raw.redirect_uri,
            discovery_url: raw.discovery_url,
            allowed_algs,
            clock_skew_seconds: raw.clock_skew_seconds,
            max_logout_token_lifetime_seconds: raw.max_logout_token_lifetime_seconds,
            claims_map: raw.claims_map,
            require_email_verified: raw.require_email_verified,
            acr_values: raw.acr_values,
        };
        match config.validate() {
            Ok(()) => finish(problems).map(|()| config),
            Err(errors) => {
                problems.extend(errors.problems().iter().cloned());
                Err(SsoErrors::new(problems))
            }
        }
    }
}

impl ClientAuth {
    /// The code to report when the reference is malformed.
    fn code(&self) -> &'static str {
        e::codes::SECRET_REF_INVALID
    }
}

/// Whether `scheme://host` is https, or http on a loopback host (development only).
fn is_secure_url(url: &Url) -> bool {
    match url.scheme() {
        "https" => true,
        "http" => match url.host() {
            Some(url::Host::Domain("localhost")) => true,
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            _ => false,
        },
        _ => false,
    }
}

fn check_issuer(issuer: &str, problems: &mut Vec<SsoError>) {
    let ok = match Url::parse(issuer) {
        Ok(url) => {
            is_secure_url(&url)
                && url.query().is_none()
                && url.fragment().is_none()
                && !url.cannot_be_a_base()
        }
        Err(_) => false,
    };
    if !ok {
        problems.push(SsoError::new(
            e::codes::ISSUER_INVALID,
            "The identity provider's issuer is not a secure URL.",
            "Set the issuer to the exact `https://` value the provider reports.",
        ));
    }
}

fn check_https_url(
    url: &str,
    code: &'static str,
    public: &'static str,
    admin: &'static str,
    problems: &mut Vec<SsoError>,
) {
    match Url::parse(url) {
        Ok(parsed) if is_secure_url(&parsed) => {}
        _ => problems.push(SsoError::new(code, public, admin)),
    }
}

fn check_redirect_uri(uri: &str, problems: &mut Vec<SsoError>) {
    let bad = |problems: &mut Vec<SsoError>| {
        problems.push(SsoError::new(
            e::codes::REDIRECT_URI_INVALID,
            "The identity provider's redirect URI is not acceptable.",
            "Use one exact `https://` URI (http on loopback for development), with no wildcard and no fragment.",
        ));
    };
    if uri.contains('*') {
        bad(problems);
        return;
    }
    match Url::parse(uri) {
        Ok(url) if is_secure_url(&url) && url.fragment().is_none() && !url.cannot_be_a_base() => {}
        _ => bad(problems),
    }
}

/// A secret reference is a **name**, not the secret: a short, opaque token with no obvious
/// secret encoding. This refuses base64 padding, `+`/`/`, whitespace and over-long values,
/// which is what a pasted secret looks like.
fn check_reference(reference: &str, code: &'static str, problems: &mut Vec<SsoError>) {
    let ok = !reference.is_empty()
        && reference.len() <= 128
        && !reference
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        && reference
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':'))
        && !reference.contains("==")
        && !reference.contains('+');
    if !ok {
        problems.push(SsoError::new(
            code,
            "The identity provider's client credential is not configured as a reference.",
            "Store the secret in `spot-secrets` and name it here; never paste the secret itself.",
        ));
    }
}

fn finish(problems: Vec<SsoError>) -> ResultList<()> {
    if problems.is_empty() {
        Ok(())
    } else {
        Err(SsoErrors::new(problems))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::codes;

    /// A configuration that passes.
    pub(crate) fn good() -> ProviderConfig {
        ProviderConfig {
            id: ProviderId::new("acme"),
            issuer: "https://idp.example.com".to_owned(),
            client_id: "spotledger".to_owned(),
            auth: ClientAuth::ClientSecretRef {
                secret_ref: "providers/acme/client-secret".to_owned(),
            },
            scopes: vec![
                "openid".to_owned(),
                "email".to_owned(),
                "profile".to_owned(),
            ],
            redirect_uri: "https://ledger.example.com/sso/callback".to_owned(),
            discovery_url: "https://idp.example.com/.well-known/openid-configuration".to_owned(),
            allowed_algs: vec![Jwa::Rs256],
            clock_skew_seconds: 60,
            max_logout_token_lifetime_seconds: 120,
            claims_map: ClaimsMap::default(),
            require_email_verified: true,
            acr_values: Vec::new(),
        }
    }

    #[test]
    fn a_good_configuration_passes() {
        assert_eq!(good().validate(), Ok(()));
    }

    #[test]
    fn every_problem_is_reported_at_once() {
        let mut config = good();
        config.issuer = "http://idp.example.com".to_owned();
        config.scopes = vec!["email".to_owned()];
        config.redirect_uri = "https://ledger.example.com/*".to_owned();
        config.clock_skew_seconds = 3600;
        let errors = match config.validate() {
            Err(errors) => errors,
            Ok(()) => panic!("expected problems"),
        };
        for code in [
            codes::ISSUER_INVALID,
            codes::SCOPES_MISSING_OPENID,
            codes::REDIRECT_URI_INVALID,
            codes::CLOCK_SKEW_EXCESSIVE,
        ] {
            assert!(errors.has_code(code), "expected {code}: {errors}");
        }
    }

    #[test]
    fn loopback_http_is_allowed_for_development() {
        let mut config = good();
        config.issuer = "http://localhost:8080".to_owned();
        config.redirect_uri = "http://127.0.0.1:9000/cb".to_owned();
        config.discovery_url = "http://localhost:8080/.well-known/openid-configuration".to_owned();
        assert_eq!(config.validate(), Ok(()));
    }

    #[test]
    fn a_pasted_secret_is_refused() {
        let mut config = good();
        config.auth = ClientAuth::ClientSecretRef {
            secret_ref: "c2VjcmV0+value==".to_owned(),
        };
        let errors = match config.validate() {
            Err(errors) => errors,
            Ok(()) => panic!("expected a problem"),
        };
        assert!(errors.has_code(codes::SECRET_REF_INVALID));
    }

    #[test]
    fn load_refuses_none_and_hmac() {
        let raw = RawProviderConfig {
            id: ProviderId::new("acme"),
            issuer: "https://idp.example.com".to_owned(),
            client_id: "spotledger".to_owned(),
            auth: ClientAuth::ClientSecretRef {
                secret_ref: "providers/acme/client-secret".to_owned(),
            },
            scopes: vec!["openid".to_owned()],
            redirect_uri: "https://ledger.example.com/sso/callback".to_owned(),
            discovery_url: "https://idp.example.com/.well-known/openid-configuration".to_owned(),
            allowed_algs: vec!["none".to_owned(), "HS256".to_owned(), "RS256".to_owned()],
            clock_skew_seconds: 0,
            max_logout_token_lifetime_seconds: 120,
            claims_map: ClaimsMap::default(),
            require_email_verified: false,
            acr_values: Vec::new(),
        };
        let errors = match ProviderConfig::load(raw) {
            Err(errors) => errors,
            Ok(_) => panic!("none and HS256 must be refused at load"),
        };
        assert_eq!(
            errors
                .codes()
                .filter(|c| *c == codes::ALG_UNSUPPORTED)
                .count(),
            2
        );
    }
}
