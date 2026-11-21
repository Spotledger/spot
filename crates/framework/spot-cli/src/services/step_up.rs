//! Step-up: `auth_step_up` (`POST /api/v1/auth/step-up`, WP-IAM-12.T8, ADR-070 section 3) over the
//! site's sign-in service, and the marker the operations that need a recent step-up read.
//!
//! `spot-http` owns the route ([`StepUpPort`], WP-IAM-12.T7); `spot-login` owns the sign-in
//! service, its user directory and its hasher. This module supplies the rest:
//!
//! - **who**: the site's sign-in service resolves the bearer on every call. Only a person's
//!   session of the staff or partner audience may step up: an API key, a consumer, a session
//!   still restricted to choosing a password and another site's request are `403
//!   permission.denied`. The port is never given a user id; the caller's own user and own session
//!   are the only ones this module reads or writes;
//! - **the proof**: the password and, when the user is enrolled, the second-factor code
//!   (`step_up/proof.rs`), re-checked with the sign-in's own hasher and one-use-per-step code
//!   check. A wrong credential is `401 auth.invalid_credentials` exactly as at sign-in, with the
//!   same body whether the account has a second factor or not; a missing code is `401
//!   auth.totp_required`; a wrong or replayed code is `401 auth.totp_invalid`;
//! - **the throttle**: the attempt is admitted through the site's sign-in throttle (the password
//!   door, counted under the user's own name and the client address, the same buckets as the
//!   sign-in) **before** the credential is looked at. A throttled caller gets `429
//!   api.rate_limited` with `Retry-After` and the credential is not checked; a throttle that
//!   cannot count refuses (`503`, fail closed);
//! - **the marker**: on success `session.step_up_at = now` is written on the caller's own
//!   session (`step_up/marker.rs`), in one transaction with the audit event. The answer is
//!   `step_up_until = now + window`, the window being `[services.identity_admin]
//!   step_up_window_secs` (900 by default), the same window
//!   [`IdentityStepUpPort::is_recent`](spot_http::api::ports::identity::IdentityStepUpPort)
//!   applies when an operation asks whether the step-up is recent;
//! - **the audit**: two consumers, written side by side.
//!   *The identity audit chain* (the one `spot audit verify` checks, WP-IAM-12.T9): a success is
//!   one `step_up_succeeded` record written through `Recorder::required` **before** the marker, so
//!   a record that cannot be written refuses the step-up (`503 server.unavailable`) and no marker
//!   exists; a refusal is one `step_up_failed` record with a reason class (`bad_credentials`,
//!   `totp_required`, `totp_invalid`, `throttled`, `not_a_person`, `marker_not_written`) written
//!   through `Recorder::failure` (budgeted, and it never changes the answer the caller sees). The
//!   record names the user, the session id, the salted source digest and the channel `api`; it has
//!   no field that could hold a password, a code or a token.
//!   *The event stream*: a success is an `identity_sign_in_succeeded` event with `step_up: true`
//!   written with the marker; a refusal after the credential was identified is an
//!   `identity_sign_in_failed` event. These carry the user, the method and (for a refusal) a
//!   reason class. The two are different consumers and both stay. Every outcome is also one line
//!   on the `spot::audit::identity` target with the outcome and the registry code;
//! - **single sign-on**: a session established by a provider steps up by `sso: true`, which would
//!   answer the `redirect_url` of a forced re-authentication at the provider. The single sign-on
//!   service offers no such operation yet, so that branch is `501 service.not_configured`. A
//!   password proof on a single sign-on session, and `sso: true` on a password session, are
//!   `400 api.invalid_request`.
//!
//! # Order of the success path
//!
//! proof, then the chain record (required), then the marker with its event. A marker write that
//! fails after the chain record stood leaves a `step_up_succeeded` record with no effect; the
//! module then writes a `step_up_failed` record with the reason `marker_not_written` for the same
//! user and session, so a reader of the chain tells a step-up that took effect (a
//! `step_up_succeeded` with no `marker_not_written` after it) from one that did not.
//!
//! # Level
//!
//! Optional. The group is mounted only with its port, never half wired: the port is provided only
//! when the site has its sign-in stores (`[services.login_stores]`, required) and its throttle
//! (`[services.signin_throttle]`, required), because a re-authentication that is not counted is a
//! password oracle. Without them, or with `enabled = false`, the route answers as an unserved
//! operation does and the operations that need a recent step-up stay refused. If the sign-in
//! service is not available the check fails and readiness reports the site degraded; the route
//! then answers `503 server.unavailable` with a retry hint, never a success.
//!
//! The setting is `[services.step_up]` (`enabled`, default true). The window is not set here.
//!
//! # Behaviour change
//!
//! Before this module "recent" meant "the session was created within the window", so a fresh
//! sign-in counted as a step-up. It no longer does: `IdentityUsers::is_recent`
//! (`services/identity_admin.rs`) reads `session.step_up_at`, which only this module writes, and
//! a session that never stepped up is not recent, whatever its age. Since WP-IAM-12.T9 the single
//! sign-on administration (`services/sso_admin.rs`) and the support-access caller
//! (`services/support_access_wiring.rs`) read the same marker through [`session_is_current`]
//! (`step_up/marker.rs`), with the same window, instead of the session's age.

use std::sync::Arc;

use serde::Deserialize;
use spot_auth::HashParams;
use spot_core::{Audience, Clock};
use spot_http::api::Ports;
use spot_http::api::codec::parse_record_id;
use spot_http::api::ports::step_up::{
    StepUpError, StepUpOut, StepUpPort, StepUpProof, StepUpResult,
};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_login::{Argon2Hasher, UserDirectory, Via, invalid_credentials};
use time::format_description::well_known::Rfc3339;

use crate::audit::{PathClass, StepUpFailure, StepUpOutcome};
use crate::live::SiteLive;
use crate::login::SiteStorage;
use crate::services::signin_throttle::{Door, SigninThrottle};
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

#[path = "step_up/marker.rs"]
mod marker;
pub use marker::{DEFAULT_WINDOW_SECS, session_is_current, set_window, window_of, within_window};
#[path = "step_up/proof.rs"]
mod proof;

/// Start order: after the sign-in stores (1005) and the sign-in throttle (1006), beside the
/// second-factor service (1040).
pub const ORDER: u32 = 1042;

/// The audit target shared with the identity administration.
const AUDIT: &str = "spot::audit::identity";

/// The catalogue code logged when the route is not mounted.
const CODE_NOT_MOUNTED: &str = "step_up_host.not_mounted";
/// The catalogue code logged when a refusal could not be written to the event outbox.
pub(crate) const CODE_AUDIT_FAILED: &str = "step_up_host.audit_failed";
/// The catalogue code logged when the success record could not be written to the identity audit
/// chain, so the step-up was refused.
pub(crate) const CODE_CHAIN_UNAVAILABLE: &str = "step_up_host.chain_unavailable";

/// `[services.step_up]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves the step-up route at all.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", serde_json::Value::from(5))
}

fn denied() -> ApiError {
    ApiError::from_code(Code::PermissionDenied)
}

fn invalid_request() -> ApiError {
    ApiError::new(
        Code::ApiInvalidRequest,
        "This kind of proof does not match how the session was established.",
    )
}

/// A UTC instant as the contract's RFC 3339 string (whole seconds, ending in `Z`).
fn instant_text(at: time::OffsetDateTime) -> StepUpResult<String> {
    let whole = at.replace_nanosecond(0).unwrap_or(at);
    whole
        .format(&Rfc3339)
        .map_err(|_| StepUpError::Api(ApiError::from_code(Code::HttpInternal)))
}

/// The port over the site's sign-in service. It holds the live hub, not the service: a reload
/// swaps the service and the next call uses the new one.
struct StepUp {
    site: String,
    live: Arc<SiteLive>,
    directory: Arc<dyn UserDirectory>,
    marker: marker::Marker,
    verifier: proof::Verifier,
    throttle: Arc<SigninThrottle>,
    clock: Arc<dyn Clock>,
    /// How long a step-up lasts, in seconds.
    window: i64,
}

impl StepUp {
    /// One line of the audit target: who, outcome and code. Never a credential.
    fn line(&self, user: &str, outcome: &'static str, code: &str) {
        tracing::info!(
            target: AUDIT,
            site = %self.site,
            op = "auth_step_up",
            outcome,
            code,
            user,
            "step-up"
        );
    }

    /// Writes one refusal record to the identity audit chain. Budgeted, never an error: the
    /// caller's answer does not depend on it.
    fn chain_refusal(
        auth: &crate::login::SiteAuth,
        user: &str,
        session: Option<&str>,
        source: Option<&str>,
        reason: StepUpFailure,
    ) {
        // A refusal's record cannot fail the call (`Recorder::failure` never errors).
        let _ = auth.record_step_up(user, session, source, StepUpOutcome::Failed(reason));
    }

    async fn run(&self, ctx: &RequestCtx, proof: StepUpProof) -> StepUpResult<StepUpOut> {
        if ctx.site.as_str() != self.site {
            return Err(denied().into());
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        // A person: a session (never an API key) of the staff or partner audience that has
        // chosen its password.
        let user_text = who.user_id.to_string();
        let Some(session_text) = who.session_id.as_deref() else {
            Self::chain_refusal(&auth, &user_text, None, None, StepUpFailure::NotAPerson);
            return Err(denied().into());
        };
        if !matches!(who.via, Via::Password)
            || !matches!(who.audience, Audience::Staff | Audience::Partner)
            || auth.is_restricted(&who)
        {
            Self::chain_refusal(
                &auth,
                &user_text,
                Some(session_text),
                None,
                StepUpFailure::NotAPerson,
            );
            return Err(denied().into());
        }
        let session = parse_record_id(session_text).ok_or_else(denied)?;

        // The session record says how it was established and whose it is.
        let facts = match self.marker.facts(session).await? {
            Some(facts) if facts.active => facts,
            _ => return Err(ApiError::from_code(Code::AuthSessionExpired).into()),
        };
        if facts.user != Some(who.user_id) {
            return Err(denied().into());
        }
        let method = if facts.is_sso() { "sso" } else { "password" };
        let source = facts.source.clone();
        match (proof.sso, facts.is_sso()) {
            (true, true) => {
                // A forced re-authentication at the provider is not offered by the single
                // sign-on service yet.
                self.line(&user_text, "unavailable", Code::ServerUnavailable.as_str());
                return Err(StepUpError::NotConfigured);
            }
            (true, false) | (false, true) => return Err(invalid_request().into()),
            (false, false) => {}
        }
        let Some(password) = proof.password.as_deref() else {
            return Err(invalid_request().into());
        };

        // The throttle comes first: a throttled caller is refused before the credential is
        // looked at, so the answer is the same for a right and a wrong password.
        let account = spot_login::normalise_identifier(&who.user_name);
        let peer = ctx.peer.map(|peer| peer.to_string());
        let connection = format!("no-peer/{account}");
        let now = self.clock.now_utc();
        if let Err(refusal) = self
            .throttle
            .admit(
                Door::Password,
                ctx.site.as_str(),
                &account,
                peer.as_deref(),
                Some(connection.as_str()),
            )
            .await
        {
            self.line(&user_text, "refused", Code::ApiRateLimited.as_str());
            if refusal.code.as_str() == Code::ApiRateLimited.as_str() {
                self.marker
                    .refused(who.user_id, now, method, "locked")
                    .await;
                Self::chain_refusal(
                    &auth,
                    &user_text,
                    Some(session_text),
                    source.as_deref(),
                    StepUpFailure::Throttled,
                );
            }
            return Err(refusal.into());
        }

        let user = match self.directory.find_by_id(who.user_id).await {
            Ok(Some(user)) => user,
            Ok(None) => return Err(invalid_credentials().into()),
            Err(error) => return Err(error.to_api_error_traced(ctx.request_id.as_str()).into()),
        };
        let checked = self
            .verifier
            .check(
                self.directory.as_ref(),
                user,
                password,
                proof.totp.as_deref(),
                ctx.request_id.as_str(),
            )
            .await;
        if let Err(refusal) = checked {
            self.line(&user_text, "refused", refusal.code());
            // A directory failure is not a wrong credential: it is not written as one.
            let class = match &refusal {
                proof::Refusal::BadPassword => Some(StepUpFailure::BadCredentials),
                proof::Refusal::TotpRequired => Some(StepUpFailure::TotpRequired),
                proof::Refusal::TotpInvalid => Some(StepUpFailure::TotpInvalid),
                proof::Refusal::Failed(_) => None,
            };
            if let Some(class) = class {
                self.marker
                    .refused(who.user_id, now, method, "bad_credentials")
                    .await;
                Self::chain_refusal(
                    &auth,
                    &user_text,
                    Some(session_text),
                    source.as_deref(),
                    class,
                );
            }
            return Err(refusal.into_api_error().into());
        }

        // The proof holds. First the record in the identity audit chain, required: an unauditable
        // step-up is not granted, and no marker exists when the record could not be written.
        if auth
            .record_step_up(
                &user_text,
                Some(session_text),
                source.as_deref(),
                StepUpOutcome::Succeeded,
            )
            .is_err()
        {
            tracing::warn!(
                code = CODE_CHAIN_UNAVAILABLE,
                "a step-up was refused: its record could not be written to the identity audit chain"
            );
            self.line(&user_text, "failed", Code::ServerUnavailable.as_str());
            return Err(unavailable().into());
        }
        // Then the marker and its event, in one transaction, on the caller's own session. An
        // outage here refuses; nothing half-written stands. The chain record already stands, so
        // a failure is followed by a `marker_not_written` refusal record for the same session.
        if let Err(error) = self.marker.mark(session, who.user_id, now, method).await {
            self.line(&user_text, "failed", error.code.as_str());
            Self::chain_refusal(
                &auth,
                &user_text,
                Some(session_text),
                source.as_deref(),
                StepUpFailure::MarkerNotWritten,
            );
            return Err(error.into());
        }
        self.line(&user_text, "succeeded", "");
        let until = now + time::Duration::seconds(self.window);
        Ok(StepUpOut {
            step_up_until: Some(instant_text(until)?),
            redirect_url: None,
        })
    }
}

impl StepUpPort for StepUp {
    fn step_up<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        proof: StepUpProof,
    ) -> BoxFuture<'a, StepUpResult<StepUpOut>> {
        Box::pin(async move { self.run(ctx, proof).await })
    }
}

/// Registers the step-up route.
///
/// # Errors
///
/// A setting that does not decode; a group that cannot be mounted.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("step_up")?;
    if !settings.enabled {
        return Ok(());
    }
    // Never half wired: the stores and the throttle must be on this site, or the route is not
    // mounted (it answers as an unserved operation does) and the site says why.
    let storage = ctx.port::<SiteStorage>();
    let throttle = ctx.port::<SigninThrottle>();
    let (Some(storage), Some(throttle)) = (storage, throttle) else {
        tracing::warn!(
            code = CODE_NOT_MOUNTED,
            "the step-up route is not mounted: the sign-in stores and the sign-in throttle are both required"
        );
        return Ok(());
    };
    let window = crate::services::identity_admin::step_up_window_secs(ctx)?;
    let env = *ctx.env()?;
    let site = env.site.site.to_string();
    let live = Arc::clone(env.live);
    let clock = Arc::clone(ctx.clock());
    if ctx.assembling() {
        let probe = Arc::clone(&live);
        ctx.add(Idle::new(
            "step_up",
            Level::Optional,
            move || -> CheckFuture<'static> {
                let live = Arc::clone(&probe);
                Box::pin(async move {
                    if live.auth().is_some() {
                        Ok(())
                    } else {
                        Err(
                            "the sign-in service is not available, so a step-up cannot be checked"
                                .to_owned(),
                        )
                    }
                })
            },
        ));
    }
    let directory: Arc<dyn UserDirectory> = Arc::new(storage.stores.user_directory());
    let hasher = Arc::new(Argon2Hasher::new(HashParams::default()));
    let totp = crate::login::default_login_config().totp;
    let port: Arc<dyn StepUpPort> = Arc::new(StepUp {
        site,
        live,
        directory,
        marker: marker::Marker::new(storage.backend.clone(), storage.catalog.clone()),
        verifier: proof::Verifier::new(hasher, totp, Arc::clone(&clock)),
        throttle,
        clock,
        window,
    });
    let mut ports = Ports::new();
    ports.provide::<dyn StepUpPort>(port);
    // Mounted here, not by the registry pass of `host.rs` (it is classified as a service-mounted
    // group there). The host authenticates every request before the router sees it, and the
    // port authenticates again and refuses what is not a person of the staff or partner audience.
    let mounted = routes::mount(&[routes::step_up::registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        return Err(Error(format!(
            "the step-up route was not mounted: {module}: {reason:?}"
        )));
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use time::macros::datetime;

    #[test]
    fn the_setting_refuses_unknown_keys() {
        let ok: std::result::Result<Settings, _> = toml::from_str("enabled = false");
        assert!(!ok.unwrap().enabled);
        let bad: std::result::Result<Settings, _> = toml::from_str("enabld = true");
        assert!(bad.is_err());
        assert!(Settings::default().enabled);
    }

    #[test]
    fn an_instant_is_utc_whole_seconds_ending_in_z() {
        let text = instant_text(datetime!(2026-11-21 09:05:00.123456 UTC)).unwrap();
        assert_eq!(text, "2026-11-21T09:05:00Z");
        assert!(text.len() <= 40);
    }

    #[test]
    fn the_refusals_the_module_makes_itself_have_their_codes() {
        assert_eq!(denied().code.as_str(), Code::PermissionDenied.as_str());
        assert_eq!(
            invalid_request().code.as_str(),
            Code::ApiInvalidRequest.as_str()
        );
        assert_eq!(
            unavailable().code.as_str(),
            Code::ServerUnavailable.as_str()
        );
    }
}
