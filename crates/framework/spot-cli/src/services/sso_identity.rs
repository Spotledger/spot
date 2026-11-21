//! The production providers behind the single sign-on service (WP-IAM-05.T16).
//!
//! `services/sso.rs` (T11), `services/signin_policy.rs` (T12) and `services/sso_logout.rs` (T13)
//! move bytes and decide nothing about the tenant's people: they hand every verified identity to
//! ports. This module is the one that provides them, so a running `spot serve` completes a real
//! sign-in. One module, one start order (**1007**: after the sign-in stores 1005 and the throttle
//! 1006, before the policy port 1008, the host 1010 and the logout receivers 1020, which all look
//! their ports up when they register):
//!
//! | Port | Where | What |
//! | --- | --- | --- |
//! | `SsoIdentityPort` | [`port`] over [`directory`], [`apply`], [`handover`] | the tenant directory, linking, just-in-time users, the one-time hand-over and the session |
//! | `SsoSecrets` | `sso_secrets.rs` | a client secret by name, from the operator's environment or a protected file |
//! | `SamlProviders` | `sso_saml_store.rs` | the tenant's SAML providers (its own table) |
//! | `PolicySource` | `sso_policy.rs` | the tenant sign-in policy (its own table, compare-and-set) |
//! | `SsoAdminPort` | `sso_admin.rs` | provider (OpenID Connect and SAML from fetched metadata) and policy administration, step-up, audit |
//! | `LogoutDirectory`, `SessionEnder` | `sso_logout_ports.rs` | linked identities, the IdP `sid` index, ending sessions |
//!
//! # Rules this module keeps
//!
//! - **Linking is an account takeover path.** An identity is linked only through an already linked
//!   `(issuer, subject)`, or through a **pending, unexpired invitation** whose address the provider
//!   verified. A match on the e-mail address alone never links; it is refused
//!   (`sso.link_requires_proof`) by the host before this module is called.
//! - **A just-in-time user has no role.** It is created by a rule of the tenant's policy, in the
//!   state `active`, with a linked identity and no assignment at all.
//! - **One-time hand-over.** The browser carries a random value for 60 seconds; its digest, not the
//!   value, is stored, and redeeming it deletes it in the same statement that reads it, so of two
//!   parallel redemptions exactly one succeeds.
//! - **The session says how it began**: `method` is `oidc` or `saml` (never `password`) and the
//!   audience is the provider's. A session of this kind records the IdP `sid`, when the provider
//!   gave one, in an index that single logout uses; API keys are not sessions and are never ended.
//! - **Secrets never leave the reader.** No client secret, token, assertion, hand-over value or
//!   password is in an error, a log line, a metric label or an audit record.
//! - **Fail closed.** A store that cannot answer refuses the sign-in; a policy that cannot be read
//!   is a refusal, never `local`.
//!
//! # Level and settings
//!
//! Optional. `[services.sso_identity]`: `enabled` (**false**: single sign-on is opt-in),
//! `handover_ttl_secs` (60, 10 to 300), `base_url` (the site's public `https://` origin, used when
//! the administration API creates a provider), `client_secrets` (a table of secret name to
//! `env:NAME` or `file:/absolute/path`; a file must not be readable by group or others).

use std::sync::Arc;

use serde::Deserialize;
use spot_http::api::Ports;
use spot_http::api::router::routes::{self, Registration};
use spot_storage_contract::{
    Expr, Name, Query, QueryPath, RecordId, Selection, TextMatch, TextMode, Transaction,
};

use crate::login::SiteStorage;
use crate::services::scim_store::{idv, nm};
use crate::services::signin_policy::PolicySource;
use crate::services::sso::{SamlProviders, SsoIdentityPort, SsoSecrets};
use crate::services::sso_logout::{LogoutDirectory, SessionEnder};
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Raised in this module's messages as `sso_providers_host.migration_failed: ...`.
#[allow(dead_code)]
const CODE_SSO_PROVIDERS_HOST_MIGRATION_FAILED: &str = "sso_providers_host.migration_failed";
/// Raised in this module's messages as `sso_providers_host.no_sign_in_storage: ...`.
#[allow(dead_code)]
const CODE_SSO_PROVIDERS_HOST_NO_SIGN_IN_STORAGE: &str = "sso_providers_host.no_sign_in_storage";
/// Raised in this module's messages as `sso_providers_host.settings_invalid: ...`.
#[allow(dead_code)]
const CODE_SSO_PROVIDERS_HOST_SETTINGS_INVALID: &str = "sso_providers_host.settings_invalid";
/// Raised in this module's messages as `sso_providers_host.tenant_invalid: ...`.
#[allow(dead_code)]
const CODE_SSO_PROVIDERS_HOST_TENANT_INVALID: &str = "sso_providers_host.tenant_invalid";

#[path = "sso_identity/apply.rs"]
pub mod apply;
#[path = "sso_identity/directory.rs"]
pub mod directory;
#[path = "sso_identity/handover.rs"]
pub mod handover;
#[path = "sso_identity/port.rs"]
pub mod port;

/// Before the policy port (1008) and the host (1010), after the sign-in stores (1005, 1006).
pub const ORDER: u32 = 1007;

/// The audit target shared with the rest of identity administration.
pub const AUDIT: &str = "spot::audit::identity";

/// A store or a collaborator could not answer. No text: a storage error can name a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavail;

/// Maps any failure to [`Unavail`], dropping its text.
pub(crate) fn st<T, E>(r: std::result::Result<T, E>) -> std::result::Result<T, Unavail> {
    r.map_err(|_| Unavail)
}

/// The ids of the records of `entity` whose text fields all equal the given values (at most 50).
pub(crate) async fn eq_ids<T: Transaction>(
    tx: &mut T,
    entity: &str,
    conditions: &[(&str, &str)],
) -> std::result::Result<Vec<RecordId>, Unavail> {
    let (e, alias) = (st(nm(entity))?, st(nm("t"))?);
    let mut query = Query::from(&e, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &st(nm("id"))?),
        alias: st(nm("v"))?,
    }];
    let mut all = Vec::with_capacity(conditions.len());
    for (field, value) in conditions {
        all.push(Expr::Text(
            Box::new(Expr::field(&alias, &st(nm(field))?)),
            TextMatch {
                mode: TextMode::Equals,
                pattern: (*value).to_owned(),
                case_insensitive: false,
            },
        ));
    }
    query.filter = Some(if all.len() == 1 {
        all.remove(0)
    } else {
        Expr::And(all)
    });
    query.limit = Some(50);
    let rows = st(tx.query(&query).await)?;
    Ok(rows
        .rows
        .into_iter()
        .flatten()
        .filter_map(|v| idv(&v))
        .collect())
}

/// A field of a loaded record, `Null` when it has none.
pub(crate) fn field(
    data: &spot_storage_contract::DocumentData,
    name: &str,
) -> spot_storage_contract::Value {
    Name::new(name)
        .ok()
        .and_then(|n| data.fields.get(&n).cloned())
        .unwrap_or(spot_storage_contract::Value::Null)
}

/// `[services.sso_identity]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the providers are installed (single sign-on is opt-in).
    pub enabled: bool,
    /// How long the one-time hand-over lives, in seconds (10 to 300).
    pub handover_ttl_secs: u32,
    /// The site's public origin (`https://host[:port]`), for providers made through the API.
    pub base_url: String,
    /// Client secret names to references: `env:NAME` or `file:/absolute/path`.
    pub client_secrets: std::collections::BTreeMap<String, String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            handover_ttl_secs: 60,
            base_url: String::new(),
            client_secrets: std::collections::BTreeMap::new(),
        }
    }
}

impl Settings {
    /// Checks the ranges and the shape of every reference.
    ///
    /// # Errors
    ///
    /// `sso_providers_host.settings_invalid`.
    pub fn validate(&self) -> Result<()> {
        let bad = |what: &str| {
            Err(Error(format!(
                "sso_providers_host.settings_invalid: services.sso_identity.{what} is not valid"
            )))
        };
        if !(10..=300).contains(&self.handover_ttl_secs) {
            return bad("handover_ttl_secs");
        }
        if !self.base_url.is_empty()
            && !(self.base_url.starts_with("https://")
                && !self.base_url.ends_with('/')
                && !self.base_url.contains(['?', '#', ' ']))
        {
            return bad("base_url");
        }
        for (name, reference) in &self.client_secrets {
            if !crate::services::sso_secrets::name_ok(name)
                || crate::services::sso_secrets::Source::parse(reference).is_none()
            {
                return bad("client_secrets");
            }
        }
        Ok(())
    }
}

/// Joins the site: installs the providers when `[services.sso_identity] enabled` and provides the
/// ports of the module table above.
///
/// # Errors
///
/// A setting out of range, a missing sign-in storage, or a table that cannot be prepared: the
/// site does not start with half-installed sign-in state.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("sso_identity")?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    let (site, backend, live, catalog) = {
        let env = ctx.env()?;
        (
            env.site.site.clone(),
            env.backend.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let storage = ctx.port::<SiteStorage>().ok_or_else(|| {
        Error("sso_providers_host.no_sign_in_storage: [services.sso_identity] needs the sign-in stores (services.login_stores)".into())
    })?;
    let pool = backend.pool();
    let tables =
        crate::services::sso_identity::handover::Tables::new(pool, &site).ok_or_else(|| {
            Error("sso_providers_host.tenant_invalid: the site name is not a valid tenant".into())
        })?;
    tables.prepare().await?;
    let clock = Arc::clone(ctx.clock());

    let secrets: Arc<dyn SsoSecrets> = Arc::new(
        crate::services::sso_secrets::ConfiguredSecrets::new(&site, &settings.client_secrets),
    );
    let saml = Arc::new(
        crate::services::sso_saml_store::PgSamlStore::new(tables.pool(), &site).ok_or_else(
            || {
                Error(
                    "sso_providers_host.tenant_invalid: the site name is not a valid tenant".into(),
                )
            },
        )?,
    );
    let policy = Arc::new(
        crate::services::sso_policy::PgPolicyStore::new(tables.pool(), &site).ok_or_else(|| {
            Error("sso_providers_host.tenant_invalid: the site name is not a valid tenant".into())
        })?,
    );
    let oidc = Arc::new(
        spot_sso_pg::PgProviderStore::new(tables.pool(), &site).ok_or_else(|| {
            Error("sso_providers_host.tenant_invalid: the site name is not a valid tenant".into())
        })?,
    );
    let sessions: Arc<dyn spot_sessions::SessionStore> = Arc::new(storage.stores.session_store());
    saml.prepare().await?;
    policy.prepare().await?;
    // The administration API reads and writes the OpenID Connect provider table: it must exist
    // whether or not the sign-in host (`[services.sso]`, which prepares it too) is enabled.
    oidc.prepare().await.map_err(|_| {
        Error("sso_providers_host.stores_unavailable: the provider table could not be prepared".into())
    })?;
    let identity = Arc::new(port::SsoIdentityService::new(port::Parts {
        site: site.clone(),
        live: Arc::clone(&live),
        catalog: Arc::clone(&catalog),
        clock: Arc::clone(&clock),
        tables: tables.clone(),
        policy: Arc::clone(&policy),
        oidc: Arc::clone(&oidc) as Arc<dyn spot_sso::ProviderStore>,
        saml: Arc::clone(&saml),
        sessions: Arc::clone(&sessions),
        handover_ttl_secs: i64::from(settings.handover_ttl_secs),
    }));
    // SAML metadata is fetched through the sign-in host's egress-policed fetcher, which registers
    // later (1010): the slot is filled once every module has registered.
    let fetcher = Arc::new(crate::services::sso_admin::FetcherSlot::new());
    let admin = Arc::new(crate::services::sso_admin::SsoAdmin::new(
        crate::services::sso_admin::Parts {
            site: site.clone(),
            live: Arc::clone(&live),
            catalog: Arc::clone(&catalog),
            clock: Arc::clone(&clock),
            policy: Arc::clone(&policy),
            oidc: Arc::clone(&oidc) as Arc<dyn spot_sso::ProviderStore>,
            saml: Arc::clone(&saml),
            identity: Arc::clone(&identity),
            base_url: settings.base_url.clone(),
            secret_names: settings.client_secrets.keys().cloned().collect(),
            fetcher: Arc::clone(&fetcher),
        },
    ));
    ctx.finally(move |ctx| {
        if let Some(found) = ctx.port::<dyn crate::services::sso::Fetcher>() {
            let _ = fetcher.set(found);
        }
        Ok(())
    });
    let logout = Arc::new(crate::services::sso_logout_ports::LogoutPorts::new(
        crate::services::sso_logout_ports::Parts {
            site: site.clone(),
            live,
            catalog,
            sessions,
            tables,
        },
    ));

    if ctx.assembling() {
        let configured = !settings.base_url.is_empty();
        ctx.add(Idle::new(
            "sso_identity",
            Level::Optional,
            move || -> CheckFuture<'static> {
                Box::pin(async move {
                    if configured {
                        Ok(())
                    } else {
                        Err("services.sso_identity.base_url is not set: providers cannot be created through the administration API (existing ones keep working)".to_owned())
                    }
                })
            },
        ));
    }

    let identity_port: Arc<dyn SsoIdentityPort> = identity;
    ctx.provide(identity_port)?;
    ctx.provide(secrets)?;
    ctx.provide::<dyn SamlProviders>(saml)?;
    ctx.provide::<dyn PolicySource>(policy)?;
    ctx.provide::<dyn LogoutDirectory>(logout.clone())?;
    ctx.provide::<dyn SessionEnder>(logout)?;
    let admin_port: Arc<dyn spot_http::api::ports::sso::SsoAdminPort> = admin;
    ctx.provide(admin_port.clone())?;

    // The administration routes are authenticated (the bearer layer), so they are mounted here
    // like the other administration groups and need no open path.
    let mut ports = Ports::new();
    ports.provide::<dyn spot_http::api::ports::sso::SsoAdminPort>(admin_port);
    let mounted = routes::mount(&[admin_registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the single sign-on administration routes were not mounted"
        );
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

/// The route group of the authenticated administration operations, registered by hand (see
/// `sso.rs`).
fn admin_registration() -> Registration {
    Registration {
        name: "sso_admin",
        serves: routes::sso_admin::SERVES,
        required_ports: routes::sso_admin::required_ports,
        routes: routes::sso_admin::routes,
        public: None,
    }
}
