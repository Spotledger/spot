//! The app runtime service of one site: it loads the installed apps' WebAssembly components,
//! verifies them, grants them only what the administrator approved, and serves their hooks and
//! functions under hard limits (WP-EXT-05.T2; ADR-16, ADR-57).
//!
//! # What it does
//!
//! - **Start and reload.** It reads the site's installed apps from the PostgreSQL registry
//!   (`spot-installer-pg`), reads each bundle's bytes by digest from the bundle store
//!   (`services.apps.bundles_dir`), and **re-verifies** the digest, the signature and the
//!   revocation list ([`verify`]). A tampered or revoked bundle is not loaded, is reported `failed`
//!   with `app.bundle_invalid` or `app.publisher_revoked`, and the site starts without it. The
//!   compiled component is cached by digest (bounded). [`AppsPort::reload`] is the install,
//!   upgrade and uninstall signal; [`AppsPort::unload`] drops one app at once.
//! - **Capabilities.** An instance is linked with exactly the approved capabilities
//!   ([`runtime::granted`]); an import that was not granted fails at link time
//!   (`app.capability_denied`). The guest has no clock, file system or network of its own, and its
//!   document reads run as the **invoking user** ([`runtime::InvokerDocs`]).
//! - **Limits** ([`limits`]): fuel, wall clock, memory, host calls and output size per call, from
//!   the defaults, per-app overrides and site maxima in `[services.apps]`. An overrun kills only
//!   that instance, answers `app.limit_exceeded`, writes an audit record (app and limit, never a
//!   payload) to `spot::audit::apps`, counts `app_kills_total{app,limit}`, and feeds a circuit
//!   breaker that disables an app killed `breaker_kills` times in `breaker_window_seconds`.
//! - **Network** ([`egress`]): the approved hosts intersected with `egress_allow`, through
//!   `spot-egress` (private, loopback, link-local and metadata addresses refused after
//!   resolution, redirects re-checked).
//! - **Hooks** ([`controller`]): `[[events]]` of an app become controllers the seam registry takes
//!   through [`AppsPort::controllers`]; the registry assigns phases and refuses a `validate` step
//!   that writes.
//! - **Isolation.** The runtime, its host and its apps belong to one site; the compile cache holds
//!   code keyed by digest and never data.
//!
//! # Level
//!
//! Optional. A site with no installed app has it idle. A failed or disabled app degrades the site
//! (the check names each with its code); it never stops it. If the registry cannot be read, the
//! service registers with no app loaded and the check says why.
//!
//! # Settings (`[services.apps]`)
//!
//! See `docs/running-spot/apps.md`.

#[path = "apps/controller.rs"]
pub mod controller;
#[path = "apps/egress.rs"]
pub mod egress;
#[path = "apps/limits.rs"]
pub mod limits;
#[path = "apps/runtime.rs"]
pub mod runtime;
#[path = "apps/store.rs"]
pub mod store;
#[path = "apps/verify.rs"]
pub mod verify;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;
use spot_app_api::{BoxFuture, Controller};
use spot_core::SiteId;
use spot_installer::AppRegistryStore;
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

pub use runtime::{
    AppAudit, AppAuditRecord, AppInfo, AppRuntime, AppStatus, BreakerConfig, CallError,
    CallIdentity, InvokerDocs, NoDocs, RuntimeSettings, TracingAudit,
};
pub use store::{BundleStore, DirBundleStore, MemoryBundleStore, StoredBundle};

/// Start order: after the framework's own services and after `function` (1020).
pub const ORDER: u32 = 1100;

fn d_true() -> bool {
    true
}
fn d_bundles() -> PathBuf {
    PathBuf::from("bundles")
}
fn d_cache() -> usize {
    32
}
fn d_calls() -> u32 {
    64
}
fn d_kills() -> u32 {
    5
}
fn d_window() -> u64 {
    300
}

/// `services.apps`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host installed apps. Default true; false leaves the service idle with nothing loaded.
    #[serde(default = "d_true")]
    pub enabled: bool,
    /// The content-addressed bundle store, relative to the configuration root.
    #[serde(default = "d_bundles")]
    pub bundles_dir: PathBuf,
    /// Compiled components kept, by digest. 1 to 1,024.
    #[serde(default = "d_cache")]
    pub cache_entries: usize,
    /// Calls that may run at once on this site. 1 to 1,024.
    #[serde(default = "d_calls")]
    pub max_concurrent_calls: u32,
    /// Kills inside the window that disable an app; 0 turns the breaker off.
    #[serde(default = "d_kills")]
    pub breaker_kills: u32,
    /// The breaker window in seconds. 1 to 86,400.
    #[serde(default = "d_window")]
    pub breaker_window_seconds: u64,
    /// The site's outbound allow-list, `host:port`. Empty means no app has any network.
    #[serde(default)]
    pub egress_allow: Vec<String>,
    /// The limits.
    #[serde(default)]
    pub limits: limits::LimitTable,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            bundles_dir: d_bundles(),
            cache_entries: d_cache(),
            max_concurrent_calls: d_calls(),
            breaker_kills: d_kills(),
            breaker_window_seconds: d_window(),
            egress_allow: Vec::new(),
            limits: limits::LimitTable::default(),
        }
    }
}

impl Settings {
    /// The first setting that is out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=1_024).contains(&self.cache_entries) {
            return Err("cache_entries");
        }
        if !(1..=1_024).contains(&self.max_concurrent_calls) {
            return Err("max_concurrent_calls");
        }
        if !(1..=86_400).contains(&self.breaker_window_seconds) {
            return Err("breaker_window_seconds");
        }
        if self.endpoints().is_none() {
            return Err("egress_allow");
        }
        self.limits.validate()
    }

    /// `egress_allow` as `(host, port)` pairs; `None` when an entry is not `host:port`.
    #[must_use]
    pub fn endpoints(&self) -> Option<Vec<(String, u16)>> {
        self.egress_allow
            .iter()
            .map(|entry| {
                let (host, port) = entry.rsplit_once(':')?;
                let port: u16 = port.parse().ok()?;
                (!host.is_empty() && port != 0).then(|| (host.to_owned(), port))
            })
            .collect()
    }

    /// The runtime's settings.
    #[must_use]
    pub fn runtime(&self) -> RuntimeSettings {
        RuntimeSettings {
            limits: self.limits.clone(),
            breaker: BreakerConfig {
                kills: self.breaker_kills,
                window: Duration::from_secs(self.breaker_window_seconds),
            },
            cache_entries: self.cache_entries,
            max_concurrent_calls: self.max_concurrent_calls,
            egress_allow: self.endpoints().unwrap_or_default(),
        }
    }
}

/// What other modules and the router take from the apps service.
pub trait AppsPort: Send + Sync {
    /// Every installed app and its state: the rows of `GET /api/v1/apps`.
    fn list(&self) -> Vec<AppInfo>;

    /// The controllers the loaded apps contribute (one per app and DocType), for the seam registry.
    fn controllers(&self) -> Vec<(String, Arc<dyn Controller>)>;

    /// Calls an app's `on-event` export with `event` as the invoking user `who`.
    fn call<'a>(
        &'a self,
        app: &'a str,
        event: Value,
        who: CallIdentity,
    ) -> BoxFuture<'a, std::result::Result<Value, CallError>>;

    /// Re-reads the registry and reloads every app (an install, upgrade or uninstall signal).
    fn reload(&self) -> BoxFuture<'_, std::result::Result<(), String>>;

    /// Unloads one app at once.
    fn unload(&self, app: &str);
}

/// The service: the runtime and what reloads it.
pub struct AppsService {
    enabled: bool,
    site: SiteId,
    runtime: Arc<AppRuntime>,
    registry: Arc<dyn AppRegistryStore>,
    store: Arc<dyn BundleStore>,
    trust: Arc<verify::Trust>,
    last_error: std::sync::Mutex<Option<String>>,
}

impl AppsService {
    /// A service over its parts (the registration builds them from the site; a test passes its own).
    pub fn new(
        enabled: bool,
        site: SiteId,
        runtime: Arc<AppRuntime>,
        registry: Arc<dyn AppRegistryStore>,
        store: Arc<dyn BundleStore>,
        trust: Arc<verify::Trust>,
    ) -> Arc<Self> {
        Arc::new(Self {
            enabled,
            site,
            runtime,
            registry,
            store,
            trust,
            last_error: std::sync::Mutex::new(None),
        })
    }

    /// The runtime.
    #[must_use]
    pub fn runtime(&self) -> &Arc<AppRuntime> {
        &self.runtime
    }

    async fn reload_inner(&self) -> std::result::Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        let state = match self.registry.load(&self.site).await {
            Ok(state) => state,
            Err(e) => {
                let why = format!("the installed-app registry cannot be read: {}", e.code);
                *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(why.clone());
                return Err(why);
            }
        };
        *self.last_error.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.runtime
            .load_all(&state, self.store.as_ref(), self.trust.as_ref());
        Ok(())
    }
}

impl AppsPort for AppsService {
    fn list(&self) -> Vec<AppInfo> {
        self.runtime.list()
    }

    fn controllers(&self) -> Vec<(String, Arc<dyn Controller>)> {
        controller::wasm_controllers(&self.runtime)
    }

    fn call<'a>(
        &'a self,
        app: &'a str,
        event: Value,
        who: CallIdentity,
    ) -> BoxFuture<'a, std::result::Result<Value, CallError>> {
        Box::pin(async move {
            self.runtime
                .call(
                    app,
                    runtime::HOOK_EXPORT,
                    Value::Array(vec![Value::String(event.to_string())]),
                    who,
                )
                .await
        })
    }

    fn reload(&self) -> BoxFuture<'_, std::result::Result<(), String>> {
        Box::pin(self.reload_inner())
    }

    fn unload(&self, app: &str) {
        self.runtime.unload(app);
    }
}

impl Service for AppsService {
    fn name(&self) -> &'static str {
        "apps"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if let Some(why) = self
                .last_error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            {
                return Err(why);
            }
            let degraded = self.runtime.degradation();
            if degraded.is_empty() {
                Ok(())
            } else {
                Err(degraded.join("; "))
            }
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

/// Registers the apps service of the site: reads `[services.apps]`, builds the runtime over the
/// site's registry and bundle store, loads the installed apps and provides [`AppsPort`].
///
/// # Errors
///
/// Settings out of range, or the engine cannot be built. A registry that cannot be read is not an
/// error: the service registers with nothing loaded and its check reports why.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("apps")?;
    if let Err(key) = settings.validate() {
        return Err(Error(format!("services.apps.{key}: out of range")));
    }
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(|e| Error(format!("services.apps: {e}")))?;
    let registry = spot_installer_pg::PgAppRegistry::new(env.backend.pool());
    if settings.enabled {
        registry.prepare().await.map_err(|e| {
            Error(format!(
                "services.apps: the registry cannot be prepared: {}",
                e.into_body().code
            ))
        })?;
    }
    let trust = verify::Trust::load(
        env.root,
        env.loaded.common.trust.roots.as_deref(),
        env.loaded.common.trust.revocations.as_deref(),
        env.loaded.common.trust.require_signed,
    );
    let runtime = Arc::new(
        AppRuntime::new(
            site.clone(),
            settings.runtime(),
            Arc::clone(ctx.clock()),
            Arc::new(TracingAudit),
            Arc::new(NoDocs),
            Arc::new(spot_egress::SystemResolver),
        )
        .map_err(|e| Error(format!("services.apps: {e}")))?,
    );
    let service = AppsService::new(
        settings.enabled,
        site,
        runtime,
        Arc::new(registry),
        Arc::new(DirBundleStore::new(env.root.join(&settings.bundles_dir))),
        Arc::new(trust),
    );
    // A registry read failure is remembered for the check; nothing is loaded.
    let _ = service.reload_inner().await;
    ctx.provide::<dyn AppsPort>(Arc::clone(&service) as Arc<dyn AppsPort>)?;
    ctx.add(service as Arc<dyn Service>);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_unknown_keys_are_refused() {
        assert!(Settings::default().validate().is_ok());
        let bad: std::result::Result<Settings, _> = toml::from_str("nope = 1");
        assert!(bad.is_err());
    }

    #[test]
    fn egress_allow_must_be_host_and_port() {
        let ok: Settings =
            toml::from_str(r#"egress_allow = ["api.example.com:443"]"#).unwrap_or_default();
        assert_eq!(
            ok.endpoints(),
            Some(vec![("api.example.com".to_owned(), 443)])
        );
        let bad: Settings = toml::from_str(r#"egress_allow = ["no-port"]"#).unwrap_or_default();
        assert_eq!(bad.endpoints(), None);
        assert_eq!(bad.validate(), Err("egress_allow"));
    }
}
