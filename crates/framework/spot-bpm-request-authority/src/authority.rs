//! The production request authority: `rg1.` grants, signed with HMAC-SHA256 under a server secret
//! resolved from the vault on every call.
//!
//! # What a grant binds
//!
//! [`Grant`] names the request, the step, the attempt, the acting person and an **absolute
//! expiry the engine passes in**. Nothing here reads a clock to decide what a grant's expiry is:
//! the caller states it, this crate refuses one that is already past or further than
//! [`MAX_GRANT_LIFETIME`] away, and [`authenticate`](RequestAuthority::authenticate) refuses one
//! whose instant has arrived. That is what makes "valid for at most the lifetime the engine
//! asked for, and never more than fifteen minutes" checkable from the payload alone.
//!
//! # Why a leaked grant dies
//!
//! The tag proves the authority issued the grant; it does not keep it alive. On every
//! `authenticate` the authority asks the [`RequestStore`] whether the request is still open and
//! whether its step is still at the attempt the grant names. A newer attempt moves the store, so
//! the older grant stops working; closing the request removes its process, so every grant for it
//! stops working. Nothing has to be revoked, listed or expired for that to happen, and a leaked
//! grant is worth nothing beyond the attempt it was issued for.
//!
//! # What is never read
//!
//! No randomness: issuing the same grant twice under the same secret, store and clock gives the
//! same string, because the payload is complete and a nonce would add nothing an attacker could
//! not already see. No wall clock in `authenticate` or `issue` beyond the injected [`Clock`] port.
//! No secret in a field: the vault value is resolved, used for one HMAC and dropped.

use std::fmt;
use std::sync::Arc;

use spot_bpm_governance::{GovernanceError, RequestAuthority, VerifiedRequest};
use spot_core::Clock;
use spot_secrets::SecretBytes;
use time::{Duration, OffsetDateTime};

use crate::error::GrantError;
use crate::grant::{GRANT_PREFIX, Grant, Token, is_typed_actor};
use crate::secret::{GrantSecrets, SecretRef};
use crate::store::RequestStore;

/// The longest lifetime a grant may have: fifteen minutes, the cap the engine's request step
/// passes its expiry against.
pub const MAX_GRANT_LIFETIME: Duration = Duration::minutes(15);

/// The shortest server secret this crate signs under, in bytes.
///
/// A vault entry that is empty or a stub is refused rather than used: an HMAC under a guessable
/// key is a signature anyone can forge, and a host that mis-wires the reference should fail
/// closed on its first issue.
pub const MIN_SECRET_BYTES: usize = 16;

/// The [`RequestAuthority`] a deployed site runs.
///
/// It implements the port the guards in `spot-bpm-governance` and `spot-bpm-entry` judge a create
/// with, so nothing about a caller changes when the authority becomes this one: the guards already
/// ask it to authenticate, and it now answers only for a grant it signed.
pub struct ProductionRequestAuthority {
    store: Arc<dyn RequestStore>,
    secrets: Arc<dyn GrantSecrets>,
    secret: SecretRef,
    clock: Arc<dyn Clock>,
}

impl ProductionRequestAuthority {
    /// An authority over `store`, signing with the vault secret named `secret`, reading the time
    /// from `clock`.
    ///
    /// The clock is the same [`Clock`] port the engine takes, so a test moves time with
    /// `ManualClock` and never sleeps.
    #[must_use]
    pub fn new(
        store: Arc<dyn RequestStore>,
        secrets: Arc<dyn GrantSecrets>,
        secret: SecretRef,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            store,
            secrets,
            secret,
            clock,
        }
    }

    /// Issues a grant for `request`'s `node` at `attempt`, held by `actor`, valid until exactly
    /// `expires_at`.
    ///
    /// The expiry is the caller's: this is the method an engine step calls with the instant it
    /// wants the grant to die at. It is refused when it is already past, when it is further away
    /// than [`MAX_GRANT_LIFETIME`], or when the request store does not hold `request`'s `node` at
    /// `attempt` — an authority never vouches for an attempt the engine is not running.
    ///
    /// Issuing for a newer attempt is what invalidates the older ones: the older grant no longer
    /// names the step's current attempt, so it stops authenticating the moment the store moves.
    ///
    /// # Errors
    /// [`GrantError::IdentifierBlank`], [`GrantError::ActorNotTyped`], [`GrantError::LifetimeTooLong`],
    /// [`GrantError::AttemptNotCurrent`] or [`GrantError::SecretUnavailable`].
    pub fn issue_until(
        &self,
        request: &str,
        node: &str,
        attempt: u32,
        actor: &str,
        expires_at: OffsetDateTime,
    ) -> Result<String, GrantError> {
        if request.trim().is_empty() || node.trim().is_empty() || actor.trim().is_empty() {
            return Err(GrantError::IdentifierBlank);
        }
        if !is_typed_actor(actor) {
            return Err(GrantError::ActorNotTyped);
        }
        let lifetime = expires_at
            .unix_timestamp()
            .saturating_sub(self.clock.now_utc().unix_timestamp());
        if lifetime <= 0 || lifetime > MAX_GRANT_LIFETIME.whole_seconds() {
            return Err(GrantError::LifetimeTooLong {
                max_minutes: MAX_GRANT_LIFETIME.whole_minutes(),
            });
        }
        if self.store.process_of(request).is_none()
            || self.store.current_attempt(request, node) != Some(attempt)
        {
            return Err(GrantError::AttemptNotCurrent { attempt });
        }
        let grant = Grant::new(request, node, attempt, actor, expires_at);
        let payload_b64 = grant.payload_base64();
        // The secret lives for this one HMAC and is dropped here, before the grant is returned:
        // nothing below this block can see it.
        let tag_b64 = {
            let secret = self.resolve_secret()?;
            let tag = Token::expected_tag(secret.expose(), &payload_b64);
            use base64::Engine;
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(tag)
        };
        Ok(format!("{GRANT_PREFIX}{payload_b64}.{tag_b64}"))
    }

    /// The vault value this authority signs under, refused when it is too short to be a key.
    fn resolve_secret(&self) -> Result<SecretBytes, GrantError> {
        let secret = self.secrets.resolve(&self.secret)?;
        if secret.expose().len() < MIN_SECRET_BYTES {
            return Err(GrantError::SecretUnavailable);
        }
        Ok(secret)
    }

    /// Wraps the authority for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }
}

impl fmt::Debug for ProductionRequestAuthority {
    /// Prints the secret's **name**, never its value, and no vault or store internals: a
    /// `Debug` line is the most likely place for a secret to escape.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProductionRequestAuthority")
            .field("secret", &self.secret)
            .field("min_secret_bytes", &MIN_SECRET_BYTES)
            .field("max_lifetime", &MAX_GRANT_LIFETIME)
            .finish_non_exhaustive()
    }
}

impl RequestAuthority for ProductionRequestAuthority {
    /// The port's `issue`, without a time argument: it asks for the longest lifetime the cap
    /// allows, from the injected clock. An engine step that wants a shorter one calls
    /// [`issue_until`](Self::issue_until) with the instant it means.
    ///
    /// # Errors
    /// [`GovernanceError::Settings`] carrying this crate's refusal, when it cannot issue.
    fn issue(
        &self,
        request: &str,
        node: &str,
        attempt: u32,
        actor: &str,
    ) -> Result<String, GovernanceError> {
        let expires_at = self
            .clock
            .now_utc()
            .checked_add(MAX_GRANT_LIFETIME)
            .ok_or_else(|| {
                GrantError::LifetimeTooLong {
                    max_minutes: MAX_GRANT_LIFETIME.whole_minutes(),
                }
                .to_governance_error()
            })?;
        self.issue_until(request, node, attempt, actor, expires_at)
            .map_err(|error| error.to_governance_error())
    }

    /// A [`VerifiedRequest`] only for a grant this authority signed, for exactly `actor`, still
    /// inside its expiry, for an open request whose step is still at the grant's attempt.
    ///
    /// Everything else is `None`: a forged tag, a changed payload, another person's grant, a
    /// replayed old attempt, an expired grant, a grant for a request that has closed. The order
    /// matters — the tag is checked over the payload **exactly as it arrived** before any of its
    /// fields are read, so no field of an unauthenticated grant is trusted, not even to look up
    /// the request it names.
    fn authenticate(&self, command_id: &str, actor: &str) -> Option<VerifiedRequest> {
        if actor.trim().is_empty() {
            return None;
        }
        let token = Token::split(command_id)?;
        // The second line, in constant time, is what makes the payload below worth parsing.
        let signed = {
            let secret = self.resolve_secret().ok()?;
            token.tag_matches(secret.expose())
        };
        if !signed {
            return None;
        }
        let grant = Grant::parse_base64(token.payload_b64)?;
        if grant.expires_at.unix_timestamp() <= self.clock.now_utc().unix_timestamp() {
            return None;
        }
        if grant.actor != actor {
            return None;
        }
        if self.store.process_of(&grant.request).is_none()
            || self.store.current_attempt(&grant.request, &grant.node) != Some(grant.attempt)
        {
            return None;
        }
        Some(VerifiedRequest {
            request: grant.request,
            node: grant.node,
            attempt: grant.attempt,
        })
    }

    /// The process `request` runs, or `None` when it is not open.
    fn process_of(&self, request: &str) -> Option<String> {
        self.store.process_of(request)
    }

    /// Whether the verified step may create a `doctype` record.
    ///
    /// The step's current attempt is read again here, not carried over from `authenticate`: a
    /// grant that a newer attempt invalidated between the two calls creates nothing.
    fn may_create(&self, step: &VerifiedRequest, doctype: &str) -> bool {
        self.store.current_attempt(&step.request, &step.node) == Some(step.attempt)
            && self.store.may_create(&step.request, &step.node, doctype)
    }
}
