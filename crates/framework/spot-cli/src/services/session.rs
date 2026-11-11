//! A user's own sessions: `auth_session_list` and `auth_session_end`
//! (`GET /api/v1/auth/sessions`, `DELETE /api/v1/auth/sessions/{id}`, WP-IAM-07.T2), over the
//! site's sign-in service.
//!
//! `spot-sessions` owns the rules (the store, the liveness, the revoke) and `spot-login` the
//! sign-in service on top of it. `spot-http` owns the routes ([`SessionPort`]). This module
//! supplies the rest:
//!
//! - **who**: the site's sign-in service resolves the bearer credential on every call. An API key
//!   is not a person and has no session, so it can neither list nor end sessions (`403
//!   permission.denied`); a session that has not chosen its password yet may do nothing here;
//! - **whose**: the port is never given a user id. The list is the caller's own live sessions
//!   (the current one first, then the others newest first). An id that is unknown, another
//!   user's or already ended is one answer, `404 auth.session_not_found`, so an id alone tells
//!   nothing about other users;
//! - **ending**: ending the current session is allowed and equals logout (the token stops
//!   working on the next request); an ending is audited before it happens (see below);
//! - **what is shown**: the contract's `SessionRecord` and nothing else. The session token and its
//!   digest never leave the store; `source` is the site's salted digest of the client address,
//!   not the address.
//!
//! # Level
//!
//! Optional. The sessions of a site are held by the sign-in service whether or not this module is
//! loaded: if the sign-in service is not available at start the check fails and readiness reports
//! the site degraded, and the two routes answer `503 server.unavailable` with a retry hint; they
//! never answer an empty list for a service that cannot answer. Everything else (sign-in,
//! sign-out, the data routes) is unaffected. The setting is `services.session` (`enabled`,
//! default true); off, the two routes are not mounted and answer as unserved operations do.
//!
//! # Audit
//!
//! An ending writes one identity audit record, kind `session_ended_by_user`, reason `user_ended`,
//! with the user's record id and the session id, before the session is revoked, and an ending
//! whose record cannot be written is refused (`503`). The sign-in service then records the
//! revoke itself (`session_revoked`). Neither record carries a token or a digest.

use std::sync::Arc;

use serde::Deserialize;
use spot_http::api::Ports;
use spot_http::api::ports::session::{SessionOut, SessionPort, SessionResult};
use spot_http::api::registry::Code;
use spot_http::api::router::routes;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::live::SiteLive;
use crate::login::{OwnSession, SESSION_METHOD};
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Start order: after the framework's own services.
pub const ORDER: u32 = 1030;

/// `[services.session]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves the session routes at all.
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

/// A unix-second instant as the contract's UTC RFC 3339 string.
fn time_text(unix_seconds: i64) -> SessionResult<String> {
    OffsetDateTime::from_unix_timestamp(unix_seconds)
        .ok()
        .and_then(|at| at.format(&Rfc3339).ok())
        .ok_or_else(|| ApiError::from_code(Code::HttpInternal))
}

/// What the adapter shows of one session. Total over [`OwnSession`]: a field is copied or
/// formatted, never invented, and there is no field for a token.
pub fn present(session: &OwnSession) -> SessionResult<SessionOut> {
    Ok(SessionOut {
        id: session.id.clone(),
        created_at: time_text(session.created)?,
        expires_at: time_text(session.expires_at)?,
        last_used_at: Some(time_text(session.last_seen)?),
        method: SESSION_METHOD.to_owned(),
        source: session.source.clone(),
        device: session.device.clone(),
        current: session.current,
    })
}

/// The port over the site's sign-in service. It holds the live hub, not the service: a reload
/// swaps the service and the next call uses the new one.
struct Sessions {
    site: String,
    live: Arc<SiteLive>,
}

impl Sessions {
    fn auth(&self, ctx: &RequestCtx) -> SessionResult<Arc<crate::login::SiteAuth>> {
        if ctx.site.as_str() != self.site {
            return Err(ApiError::from_code(Code::PermissionDenied));
        }
        self.live.auth().ok_or_else(unavailable)
    }
}

impl SessionPort for Sessions {
    fn list<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, SessionResult<Vec<SessionOut>>> {
        Box::pin(async move {
            let auth = self.auth(ctx)?;
            let own = auth.own_sessions(ctx).await?;
            own.iter().map(present).collect()
        })
    }

    fn end<'a>(&'a self, ctx: &'a RequestCtx, id: &'a str) -> BoxFuture<'a, SessionResult<()>> {
        Box::pin(async move {
            let auth = self.auth(ctx)?;
            auth.end_own_session(ctx, id).await
        })
    }
}

/// Registers the session routes.
///
/// # Errors
///
/// A setting that does not decode.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("session")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.to_string();
    let live = Arc::clone(env.live);
    if ctx.assembling() {
        let probe = Arc::clone(&live);
        ctx.add(Idle::new("session", Level::Optional, move || -> CheckFuture<'static> {
            let live = Arc::clone(&probe);
            Box::pin(async move {
                if live.auth().is_some() {
                    Ok(())
                } else {
                    Err("the sign-in service is not available, so sessions cannot be listed or ended".to_owned())
                }
            })
        }));
    }
    let port: Arc<dyn SessionPort> = Arc::new(Sessions { site, live });
    let mut ports = Ports::new();
    ports.provide::<dyn SessionPort>(port);
    // The group is mounted here, not by the route registry (see `routes/session.rs`). The host
    // authenticates every request before the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[routes::session::registration()], &ports, &[]);
    let router = mounted.authenticated;
    if let Some((module, reason)) = mounted.report.skipped.first() {
        return Err(Error(format!(
            "the session routes were not mounted: {module}: {reason:?}"
        )));
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn own() -> OwnSession {
        OwnSession {
            id: "s1".into(),
            created: 1_800_000_000,
            last_seen: 1_800_000_100,
            expires_at: 1_800_040_000,
            source: "ab12".into(),
            device: Some("laptop".into()),
            current: true,
        }
    }

    #[test]
    fn the_presented_session_has_the_contract_fields_only() {
        let out = present(&own()).unwrap();
        assert_eq!(out.method, "password");
        assert_eq!(out.created_at, "2027-01-15T08:00:00Z");
        assert_eq!(out.last_used_at.as_deref(), Some("2027-01-15T08:01:40Z"));
        assert!(out.current);
        assert_eq!(out.device.as_deref(), Some("laptop"));
    }

    #[test]
    fn an_instant_out_of_range_is_an_internal_error_not_a_panic() {
        let mut bad = own();
        bad.created = i64::MAX;
        assert!(present(&bad).is_err());
    }

    #[test]
    fn the_setting_refuses_unknown_keys() {
        let ok: std::result::Result<Settings, _> = toml::from_str("enabled = false");
        assert!(!ok.unwrap().enabled);
        let bad: std::result::Result<Settings, _> = toml::from_str("enabld = true");
        assert!(bad.is_err());
    }
}
