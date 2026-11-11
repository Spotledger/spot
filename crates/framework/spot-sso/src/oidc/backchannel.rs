//! OIDC back-channel logout (OpenID Connect Back-Channel Logout 1.0), as a sans-I/O check.
//!
//! The provider POSTs a **logout token** — a JWS, never an ID token — and the relying party
//! decides which sessions to end. This module verifies the token and returns the
//! [`LogoutTarget`] it names; it never touches a session. The caller maps the target to linked
//! identities ([`crate::logout::resolve_target`]) and to its session table, then ends them.
//!
//! # What a logout token must satisfy
//!
//! - a signature by one of the provider's keys, with the same algorithm allow-list as an ID
//!   token: `none` and the HMAC algorithms are never candidates, so an `HS256` token cannot be
//!   confused with an RSA key;
//! - `iss` exactly the configured and discovered issuer;
//! - `aud` containing the client id;
//! - `iat` present and inside the tolerated clock skew;
//! - `exp` present, an integer, not passed (with the skew) and no more than
//!   [`ProviderConfig::max_logout_token_lifetime_seconds`] after `iat`: a token without an
//!   expiry would be a standing credential to sign a user out;
//! - a `jti` recorded once through a [`crate::replay::ReplayStore`];
//! - an `events` claim carrying the back-channel-logout event member;
//! - **no `nonce`**: a token with one is an ID token, and an ID token is not a logout;
//! - at least one of `sub` or `sid`, naming the person or the session.
//!
//! No token, claim set or key material appears in an error; the two texts are public guidance
//! and an administrator remedy.

use serde_json::Value;

use crate::error::{self as e, Result, SsoError};
use crate::logout::LogoutTarget;
use crate::oidc::discovery::Discovery;
use crate::oidc::jwks::KeyCache;
use crate::oidc::jwt;
use crate::provider::{Jwa, ProviderConfig};
use crate::replay::{Claim, ReplayStore, namespaced};

/// The event member a back-channel logout token must carry.
pub const BACKCHANNEL_LOGOUT_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

/// Validates a back-channel logout token and returns what it names.
///
/// `now` is the caller's clock; the token's `iat` must lie within
/// [`ProviderConfig::clock_skew_seconds`] of it, because the issuer delivers a logout token
/// at once, and its `exp` must be present, not passed, and within
/// [`ProviderConfig::max_logout_token_lifetime_seconds`] of `iat`. `replay` records the `jti` only after every other check
/// has passed, so an unauthenticated token cannot fill the store.
///
/// # Errors
///
/// [`e::codes::REPLAY_STORE_UNAVAILABLE`] when the replay store cannot answer (the token is
/// refused, never accepted unchecked); [`e::codes::JWKS_STALE`] when the token names a key the cache does not hold (the caller may
/// refetch once); the token, algorithm, signature, issuer, audience, timing, `jti`, events,
/// `nonce` and target codes otherwise, each with its own stable `sso.*` value.
pub async fn validate_logout_token(
    config: &ProviderConfig,
    discovery: &Discovery,
    keys: &KeyCache,
    token: &str,
    now: i64,
    replay: &impl ReplayStore,
) -> Result<LogoutTarget> {
    let token = jwt::parse(token)?;
    // 1. The algorithm is on the allow-list and agrees with the key's own type and algorithm.
    let alg = Jwa::parse(&token.header.alg).ok_or_else(|| {
        SsoError::new(
            e::codes::ALG_UNSUPPORTED,
            "The sign-out request used a method this tenant does not allow.",
            "Allow only RS256 and ES256 for this provider.",
        )
    })?;
    if !config.allowed_algs.contains(&alg) {
        return Err(SsoError::new(
            e::codes::ALG_UNSUPPORTED,
            "The sign-out request used a method this tenant does not allow.",
            "Allow only RS256 and ES256 for this provider.",
        ));
    }
    let key = keys
        .lookup(token.header.kid.as_deref(), now)
        .ok_or_else(|| {
            SsoError::new(
                e::codes::JWKS_STALE,
                "The sign-out request names a key the platform does not hold.",
                "Refresh the provider's key set and try once more.",
            )
        })?;
    if key.alg.is_some_and(|key_alg| key_alg != alg) || key.key.is_rsa() != alg.needs_rsa() {
        return Err(SsoError::new(
            e::codes::ALG_MISMATCH,
            "The sign-out request was not trusted.",
            "Check the provider signs with the algorithm its keys declare.",
        ));
    }
    // 2. The signature.
    if !key.key.verify(alg, &token.signing_input, &token.signature) {
        return Err(SsoError::new(
            e::codes::SIGNATURE_INVALID,
            "The sign-out request was not trusted.",
            "Check the provider's keys are current and the signing algorithm matches.",
        ));
    }
    // 3. `iss`, exactly, against the configuration and the discovery document.
    let iss = jwt::claim_str(&token.claims, "iss").unwrap_or_default();
    if iss != config.issuer || iss != discovery.issuer {
        return Err(SsoError::new(
            e::codes::ISSUER_MISMATCH,
            "The sign-out request was not trusted.",
            "Check the configured issuer matches the provider's `iss` exactly.",
        ));
    }
    // 4. `aud` contains the client id.
    if !jwt::audiences(&token.claims)
        .iter()
        .any(|aud| aud == &config.client_id)
    {
        return Err(SsoError::new(
            e::codes::AUDIENCE_MISMATCH,
            "The sign-out request was not for this application.",
            "Check the client id registered with the provider.",
        ));
    }
    // 5. `iat` is present and inside the tolerated skew; `exp` is present, has not passed and is
    //    no further from `iat` than the configured lifetime.
    let skew = config.clock_skew_seconds;
    let iat = jwt::claim_i64(&token.claims, "iat").ok_or_else(|| {
        SsoError::new(
            e::codes::TOKEN_MALFORMED,
            "The sign-out request could not be read.",
            "Check the provider returns a standard back-channel logout token.",
        )
    })?;
    if iat > now + skew {
        return Err(SsoError::new(
            e::codes::ISSUED_AT_INVALID,
            "The sign-out request is not valid yet.",
            "Check the server clock and the tolerated skew.",
        ));
    }
    if now.saturating_sub(iat) > skew {
        return Err(SsoError::new(
            e::codes::TOKEN_EXPIRED,
            "The sign-out request has taken too long.",
            "Check the server clock, and that the provider delivers logout tokens at once.",
        ));
    }
    // `exp` is required: absent or not an integer is a refusal, not a default.
    let exp = jwt::claim_i64(&token.claims, "exp").ok_or_else(|| {
        SsoError::new(
            e::codes::LOGOUT_TOKEN_NO_EXP,
            "The sign-out request was incomplete.",
            "The provider must give every logout token an integer exp.",
        )
    })?;
    // `exp` at or before `iat` is a token whose own times contradict each other, not one that
    // merely ran out.
    if exp <= iat {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_MALFORMED_TIMES,
            "The sign-out request could not be read.",
            "The logout token's exp is not after its iat; check the provider's clock and token lifetime.",
        ));
    }
    if now >= exp.saturating_add(skew) {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_EXPIRED,
            "The sign-out request has taken too long.",
            "Check the server clock and that the provider delivers logout tokens at once.",
        ));
    }
    if exp.saturating_sub(iat) > config.max_logout_token_lifetime_seconds {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_LIFETIME_EXCESSIVE,
            "The sign-out request was refused.",
            "The logout token lives longer than this tenant allows; shorten the provider's logout token lifetime.",
        ));
    }
    // 6. `jti` is present; it is recorded below, once everything else has passed.
    let jti = jwt::claim_str(&token.claims, "jti")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            SsoError::new(
                e::codes::LOGOUT_TOKEN_MISSING_JTI,
                "The sign-out request was incomplete.",
                "The provider must give every logout token a `jti`.",
            )
        })?;
    // 7. The back-channel event member is present and is an object.
    let is_backchannel = token
        .claims
        .get("events")
        .and_then(Value::as_object)
        .is_some_and(|events| {
            events
                .get(BACKCHANNEL_LOGOUT_EVENT)
                .is_some_and(Value::is_object)
        });
    if !is_backchannel {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_EVENTS_MISSING,
            "The sign-out request was refused.",
            "A logout token must carry the back-channel-logout `events` member.",
        ));
    }
    // 8. A token carrying `nonce` is an ID token: refuse it, do not treat it as a logout.
    if token.claims.get("nonce").is_some() {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_NONCE_PRESENT,
            "The sign-out request was refused.",
            "A logout token never carries `nonce`; an ID token is not a logout.",
        ));
    }
    // 9. At least one of `sub` or `sid`.
    let subject = jwt::claim_str(&token.claims, "sub").filter(|value| !value.is_empty());
    let session_id = jwt::claim_str(&token.claims, "sid").filter(|value| !value.is_empty());
    if subject.is_none() && session_id.is_none() {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_NO_TARGET,
            "The sign-out request named no session.",
            "A logout token must carry `sub`, `sid`, or both.",
        ));
    }
    // 10. Replay: the jti is single use, remembered until the token could no longer pass. It
    // cannot pass once `exp + skew` is reached, and `exp` is at most the configured lifetime
    // after `iat`, which is at most `skew` ahead of `now`: the record is always bounded.
    let remember_until = exp.saturating_add(skew).saturating_add(1);
    // A store failure is a refusal (fail closed), never a fresh token.
    let key = namespaced("oidc-logout", config.id.as_str(), &jti);
    if replay.claim(&key, now, remember_until).await? == Claim::Replayed {
        return Err(SsoError::new(
            e::codes::LOGOUT_TOKEN_REPLAYED,
            "This sign-out request has already been used.",
            "Refuse a replayed logout token; the `jti` is single use.",
        ));
    }
    Ok(LogoutTarget::for_oidc(
        config.id.clone(),
        iss,
        subject,
        session_id,
    ))
}
