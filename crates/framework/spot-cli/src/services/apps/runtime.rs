//! The per-site app runtime: loaded apps, the compile cache, limits, kills and the circuit breaker.
//!
//! One [`AppRuntime`] belongs to one site and holds its own [`WasmHost`], so instances are never
//! shared across sites. The compiled-component cache is keyed by the bundle digest only: it holds
//! code, never data, and a component compiled for one app is linked fresh (with that app's own
//! granted capabilities) for every instance.
//!
//! A call that overruns a limit kills only its own instance (the host returns the pool slot and
//! nothing of the call survives). The runtime then writes an audit record naming the app and the
//! limit, never the payload, counts `app_kills_total{app,limit}`, and feeds the circuit breaker:
//! an app killed [`BreakerConfig::kills`] times inside the window is disabled until the next load.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde_json::Value;
use spot_core::{Clock, SiteId};
use spot_installer::SiteState;
use spot_manifest::{AppManifest, FileAccess as ManifestFiles};
use spot_wasm::{
    CompiledComponent, DocTypeAccess, Endpoint, FileAccess, GrantedCapabilities, HostConfig,
    Imports, LogLevel, WasmError, WasmHost,
};

use super::egress::AppEgress;
use super::limits::LimitTable;
use super::store::{BundleStore, StoreError};
use super::verify::{self, BUNDLE_INVALID, Trust};

/// `app.limit_exceeded`: the call overran a limit and its instance was killed.
pub const LIMIT_EXCEEDED: &str = "app.limit_exceeded";
/// `app.disabled`: the app was disabled by the circuit breaker.
pub const DISABLED: &str = "app.disabled";
/// `app.not_loaded`: no loaded app has this name.
pub const NOT_LOADED: &str = "app.not_loaded";
/// `app.capability_denied`: the app imports something it was not granted.
pub const CAPABILITY_DENIED: &str = "app.capability_denied";
/// `app.failed`: the app trapped or the host could not run it.
pub const FAILED: &str = "app.failed";
/// `app.rejected`: the app returned its own error; the message is its sanitised text.
pub const REJECTED: &str = "app.rejected";

/// The one export the hosted world calls: `on-event(event: string) -> result<string, error>`.
pub const HOOK_EXPORT: &str = "on-event";

/// The circuit breaker's setting.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerConfig {
    /// Kills inside the window that disable the app. 0 turns the breaker off.
    pub kills: u32,
    /// The window.
    pub window: Duration,
}

/// What the runtime reports about one app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AppStatus {
    /// Loaded and serving.
    Loaded,
    /// Not loaded, with the stable code (`app.bundle_invalid`, `app.publisher_revoked`, ...).
    Failed(String),
    /// Disabled by the circuit breaker.
    Disabled,
}

/// One row of `GET /api/v1/apps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    /// The app's name.
    pub name: String,
    /// Its installed version.
    pub version: String,
    /// The digest it was installed with, 64 hex digits.
    pub digest: String,
    /// The publisher, when the bundle is signed.
    pub publisher: Option<String>,
    /// What state it is in.
    pub status: AppStatus,
    /// Kills seen in the breaker's current window.
    pub recent_kills: u32,
}

/// An audit record: which app, what happened, which limit. Never a payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppAuditRecord {
    /// The site.
    pub site: String,
    /// The app.
    pub app: String,
    /// `killed`, `disabled`, `load_failed`, `loaded` or `unloaded`.
    pub event: &'static str,
    /// The limit that was overrun, or the failure code.
    pub detail: Option<String>,
    /// Seconds since the Unix epoch, from the site clock.
    pub at_unix: i64,
}

/// Where audit records go.
pub trait AppAudit: Send + Sync {
    /// Records one event.
    fn record(&self, record: AppAuditRecord);
}

/// Writes audit records to the `spot::audit::apps` tracing target.
pub struct TracingAudit;

impl AppAudit for TracingAudit {
    fn record(&self, r: AppAuditRecord) {
        tracing::info!(
            target: "spot::audit::apps",
            site = %r.site, app = %r.app, event = r.event,
            detail = r.detail.as_deref().unwrap_or(""), at = r.at_unix,
            "app runtime"
        );
    }
}

/// Reads a document as the invoking user. The implementation applies the permission engine and the
/// field guard for that user; the runtime never reads as a service identity.
pub trait InvokerDocs: Send + Sync {
    /// The document as JSON text with the fields `user` may see, or `None`.
    fn get(&self, site: &SiteId, user: &str, doctype: &str, id: &str) -> Option<String>;
}

/// Reads nothing: the default until the host wires the document engine.
pub struct NoDocs;

impl InvokerDocs for NoDocs {
    fn get(&self, _site: &SiteId, _user: &str, _doctype: &str, _id: &str) -> Option<String> {
        None
    }
}

/// Who and when one call runs for.
#[derive(Clone, Debug)]
pub struct CallIdentity {
    /// The invoking user (`None` for a system or consumer actor: it reads no document).
    pub user: Option<String>,
    /// The invocation's business time, in microseconds since the Unix epoch.
    pub now_micros: i64,
    /// Whether the phase allows external calls (ADR-51).
    pub external_allowed: bool,
}

/// Why a call failed, with a code and a message that is safe to show a caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallError {
    /// The stable code.
    pub code: &'static str,
    /// A generic message: no payload, no host path.
    pub message: String,
    /// The limit that was overrun, for [`LIMIT_EXCEEDED`].
    pub limit: Option<&'static str>,
}

impl CallError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            limit: None,
        }
    }
}

/// The imports one call sees: its identity and nothing else.
struct CallImports {
    site: SiteId,
    who: CallIdentity,
    docs: Arc<dyn InvokerDocs>,
}

impl Imports for CallImports {
    fn log(&self, level: LogLevel, message: &str) {
        // Guest text is bounded and goes to the debug stream only.
        let line: String = message.chars().take(512).collect();
        tracing::debug!(target: "spot::app", level = ?level, site = %self.site, message = %line);
    }

    fn now(&self) -> i64 {
        self.who.now_micros
    }

    fn get_doc(&self, doctype: &str, id: &str) -> Option<String> {
        // As the invoking user, or not at all.
        let user = self.who.user.as_deref()?;
        self.docs.get(&self.site, user, doctype, id)
    }
}

/// A loaded app.
pub struct LoadedApp {
    /// The manifest, as signed.
    pub manifest: Option<AppManifest>,
    instance: spot_wasm::AppInstance,
    info: AppInfo,
    /// The app's outbound gate.
    pub egress: Arc<AppEgress>,
}

struct Cache {
    cap: usize,
    items: HashMap<String, CompiledComponent>,
    order: VecDeque<String>,
}

impl Cache {
    fn get(&self, digest: &str) -> Option<CompiledComponent> {
        self.items.get(digest).cloned()
    }

    fn put(&mut self, digest: &str, component: CompiledComponent) {
        if self.items.insert(digest.to_owned(), component).is_none() {
            self.order.push_back(digest.to_owned());
        }
        while self.items.len() > self.cap.max(1) {
            match self.order.pop_front() {
                Some(oldest) => {
                    self.items.remove(&oldest);
                }
                None => break,
            }
        }
    }
}

/// Everything a runtime is built from.
pub struct RuntimeSettings {
    /// The limit table.
    pub limits: LimitTable,
    /// The circuit breaker.
    pub breaker: BreakerConfig,
    /// Compiled components kept (by digest).
    pub cache_entries: usize,
    /// Calls that may run at once.
    pub max_concurrent_calls: u32,
    /// The site's outbound allow-list, `host:port`.
    pub egress_allow: Vec<(String, u16)>,
}

/// The runtime of one site.
pub struct AppRuntime {
    site: SiteId,
    host: WasmHost,
    settings: RuntimeSettings,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn AppAudit>,
    docs: Arc<dyn InvokerDocs>,
    resolver: Arc<dyn spot_egress::Resolver>,
    cache: Mutex<Cache>,
    apps: RwLock<BTreeMap<String, Arc<LoadedApp>>>,
    failed: Mutex<BTreeMap<String, (String, String, &'static str)>>,
    kills: Mutex<BTreeMap<String, VecDeque<Duration>>>,
    disabled: Mutex<std::collections::BTreeSet<String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// The runtime's capability type for what an administrator approved.
#[must_use]
pub fn granted(approved: &spot_installer::GrantedCapabilities) -> GrantedCapabilities {
    GrantedCapabilities {
        network: approved
            .network
            .iter()
            .map(|e| Endpoint {
                host: e.host.clone(),
                port: e.port,
            })
            .collect(),
        email: approved.email,
        files: match approved.files {
            ManifestFiles::None => FileAccess::None,
            ManifestFiles::Read => FileAccess::Read,
            ManifestFiles::ReadWrite => FileAccess::ReadWrite,
        },
        doctypes: DocTypeAccess {
            read: approved.doctypes.read.clone(),
            write: approved.doctypes.write.clone(),
        },
    }
}

impl AppRuntime {
    /// A runtime for `site`.
    ///
    /// # Errors
    ///
    /// The host's error text when the engine cannot be built.
    pub fn new(
        site: SiteId,
        settings: RuntimeSettings,
        clock: Arc<dyn Clock>,
        audit: Arc<dyn AppAudit>,
        docs: Arc<dyn InvokerDocs>,
        resolver: Arc<dyn spot_egress::Resolver>,
    ) -> Result<Self, String> {
        let host = WasmHost::new(HostConfig {
            max_concurrent_calls: settings.max_concurrent_calls,
            max_memory_bytes: settings.limits.max_memory_bytes(),
            ..HostConfig::default()
        })
        .map_err(|e| e.to_string())?;
        let cap = settings.cache_entries;
        Ok(Self {
            site,
            host,
            settings,
            clock,
            audit,
            docs,
            resolver,
            cache: Mutex::new(Cache {
                cap,
                items: HashMap::new(),
                order: VecDeque::new(),
            }),
            apps: RwLock::new(BTreeMap::new()),
            failed: Mutex::new(BTreeMap::new()),
            kills: Mutex::new(BTreeMap::new()),
            disabled: Mutex::new(std::collections::BTreeSet::new()),
        })
    }

    fn event(&self, app: &str, event: &'static str, detail: Option<String>) {
        self.audit.record(AppAuditRecord {
            site: self.site.as_str().to_owned(),
            app: app.to_owned(),
            event,
            detail,
            at_unix: self.clock.now_utc().unix_timestamp(),
        });
    }

    /// Loads (or reloads) every installed app of `state`, replacing what was loaded. An app that
    /// fails verification or linking is recorded as failed and the others still load.
    pub fn load_all(&self, state: &SiteState, store: &dyn BundleStore, trust: &Trust) {
        let now = self.clock.now_utc().unix_timestamp();
        let mut loaded = BTreeMap::new();
        let mut failed = BTreeMap::new();
        for installed in &state.apps {
            match self.load_one(installed, store, trust, now) {
                Ok(Some(app)) => {
                    self.event(&installed.name, "loaded", None);
                    loaded.insert(installed.name.clone(), Arc::new(app));
                }
                Ok(None) => {}
                Err(code) => {
                    self.event(&installed.name, "load_failed", Some(code.to_owned()));
                    failed.insert(
                        installed.name.clone(),
                        (
                            installed.version.to_string(),
                            installed.digest.to_hex(),
                            code,
                        ),
                    );
                }
            }
        }
        *self.apps.write().unwrap_or_else(|p| p.into_inner()) = loaded;
        *lock(&self.failed) = failed;
        lock(&self.kills).clear();
        lock(&self.disabled).clear();
    }

    fn load_one(
        &self,
        installed: &spot_installer::InstalledApp,
        store: &dyn BundleStore,
        trust: &Trust,
        now: i64,
    ) -> Result<Option<LoadedApp>, &'static str> {
        let digest = installed.digest.to_hex();
        let stored = store.read(&digest).map_err(|e| match e {
            StoreError::Missing | StoreError::Unreadable => BUNDLE_INVALID,
        })?;
        let verified = verify::verify(&stored, &digest, trust, now).map_err(|r| r.code)?;
        if verified.wasm.is_empty() {
            // A native app: nothing for this runtime to host.
            return Ok(None);
        }
        self.link(
            &installed.name,
            &installed.version.to_string(),
            &digest,
            &verified.wasm,
            &installed.approved,
            Some(verified.manifest),
            verified.publisher,
        )
        .map(Some)
    }

    /// Compiles (through the digest-keyed cache) and links `wasm` for `name` with the approved
    /// capabilities. Exposed so a host that has already verified a bundle, and the tests, load
    /// without a bundle store; [`AppRuntime::load_all`] is the verified path.
    ///
    /// # Errors
    ///
    /// `app.bundle_invalid` when the component does not compile, `app.capability_denied` when it
    /// imports something that was not approved.
    #[allow(clippy::too_many_arguments)]
    pub fn link(
        &self,
        name: &str,
        version: &str,
        digest: &str,
        wasm: &[u8],
        approved: &spot_installer::GrantedCapabilities,
        manifest: Option<AppManifest>,
        publisher: Option<String>,
    ) -> Result<LoadedApp, &'static str> {
        let cached = lock(&self.cache).get(digest);
        let component = match cached {
            Some(component) => component,
            None => {
                let compiled = self.host.compile(wasm).map_err(|_| BUNDLE_INVALID)?;
                lock(&self.cache).put(digest, compiled.clone());
                compiled
            }
        };
        let caps = granted(approved);
        let endpoints: Vec<(String, u16)> = caps
            .network
            .iter()
            .map(|e| (e.host.clone(), e.port))
            .collect();
        let instance = self
            .host
            .instantiate(&component, caps)
            .map_err(|e| match e {
                WasmError::CapabilityDenied(_) => CAPABILITY_DENIED,
                _ => BUNDLE_INVALID,
            })?;
        let egress = Arc::new(AppEgress::new(
            self.site.as_str(),
            name,
            &endpoints,
            &self.settings.egress_allow,
            Arc::clone(&self.resolver),
            Arc::clone(&self.clock),
        ));
        Ok(LoadedApp {
            manifest,
            instance,
            info: AppInfo {
                name: name.to_owned(),
                version: version.to_owned(),
                digest: digest.to_owned(),
                publisher,
                status: AppStatus::Loaded,
                recent_kills: 0,
            },
            egress,
        })
    }

    /// Makes `app` the loaded app `name` (replacing any), as a verified install or upgrade does.
    pub fn insert(&self, app: LoadedApp) {
        let name = app.info.name.clone();
        lock(&self.failed).remove(&name);
        lock(&self.disabled).remove(&name);
        lock(&self.kills).remove(&name);
        self.apps
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name, Arc::new(app));
    }

    /// Unloads one app at once (an uninstall signal).
    pub fn unload(&self, app: &str) {
        let removed = self
            .apps
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(app)
            .is_some();
        lock(&self.failed).remove(app);
        lock(&self.kills).remove(app);
        lock(&self.disabled).remove(app);
        if removed {
            self.event(app, "unloaded", None);
        }
    }

    /// The loaded app called `app`.
    #[must_use]
    pub fn app(&self, app: &str) -> Option<Arc<LoadedApp>> {
        self.apps
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(app)
            .cloned()
    }

    /// Every app the registry holds and what became of it, in name order.
    #[must_use]
    pub fn list(&self) -> Vec<AppInfo> {
        let disabled = lock(&self.disabled).clone();
        let kills = lock(&self.kills).clone();
        let mut rows: Vec<AppInfo> = self
            .apps
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .map(|a| {
                let mut info = a.info.clone();
                if disabled.contains(&info.name) {
                    info.status = AppStatus::Disabled;
                }
                info.recent_kills = kills
                    .get(&info.name)
                    .map_or(0, |q| u32::try_from(q.len()).unwrap_or(u32::MAX));
                info
            })
            .collect();
        for (name, (version, digest, code)) in lock(&self.failed).iter() {
            rows.push(AppInfo {
                name: name.clone(),
                version: version.clone(),
                digest: digest.clone(),
                publisher: None,
                status: AppStatus::Failed((*code).to_owned()),
                recent_kills: 0,
            });
        }
        rows.sort_by(|a, b| a.name.cmp(&b.name));
        rows
    }

    /// Why the site is degraded: one sentence per app that failed or was disabled. Empty when all
    /// is well.
    #[must_use]
    pub fn degradation(&self) -> Vec<String> {
        self.list()
            .into_iter()
            .filter_map(|row| match row.status {
                AppStatus::Loaded => None,
                AppStatus::Failed(code) => {
                    Some(format!("{code}: app `{}` is not loaded", row.name))
                }
                AppStatus::Disabled => Some(format!(
                    "{DISABLED}: app `{}` was disabled by the breaker",
                    row.name
                )),
            })
            .collect()
    }

    /// Calls the loaded app's export under its limits. A kill is audited, counted and fed to the
    /// breaker; the runtime and every other app are unaffected.
    ///
    /// # Errors
    ///
    /// A [`CallError`] with a stable code and a generic message.
    pub async fn call(
        &self,
        app: &str,
        export: &str,
        args: Value,
        who: CallIdentity,
    ) -> Result<Value, CallError> {
        let loaded = self
            .app(app)
            .ok_or_else(|| CallError::new(NOT_LOADED, "the app is not loaded"))?;
        if lock(&self.disabled).contains(app) {
            return Err(CallError::new(DISABLED, "the app is disabled"));
        }
        let imports = Arc::new(CallImports {
            site: self.site.clone(),
            who,
            docs: Arc::clone(&self.docs),
        });
        let limits = self.settings.limits.for_app(app);
        match loaded.instance.call(export, &args, limits, imports).await {
            Ok(value) => Ok(value),
            Err(error) => Err(self.failure(app, &error)),
        }
    }

    fn failure(&self, app: &str, error: &WasmError) -> CallError {
        let limit = match error {
            WasmError::OutOfFuel { .. } => Some("fuel"),
            WasmError::Timeout { .. } => Some("wall_clock"),
            WasmError::Memory(_) => Some("memory"),
            WasmError::TooManyHostCalls { .. } => Some("host_calls"),
            WasmError::OutputTooLarge { .. } => Some("output"),
            _ => None,
        };
        if let Some(limit) = limit {
            self.killed(app, limit);
            return CallError {
                code: LIMIT_EXCEEDED,
                message: "the app exceeded a resource limit and was stopped".to_owned(),
                limit: Some(limit),
            };
        }
        match error {
            WasmError::CapabilityDenied(_) => CallError::new(
                CAPABILITY_DENIED,
                "the app used a capability it was not granted",
            ),
            WasmError::App(value) => CallError::new(REJECTED, sanitise(value)),
            _ => CallError::new(FAILED, "the app could not complete the call"),
        }
    }

    fn killed(&self, app: &str, limit: &'static str) {
        metrics::counter!("app_kills_total", "app" => app.to_owned(), "limit" => limit)
            .increment(1);
        self.event(app, "killed", Some(limit.to_owned()));
        let cfg = self.settings.breaker;
        if cfg.kills == 0 {
            return;
        }
        let now = self.clock.monotonic();
        let count = {
            let mut kills = lock(&self.kills);
            let queue = kills.entry(app.to_owned()).or_default();
            queue.push_back(now);
            while queue
                .front()
                .is_some_and(|t| now.saturating_sub(*t) > cfg.window)
            {
                queue.pop_front();
            }
            u32::try_from(queue.len()).unwrap_or(u32::MAX)
        };
        if count >= cfg.kills && lock(&self.disabled).insert(app.to_owned()) {
            self.event(app, "disabled", Some("circuit_breaker".to_owned()));
        }
    }
}

/// The app's own error as text a caller may see: its `message` when it is a string, cut to 200
/// characters with control characters removed; otherwise a fixed sentence.
fn sanitise(value: &Value) -> String {
    let text = value
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the app refused the operation");
    let clean: String = text.chars().filter(|c| !c.is_control()).take(200).collect();
    if clean.is_empty() {
        "the app refused the operation".to_owned()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_app_error_is_cut_and_stripped() {
        let long = "x".repeat(500);
        assert_eq!(sanitise(&json!({"message": long})).len(), 200);
        assert_eq!(sanitise(&json!({"message": "a\u{0}b\nc"})), "abc");
        assert_eq!(
            sanitise(&json!({"code": 1})),
            "the app refused the operation"
        );
    }
}
