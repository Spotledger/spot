//! Single sign-on, hosted (WP-IAM-05.T11, ADR-070 sections 5 to 7, ADR-083).
//!
//! `spot-sso` decides: it validates states, ID tokens and assertions and turns a verified identity
//! into a sign-in decision, without I/O. This module hosts it. It implements `SsoPort` (the public
//! sign-in operations the `spot-http` routes call) over:
//!
//! - the **stores** ([`stores`]): the provider store, the pending-authorisation store and the
//!   replay store, in PostgreSQL in the site database (sealed pending records, keyed binding
//!   digests, one-statement single use);
//! - the **egress client** ([`fetch`]): discovery, JWKS, the token endpoint and SAML metadata go
//!   through the egress guard, HTTPS only, 5 s, 256 KiB, no redirects, a bounded key cache with a
//!   refetch limit;
//! - the **coordinator throttle** of WP-IAM-11.T23: every attempt is admitted first, charged to
//!   the provider and the source;
//! - the **ports** of [`ports`]: the directory and session (`SsoIdentityPort`), the client secret
//!   by name (`SsoSecrets`), the audit (`SsoAudit`), SAML providers and the logout handler.
//!
//! The operations are in [`flow`].
//!
//! # Level and failure
//!
//! **Optional.** Without a provider of the identity port, with an unreadable store or with a
//! provider whose issuer or URLs do not validate, the service reports itself degraded (by
//! provider id) and every sign-in through it is refused: nothing is allowed on an error. The site
//! keeps serving passwords and API keys, and the other providers keep working.
//!
//! # Production refuses memory
//!
//! `store = "postgres"` is the default. `store = "memory"` is accepted **only** in the
//! `single-node` profile; anywhere else the site does not start (`sso_host.memory_store_refused`),
//! because an in-memory replay store forgets an assertion at a restart and a pending store loses
//! the sign-ins in flight.
//!
//! # Config (`[services.sso]`, unknown keys refused)
//!
//! `enabled` (default `false`), `store` (`postgres` | `memory`), `fetch_timeout_secs` (5),
//! `max_fetch_bytes` (262144), `jwks_max_age_secs` (3600), `jwks_refetch_min_secs` (60),
//! `jwks_max_keys` (32) and `[[services.sso.private_providers]]` (`host`, `addresses`): an identity
//! provider on a private network is reachable only when the site names its host and addresses.

#[path = "sso/fetch.rs"]
mod fetch;
#[path = "sso/flow.rs"]
mod flow;
#[path = "sso/ports.rs"]
mod ports;
#[path = "sso/stores.rs"]
mod stores;
#[cfg(test)]
#[path = "sso/tests.rs"]
mod tests;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::Method;
use serde::Deserialize;
use spot_core::Clock;
use spot_egress::{
    AllowEntry, EgressConnector, EgressPolicy, Guard, HttpSender, InMemorySink, Purpose, Sender,
    SystemResolver, TenantPolicy,
};
use spot_http::api::Ports;
use spot_http::api::ports::sso::{
    AcsParams, Begin, CallbackParams, Handover, ProviderList, SignedIn, SloOutcome, SloParams,
    SsoFailure, SsoPort,
};
use spot_http::api::router::routes::{self, Registration};
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_sso::saml::SamlPendingAuth;
use spot_sso::{
    CompleteError, Discovery, IdpConfig, KeyCache, PendingAuth, ProviderConfig, ProviderId,
    ReplayStore, SamlSignIn, SpConfig, SsoError, SsoIdentity, TokenResponse,
};

use crate::services::secrets::SecretsHost;
use crate::services::signin_throttle::{Door, SigninThrottle};
use crate::services::{CheckFuture, Idle, Level, OpenPath, RateLimit, SiteCtx};
use crate::{Error, Result};

/// Raised in this module's messages as `sso_host.memory_store_refused: ...`.
#[allow(dead_code)]
const CODE_SSO_HOST_MEMORY_STORE_REFUSED: &str = "sso_host.memory_store_refused";
/// Raised in this module's messages as `sso_host.settings_invalid: ...`.
#[allow(dead_code)]
const CODE_SSO_HOST_SETTINGS_INVALID: &str = "sso_host.settings_invalid";
/// Raised in this module's messages as `sso_host.stores_unavailable: ...`.
#[allow(dead_code)]
const CODE_SSO_HOST_STORES_UNAVAILABLE: &str = "sso_host.stores_unavailable";

pub use fetch::{CacheLimits, EgressFetcher, FetchError, FetchLimits, Fetcher, ProviderCache};
pub use ports::{
    AuditUnavailable, Collaborators, DirectorySnapshot, IdentityFailure, NoSamlProviders,
    SamlProvider, SamlProviders, SignInRequest, SsoAudit, SsoAuditEvent, SsoIdentityPort,
    SsoLogoutHandler, SsoSecrets, TracingAudit,
};
pub use stores::{StoreKind, Stores, check_store_choice};

/// Start order: after the secrets service (1000), the sign-in stores (1005) and the sign-in
/// throttle (1006); `sso_logout` and the sign-in policy come after.
pub const ORDER: u32 = 1010;

/// A provider on a private network the site allows explicitly.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateProvider {
    /// The provider's host name.
    pub host: String,
    /// The addresses (or CIDR ranges) it may resolve to: nothing else on that host is reachable.
    pub addresses: Vec<String>,
}

/// `[services.sso]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module serves single sign-on.
    pub enabled: bool,
    /// `postgres` (the default) or `memory` (single-node profile only).
    pub store: String,
    /// The timeout of one outbound exchange, in seconds.
    pub fetch_timeout_secs: u64,
    /// The most response bytes read from an identity provider.
    pub max_fetch_bytes: u64,
    /// How long cached discovery and keys are used, in seconds.
    pub jwks_max_age_secs: i64,
    /// The shortest time between two forced key refetches of one provider, in seconds.
    pub jwks_refetch_min_secs: i64,
    /// The most keys one provider may publish.
    pub jwks_max_keys: usize,
    /// Identity providers on private networks, named explicitly.
    pub private_providers: Vec<PrivateProvider>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            store: "postgres".to_owned(),
            fetch_timeout_secs: 5,
            max_fetch_bytes: 256 * 1024,
            jwks_max_age_secs: 3600,
            jwks_refetch_min_secs: 60,
            jwks_max_keys: 32,
            private_providers: Vec::new(),
        }
    }
}

impl Settings {
    /// Checks the ranges.
    ///
    /// # Errors
    ///
    /// `sso_host.settings_invalid` naming the key.
    pub fn validate(&self) -> Result<()> {
        let bad = |key: &str| {
            Err(Error(format!(
                "sso_host.settings_invalid: services.sso.{key} is out of range"
            )))
        };
        if !(1..=30).contains(&self.fetch_timeout_secs) {
            return bad("fetch_timeout_secs");
        }
        if !(1024..=1_048_576).contains(&self.max_fetch_bytes) {
            return bad("max_fetch_bytes");
        }
        if !(60..=86_400).contains(&self.jwks_max_age_secs) {
            return bad("jwks_max_age_secs");
        }
        if !(1..=3600).contains(&self.jwks_refetch_min_secs) {
            return bad("jwks_refetch_min_secs");
        }
        if !(1..=256).contains(&self.jwks_max_keys) {
            return bad("jwks_max_keys");
        }
        if self
            .private_providers
            .iter()
            .any(|p| p.host.trim().is_empty() || p.addresses.is_empty())
        {
            return bad("private_providers");
        }
        StoreKind::parse(&self.store).map(|_| ())
    }

    fn cache_limits(&self) -> CacheLimits {
        CacheLimits {
            max_entries: 64,
            max_keys: self.jwks_max_keys,
            max_age_seconds: self.jwks_max_age_secs,
            refetch_min_seconds: self.jwks_refetch_min_secs,
        }
    }

    fn fetch_limits(&self) -> FetchLimits {
        FetchLimits {
            timeout: Duration::from_secs(self.fetch_timeout_secs),
            max_bytes: self.max_fetch_bytes,
        }
    }
}

/// Admits one sign-in attempt. The production implementation is the site's coordinator throttle.
pub trait SsoAdmission: Send + Sync + 'static {
    /// Counts the attempt and says whether it may go on; an `Err` is a refusal (a limit, or an
    /// outage: fail closed).
    fn admit<'a>(
        &'a self,
        tenant: &'a str,
        account: &'a str,
        peer: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<(), ApiError>>;
}

/// The throttle handle of the site, as the SSO door.
struct ThrottleAdmission(Arc<SigninThrottle>);

impl SsoAdmission for ThrottleAdmission {
    fn admit<'a>(
        &'a self,
        tenant: &'a str,
        account: &'a str,
        peer: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<(), ApiError>> {
        Box::pin(async move {
            self.0
                .admit(Door::SsoCallback, tenant, account, peer, None)
                .await
        })
    }
}

/// The checks `spot-sso` runs, as a seam so a unit test does not need a signing key. The
/// production implementation calls the library unchanged: the host does not weaken it.
pub trait Verifier: Send + Sync + 'static {
    /// Validates an ID token (`spot_sso::complete`).
    fn oidc(
        &self,
        config: &ProviderConfig,
        discovery: &Discovery,
        keys: &KeyCache,
        pending: &PendingAuth,
        tokens: &TokenResponse,
        now: i64,
    ) -> std::result::Result<SsoIdentity, CompleteError>;

    /// Validates a SAML response (`spot_sso::saml::complete`).
    #[allow(clippy::too_many_arguments)]
    fn saml<'a>(
        &'a self,
        sp: &'a SpConfig,
        idp: &'a IdpConfig,
        pending: &'a SamlPendingAuth,
        response: &'a str,
        relay_state: Option<&'a str>,
        now: i64,
        replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, std::result::Result<SamlSignIn, SsoError>>;
}

/// The library's own checks.
#[derive(Clone, Copy, Debug, Default)]
pub struct LibraryVerifier;

impl Verifier for LibraryVerifier {
    fn oidc(
        &self,
        config: &ProviderConfig,
        discovery: &Discovery,
        keys: &KeyCache,
        pending: &PendingAuth,
        tokens: &TokenResponse,
        now: i64,
    ) -> std::result::Result<SsoIdentity, CompleteError> {
        spot_sso::complete(config, discovery, keys, pending, tokens, now)
    }

    fn saml<'a>(
        &'a self,
        sp: &'a SpConfig,
        idp: &'a IdpConfig,
        pending: &'a SamlPendingAuth,
        response: &'a str,
        relay_state: Option<&'a str>,
        now: i64,
        replay: &'a Arc<dyn ReplayStore>,
    ) -> BoxFuture<'a, std::result::Result<SamlSignIn, SsoError>> {
        Box::pin(async move {
            spot_sso::saml::complete(sp, idp, pending, response, relay_state, now, replay).await
        })
    }
}

/// The provider a request named.
#[derive(Debug)]
pub enum Kind {
    /// An OpenID Connect provider (from the provider store).
    Oidc(Box<ProviderConfig>),
    /// A SAML provider (from the `SamlProviders` port).
    Saml(Box<SamlProvider>),
}

impl Kind {
    /// The provider's id.
    #[must_use]
    pub fn id(&self) -> ProviderId {
        match self {
            Self::Oidc(config) => config.id.clone(),
            Self::Saml(saml) => saml.idp.provider.clone(),
        }
    }
}

/// One site's single sign-on host: the `SsoPort` the routes call.
pub struct SsoHost {
    site: String,
    clock: Arc<dyn Clock>,
    stores: Stores,
    fetcher: Arc<dyn Fetcher>,
    cache: ProviderCache,
    collab: Collaborators,
    admission: Arc<dyn SsoAdmission>,
    verifier: Arc<dyn Verifier>,
    logout: OnceLock<Arc<dyn SsoLogoutHandler>>,
}

impl SsoHost {
    /// A host for `site` over its parts.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        site: impl Into<String>,
        clock: Arc<dyn Clock>,
        stores: Stores,
        fetcher: Arc<dyn Fetcher>,
        cache: CacheLimits,
        collab: Collaborators,
        admission: Arc<dyn SsoAdmission>,
        verifier: Arc<dyn Verifier>,
    ) -> Self {
        Self {
            site: site.into(),
            clock,
            stores,
            fetcher,
            cache: ProviderCache::new(cache),
            collab,
            admission,
            verifier,
            logout: OnceLock::new(),
        }
    }

    /// Installs the logout handler (`sso_logout` does it at its own start). Only the first call
    /// takes effect; until one does, a logout request is rejected with nothing ended.
    pub fn install_logout(&self, handler: Arc<dyn SsoLogoutHandler>) -> bool {
        self.logout.set(handler).is_ok()
    }

    /// Whether a forced refetch of `provider`'s keys is allowed now (counted when it is): the
    /// refetch limit the sign-in and the logout receivers share.
    pub fn may_refetch(&self, provider: &str) -> bool {
        self.cache.allow_refetch(provider, self.now())
    }

    /// The collaborators (audit sink and the rest), for the logout handler.
    #[must_use]
    pub fn collaborators(&self) -> &Collaborators {
        &self.collab
    }

    /// The stores, for the logout handler (the replay store of `jti` and request ids).
    #[must_use]
    pub fn stores(&self) -> &Stores {
        &self.stores
    }

    /// The injected clock.
    #[must_use]
    pub fn clock(&self) -> &Arc<dyn Clock> {
        &self.clock
    }

    /// This site's name.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }
}

impl SsoPort for SsoHost {
    fn providers<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, std::result::Result<ProviderList, SsoFailure>> {
        Box::pin(self.do_providers(ctx))
    }

    fn begin<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        return_to: Option<&'a str>,
    ) -> BoxFuture<'a, std::result::Result<Begin, SsoFailure>> {
        Box::pin(self.do_begin(ctx, provider, return_to))
    }

    fn callback<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a CallbackParams,
    ) -> BoxFuture<'a, std::result::Result<Handover, SsoFailure>> {
        Box::pin(self.do_callback(ctx, provider, params))
    }

    fn acs<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a AcsParams,
    ) -> BoxFuture<'a, std::result::Result<Handover, SsoFailure>> {
        Box::pin(self.do_acs(ctx, provider, params))
    }

    fn complete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        handover: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SignedIn, SsoFailure>> {
        Box::pin(self.do_complete(ctx, handover))
    }

    fn backchannel_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        logout_token: &'a str,
    ) -> BoxFuture<'a, std::result::Result<(), SsoFailure>> {
        match self.logout.get() {
            Some(handler) => handler.backchannel_logout(ctx, provider, logout_token),
            None => Box::pin(async { Err(SsoFailure::LogoutRejected) }),
        }
    }

    fn single_logout<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
        params: &'a SloParams,
    ) -> BoxFuture<'a, std::result::Result<SloOutcome, SsoFailure>> {
        match self.logout.get() {
            Some(handler) => handler.single_logout(ctx, provider, params),
            None => Box::pin(async { Err(SsoFailure::LogoutRejected) }),
        }
    }

    fn saml_metadata<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        provider: &'a str,
    ) -> BoxFuture<'a, std::result::Result<String, SsoFailure>> {
        Box::pin(self.do_metadata(ctx, provider))
    }
}

/// The egress of one site: the `connector` purpose, the public internet by default, and only
/// the private hosts the site named (with only the addresses it named).
fn build_fetcher(site: &str, settings: &Settings, clock: Arc<dyn Clock>) -> Arc<dyn Fetcher> {
    let mut policy = EgressPolicy::default();
    if !settings.private_providers.is_empty() {
        let allow = settings
            .private_providers
            .iter()
            .map(|p| AllowEntry {
                scheme: Some("https".to_owned()),
                host: p.host.clone(),
                ports: None,
                purpose: Some(Purpose::Connector.as_str().to_owned()),
                allow_private: Some(p.addresses.clone()),
            })
            .collect();
        policy.tenants.insert(
            site.to_owned(),
            TenantPolicy {
                allow,
                ..TenantPolicy::default()
            },
        );
    }
    let guard = Arc::new(Guard::new(
        policy,
        Arc::new(SystemResolver),
        Arc::new(InMemorySink::new()),
        clock,
    ));
    let limits = settings.fetch_limits();
    let sender: Arc<dyn Sender> = Arc::new(HttpSender::new(
        EgressConnector::new(Arc::clone(&guard), site.to_owned(), Purpose::Connector),
        limits.timeout,
    ));
    Arc::new(EgressFetcher::new(guard, sender, limits))
}

/// The open paths of the sign-in operations (no session exists yet). The logout receivers declare
/// their own in `sso_logout`.
fn open_paths() -> Vec<OpenPath> {
    let limit = RateLimit { per_minute: 600 };
    [
        (Method::GET, "/api/v1/auth/sso/providers"),
        (Method::POST, "/api/v1/auth/sso/{provider}/begin"),
        (Method::GET, "/api/v1/auth/sso/{provider}/callback"),
        (Method::POST, "/api/v1/auth/sso/{provider}/acs"),
        (Method::POST, "/api/v1/auth/sso/complete"),
        (Method::GET, "/api/v1/auth/sso/{provider}/saml-metadata"),
    ]
    .into_iter()
    .map(|(method, pattern)| OpenPath {
        method,
        pattern,
        limit,
    })
    .collect()
}

/// Joins the site: builds the host over the durable stores and the egress client, provides
/// `dyn SsoPort` and the concrete [`SsoHost`], declares the open paths and mounts the routes.
///
/// # Errors
///
/// A setting out of range, a memory store outside the single-node profile, a missing secrets or
/// throttle module, or a store that cannot be prepared: the site does not start without durable
/// sign-in state.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("sso")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    let kind = StoreKind::parse(&settings.store)?;
    let (site, backend, profile) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            env.backend.clone(),
            env.loaded.common.services.profile.clone(),
        )
    };
    check_store_choice(kind, &profile)?;
    let stores = match kind {
        StoreKind::Postgres => {
            let secrets = ctx.port::<SecretsHost>().ok_or_else(|| {
                Error("sso_host.stores_unavailable: [services.sso] needs [services.secrets] for the sealing keys".into())
            })?;
            stores::postgres(&backend, &site, secrets.keyring()).await?
        }
        StoreKind::Memory => stores::memory(&site),
    };
    let throttle = ctx.require_port::<SigninThrottle>()?;
    let collab = Collaborators {
        identity: ctx.port::<dyn SsoIdentityPort>(),
        secrets: ctx.port::<dyn SsoSecrets>(),
        audit: ctx
            .port::<dyn SsoAudit>()
            .unwrap_or_else(|| Arc::new(TracingAudit)),
        saml: ctx
            .port::<dyn SamlProviders>()
            .unwrap_or_else(|| Arc::new(NoSamlProviders)),
    };
    let has_identity = collab.identity.is_some();
    let host = Arc::new(SsoHost::new(
        site.clone(),
        Arc::clone(ctx.clock()),
        stores,
        build_fetcher(&site, &settings, Arc::clone(ctx.clock())),
        settings.cache_limits(),
        collab,
        Arc::new(ThrottleAdmission(throttle)),
        Arc::new(LibraryVerifier),
    ));

    if ctx.assembling() {
        let probe = Arc::clone(&host);
        ctx.add(Idle::new("sso", Level::Optional, move || -> CheckFuture<'static> {
            let probe = Arc::clone(&probe);
            Box::pin(async move {
                if !has_identity {
                    return Err("no module provides the sign-in directory and session (SsoIdentityPort), so every single sign-on attempt is refused".to_owned());
                }
                match probe.bad_providers().await {
                    Err(_) => Err("the provider store cannot be read, so every single sign-on attempt is refused".to_owned()),
                    Ok(bad) if bad.is_empty() => Ok(()),
                    Ok(bad) => Err(format!(
                        "these identity providers do not validate and are not served: {}",
                        bad.join(", ")
                    )),
                }
            })
        }));
    }

    ctx.open_paths(&open_paths())?;
    let port: Arc<dyn SsoPort> = host.clone();
    ctx.provide(port.clone())?;
    ctx.provide(host)?;
    let mut ports = Ports::new();
    ports.provide::<dyn SsoPort>(port);
    let mounted = routes::mount(&[sso_registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        return Err(Error(format!(
            "the single sign-on routes were not mounted: {module}: {reason:?}"
        )));
    }
    let mut router = mounted.authenticated;
    for (_, public) in mounted.public {
        router = router.merge(public);
    }
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

/// The route group of the public sign-in operations, registered by hand: `spot-http` mounts only
/// the groups it is given ports for, and the host gives the router its ports one module at a time.
fn sso_registration() -> Registration {
    Registration {
        name: "sso",
        serves: routes::sso::SERVES,
        required_ports: routes::sso::required_ports,
        routes: routes::sso::routes,
        public: Some(routes::sso::PUBLIC),
    }
}
