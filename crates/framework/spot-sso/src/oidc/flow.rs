//! The OIDC authorization-code flow with PKCE, as a sans-I/O state machine.
//!
//! Four steps, each a pure function that returns what to do next as data:
//!
//! 1. [`begin`] builds the authorization URL and the [`PendingAuth`] to store.
//! 2. [`callback`] checks the redirect, then returns the [`TokenRequest`] to send.
//! 3. [`complete`] validates the ID token fully and returns an [`SsoIdentity`].
//! 4. [`crate::resolve`] turns that identity into a sign-in decision.
//!
//! Every function takes the data it needs — the discovery document, the key set, the token
//! response, the current time and random bytes — so a test needs no socket, no clock and no
//! entropy source, and the caller makes every HTTP call through the egress connector.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::error::{self as e, Result, SsoError};
use crate::identity::SsoIdentity;
use crate::jose;
use crate::oidc::discovery::Discovery;
use crate::oidc::jwks::KeyCache;
use crate::oidc::jwt::{self};
use crate::pending::{PENDING_TTL_SECONDS, PendingAuth};
use crate::provider::{Jwa, ProviderConfig, ProviderId};

/// A prepared authorization request: the URL to send the browser to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthRequest {
    /// The full authorization URL.
    pub url: String,
    /// The provider it is for.
    pub provider: ProviderId,
}

/// The parameters the provider put on the redirect back (query or form encoded).
///
/// Unknown parameters are ignored: providers add their own (`iss`, `session_state`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallbackParams {
    /// The `state` echoed back.
    pub state: Option<String>,
    /// The authorization code.
    pub code: Option<String>,
    /// An OAuth error code, when the provider refused.
    pub error: Option<String>,
    /// The provider's error description, when given.
    pub error_description: Option<String>,
}

/// What to do after a successful callback.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CallbackNext {
    /// Exchange the code at the token endpoint.
    ExchangeCode {
        /// The request to send.
        request: TokenRequest,
    },
}

/// The token request the caller sends to the provider.
///
/// The `code` and `code_verifier` fields are secrets: [`Debug`] redacts them.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenRequest {
    /// The token endpoint.
    pub endpoint: String,
    /// The form fields, in order.
    pub form: Vec<(String, String)>,
    /// How the client authenticates.
    pub client_auth: crate::provider::ClientAuth,
}

impl std::fmt::Debug for TokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenRequest")
            .field("endpoint", &self.endpoint)
            .field("form", &"<redacted>")
            .finish()
    }
}

/// The provider's token response, as the caller read it.
///
/// Unknown members (`expires_in`, `scope`, `refresh_token`, ...) are ignored.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenResponse {
    /// The ID token, when the provider returned one.
    pub id_token: Option<String>,
    /// The access token, when the provider returned one.
    pub access_token: Option<String>,
    /// The token type, when returned.
    pub token_type: Option<String>,
    /// An OAuth error code, when the provider refused.
    pub error: Option<String>,
    /// The provider's error description, when given.
    pub error_description: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    /// Redacts the ID and access tokens: they must never reach a log.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("id_token", &self.id_token.as_ref().map(|_| "<redacted>"))
            .field(
                "access_token",
                &self.access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("token_type", &self.token_type)
            .field("error", &self.error)
            .finish()
    }
}

impl TokenResponse {
    /// Parses a JSON token response.
    ///
    /// # Errors
    ///
    /// [`e::codes::TOKEN_MALFORMED`] when the bytes are not a JSON token response.
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes).map_err(|_| malformed())
    }
}

/// A validation failure that needs a fresh key set before it can be decided, or a refusal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompleteError {
    /// The token was refused.
    Refused(SsoError),
    /// The key id is not in the key set; the caller may refetch the JWKS and try once more.
    NeedFreshJwks {
        /// The key id the token named, when it named one.
        kid: Option<String>,
    },
}

impl CompleteError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Refused(error) => error.code(),
            // A refresh is the remedy; the code names the condition.
            Self::NeedFreshJwks { .. } => e::codes::JWKS_STALE,
        }
    }
}

impl From<SsoError> for CompleteError {
    fn from(error: SsoError) -> Self {
        Self::Refused(error)
    }
}

impl std::fmt::Display for CompleteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(error) => write!(f, "{error}"),
            Self::NeedFreshJwks { .. } => {
                f.write_str("sso.jwks_stale: the key id is not in the key set")
            }
        }
    }
}

impl std::error::Error for CompleteError {}

/// Begins a sign-in: builds the authorization URL and the pending state to store.
///
/// `rng` must be at least 32 bytes from the caller's CSPRNG; `state`, `nonce` and the PKCE
/// verifier are derived from it with domain separation, so one call needs one source.
///
/// # Errors
///
/// [`e::codes::BAD_RETURN_TO`] for a `return_to` that is not a same-site relative path;
/// [`e::codes::RNG_INSUFFICIENT`] when fewer than 32 random bytes are supplied.
pub fn begin(
    config: &ProviderConfig,
    discovery: &Discovery,
    rng: &[u8],
    now: i64,
    return_to: &str,
) -> Result<(AuthRequest, PendingAuth)> {
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
    let state = jose::encode(&jose::sha256(&[b"state:".as_slice(), rng].concat()));
    let nonce = jose::encode(&jose::sha256(&[b"nonce:".as_slice(), rng].concat()));
    let code_verifier = jose::encode(&jose::sha256(&[b"pkce:".as_slice(), rng].concat()));
    let code_challenge = jose::encode(&jose::sha256(code_verifier.as_bytes()));

    let mut url = Url::parse(&discovery.authorization_endpoint).map_err(|_| malformed())?;
    {
        let mut query = url.query_pairs_mut();
        query.append_pair("response_type", "code");
        query.append_pair("client_id", &config.client_id);
        query.append_pair("redirect_uri", &config.redirect_uri);
        query.append_pair("scope", &config.scopes.join(" "));
        query.append_pair("state", &state);
        query.append_pair("nonce", &nonce);
        query.append_pair("code_challenge", &code_challenge);
        query.append_pair("code_challenge_method", "S256");
        if !config.acr_values.is_empty() {
            query.append_pair("acr_values", &config.acr_values.join(" "));
        }
    }
    let pending = PendingAuth {
        provider: config.id.clone(),
        state,
        nonce,
        code_verifier,
        code_challenge,
        redirect_uri: config.redirect_uri.clone(),
        return_to: return_to.to_owned(),
        created_at: now,
        expires_at: now + PENDING_TTL_SECONDS,
        used: false,
    };
    Ok((
        AuthRequest {
            url: url.to_string(),
            provider: config.id.clone(),
        },
        pending,
    ))
}

/// Checks the redirect and returns the token request to send.
///
/// # Errors
///
/// [`e::codes::STATE_MISMATCH`] when `state` does not match (constant time);
/// [`e::codes::STATE_REPLAYED`] / [`e::codes::STATE_EXPIRED`] for a used or stale attempt;
/// [`e::codes::PKCE_MISMATCH`] when the verifier no longer matches its challenge;
/// [`e::codes::IDP_ERROR`] when the provider refused; [`e::codes::MISSING_PARAM`] when the
/// code is absent.
pub fn callback(
    config: &ProviderConfig,
    discovery: &Discovery,
    pending: &PendingAuth,
    params: &CallbackParams,
    now: i64,
) -> Result<CallbackNext> {
    let state_ok = params
        .state
        .as_deref()
        .is_some_and(|state| jose::ct_eq(state.as_bytes(), pending.state.as_bytes()));
    if !state_ok {
        return Err(SsoError::new(
            e::codes::STATE_MISMATCH,
            "The sign-in response did not match this attempt.",
            "Start the sign-in again from the beginning.",
        ));
    }
    pending.consume(now)?;
    if let Some(error) = &params.error {
        return Err(SsoError::new(
            e::codes::IDP_ERROR,
            "The identity provider refused the sign-in.",
            "Check the provider's configuration and the user's account there.",
        )
        .with_detail(error.clone()));
    }
    let expected_challenge = jose::encode(&jose::sha256(pending.code_verifier.as_bytes()));
    if !jose::ct_eq(
        expected_challenge.as_bytes(),
        pending.code_challenge.as_bytes(),
    ) {
        return Err(SsoError::new(
            e::codes::PKCE_MISMATCH,
            "The sign-in attempt was altered.",
            "Start the sign-in again from the beginning.",
        ));
    }
    let code = params.code.clone().ok_or_else(|| {
        SsoError::new(
            e::codes::MISSING_PARAM,
            "The sign-in response was incomplete.",
            "Check the provider returns an authorization code.",
        )
    })?;
    Ok(CallbackNext::ExchangeCode {
        request: TokenRequest {
            endpoint: discovery.token_endpoint.clone(),
            form: vec![
                ("grant_type".to_owned(), "authorization_code".to_owned()),
                ("code".to_owned(), code),
                ("redirect_uri".to_owned(), pending.redirect_uri.clone()),
                ("client_id".to_owned(), config.client_id.clone()),
                ("code_verifier".to_owned(), pending.code_verifier.clone()),
            ],
            client_auth: config.auth.clone(),
        },
    })
}

/// Validates the ID token fully, in order, and returns the identity it proves.
///
/// # Errors
///
/// [`CompleteError::NeedFreshJwks`] when the token names a key the cache does not hold;
/// [`CompleteError::Refused`] for every check that fails, in this order: structure, `alg`
/// allow-list and key agreement, key selection, signature, `iss`, `aud`/`azp`, `exp`, `nbf`,
/// `iat`, `nonce`, `at_hash`, `acr`, `email_verified`.
pub fn complete(
    config: &ProviderConfig,
    discovery: &Discovery,
    keys: &KeyCache,
    pending: &PendingAuth,
    token_response: &TokenResponse,
    now: i64,
) -> std::result::Result<SsoIdentity, CompleteError> {
    let skew = config.clock_skew_seconds;
    if let Some(error) = &token_response.error {
        return Err(SsoError::new(
            e::codes::IDP_ERROR,
            "The identity provider refused the sign-in.",
            "Check the provider's configuration and the user's account there.",
        )
        .with_detail(error.clone())
        .into());
    }
    let token_text = token_response.id_token.as_deref().ok_or_else(|| {
        SsoError::new(
            e::codes::MISSING_PARAM,
            "The sign-in response was incomplete.",
            "Check the provider returns an ID token.",
        )
    })?;
    let token = jwt::parse(token_text)?;

    // 1. `alg` is in the allow-list, and agrees with the key's own algorithm.
    let alg = Jwa::parse(&token.header.alg).ok_or_else(|| refused(e::codes::ALG_UNSUPPORTED))?;
    if !config.allowed_algs.contains(&alg) {
        return Err(refused(e::codes::ALG_UNSUPPORTED).into());
    }
    // 2. The key id selects a key; a miss asks the caller for a fresh key set.
    let key =
        keys.lookup(token.header.kid.as_deref(), now)
            .ok_or(CompleteError::NeedFreshJwks {
                kid: token.header.kid.clone(),
            })?;
    // 3. No algorithm confusion: the header's algorithm matches the key's.
    if key.alg.is_some_and(|key_alg| key_alg != alg) || key.key.is_rsa() != alg.needs_rsa() {
        return Err(refused(e::codes::ALG_MISMATCH).into());
    }
    // 4. The signature.
    if !key.key.verify(alg, &token.signing_input, &token.signature) {
        return Err(refused(e::codes::SIGNATURE_INVALID).into());
    }
    // 5. The issuer, exactly, against both the config and the discovery document.
    let iss = jwt::claim_str(&token.claims, "iss").unwrap_or_default();
    if iss != config.issuer || iss != discovery.issuer {
        return Err(refused(e::codes::ISSUER_MISMATCH).into());
    }
    // 6. The audience contains the client id; with several, `azp` names us.
    let auds = jwt::audiences(&token.claims);
    if !auds.iter().any(|aud| aud == &config.client_id) {
        return Err(refused(e::codes::AUDIENCE_MISMATCH).into());
    }
    if auds.len() > 1
        && jwt::claim_str(&token.claims, "azp").as_deref() != Some(config.client_id.as_str())
    {
        return Err(refused(e::codes::AZP_MISMATCH).into());
    }
    // 7. `exp`, `nbf` and `iat`, each with skew.
    let exp =
        jwt::claim_i64(&token.claims, "exp").ok_or_else(|| refused(e::codes::TOKEN_MALFORMED))?;
    if now >= exp + skew {
        return Err(refused(e::codes::TOKEN_EXPIRED).into());
    }
    if let Some(nbf) = jwt::claim_i64(&token.claims, "nbf")
        && now + skew < nbf
    {
        return Err(refused(e::codes::TOKEN_NOT_YET_VALID).into());
    }
    if let Some(iat) = jwt::claim_i64(&token.claims, "iat")
        && iat > now + skew
    {
        return Err(refused(e::codes::ISSUED_AT_INVALID).into());
    }
    // 8. The nonce, in constant time.
    let nonce = jwt::claim_str(&token.claims, "nonce").unwrap_or_default();
    if !jose::ct_eq(nonce.as_bytes(), pending.nonce.as_bytes()) {
        return Err(refused(e::codes::NONCE_MISMATCH).into());
    }
    // 9. `at_hash`, when an access token came with the response.
    if let Some(access_token) = &token_response.access_token
        && let Some(at_hash) = jwt::claim_str(&token.claims, "at_hash")
    {
        let expected = at_hash_of(access_token);
        if !jose::ct_eq(expected.as_bytes(), at_hash.as_bytes()) {
            return Err(refused(e::codes::AT_HASH_MISMATCH).into());
        }
    }
    // 10. `acr`, when the tenant requires particular values.
    if !config.acr_values.is_empty() {
        let acr = jwt::claim_str(&token.claims, "acr");
        if !acr
            .as_deref()
            .is_some_and(|acr| config.acr_values.iter().any(|wanted| wanted == acr))
        {
            return Err(refused(e::codes::ACR_INSUFFICIENT).into());
        }
    }
    // 11. A verified e-mail, when required.
    let email = jwt::claim_str(&token.claims, &config.claims_map.email);
    let email_verified = jwt::claim_bool(&token.claims, "email_verified").unwrap_or(false);
    if config.require_email_verified && !email_verified {
        return Err(refused(e::codes::EMAIL_NOT_VERIFIED).into());
    }
    // 12. The subject the link is keyed by.
    let subject = jwt::claim_str(&token.claims, &config.claims_map.subject)
        .ok_or_else(|| refused(e::codes::CLAIM_MISSING))?;

    Ok(SsoIdentity {
        provider: config.id.clone(),
        issuer: iss,
        subject,
        email,
        email_verified,
        display_name: jwt::claim_str(&token.claims, &config.claims_map.name),
        groups: jwt::claim_str_list(&token.claims, &config.claims_map.groups),
        acr: jwt::claim_str(&token.claims, "acr"),
        amr: jwt::claim_str_list(&token.claims, "amr"),
        auth_time: jwt::claim_i64(&token.claims, "auth_time"),
        raw_claims_hash: jose::encode(&jose::sha256(&token.payload)),
    })
}

/// The `at_hash` value for an access token: the left half of its SHA-256, Base64url.
fn at_hash_of(access_token: &str) -> String {
    let digest = jose::sha256(access_token.as_bytes());
    jose::encode(&digest[..16])
}

/// Whether `return_to` is a same-site relative path (blocks open redirects).
fn is_safe_return_to(return_to: &str) -> bool {
    return_to.starts_with('/')
        && !return_to.starts_with("//")
        && !return_to.starts_with("/\\")
        && !return_to.chars().any(char::is_control)
}

fn refused(code: &'static str) -> SsoError {
    let (public, admin) = match code {
        e::codes::TOKEN_MALFORMED => (
            "The sign-in response could not be read.",
            "Check the identity provider returns a standard ID token.",
        ),
        e::codes::ALG_UNSUPPORTED => (
            "The sign-in response used a method this tenant does not allow.",
            "Allow only RS256 and ES256 for this provider.",
        ),
        e::codes::ALG_MISMATCH => (
            "The sign-in response was not trusted.",
            "Check the provider signs with the algorithm its keys declare.",
        ),
        e::codes::SIGNATURE_INVALID => (
            "The sign-in response was not trusted.",
            "Check the provider's keys are current and the signing algorithm matches.",
        ),
        e::codes::ISSUER_MISMATCH => (
            "The sign-in response was not trusted.",
            "Check the configured issuer matches the provider's `iss` exactly.",
        ),
        e::codes::AUDIENCE_MISMATCH => (
            "The sign-in response was not for this application.",
            "Check the client id registered with the provider.",
        ),
        e::codes::AZP_MISMATCH => (
            "The sign-in response was not for this application.",
            "Check the `azp` claim names this client id.",
        ),
        e::codes::TOKEN_EXPIRED => (
            "The sign-in has taken too long.",
            "Check the server clock, and the provider's token lifetime.",
        ),
        e::codes::TOKEN_NOT_YET_VALID => (
            "The sign-in response is not valid yet.",
            "Check the server clock and the tolerated skew.",
        ),
        e::codes::ISSUED_AT_INVALID => (
            "The sign-in response is not valid yet.",
            "Check the server clock and the tolerated skew.",
        ),
        e::codes::NONCE_MISMATCH => (
            "The sign-in response did not match this attempt.",
            "Start the sign-in again from the beginning.",
        ),
        e::codes::AT_HASH_MISMATCH => (
            "The sign-in response was not trusted.",
            "Check the provider computes `at_hash` over the access token.",
        ),
        e::codes::ACR_INSUFFICIENT => (
            "The sign-in did not meet this tenant's requirements.",
            "Ask the provider to authenticate the user with the required context.",
        ),
        e::codes::EMAIL_NOT_VERIFIED => (
            "The account's e-mail has not been verified by the provider.",
            "Have the user verify their e-mail at the identity provider.",
        ),
        e::codes::CLAIM_MISSING => (
            "The sign-in response was incomplete.",
            "Check the provider includes the configured subject claim.",
        ),
        _ => (
            "The sign-in response could not be trusted.",
            "Check the identity provider's configuration.",
        ),
    };
    SsoError::new(code, public, admin)
}

fn malformed() -> SsoError {
    SsoError::new(
        e::codes::TOKEN_MALFORMED,
        "The sign-in response could not be read.",
        "Check the identity provider returns a standard ID token.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_return_to_rejects_other_sites() {
        assert!(is_safe_return_to("/app"));
        assert!(is_safe_return_to("/app?x=1"));
        for bad in [
            "https://evil.example",
            "//evil.example",
            "/\\evil",
            "",
            "app",
        ] {
            assert!(!is_safe_return_to(bad), "{bad:?} must be refused");
        }
    }

    #[test]
    fn at_hash_is_the_left_half_of_sha256() {
        // "abc" -> SHA-256 base64url, first 16 bytes.
        assert_eq!(at_hash_of("abc").len(), 22);
        assert_eq!(at_hash_of("abc"), "ungWv48Bz-pBQUDeXa4iIw");
    }

    #[test]
    fn token_response_debug_redacts_tokens() {
        let response = TokenResponse {
            id_token: Some("the-id-token".to_owned()),
            access_token: Some("the-access-token".to_owned()),
            ..TokenResponse::default()
        };
        let rendered = format!("{response:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("the-id-token"));
        assert!(!rendered.contains("the-access-token"));
    }
}
