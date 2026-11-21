//! Who is calling: the credential of a request, its authentication through the site's sign-in
//! service, and the [`McpPrincipal`] the call runs as.
//!
//! `spot-mcp`'s `Authenticator` is synchronous and the site's sign-in service is not, so the
//! host authenticates first, then runs the call inside a task-local scope holding the resolved
//! principal; [`ScopedAuthenticator`] hands exactly that principal back and nothing else. A call
//! made outside the scope is unauthorised, and the real credential never reaches `spot-mcp`.

use axum::http::{HeaderMap, header};
use spot_api_services::PrincipalSource;
use spot_app_api::context::Actor;
use spot_auth::ApiKey;
use spot_http::api::service::{Credential, RequestCtx};
use spot_http::site::SiteContext;
use spot_login::Via;
use spot_mcp::error::McpError;
use spot_mcp::{Authenticator, McpPrincipal};

use crate::audit::PathClass;
use crate::live::SiteLive;

use super::seams::McpHandle;

tokio::task_local! {
    /// The principal of the request being served; set by the handler around the call.
    pub static RESOLVED: McpPrincipal;
}

/// Hands `spot-mcp` the principal the host already authenticated for this request.
pub struct ScopedAuthenticator;

impl Authenticator for ScopedAuthenticator {
    fn authenticate(
        &self,
        _credential: &spot_mcp::Credential,
        _now_unix: i64,
    ) -> Result<McpPrincipal, McpError> {
        RESOLVED
            .try_with(Clone::clone)
            .map_err(|_| McpError::unauthorized("a credential is required"))
    }
}

/// What the request presented, as the site's sign-in service reads it.
pub struct Presented {
    /// The token in the form `RequestCtx` carries: a session token, or `spk_<id>_<secret>`.
    pub token: String,
    /// The public key id, when an API key was presented.
    pub key_id: Option<String>,
}

/// Why no usable credential was found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoCredential {
    /// Nothing was presented.
    Missing,
    /// Something was presented and it is not a credential this endpoint accepts.
    Malformed,
}

/// Reads the credential: `Authorization: Bearer <token>` or `X-Api-Key: <key id>:<secret>`.
/// Both at once, an empty value or a malformed key is refused; a token in a URL is never read.
pub fn presented(headers: &HeaderMap) -> Result<Presented, NoCredential> {
    let bearer = headers.get(header::AUTHORIZATION);
    let key = headers.get("x-api-key");
    match (bearer, key) {
        (None, None) => Err(NoCredential::Missing),
        (Some(_), Some(_)) => Err(NoCredential::Malformed),
        (Some(value), None) => {
            let raw = value.to_str().map_err(|_| NoCredential::Malformed)?;
            let token = raw
                .strip_prefix("Bearer ")
                .or_else(|| raw.strip_prefix("bearer "))
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .ok_or(NoCredential::Malformed)?;
            let key_id = ApiKey::parse(token).ok().map(|k| k.key_id().to_owned());
            Ok(Presented {
                token: token.to_owned(),
                key_id,
            })
        }
        (None, Some(value)) => {
            let raw = value.to_str().map_err(|_| NoCredential::Malformed)?;
            let (id, secret) = raw.split_once(':').ok_or(NoCredential::Malformed)?;
            let text = format!("spk_{id}_{secret}");
            let key = ApiKey::parse(&text).map_err(|_| NoCredential::Malformed)?;
            Ok(Presented {
                token: text,
                key_id: Some(key.key_id().to_owned()),
            })
        }
    }
}

/// Why a presented credential did not become a principal. The caller maps both `Rejected` and
/// `Restricted` to answers that do not say which credential property failed.
#[derive(Debug)]
pub enum Refused {
    /// Expired, revoked, locked out, unknown or from another site: one answer for all.
    Rejected,
    /// The caller may do nothing but choose a password yet.
    Restricted,
    /// The sign-in service itself could not answer.
    Unavailable,
}

/// Authenticates `presented` for the site of `site_ctx` and builds the principal of the call.
pub async fn authenticate(
    live: &SiteLive,
    handle: &McpHandle,
    site_ctx: &SiteContext,
    presented: &Presented,
    now_unix: i64,
) -> Result<McpPrincipal, Refused> {
    let auth = live.auth().ok_or(Refused::Unavailable)?;
    let ctx = RequestCtx {
        site: site_ctx.site.clone(),
        credential: Some(Credential::new(presented.token.clone())),
        idempotency_key: None,
        language: None,
        request_id: site_ctx.request_id.clone(),
        peer: site_ctx.peer,
    };
    let who = match auth.authenticate_on(&ctx, PathClass::Api).await {
        Ok(who) => who,
        Err(error) => {
            return Err(match error.code.as_str() {
                // Every way a credential can fail to authenticate answers the same 401.
                "auth.invalid_credentials"
                | "auth.required"
                | "auth.session_expired"
                | "auth.session_not_found"
                | "auth.account_disabled" => Refused::Rejected,
                _ => Refused::Unavailable,
            });
        }
    };
    if auth.is_restricted(&who) {
        return Err(Refused::Restricted);
    }
    let principals = live.principals().ok_or(Refused::Unavailable)?;
    let resolved = principals.resolve(&who.user_id).await;
    let user = who.user_id.to_string();
    let actor = Actor::User {
        id: who.user_id,
        via: None,
    };
    let tenant = site_ctx.site.as_str();
    let agent = match handle.directory() {
        Some(directory) => directory.resolve(tenant, &user).await,
        None => None,
    };
    let mut principal = match agent {
        Some(agent) => McpPrincipal::new_agent(
            &user,
            actor,
            resolved.assignments,
            &agent.agent_id,
            agent.tier,
        ),
        None => McpPrincipal::new(&user, actor, resolved.assignments),
    }
    .at(now_unix);
    if matches!(who.via, Via::ApiKey)
        && let Some(key_id) = &presented.key_id
        && let Some(scopes) = handle.keys()
        && let Some(scope) = scopes.scope_of(tenant, key_id).await
    {
        principal = principal.with_key(key_id, scope);
    }
    Ok(principal)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(*k, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    #[test]
    fn nothing_presented_is_missing() {
        assert!(matches!(
            presented(&HeaderMap::new()),
            Err(NoCredential::Missing)
        ));
    }

    #[test]
    fn a_bearer_token_is_read_and_an_empty_or_unprefixed_one_is_malformed() {
        let ok = presented(&headers(&[("authorization", "Bearer abc")])).unwrap();
        assert_eq!(ok.token, "abc");
        assert!(ok.key_id.is_none());
        for bad in ["Bearer ", "Basic abc", "abc"] {
            assert!(matches!(
                presented(&headers(&[("authorization", bad)])),
                Err(NoCredential::Malformed)
            ));
        }
    }

    #[test]
    fn both_credentials_at_once_are_refused() {
        let both = headers(&[("authorization", "Bearer abc"), ("x-api-key", "a:b")]);
        assert!(matches!(presented(&both), Err(NoCredential::Malformed)));
    }

    #[test]
    fn a_malformed_api_key_is_refused_without_echo() {
        for bad in ["nocolon", "abc:def", ":"] {
            assert!(matches!(
                presented(&headers(&[("x-api-key", bad)])),
                Err(NoCredential::Malformed)
            ));
        }
    }

    #[test]
    fn a_well_formed_api_key_becomes_its_rest_form() {
        let (key, _) = ApiKey::generate();
        let text = key.text();
        let (id, secret) = text
            .strip_prefix("spk_")
            .and_then(|r| r.split_once('_'))
            .unwrap();
        let got = presented(&headers(&[("x-api-key", &format!("{id}:{secret}"))])).unwrap();
        assert_eq!(got.token, text);
        assert_eq!(got.key_id.as_deref(), Some(id));
    }

    #[tokio::test]
    async fn outside_a_scope_nobody_is_authenticated() {
        let credential = spot_mcp::Credential::bearer("x");
        assert!(ScopedAuthenticator.authenticate(&credential, 0).is_err());
    }
}
