//! Field reveal: `field_reveal` (`POST /api/v1/resource/{doctype}/{id}/reveal`, WP-SEC-10.T8,
//! ADR-070 section 12 rules 2 and 9), the one place a real value of a masked or protected field
//! leaves the server through the native API.
//!
//! `spot-http` owns the route ([`RevealService`]: the body, the bounds of the reason, the
//! `no-store` answer, the one refusal) and `spot-permissions` the decision. This module is the
//! adapter the host gives the router (`host.rs`, `ApiServices::with_reveal`):
//!
//! - **who**: the site's sign-in service resolves the bearer on every call. The caller is a user
//!   of the staff or partner audience (a consumer, a session still restricted to choosing a
//!   password and another site's request are refused), acting as themselves: the right is
//!   decided for **that user**, never for a system principal;
//! - **what can be revealed**: one field of one document, and only a field that has something to
//!   reveal: one in a protection group or with a mask, read from the metadata the site serves
//!   *now*. A plain field, a field of a child-table path, an unknown field and an unknown DocType
//!   have nothing to reveal and are refused exactly like a caller without the right;
//! - **the right**: the permission engine's `field_access` for that user, DocType, field and the
//!   document's own facts (owner, state, scope values): `can_reveal`. The document is read as the
//!   platform only so that the decision can see its facts, and the value never leaves this module
//!   unless the decision allowed it;
//! - **sealed and encrypted fields**: a stored value that is a `$sealed:v1:` or `$enc:v1:`
//!   envelope is not a value a node can open. With the sealed-field service on (`[services.
//!   sealed_fields]`) the reveal goes through its [`Revealer`] (the hub's audited, per-user rate
//!   limited path); without it the field is refused;
//! - **the audit**: one durable event per call, allowed or refused, committed **before** the value
//!   is returned (`field_reveal/audit.rs`); a reveal that cannot be audited is not released. The
//!   event carries the caller's reason verbatim (events contract 0.7.9; bounded, untrusted text,
//!   never scanned for a value, never in a log line) and is validated against the registered
//!   schema first: a payload the schema refuses fails the reveal closed;
//! - **never cached**: nothing here keeps a value. It is held in memory for the length of the
//!   call, put in no log line, no metric label, no error and no audit record, and the route
//!   answers `Cache-Control: no-store` and ignores `Idempotency-Key`.
//!
//! Every refusal is the one error `permission.reveal_denied` (the router also folds the codes
//! `permission.denied`, `permission.field_denied`, `document.not_found`, `meta.unknown_doctype`
//! and `document.unknown_field` into it). What passes through unchanged: `401` for a bad
//! credential, `429 api.rate_limited` from the sealed-field rate limit, and `503
//! server.unavailable` when the site cannot answer (no engine, nothing served, the audit cannot be
//! written).
//!
//! # Level
//!
//! Optional. The module provides the port as soon as the site carries its runtime hooks; the
//! engine, the served metadata and the sign-in service are looked up on every call (a reload swaps
//! them), and a call made while one is missing answers `503`, never a value. With `[services.
//! field_reveal] enabled = false` no port is provided and the route answers as an unserved
//! operation does.

use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_api_services::PrincipalSource;
use spot_api_services::access::doc_facts;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::error::ErrorClass;
use spot_app_api::prelude::Name;
use spot_core::{Audience, Clock};
use spot_field_guard::ProtectionSource as _;
use spot_http::api::RevealService;
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_permissions::Principal;
use spot_sealed_fields::RevealError;
use spot_storage_contract::RecordId;

use crate::Result;
use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::sealed_fields::{RevealRefusal, Revealer};
use crate::services::{CheckFuture, Idle, Level, SiteCtx};

#[path = "field_reveal/audit.rs"]
mod audit;
#[path = "field_reveal/rules.rs"]
mod rules;

/// Start order: after the sealed-field service (1070), whose `Revealer` it takes when it is on.
pub const ORDER: u32 = 1075;

/// `[services.field_reveal]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site serves `field_reveal` at all.
    pub enabled: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// What a call that can be refused answers.
type ApiResult<T> = std::result::Result<T, ApiError>;

fn refused() -> ApiError {
    ApiError::from_code(Code::PermissionRevealDenied)
}

fn unavailable() -> ApiError {
    ApiError::from_code(Code::ServerUnavailable).detail("retry_after", Json::from(5))
}

/// Why an attempt did not produce a value.
enum Failed {
    /// Nothing to reveal, or no right: the one refusal.
    Refused,
    /// Anything else the caller should see as it is.
    Api(ApiError),
}

/// The reveal service of one site. It holds the live hub, not the parts: a reload swaps the
/// engine, the metadata and the sign-in service, and the next call uses the new ones.
struct FieldReveal {
    site: String,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    audit: audit::RevealAudit,
    sealed: Option<Arc<Revealer>>,
}

impl FieldReveal {
    /// The platform's own invocation, used only to read the stored document so the decision can
    /// see its facts. The right is decided for the caller before any value is used.
    fn reader(&self, ctx: &RequestCtx) -> ApiResult<Invocation> {
        let component = Name::new("field_reveal").map_err(|_| unavailable())?;
        Ok(Invocation {
            site: ctx.site.clone(),
            actor: Actor::System { component },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }

    async fn run(
        &self,
        ctx: &RequestCtx,
        doctype: &str,
        id: RecordId,
        field: &str,
        reason: &str,
    ) -> ApiResult<Json> {
        if ctx.site.as_str() != self.site || reason.trim().is_empty() {
            return Err(refused());
        }
        let auth = self.live.auth().ok_or_else(unavailable)?;
        let who = auth.authenticate_on(ctx, PathClass::Api).await?;
        if !matches!(who.audience, Audience::Staff | Audience::Partner) || auth.is_restricted(&who)
        {
            return Err(refused());
        }
        let served = self.live.current()?;
        let principals = self.live.principals().ok_or_else(unavailable)?;
        let principal = principals.resolve(&who.user_id).await;
        let now = self.clock.now_utc();

        let protection = spot_field_guard::MetaProtection::new(&served.meta).protection(doctype);
        let facts = protection.map(|p| p.field(field));
        let revealable =
            rules::is_simple_field(field) && facts.as_ref().is_some_and(rules::is_revealable);

        let outcome = match (&facts, revealable) {
            (Some(facts), true) => {
                self.attempt(ctx, &served, principal, doctype, id, field, facts, now)
                    .await
            }
            _ => Err(Failed::Refused),
        };

        // The audit is committed before the value is handed back: allowed or refused.
        let group = rules::group_of(facts.as_ref());
        let written = self
            .audit
            .record(&audit::Entry {
                user: who.user_id,
                at: now,
                doctype,
                document: id,
                group: &group,
                field,
                authorised: outcome.is_ok(),
                reason,
            })
            .await;
        if let Err(error) = written {
            // Nothing is released that could not be audited.
            drop(outcome);
            return Err(error);
        }
        match outcome {
            Ok(value) => Ok(value),
            Err(Failed::Refused) => Err(refused()),
            Err(Failed::Api(error)) => Err(error),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn attempt(
        &self,
        ctx: &RequestCtx,
        served: &crate::live::Served,
        principal: Principal,
        doctype: &str,
        id: RecordId,
        field: &str,
        facts: &spot_permissions::FieldFacts,
        now: time::OffsetDateTime,
    ) -> std::result::Result<Json, Failed> {
        let engine = self
            .live
            .engine()
            .ok_or_else(|| Failed::Api(unavailable()))?;
        let inv = self.reader(ctx).map_err(Failed::Api)?;
        let loaded = match engine.load(&inv, doctype, id).await {
            Ok(loaded) => loaded.value,
            Err(error) if error.class == ErrorClass::NotFound => return Err(Failed::Refused),
            Err(_) => return Err(Failed::Api(unavailable())),
        };
        // The right of this user on this field of this document.
        let document = doc_facts(&loaded, Some(&id.to_string()));
        let decision = served
            .decision
            .field_access(&principal, doctype, facts, &document);
        if !decision.can_reveal {
            return Err(Failed::Refused);
        }
        let stored = loaded
            .fields()
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .map(|(_, value)| value.clone());
        match stored {
            None => Ok(Json::Null),
            Some(spot_storage_contract::Value::Text(text)) if rules::is_envelope(&text) => {
                self.open_envelope(principal, doctype, id, field, now).await
            }
            Some(value) => rules::encode(&value).ok_or_else(|| Failed::Api(unavailable())),
        }
    }

    /// A sealed or encrypted value is opened by the hub, through the sealed-field service's
    /// audited and rate-limited path. Without the service the field is refused.
    async fn open_envelope(
        &self,
        principal: Principal,
        doctype: &str,
        id: RecordId,
        field: &str,
        now: time::OffsetDateTime,
    ) -> std::result::Result<Json, Failed> {
        let Some(revealer) = self.sealed.clone() else {
            return Err(Failed::Refused);
        };
        let micros = i64::try_from(now.unix_timestamp_nanos() / 1_000).unwrap_or(0);
        let (doctype, id, field) = (doctype.to_owned(), id.to_string(), field.to_owned());
        // The transport to the hub is synchronous and may wait: not on a request worker.
        let answered = tokio::task::spawn_blocking(move || {
            revealer
                .reveal(&principal, &doctype, &id, &field, micros)
                .map(|revealed| String::from_utf8(revealed.expose().to_vec()))
        })
        .await;
        match answered {
            Ok(Ok(Ok(text))) => Ok(Json::String(text)),
            Ok(Ok(Err(_))) | Err(_) => Err(Failed::Api(unavailable())),
            Ok(Err(RevealRefusal::RateLimited { retry_after_secs })) => Err(Failed::Api(
                ApiError::from_code(Code::ApiRateLimited)
                    .detail("retry_after", Json::from(retry_after_secs.max(1))),
            )),
            Ok(Err(RevealRefusal::Reveal(
                RevealError::Denied
                | RevealError::NodeRevoked { .. }
                | RevealError::BulkDenied
                | RevealError::GroupUndeclared { .. },
            ))) => Err(Failed::Refused),
            Ok(Err(RevealRefusal::Reveal(_))) => Err(Failed::Api(unavailable())),
        }
    }
}

impl RevealService for FieldReveal {
    fn reveal<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        field: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, std::result::Result<Json, ApiError>> {
        Box::pin(async move { self.run(ctx, doctype, id, field, reason).await })
    }
}

/// Registers the reveal service: provides `dyn RevealService` for the host to hand to the router.
///
/// # Errors
///
/// A setting that does not decode, or a context that carries no site.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("field_reveal")?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.to_string();
    let live = Arc::clone(env.live);
    let service = FieldReveal {
        site,
        live: Arc::clone(&live),
        clock: Arc::clone(ctx.clock()),
        audit: audit::RevealAudit::new(
            env.site.site.to_string(),
            env.backend.clone(),
            env.permission.catalog.clone(),
        ),
        sealed: ctx.port::<Revealer>(),
    };
    if ctx.assembling() {
        let probe = Arc::clone(&live);
        ctx.add(Idle::new(
            "field_reveal",
            Level::Optional,
            move || -> CheckFuture<'static> {
                let live = Arc::clone(&probe);
                Box::pin(async move {
                    if live.auth().is_some() {
                        Ok(())
                    } else {
                        Err(
                            "the sign-in service is not available, so a reveal cannot be decided"
                                .to_owned(),
                        )
                    }
                })
            },
        ));
    }
    let port: Arc<dyn RevealService> = Arc::new(service);
    ctx.provide::<dyn RevealService>(port)?;
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

    fn ctx_with(settings: &[(&str, &str)]) -> SiteCtx<'static> {
        let clock: Arc<dyn Clock> = Arc::new(spot_core::ManualClock::new(
            time::OffsetDateTime::UNIX_EPOCH,
        ));
        let settings = settings
            .iter()
            .map(|(name, text)| {
                (
                    (*name).to_owned(),
                    toml::from_str::<toml::Value>(text).unwrap(),
                )
            })
            .collect();
        SiteCtx::new(
            "acme",
            std::time::Duration::from_secs(1),
            clock,
            settings,
            true,
        )
    }

    #[test]
    fn a_context_without_a_site_provides_no_port() {
        // No site means no engine, no metadata and no sign-in service to decide with: the module
        // refuses to register and the router is never given a reveal service.
        let mut ctx = ctx_with(&[]);
        assert!(register(&mut ctx).is_err());
        assert!(ctx.port::<dyn RevealService>().is_none());
    }

    #[test]
    fn a_disabled_module_provides_no_port() {
        let mut ctx = ctx_with(&[("field_reveal", "enabled = false")]);
        assert!(register(&mut ctx).is_ok());
        assert!(ctx.port::<dyn RevealService>().is_none());
    }

    #[test]
    fn the_refusal_is_the_one_registry_code() {
        assert_eq!(
            refused().code.as_str(),
            Code::PermissionRevealDenied.as_str()
        );
        assert_eq!(
            unavailable().code.as_str(),
            Code::ServerUnavailable.as_str()
        );
    }
}
