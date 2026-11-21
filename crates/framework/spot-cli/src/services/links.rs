//! Tenant links, hosted (WP-CTL-09.T8, ADR-058): the tenant host's link service, optional and
//! **off by default**.
//!
//! `spot-tenant-link` is the handshake state machine and `spot-tenant-link-pg` the tenant's link
//! store and the administration operations over it (`LinkAdmin`: propose, accept, reject, grant,
//! revoke, list, show, each behind the `link.manage` right and audited before it acts). This
//! module hosts them: it reads `[services.links]`, builds the tenant's [`PgLinkStore`], and
//! provides a [`LinksPort`] to the modules that start after it (the native API's link operations
//! mount through the route registry as a ports group once the contract lists them).
//!
//! # Consent rules the host keeps
//!
//! - A link carries no data without both sides' consent: activation needs the two consents of
//!   distinct parties and actors, each re-checked through the step-up gate on every use. The host
//!   cannot weaken that: the operations are the library's.
//! - The grants list is the only filter on what crosses; the default is none.
//! - Revoking by either side ends exchange for new messages at once ([`LinksPort::authorize`]).
//! - Peer identity keys are pinned by the handshake (`PeerKeyDirectory`), never taken from a
//!   message; a changed key is a refusal until an administrator confirms the new one again.
//!
//! # Ports it needs
//!
//! The audit chain adapter, the tenant's step-up gate and the `link.manage` check are other
//! modules' to provide (they own the identity and audit services). They arrive together as a
//! [`LinksPorts`] value in the site's ports map; when `[services.links]` is enabled and no module
//! provided it, the site refuses to start and says so, rather than running links without audit or
//! consent checks.

use std::sync::Arc;

use serde::Deserialize;
use spot_tenancy::TenantId;
use spot_tenant_link::{AuditSink, Link, LinkId, RevokeReason, StepUpGate};
use spot_tenant_link_pg::{Caller, LinkAdmin, LinkPermissions, OpError, PgLinkStore};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// Start order: after the identity and audit modules that provide the ports.
pub const ORDER: u32 = 1400;

/// `[services.links]`. Unknown keys are refused.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs. Off by default: a site without links has no link tables touched.
    pub enabled: bool,
    /// The tenant this site's links belong to: a UUIDv7.
    pub tenant: String,
}

/// What the link operations need from the rest of the host. Provide one with
/// `SiteCtx::provide::<LinksPorts>` from the module that owns the identity and audit services.
pub struct LinksPorts {
    /// The audit chain adapter.
    pub audit: Arc<dyn AuditSink>,
    /// The tenant's step-up gate for consents.
    pub gate: Arc<dyn StepUpGate + Send + Sync>,
    /// The `link.manage` check.
    pub permissions: Arc<dyn LinkPermissions>,
}

/// The link operations other modules (the native API's link ports group) call.
pub trait LinksPort: Send + Sync {
    /// Every link of this tenant.
    ///
    /// # Errors
    /// [`OpError`] (permission, store).
    fn list(&self, caller: &Caller, now: u64) -> std::result::Result<Vec<Link>, OpError>;

    /// One link.
    ///
    /// # Errors
    /// [`OpError`].
    fn show(&self, caller: &Caller, id: &LinkId, now: u64) -> std::result::Result<Link, OpError>;

    /// Revokes a link: exchange stops for new messages at once.
    ///
    /// # Errors
    /// [`OpError`].
    fn revoke(
        &self,
        caller: &Caller,
        id: &LinkId,
        reason: RevokeReason,
        now: u64,
    ) -> std::result::Result<Link, OpError>;

    /// The tenant's [`PgLinkStore`], for the handshake driver and the exchange.
    fn store(&self) -> Arc<PgLinkStore>;
}

/// The hosted link service.
pub struct LinkHost {
    store: Arc<PgLinkStore>,
    ports: LinksPorts,
}

impl LinkHost {
    /// A host over a store and its ports.
    #[must_use]
    pub fn new(store: Arc<PgLinkStore>, ports: LinksPorts) -> Self {
        Self { store, ports }
    }

    fn admin(&self) -> LinkAdmin<'_> {
        LinkAdmin::new(
            &self.store,
            &*self.ports.audit,
            &*self.ports.gate,
            &*self.ports.permissions,
        )
    }
}

impl LinksPort for LinkHost {
    fn list(&self, caller: &Caller, now: u64) -> std::result::Result<Vec<Link>, OpError> {
        self.admin().list(caller, now)
    }

    fn show(&self, caller: &Caller, id: &LinkId, now: u64) -> std::result::Result<Link, OpError> {
        self.admin().show(caller, id, now)
    }

    fn revoke(
        &self,
        caller: &Caller,
        id: &LinkId,
        reason: RevokeReason,
        now: u64,
    ) -> std::result::Result<Link, OpError> {
        self.admin().revoke(caller, id, reason, now)
    }

    fn store(&self) -> Arc<PgLinkStore> {
        Arc::clone(&self.store)
    }
}

/// The service in the site's assembly: its check prepares the tenant's link tables.
struct LinksService {
    store: Arc<PgLinkStore>,
}

impl Service for LinksService {
    fn name(&self) -> &'static str {
        "links"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.store
                .prepare()
                .await
                .map_err(|_| "the link tables could not be prepared".to_owned())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}

/// Parses the tenant id of the settings.
///
/// # Errors
/// A message naming the setting, never its value.
pub fn parse_tenant(settings: &Settings) -> Result<TenantId> {
    let uuid = uuid::Uuid::parse_str(&settings.tenant)
        .map_err(|_| Error("services.links.tenant is not a UUID".to_owned()))?;
    TenantId::from_uuid(uuid)
        .map_err(|_| Error("services.links.tenant must be a UUIDv7".to_owned()))
}

/// Registers the service when `[services.links]` is enabled.
///
/// # Errors
/// A setting that does not decode, a tenant that is not a UUIDv7, a missing [`LinksPorts`] or a
/// runtime that cannot start.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("links")?;
    if !ctx.assembling() || !settings.enabled {
        return Ok(());
    }
    let tenant = parse_tenant(&settings)?;
    let ports = ctx.require_port::<LinksPorts>()?;
    let pool = ctx.env()?.backend.pool();
    // The store's synchronous methods block on this runtime; it is the store's own, so a call from
    // a request thread never blocks the server's runtime.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|_| Error("the link store runtime could not start".to_owned()))?;
    let store = Arc::new(PgLinkStore::new(pool, Arc::new(runtime), tenant));
    let host = LinkHost::new(
        Arc::clone(&store),
        LinksPorts {
            audit: Arc::clone(&ports.audit),
            gate: Arc::clone(&ports.gate),
            permissions: Arc::clone(&ports.permissions),
        },
    );
    ctx.provide::<dyn LinksPort>(Arc::new(host))?;
    ctx.add(Arc::new(LinksService { store }));
    Ok(())
}
