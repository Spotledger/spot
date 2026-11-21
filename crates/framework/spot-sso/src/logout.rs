//! Single logout: the target a provider names, the identities it maps to, and the
//! RP-initiated logout URL.
//!
//! Three ways a sign-out reaches us, and the one vocabulary they share:
//!
//! - **RP-initiated logout** ([`end_session_url`]): the browser is sent to the provider with an
//!   `id_token_hint`, and a `post_logout_redirect_uri` must exactly match a registered URI — a
//!   wildcard or a look-alike is refused, so a logout cannot become an open redirect.
//! - **OIDC back-channel logout** ([`crate::oidc::backchannel::validate_logout_token`]): the
//!   provider POSTs a signed logout token to us.
//! - **SAML IdP-initiated single logout** ([`crate::saml::slo::validate_logout_request`]): the
//!   provider sends a signed `LogoutRequest`.
//!
//! The two unsolicited paths decide nothing about sessions. They verify the message and return
//! a [`LogoutTarget`]: *which provider* said so, *whom* it names (`sub`/`NameID`) and *which
//! sessions* it names (`sid`, or SAML `SessionIndex` values). [`resolve_target`] maps the subject
//! to the linked identities it belongs to; the caller (spot-login and spot-sessions) ends the
//! sessions those users hold. A logout for a person we have never seen is acknowledged and
//! changes nothing — it is not an error, and it must not create anything.

use serde::{Deserialize, Serialize};
use spot_iam_directory::LinkedIdentity;
use spot_storage_contract::RecordId;
use url::Url;

use crate::error::{self as e, Result, SsoError};
use crate::oidc::discovery::Discovery;
use crate::provider::{ProviderConfig, ProviderId};

/// What a validated back-channel logout token or SAML `LogoutRequest` names.
///
/// The target is data, not an action: this crate never touches a session. `subject` and the
/// session identifiers are all optional because the two profiles differ — an OIDC token carries
/// `sub`, `sid` or both, and a SAML request always carries a `NameID` but may name any number of
/// `SessionIndex` values (including none).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogoutTarget {
    /// The tenant provider that authenticated the message.
    pub provider: ProviderId,
    /// The provider's issuer: the OIDC `iss`, or the SAML IdP entity id. Linked identities are
    /// keyed by this value, so it is what [`resolve_target`] matches against.
    pub issuer: String,
    /// The provider's stable subject (`sub` or `NameID`), when the message names one.
    pub subject: Option<String>,
    /// The OIDC session id (`sid`), when the token names one session.
    pub session_id: Option<String>,
    /// The SAML `SessionIndex` values, when the request names particular sessions. Empty for
    /// OIDC and for a request that ends every session of the subject.
    pub session_indexes: Vec<String>,
}

impl LogoutTarget {
    /// A target for an OIDC back-channel logout token.
    #[must_use]
    pub fn for_oidc(
        provider: ProviderId,
        issuer: impl Into<String>,
        subject: Option<String>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            provider,
            issuer: issuer.into(),
            subject,
            session_id,
            session_indexes: Vec::new(),
        }
    }
}

/// One linked identity a logout target resolves to: the user, and the identity that matched.
///
/// The caller ends this user's sessions (all of them, or only those named by
/// [`LogoutTarget::session_id`] / [`LogoutTarget::session_indexes`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdentityRef {
    /// The user the identity signs in as.
    pub user: RecordId,
    /// The provider issuer the identity belongs to.
    pub issuer: String,
    /// The provider's stable subject.
    pub subject: String,
}

impl From<&LinkedIdentity> for IdentityRef {
    fn from(identity: &LinkedIdentity) -> Self {
        Self {
            user: identity.user,
            issuer: identity.issuer.clone(),
            subject: identity.subject.clone(),
        }
    }
}

/// Maps a validated logout target to the linked identities it names.
///
/// Only the identity of **exactly that provider** matches: the same subject string at another
/// provider is a different person, and is never returned. A target without a subject (a bare
/// `sid`, which the caller resolves through its session table) and an unknown subject both
/// return an empty list — a logout for someone we do not know is acknowledged, never an error.
#[must_use]
pub fn resolve_target(target: &LogoutTarget, linked: &[LinkedIdentity]) -> Vec<IdentityRef> {
    let Some(subject) = target.subject.as_deref().filter(|value| !value.is_empty()) else {
        return Vec::new();
    };
    let mut resolved: Vec<IdentityRef> = Vec::new();
    for identity in linked {
        if identity.issuer == target.issuer
            && identity.subject == subject
            && !resolved.iter().any(|seen| seen.user == identity.user)
        {
            resolved.push(IdentityRef::from(identity));
        }
    }
    resolved
}

/// Builds the RP-initiated logout URL.
///
/// # Errors
///
/// [`e::codes::NO_END_SESSION`] when the provider has no `end_session_endpoint`;
/// [`e::codes::LOGOUT_REDIRECT`] when the redirect does not exactly match the registered one.
pub fn end_session_url(
    config: &ProviderConfig,
    discovery: &Discovery,
    id_token_hint: Option<&str>,
    post_logout_redirect_uri: Option<&str>,
) -> Result<String> {
    let endpoint = discovery.end_session_endpoint.as_deref().ok_or_else(|| {
        SsoError::new(
            e::codes::NO_END_SESSION,
            "This sign-in method cannot sign you out at the provider.",
            "The provider does not publish an end-session endpoint.",
        )
    })?;
    if let Some(uri) = post_logout_redirect_uri
        && uri != config.redirect_uri
    {
        return Err(SsoError::new(
            e::codes::LOGOUT_REDIRECT,
            "The page to return to after signing out is not allowed.",
            "Use the exact redirect URI registered with the provider.",
        ));
    }
    let mut url = Url::parse(endpoint).map_err(|_| {
        SsoError::new(
            e::codes::NO_END_SESSION,
            "This sign-in method cannot sign you out at the provider.",
            "Check the provider's end-session endpoint.",
        )
    })?;
    {
        let mut query = url.query_pairs_mut();
        if let Some(hint) = id_token_hint {
            query.append_pair("id_token_hint", hint);
        }
        if let Some(redirect) = post_logout_redirect_uri {
            query.append_pair("post_logout_redirect_uri", redirect);
        }
    }
    Ok(url.to_string())
}
