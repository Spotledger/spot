//! Second-factor self-service: `auth_totp_enrol`, `auth_totp_confirm`, `auth_totp_disable` and
//! `auth_recovery_codes_regenerate` (`POST /api/v1/auth/totp/{enrol,confirm,disable}` and
//! `POST /api/v1/auth/recovery-codes/regenerate`, WP-PLT-02.T9), over the site's sign-in service.
//!
//! `spot-auth` owns the primitives, `spot-login` the rules (`LoginService::totp_*`: who may call,
//! the proof each change needs, replay, throttling, session effects, events) and `spot-http` the
//! routes ([`TotpPort`]). This module supplies the rest:
//!
//! - **who**: the site's sign-in service resolves the bearer on every call; a session that has not
//!   chosen its password yet may do nothing here (the request gate stops it first and the adapter
//!   refuses it again); an API key is not a person and is refused by the service;
//! - **whose**: the port is never given a user id: the factor is the caller's own;
//! - **what leaves**: the provisioning secret and the recovery codes, once, in the response. They
//!   are in no log line, no audit record and no idempotency record (these operations take no
//!   `Idempotency-Key`).
//!
//! # Level
//!
//! Optional. The factor and its codes are records of the site database whether or not this module
//! is loaded; with `enabled = false` the four routes are not mounted and answer as unserved
//! operations do. If the sign-in service is not available the check fails and readiness reports
//! the site degraded; the routes then answer `503 server.unavailable` with a retry hint, never a
//! success. The group is mounted only with its port, never half wired: the port is provided only
//! when the site has its sign-in stores (`[services.login_stores]`, required) and its throttle
//! (`[services.signin_throttle]`, required), because a second factor that can be changed without
//! a counted proof is a bypass.
//!
//! The setting is `[services.totp]` (`enabled`, default true).

use std::sync::Arc;

use serde::Deserialize;
use spot_http::api::Ports;
use spot_http::api::ports::totp::{CodesOut, EnrolOut, TotpPort, TotpResult};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};

use crate::live::SiteLive;
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Start order: after the sign-in stores (1005) and the sign-in throttle (1006).
pub const ORDER: u32 = 1040;

/// `[services.totp]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves the second-factor routes at all.
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

/// The port over the site's sign-in service. It holds the live hub, not the service: a reload
/// swaps the service and the next call uses the new one.
struct Totp {
    site: String,
    live: Arc<SiteLive>,
}

impl Totp {
    /// The sign-in service, and the caller refused when it is not a free person: another site's
    /// request, a session still restricted to choosing a password.
    async fn caller(
        &self,
        ctx: &RequestCtx,
    ) -> std::result::Result<Arc<crate::login::SiteAuth>, ApiError> {
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate(ctx).await?;
        if auth.is_restricted(&who) {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        Ok(auth)
    }
}

impl TotpPort for Totp {
    fn enrol<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, TotpResult<EnrolOut>> {
        Box::pin(async move {
            let auth = self.caller(ctx).await?;
            let out = auth.login_service().totp_enrol(ctx).await?;
            Ok(EnrolOut {
                secret: out.secret.clone(),
                provisioning_uri: out.provisioning_uri.clone(),
            })
        })
    }

    fn confirm<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<CodesOut>> {
        Box::pin(async move {
            let auth = self.caller(ctx).await?;
            let recovery_codes = auth.login_service().totp_confirm(ctx, code).await?;
            Ok(CodesOut { recovery_codes })
        })
    }

    fn disable<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        password: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<()>> {
        Box::pin(async move {
            let auth = self.caller(ctx).await?;
            auth.login_service().totp_disable(ctx, password, code).await
        })
    }

    fn regenerate<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        password: &'a str,
        code: &'a str,
    ) -> BoxFuture<'a, TotpResult<CodesOut>> {
        Box::pin(async move {
            let auth = self.caller(ctx).await?;
            let recovery_codes = auth
                .login_service()
                .totp_regenerate_recovery(ctx, password, code)
                .await?;
            Ok(CodesOut { recovery_codes })
        })
    }
}

/// The catalogue code logged when the routes are not mounted.
const CODE_NOT_MOUNTED: &str = "totp_host.not_mounted";

/// Registers the second-factor routes.
///
/// # Errors
///
/// A setting that does not decode; a group that cannot be mounted.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("totp")?;
    if !settings.enabled {
        return Ok(());
    }
    // Never half wired: the stores and the throttle must be on this site, or the group is not
    // mounted (it answers as an unserved operation does) and the site says why.
    let has_stores = ctx.port::<crate::login::SiteStorage>().is_some();
    let has_throttle = ctx
        .port::<crate::services::signin_throttle::SigninThrottle>()
        .is_some();
    if !(has_stores && has_throttle) {
        tracing::warn!(
            code = CODE_NOT_MOUNTED,
            stores = has_stores,
            throttle = has_throttle,
            "the second-factor routes are not mounted: the sign-in stores and the sign-in throttle are both required"
        );
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.to_string();
    let live = Arc::clone(env.live);
    if ctx.assembling() {
        let probe = Arc::clone(&live);
        ctx.add(Idle::new(
            "totp",
            Level::Optional,
            move || -> CheckFuture<'static> {
                let live = Arc::clone(&probe);
                Box::pin(async move {
                    if live.auth().is_some() {
                        Ok(())
                    } else {
                        Err("the sign-in service is not available, so second factors cannot be managed".to_owned())
                    }
                })
            },
        ));
    }
    let port: Arc<dyn TotpPort> = Arc::new(Totp { site, live });
    let mut ports = Ports::new();
    ports.provide::<dyn TotpPort>(port);
    // Mounted here, not by the route registry (see `routes/totp.rs`). The host authenticates
    // every request before the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::totp::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        return Err(Error(format!(
            "the second-factor routes were not mounted: {module}: {reason:?}"
        )));
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_setting_refuses_unknown_keys() {
        let ok: std::result::Result<Settings, _> = toml::from_str("enabled = false");
        assert!(!ok.unwrap().enabled);
        let bad: std::result::Result<Settings, _> = toml::from_str("enabld = true");
        assert!(bad.is_err());
        assert!(Settings::default().enabled);
    }
}
