//! Per-site runtime assembly and dispatch after spot-http resolves the host.
//!
//! Each site gets its own metadata (platform first), one merged permission snapshot, an engine
//! that enforces it, a storage-backed principal source, and a sign-in service on the in-memory
//! session and throttle stores. Dispatch authenticates every request, including API keys and
//! sessions, before the `/api/v1` router sees it; `/api/v1/auth/login` is the only open route.
//! A site whose snapshot is bad is reported as `503` and does not stop the other sites.
//!
//! Reload: `spot reload` touches `sites/<site>/reload` and SIGHUP touches them all. A polling
//! task (500 ms) is the portable trigger, with the signal wired on Unix as well. Rebuilding a
//! site creates new metadata, snapshot, engine and router, keeps its session and throttle
//! stores, and swaps the runtime; requests already running finish on the old one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value as Json, json};
use spot_api_services::{MetaResources, PrincipalSource};
use spot_app_api::context::Actor;
use spot_auth::ApiKey;
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_http::api::service::{ApiError, Credential, LoginBody, RequestCtx};
use spot_http::api::{
    idempotency::MemoryIdempotencyStore,
    ports::link_inbound::LinkInboundPort,
    registry::Code,
    router::{ApiServices, router_for, router_with, routes},
};
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_http::request::RequestId;
use spot_http::site::SiteContext;
use spot_http::{ListenerAudience, ServerBuilder, ServerHandle, StaticSites};
use spot_login::LoginConfig;
use spot_postgres::PgBackend;
use spot_storage_contract::{HealthStatus, RecordId, StorageBackend};
use tower::ServiceExt;

use crate::services::{self, CheckFuture, Level, RunFuture, Service, Services};

use crate::gate;
use crate::live::{LivePermissions, LiveReady, LiveResources, SiteLive};
use crate::login::{self, Credentials, LoginStores, SiteAuth};
use crate::permissions::{self, GuardedEngine};
use crate::{Error, Result, config, error, users};

tokio::task_local! {
    /// The principal dispatch authenticated for the request currently running.
    static REQUEST_ACTOR: Actor;
}

fn actor() -> std::result::Result<Actor, ApiError> {
    REQUEST_ACTOR
        .try_with(Clone::clone)
        .map_err(|_| ApiError::from_code(Code::AuthRequired))
}

/// The live database check readiness has always rendered as `storage:<site>`: the assembly's
/// own state records how the site started, this asks the pool now.
struct StorageReady {
    name: String,
    backend: PgBackend,
}

impl ReadinessCheck for StorageReady {
    fn name(&self) -> &str {
        &self.name
    }
    fn check(&self) -> std::pin::Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async {
            if self.backend.health().await.status == HealthStatus::Healthy {
                CheckResult::ok()
            } else {
                CheckResult::failed("storage is not healthy")
            }
        })
    }
}

/// The capture invariant's line of `/api/health/ready` (`capture_invariant:<site>`, WP-JRN-02.T7):
/// the service's own readiness text. It is always `ok`: the service is optional and must never
/// stop trading, so three failed passes in a row render as `degraded: ...` (readiness stays 200),
/// never as a failing check. It reads in-memory counters only, so it never waits on the store.
struct CaptureReady {
    name: String,
    site: String,
}

impl CaptureReady {
    fn new(site: &str) -> Self {
        Self {
            name: format!("capture_invariant:{site}"),
            site: site.to_owned(),
        }
    }
}

impl ReadinessCheck for CaptureReady {
    fn name(&self) -> &str {
        &self.name
    }
    fn check(&self) -> std::pin::Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async {
            let Some(service) = services::capture_invariant::hosted(&self.site) else {
                // Stopped (the site is shutting down): not a reason to leave rotation.
                return CheckResult {
                    ok: true,
                    detail: "degraded: capture_invariant: stopped".to_owned(),
                };
            };
            let text = service.readiness_text();
            let degraded =
                service.failures() >= services::capture_invariant::DEGRADED_AFTER_FAILURES;
            CheckResult {
                ok: true,
                detail: if degraded {
                    format!("degraded: {text}")
                } else {
                    text
                },
            }
        })
    }
}

#[derive(Clone)]
enum SiteRuntime {
    Ready {
        router: Router,
        /// The router of the consumer listeners (`router_for(&[Audience::Consumer], ..)`).
        consumer: Router,
        /// The realtime socket route, authenticated by its own first message.
        realtime: Option<Router>,
        auth: Arc<SiteAuth>,
        dev_token: Option<HeaderValue>,
        /// Who holds which roles on this site: `/metrics` is for its administrators.
        principals: Arc<dyn PrincipalSource>,
        /// The routes modules declared reachable without a session.
        open: Arc<services::OpenPaths>,
        /// The site's function service serves guest functions (calls with no credential).
        guest_functions: bool,
    },
    Unavailable {
        detail: String,
    },
}

type RuntimeMap = Arc<RwLock<BTreeMap<String, SiteRuntime>>>;

/// Options a host runs with. The acceptance tests shorten the throttle and session windows;
/// production uses the shipped defaults.
pub struct Options {
    /// Sign-in configuration: policy, throttle, and how long a session cache is trusted.
    pub login: LoginConfig,
    /// The clock every service reads. Production uses the system clock; tests advance a
    /// manual clock to reach an expiry without sleeping.
    pub clock: Option<Arc<dyn spot_core::Clock>>,
}

impl Clone for Options {
    fn clone(&self) -> Self {
        Self {
            login: clone_login(&self.login),
            clock: self.clock.clone(),
        }
    }
}

impl Options {
    fn login_config(&self) -> LoginConfig {
        clone_login(&self.login)
    }
}

fn clone_login(config: &LoginConfig) -> LoginConfig {
    LoginConfig {
        session_policy: config.session_policy,
        throttle: config.throttle,
        totp: config.totp,
        password_policy: config.password_policy,
        recovery_codes: config.recovery_codes,
        cache_ttl: config.cache_ttl,
        hash_params: config.hash_params.clone(),
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            login: login::default_login_config(),
            clock: None,
        }
    }
}

/// A running host and the counter its tests use to prove an unauthenticated request never
/// reached the engine.
pub struct Running {
    /// The server handle.
    pub handle: ServerHandle,
    ops: Arc<AtomicUsize>,
    /// Every site's service assembly, stopped after the server finished draining.
    services: Vec<Services>,
    lives: BTreeMap<String, Arc<SiteLive>>,
    /// The lock that tells the operator commands a host serves this root (released when the
    /// host is dropped). `None` when another host of this root already holds it.
    _serving: Option<std::fs::File>,
}

impl Running {
    /// The number of engine calls served by this host.
    pub fn engine_ops(&self) -> usize {
        self.ops.load(Ordering::Relaxed)
    }

    /// Stops serving, then stops every site's services in the reverse of their start order.
    ///
    /// The order is the documented one: the listener stops accepting and drains its in-flight
    /// requests first, so no accepted request loses its engine; the services that feed the
    /// server (jobs, scheduler, metadata) stop after it.
    pub async fn join(mut self) -> std::result::Result<(), spot_http::ServerError> {
        let result = self.handle.join().await;
        for assembly in &mut self.services {
            assembly.shutdown().await;
        }
        result
    }

    /// What the site called `site` serves right now: its realtime hub, mail, print and file
    /// services, for operators' tooling and for tests.
    pub fn live(&self, site: &str) -> Option<Arc<SiteLive>> {
        self.lives.get(site).cloned()
    }

    /// The routes open without a session on every site that started, as
    /// `"METHOD pattern (module)"` (the operator's view of the allow-list; for tests).
    pub fn open_paths(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .services
            .iter()
            .flat_map(|assembly| assembly.health().open_paths())
            .collect();
        out.sort();
        out
    }

    /// The bound addresses.
    pub fn local_addrs(&self) -> &[std::net::SocketAddr] {
        self.handle.local_addrs()
    }
}

/// Starts all configured sites and returns the running host.
///
/// The returned [`Running`] owns the sites' services: join it to stop serving and then stop
/// them in reverse order. The caller owns the shutdown future.
pub async fn start(
    root: &Path,
    dev: bool,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Running> {
    start_with(root, dev, Options::default(), shutdown).await
}

/// Like [`start`], with explicit options and the engine-call probe.
pub async fn start_with(
    root: &Path,
    dev: bool,
    options: Options,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Running> {
    start_with_audit(root, dev, options, None, shutdown).await
}

/// The sink of one site's identity records and whether it is the file log alone.
struct IdentitySink {
    sink: Arc<dyn crate::audit::IdentityAudit>,
    file_only: bool,
}

/// Chooses the identity sink of `site` (WP-PRD-01.T16).
///
/// * A sink handed in (tests) is used as it is.
/// * Otherwise the site's database decides: with the audit store prepared, the sink is the
///   hash-chained [`crate::audit::chain::ChainAudit`], with the file log beside it only when
///   `[audit] identity_file_mirror` is set.
/// * With no usable chain (the store is not prepared, or the database cannot be reached) the file
///   log stays the sink, and the host says once that the identity trail is not tamper-evident.
///   A database that cannot be reached leaves the site unavailable anyway; a store that is not
///   prepared is also reported by the pending-table check, which names `spot migrate --apply`.
async fn identity_sink(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    given: Option<&Arc<dyn crate::audit::IdentityAudit>>,
    chains: &mut BTreeMap<String, crate::audit::chain::IdentityChain>,
) -> Result<IdentitySink> {
    if let Some(sink) = given {
        return Ok(IdentitySink {
            sink: sink.clone(),
            file_only: false,
        });
    }
    let file = || -> Result<Arc<dyn crate::audit::IdentityAudit>> {
        Ok(Arc::new(crate::audit::FileAudit::open(
            &root
                .join("sites")
                .join(&site.site)
                .join("identity-audit.log"),
        )?))
    };
    let mirror = if loaded.common.audit.identity_file_mirror {
        Some(file()?)
    } else {
        None
    };
    let mut pg = loaded.admin.clone().with_dbname(&site.database);
    pg.pool_max_size = loaded.common.postgres.pool_max_size.clamp(1, 4);
    match crate::audit::chain::ChainAudit::open(pg, &site.site, mirror).await {
        Ok(Ok((chain, reader))) => {
            chains.insert(site.site.clone(), reader);
            Ok(IdentitySink {
                sink: Arc::new(chain),
                file_only: false,
            })
        }
        other => {
            let why = match other {
                Ok(Err(missing)) => format!(
                    "the audit store is not prepared (missing {}); run `spot migrate --apply`",
                    missing.join(", ")
                ),
                _ => "the site's database cannot be reached".to_owned(),
            };
            // Strict mode never serves on a trail an auditor cannot rely on.
            if loaded.common.audit.identity_chain_strict {
                return Err(Error(format!(
                    "site `{}`: `[audit] identity_chain_strict` is set and the identity audit \
                     chain is unavailable: {why}",
                    site.site
                )));
            }
            tracing::warn!(
                site = %site.site,
                "the identity audit trail is the file log only and is NOT tamper-evident: {why}"
            );
            Ok(IdentitySink {
                sink: file()?,
                file_only: true,
            })
        }
    }
}

/// Like [`start_with`], with the sink identity audit records go to. `None` (production) appends
/// to `sites/<site>/identity-audit.log`, and a host that cannot open it does not serve the site.
pub async fn start_with_audit(
    root: &Path,
    dev: bool,
    options: Options,
    audit: Option<Arc<dyn crate::audit::IdentityAudit>>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Running> {
    start_with_modules(root, dev, options, audit, &[], shutdown).await
}

/// Like [`start_with_audit`], with extra module registrations the host registers next to the
/// built-in table and [`services::REGISTRY`]. It is for embedding hosts and tests (a test module
/// cannot be part of `build.rs`'s table); production calls [`start`].
pub async fn start_with_modules(
    root: &Path,
    dev: bool,
    options: Options,
    audit: Option<Arc<dyn crate::audit::IdentityAudit>>,
    modules: &'static [services::Registration],
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<Running> {
    let loaded = config::load(root, dev)?;
    if loaded.sites.is_empty() {
        return Err(Error("no sites configured".into()));
    }
    // An invalid listener, address overlap or rate-limit setting stops the start before any site
    // is built or any socket bound; the error names the key (`ServerError::InvalidSetting`).
    loaded.server.validate().map_err(error)?;
    for notice in loaded
        .sites
        .iter()
        .filter_map(config::Site::legacy_secret_notice)
    {
        eprintln!("{notice}");
    }
    let ops = Arc::new(AtomicUsize::new(0));
    let identity_chain_strict = loaded.common.audit.identity_chain_strict;
    let mut stores: BTreeMap<String, LoginStores> = BTreeMap::new();
    let audit_clock = options.clock.clone().unwrap_or_else(login::system_clock);
    // The sites whose identity trail is the file log only, and the chains of those that have one.
    let mut file_only: Vec<String> = Vec::new();
    let mut chains: BTreeMap<String, crate::audit::chain::IdentityChain> = BTreeMap::new();
    for site in &loaded.sites {
        let sink = identity_sink(root, &loaded, site, audit.as_ref(), &mut chains).await?;
        if sink.file_only {
            file_only.push(site.site.clone());
        }
        let recorder = Arc::new(crate::audit::Recorder::new(
            &site.site,
            sink.sink,
            audit_clock.clone(),
        ));
        stores.insert(
            site.site.clone(),
            LoginStores::new(recorder, audit_clock.clone()),
        );
        tracing::warn!(site = %site.site, "{}", LoginStores::LIMITATION);
    }
    // One hub of live state per site, shared by every runtime build of the site: a reload
    // starts from what the site serves now.
    let lives: BTreeMap<String, Arc<SiteLive>> = loaded
        .sites
        .iter()
        .map(|site| (site.site.clone(), Arc::new(SiteLive::new())))
        .collect();
    // One ownership bridge per database and one registry per site, built on a plain thread (the
    // blocking bridge and `spot-partition-pg` refuse a thread that belongs to a runtime), shared
    // by every build of a site so a reload opens neither a second bridge nor a second refresh.
    let partitions = Arc::new(services::partition::Host::new());
    let mut runtimes = BTreeMap::new();
    let mut readiness = Vec::new();
    let mut participant_checks = Vec::new();
    let mut storage_checks = Vec::new();
    let mut capture_checks = Vec::new();
    let mut live_checks = Vec::new();
    let mut snapshot_checks = Vec::new();
    let mut assemblies: Vec<Services> = Vec::new();
    for site in &loaded.sites {
        match build_site(
            root,
            &loaded,
            site,
            dev,
            &options,
            &stores[&site.site],
            &lives[&site.site],
            &partitions,
            ops.clone(),
            true,
            modules,
        )
        .await
        {
            Ok((runtime, Some(mut assembly), participant_names)) => {
                // The site serves only once its required services passed their checks; an
                // optional one that did not started degraded and says so on readiness.
                if let Err(problem) = assembly.start().await {
                    eprintln!("site `{}` is unavailable: {problem}", site.site);
                    runtimes.insert(
                        site.site.clone(),
                        SiteRuntime::Unavailable { detail: problem },
                    );
                    continue;
                }
                if let Ok(backend) = crate::schema::backend(&loaded, site).await {
                    storage_checks.push(StorageReady {
                        name: format!("storage:{}", site.site),
                        backend,
                    });
                }
                // A site that hosts the capture invariant (`[services.capture_invariant]
                // enabled`) shows its text; a site without it shows no line.
                if services::capture_invariant::hosted(&site.site).is_some() {
                    capture_checks.push(CaptureReady::new(&site.site));
                }
                readiness.push(assembly.health_check());
                // What the site's participants are, and whether it is a partitioned node: `ok`
                // always, because a site that cannot check ownership refuses to start instead.
                participant_checks.push(
                    services::participants::ParticipantsReady::new(
                        &site.site,
                        site.partition.as_ref(),
                        &participant_names,
                    )
                    .with_audit(
                        lives[&site.site]
                            .doc_audit()
                            .map(|host| host as Arc<dyn crate::audit::doc::AuditReport>),
                    ),
                );
                live_checks.push(LiveReady::new(&site.site, lives[&site.site].clone()));
                // A partitioned site also reports its ownership snapshot (stale is `degraded:`).
                snapshot_checks.extend(partitions.readiness(site));
                runtimes.insert(site.site.clone(), runtime);
                assemblies.push(assembly);
            }
            Err(e) => {
                eprintln!("site `{}` is unavailable: {e}", site.site);
                runtimes.insert(
                    site.site.clone(),
                    SiteRuntime::Unavailable {
                        detail: e.to_string(),
                    },
                );
            }
            Ok((_, None, _)) => {
                // Only the initial start asks for services; a runtime built without them
                // cannot serve, so it is reported rather than silently mounted.
                eprintln!(
                    "site `{}` is unavailable: no services were assembled",
                    site.site
                );
                runtimes.insert(
                    site.site.clone(),
                    SiteRuntime::Unavailable {
                        detail: "the site was built without its services".into(),
                    },
                );
            }
        }
    }
    let runtimes: RuntimeMap = Arc::new(RwLock::new(runtimes));
    // The listeners: staff listeners (`bind`, and `listeners` marked staff) serve the staff
    // router, consumer listeners the consumer router. Listener changes need a restart (a reload
    // swaps the site runtimes behind the listeners, never the listeners). While no route is
    // marked for consumers, a consumer listener has nothing to serve: it is not started, and
    // that is said once.
    let mut server = loaded.server;
    let consumer_wanted = server
        .listeners
        .iter()
        .any(|l| l.audience == ListenerAudience::Consumer);
    let consumer_routes_exist = spot_http::api::routes::route_table()
        .iter()
        .any(|entry| entry.audiences.contains(&spot_core::Audience::Consumer));
    if consumer_wanted && !consumer_routes_exist {
        server
            .listeners
            .retain(|l| l.audience != ListenerAudience::Consumer);
        tracing::info!(
            "the configured consumer listeners are not started: no route is marked for consumers yet"
        );
    }
    let serves_consumers = consumer_wanted && consumer_routes_exist;
    let mut builder = ServerBuilder::new(server, {
        let mut resolver = StaticSites::new();
        for site in &loaded.sites {
            let id = SiteId::new(&site.site).map_err(error)?;
            resolver = resolver.add_site_name(id.clone());
            for host in &site.aliases {
                resolver = resolver.add(host, id.clone());
            }
        }
        resolver
    });
    for check in storage_checks {
        builder = builder.readiness_check(check);
    }
    for check in readiness {
        builder = builder.readiness_check(check);
    }
    for check in capture_checks {
        builder = builder.readiness_check(check);
    }
    for check in participant_checks {
        builder = builder.readiness_check(check);
    }
    for check in live_checks {
        builder = builder.readiness_check(check);
    }
    // The identity trail's status per site: the chain verifies a page at a time (a break is a
    // warning unless `[audit] identity_chain_strict`), and a file-only site says it is not
    // tamper-evident.
    for (site, chain) in chains {
        builder = builder.readiness_check(crate::audit::chain::IdentityChainReady::new(
            &site,
            chain,
            identity_chain_strict,
        ));
    }
    for site in file_only {
        builder = builder.readiness_check(crate::audit::chain::FileOnlyReady::new(&site));
    }
    for check in snapshot_checks {
        builder = builder.readiness_check(check);
    }
    let metrics = loaded.common.services.metrics.clone();
    let dispatch = {
        let runtimes = runtimes.clone();
        Router::new().route(
            "/{*path}",
            any(move |request: Request| {
                let runtimes = runtimes.clone();
                let metrics = metrics.clone();
                async move { dispatch(request, runtimes, metrics).await }
            }),
        )
    };
    let mut builder = builder.site_routes(dispatch);
    if serves_consumers {
        let consumer_dispatch = {
            let runtimes = runtimes.clone();
            Router::new().route(
                "/{*path}",
                any(move |request: Request| {
                    let runtimes = runtimes.clone();
                    async move { consumer_dispatch(request, runtimes).await }
                }),
            )
        };
        builder = builder.consumer_routes(consumer_dispatch);
    }
    let handle = builder.start(shutdown).await.map_err(error)?;
    let serving = take_serving_lock(root);
    spawn_reload(
        root.to_path_buf(),
        dev,
        options,
        stores,
        lives.clone(),
        partitions,
        runtimes,
        ops.clone(),
        modules,
    );
    Ok(Running {
        handle,
        ops,
        services: assemblies,
        lives,
        _serving: serving,
    })
}

/// The file whose exclusive lock a serving host holds.
fn serving_path(root: &Path) -> PathBuf {
    root.join("host.running")
}

/// Takes the serving lock of `root`, so `spot add-user`, `set-password` and the like can tell a
/// host is running and wait for it to reload. A second host on the same root does not get it.
fn take_serving_lock(root: &Path) -> Option<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(serving_path(root))
        .ok()?;
    file.try_lock().ok()?;
    Some(file)
}

/// Whether a host holds the serving lock of `root`.
fn host_serving(root: &Path) -> bool {
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .open(serving_path(root))
    else {
        return false;
    };
    match file.try_lock() {
        Ok(()) => false,
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

fn nanos(at: std::time::SystemTime) -> u128 {
    at.duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// Records, per site, the reload marker the host has now acted on.
fn acknowledge_reloads(root: &Path, stamps: &BTreeMap<String, std::time::SystemTime>) {
    for (site, stamp) in stamps {
        let path = root.join("sites").join(site).join("reload.ack");
        let _ = std::fs::write(path, nanos(*stamp).to_string());
    }
}

#[allow(clippy::too_many_arguments)]
async fn build_site(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    dev: bool,
    options: &Options,
    stores: &LoginStores,
    live: &Arc<SiteLive>,
    partitions: &Arc<services::partition::Host>,
    ops: Arc<AtomicUsize>,
    with_services: bool,
    modules: &'static [services::Registration],
) -> Result<(SiteRuntime, Option<Services>, Vec<&'static str>)> {
    // A rebuild (a reload) starts from the metadata the site serves now, not from the files
    // alone, or it would drop the DocTypes made at runtime.
    let serving = live.rebuild_from();
    let permission = permissions::site_serving(root, loaded, site, serving, &live.grants()).await?;
    let backend = crate::schema::backend(loaded, site).await?;
    // The participants write to tables that are not documents: edge sync's `spot_graph_edge` and
    // its drift tables, the mail sent log. They are created by the migrator, never here, so a site
    // whose database is behind refuses to serve and says how to fix it rather than writing a
    // document whose edges cannot be stored.
    let pending = crate::schema::pending_platform_tables(&backend, &permission.catalog).await?;
    if !pending.is_empty() {
        return Err(Error(format!(
            "site `{}`: the database is missing the platform tables {}; run `spot migrate --apply`",
            site.site,
            pending.join(", ")
        )));
    }
    // The audit store's tables are in the plan above; its role, grants and guard triggers are not
    // (a plan step cannot hold them), so a database whose chain tables exist without the guard
    // refuses to serve: an unguarded chain is not an identity trail an auditor can rely on.
    if let Some(missing) = crate::schema::audit_protection_gap(&backend).await? {
        return Err(Error(format!(
            "site `{}`: the audit chain is not fully protected (missing {missing}); run \
             `spot migrate --apply`",
            site.site
        )));
    }
    // The ownership registry's tables are the same kind of requirement: `spot-partition-pg`
    // creates them under its own advisory lock from `spot migrate` (its DDL carries triggers the
    // plan hook cannot hold), so a node whose database lacks them refuses to serve rather than
    // running an owner check it can never answer.
    if site.partition.is_some() {
        let missing = services::partition::missing_tables(&backend).await?;
        if !missing.is_empty() {
            return Err(Error(format!(
                "site `{}`: the database is missing the partition ownership tables {}; run \
                 `spot migrate --apply`",
                site.site,
                missing.join(", ")
            )));
        }
    }
    let site_id = SiteId::new(&site.site).map_err(error)?;
    let clock = options
        .clock
        .clone()
        .unwrap_or_else(|| Arc::new(SystemClock::new()));
    // The installed apps' native controllers: their pipeline steps go to the engine's seam
    // registry, their whitelisted methods to the engine, and the named permissions they require
    // are decided over this site's own snapshot (never shared with another site).
    let installed =
        services::controllers::build(root, loaded, site, &permission.meta, &permission.apps)?;
    live.set_degraded(services::controllers::degradation(&installed.unknown));
    // The secrets service (WP-SEC-03.T9): the key provider and the vault behind `secret` fields,
    // built from `[services.secrets]` before the engine so the engine seals in the same
    // transaction as the record. `None` when the host configures none; a table that is present
    // and unusable refuses the site here (fail closed, no default key, no plaintext fallback).
    let secrets = services::secrets::open(root, loaded, site).await?;
    if let Some(host) = &secrets {
        // Also on a reload: a key file swapped for an older copy must not be adopted silently.
        services::secrets::verify_values(host, &backend, &permission.catalog, &permission.meta)
            .await?;
    }
    // The committed phase also drives the files service's attach lifecycle: the wrapper delegates
    // to the delivery the engine already has and then marks the files a saved document names as
    // attached (and orphans the ones it no longer names, or a deleted document's). The service
    // does not exist yet, so it fills the slot below once it is built.
    let attach_slot = services::files::AttachSlot::new();
    let delivery = services::files::AttachDelivery::new(
        Arc::new(spot_document::engine::InProcessDelivery),
        attach_slot.clone(),
        live.clone(),
    );
    // `[services.idempotency]`, parsed once: the HTTP store's options and the command ledger's
    // retention (`ledger.retention`). Validated for every site, so an invalid section never
    // starts anywhere.
    let idempotency_settings = idempotency_settings(&site.site, loaded)?;
    let ledger_policy = ledger_policy(&site.site, &idempotency_settings.ledger.retention)?;
    let engine = Engine::new(
        backend.clone(),
        permission.meta.clone(),
        installed.seams.clone(),
        clock.clone(),
        Arc::new(V7Ids),
    )
    .map_err(error)?
    .with_delivery(delivery)
    .with_authorizer(permission.authorizer.clone())
    .with_app_permissions(Arc::new(spot_app_permissions::SnapshotAppPermissions::new(
        permission.authorizer.clone(),
    )))
    .with_controllers(installed.controllers.clone())
    // The command ledger (WP-DOC-04.T8) with the site's retention policy (WP-PLT-05.T9): read
    // here, before the engine serves a write, so an invalid section refuses the start.
    .with_command_ledger(ledger_policy);
    let engine = match &secrets {
        Some(host) => engine.with_secrets(host.vault()),
        None => engine,
    };
    // The participants every write passes through, in the one place that decides their order
    // (audit, then the owner check, then edge sync last). Installed before the engine serves a
    // single write, and rebuilt for every metadata generation by `SiteLive::swap_to`.
    let controllers = Arc::new(installed.registry);
    let meta_source = live_meta_source(live, &permission.meta);
    // The ownership registry of this site, from the host's one bridge per database. It answers
    // the owner check from a snapshot refreshed on a plain thread, so a write never crosses the
    // blocking bridge inside its own request (ADR-082 addendum A1 to A5).
    let partitions: Option<Arc<dyn spot_partition::PartitionRegistry>> = partitions
        .for_site(site, &backend, loaded.common.postgres.pool_max_size)?
        .map(|registry| registry as Arc<dyn spot_partition::PartitionRegistry>);
    // The document audit (WP-PRD-01.T22): the masker of this metadata generation, the lane
    // appenders over the chain and appender tables, the denial sink and the counters. A site whose
    // audit cannot be built does not serve: audit is never silently off.
    let audit_clock: Arc<dyn spot_app_api::prelude::Clock> = clock.clone();
    let masker =
        crate::audit::doc::masker_for(&permission.meta, &permission.decision, &audit_clock)?;
    let doc_audit = crate::audit::doc::DocAuditHost::new(
        &site_id,
        &backend,
        permission.catalog.clone(),
        audit_clock.clone(),
        &loaded.common.audit,
        masker.clone(),
    )?;
    engine.set_denial_sink(
        doc_audit.denial_sink(site.partition.as_ref().map(|profile| profile.node.as_str())),
    );
    live.set_doc_audit(doc_audit);
    let deps = services::participants::Deps {
        audit: Some(services::participants::Audit::new(&permission.meta, masker)),
        controllers: controllers.clone(),
        // A site with no `[partition]` gets `None` and installs no owner check; one with a
        // profile that somehow has no registry is refused (`cli.partition_registry_missing`).
        partitions: partitions.clone(),
        meta: meta_source,
    };
    let participants = services::participants::build(&site_id, site, &deps)?;
    live.remember_participants(participants.participants.clone());
    live.set_participants_factory({
        let live = live.clone();
        let site = site.clone();
        let site_id = site_id.clone();
        let audit_clock = audit_clock.clone();
        Arc::new(
            move |meta: &Arc<spot_meta::EffectiveMeta>,
                  decision: &Arc<spot_permissions::PermissionEngine>| {
                // The masker of the new generation, over the new metadata and its decision.
                let masker = crate::audit::doc::masker_for(meta, decision, &audit_clock)
                    .map_err(|e| e.to_string())?;
                let new_deps = services::participants::Deps {
                    audit: Some(services::participants::Audit::new(meta, masker)),
                    controllers: controllers.clone(),
                    partitions: partitions.clone(),
                    meta: live_meta_source(&live, meta),
                };
                services::participants::build(&site_id, &site, &new_deps)
                    .map(|installed| installed.participants)
                    .map_err(|e| e.to_string())
            },
        )
    });
    let participant_names = participants.names.clone();
    engine.set_participants(participants.participants);
    engine.prepare().await.map_err(error)?;
    let engine = Arc::new(engine);
    // The same engine, for the ledger prune and for the entry points' class handles.
    let ledger_engine = Arc::clone(&engine);
    live.attach(
        permission.meta.clone(),
        permission.model.clone(),
        permission.apps.clone(),
        permission.decision.clone(),
        engine.clone(),
        permission.authorizer.clone(),
        permission.principals.clone(),
    );
    // The one-time secrets of the site file. The sign-in service itself is built after the
    // modules registered, because the sign-in storage and the throttle are ports they provide
    // (WP-IAM-11.T24); the engine is attached above, before any module registers.
    let credentials = Arc::new(Credentials::load(root, site, clock.clone())?);
    let keys = SiteKeys::new(site.api_keys.clone());
    let mut keys = keys;
    let dev_token = if dev {
        let (key, stored) = ApiKey::generate();
        let user = match site.api_keys.first() {
            Some(key) => key.user.clone(),
            None => users::administrator(&backend, permission.catalog.clone())
                .await?
                .to_string(),
        };
        keys.push(config::Key {
            key_id: stored.key_id,
            digest: crate::auth::encode_digest(stored.digest.as_bytes()),
            user,
            expires_at: None,
        });
        Some(HeaderValue::from_str(&format!("Bearer {}", key.text())).map_err(error)?)
    } else {
        None
    };
    let guarded = Arc::new(GuardedEngine::new(
        engine,
        permission.authorizer.clone(),
        ops,
    ));
    // Files: one service per site for the life of the process (the memory provider keeps its
    // bytes in it); it asks the running site for the engine and the sign-in service.
    let files = {
        let settings = services::files::files_settings(root, loaded, site, live, &backend, &clock)?;
        live.files_with(|| services::files::Files::new(&settings))
    };
    attach_slot.fill(files.clone());
    // Print: the template folder, the fonts and the renderer, built once per site.
    let printing = services::print::print_settings(root, loaded, site, live, &clock)
        .map(|settings| live.print_with(|| services::print::Printing::new(&settings)));
    // Realtime: one hub per site that outlives a reload; its route is mounted outside the
    // bearer check because a socket authenticates with its first message.
    let realtime = if loaded.common.services.realtime.enabled {
        match live.realtime() {
            Some(hub) => Some(hub),
            None => services::realtime::Realtime::new(services::realtime::Settings {
                site: site_id.clone(),
                live: live.clone(),
                backend: backend.clone(),
                clock: clock.clone(),
                heartbeat_seconds: loaded.common.services.realtime.heartbeat_seconds,
                poll: std::time::Duration::from_millis(loaded.common.services.realtime.poll_ms),
            })
            .map(|hub| live.realtime_with(|| hub)),
        }
    } else {
        None
    };
    // The registry: the fixed core and every module add their services, ports, routes and open
    // paths to one per-site context (see `services::SiteCtx`).
    let mut ctx = services::SiteCtx::new(
        site.site.clone(),
        std::time::Duration::from_millis(loaded.common.services.shutdown_grace_ms),
        clock.clone(),
        loaded.common.services.services_extra.clone(),
        with_services,
    )
    .with_env(services::SiteEnv {
        root,
        loaded,
        site,
        permission: &permission,
        backend: &backend,
        live,
    });
    ctx.as_module("host", 0);
    // The parts the modules that start after the host may use.
    ctx.provide(files.clone())?;
    // The engine seen through a command-ledger retention class: a module that writes documents
    // asks for its class by name (`ledger_classes::BPM`, ...) and never passes a duration.
    ctx.provide(Arc::new(ledger_classes::EntryEngines::new(Arc::clone(
        &ledger_engine,
    ))))?;
    if let Some(hub) = &realtime {
        ctx.provide(hub.clone())?;
    }
    if let Some(host) = &secrets {
        ctx.provide(host.clone())?;
    }
    let extra = crate::live::extra_modules();
    // The cluster-redis plugin, built once and before every module (WP-PLT-19.T3). Its registry
    // entry runs at 1000, after jobs (600) and metadata (300), which build their coordinator and
    // bus first; so with `[services.cluster_redis] enabled = true` in the `cluster` profile the
    // host connects here, provides the coordinator, cache, bus and status as ports, adds the
    // supervisor, and leaves the module's own entry out of the table (one connection, one
    // `Status`, never a second coordinator). A bad setting or an unreachable server stops the
    // site: nothing falls back to PostgreSQL leases. Disabled, or another profile: the table is
    // untouched.
    let redis_early = if with_services && loaded.common.services.profile == "cluster" {
        let settings: services::cluster_redis::Settings =
            ctx.settings_or_default(services::cluster_redis::NAME)?;
        if settings.enabled {
            let site_id = SiteId::new(&site.site).map_err(error)?;
            let parts = services::cluster_redis::build(&settings, &site_id, clock.clone()).await?;
            ctx.as_module(
                services::cluster_redis::NAME,
                services::cluster_redis::ORDER,
            );
            ctx.provide(parts.coordinator)?;
            ctx.provide(parts.cache)?;
            ctx.provide(parts.bus)?;
            ctx.provide(parts.status)?;
            ctx.add(parts.service);
            ctx.as_module("host", 0);
            true
        } else {
            false
        }
    } else {
        false
    };
    let table: Vec<services::Registration> = services::REGISTRY
        .iter()
        .filter(|r| !(redis_early && r.name == services::cluster_redis::NAME))
        .copied()
        .collect();
    services::register_all(&mut ctx, &[builtin(), table.as_slice(), &extra, modules]).await?;
    // The sign-in service, built now because the throttle module (`signin_throttle`) provides the
    // admission port while it registers. Nothing before this point calls `live.auth()` except
    // inside a request or a readiness probe, so a request cannot reach a half-built site.
    // Production passes the coordinator admission, wrapped so every refusal is audited; a site
    // whose throttle is not hosted does not start, because without it nobody could be admitted
    // safely (fail closed, ADR-083 task 7).
    let storage = ctx.port::<login::SiteStorage>().ok_or_else(|| {
        Error(format!(
            "site `{}`: the sign-in stores are not hosted (see [services.login_stores]), so nobody can sign in; refusing to start",
            site.site
        ))
    })?;
    let admission = ctx
        .port::<dyn spot_login::SignInAdmission>()
        .ok_or_else(|| {
            Error(format!(
                "site `{}`: the sign-in throttle is not hosted (see [services.signin_throttle] and [services.secrets]), so no sign-in can be admitted; refusing to start",
                site.site
            ))
        })?;
    let admission: Arc<dyn spot_login::SignInAdmission> =
        Arc::new(crate::audit::AuditedAdmission::new(
            Arc::new(PortAdmission(admission)),
            stores.recorder.clone(),
        ));
    let signin = ctx
        .port::<services::signin_throttle::SigninThrottle>()
        .ok_or_else(|| {
            Error(format!(
                "site `{}`: the sign-in throttle handle is not hosted, so password changes cannot be admitted; refusing to start",
                site.site
            ))
        })?;
    let auth = Arc::new(
        SiteAuth::new(
            options.login_config(),
            &storage,
            credentials,
            keys.into_inner(),
            clock.clone(),
            stores,
            admission,
        )?
        .with_signin_throttle(signin),
    );
    live.set_auth(auth.clone());
    // The function and report delegates a hosting module provided (WP-PRD-01.T48). They are read
    // here, after every module registered and before the router is built, because the resource
    // adapter takes them at construction; absent stays absent, and the adapter then answers
    // `method.not_found` and `report.not_found` as it always did.
    let mut delegates = spot_api_services::Delegates::none();
    if let Some(function) = ctx.port::<dyn spot_api_services::FunctionDelegate>() {
        delegates = delegates.with_function(function);
    }
    if let Some(report) = ctx.port::<dyn spot_api_services::ReportDelegate>() {
        delegates = delegates.with_report(report);
    }
    // Metadata is read per request from what the site serves now, so a DocType made while
    // the server runs is served by the very next request.
    // The Idempotency-Key store: the site's own PostgreSQL table, or the in-memory
    // store for the dev switch and for a site that says so.
    let idempotency = idempotency_store(
        &mut ctx,
        &site.site,
        dev,
        &backend,
        &clock,
        loaded.server.request_timeout,
        &idempotency_settings,
    )
    .await?;
    // The command ledger's prune: once at start, then every ten minutes, stopped on shutdown.
    ctx.add(Arc::new(CommandLedgerPrune {
        site: site.site.clone(),
        engine: Arc::clone(&ledger_engine),
        clock: Arc::clone(&clock),
    }));
    let meta_service = Arc::new(MetaResources::live(live.meta_source()));
    let resources = Arc::new(LiveResources {
        live: live.clone(),
        engine: guarded.clone(),
        clock: clock.clone(),
        actor: Arc::new(|_| actor()),
        delegates,
        principals: permission.principals.clone(),
    });
    // The link lookup, with the title behind the field guard (WP-SEC-04.T14).
    let lookup = Arc::new(crate::live::LiveLookup {
        live: live.clone(),
        engine: guarded.clone(),
        clock: clock.clone(),
        actor: Arc::new(|_| actor()),
        principals: permission.principals.clone(),
    });
    let permissions = Arc::new(LivePermissions {
        live: live.clone(),
        principals: permission.principals.clone(),
        clock: clock.clone(),
        actor: Arc::new(|_| actor()),
    });
    // The configured public-route rate limits go on every router (WP-PRD-01.T53); the server
    // refuses an invalid value before it binds (`ServerError::InvalidSetting`).
    let rate_limits = loaded.server.rate_limits();
    // The report catalogue port a hosting module provided (WP-PLT-14.T5): the route registry
    // mounts `report_list` and `report_describe` once, behind the router's bearer layer. Absent
    // stays absent (the group is skipped and the operations answer as unserved ones do), and a
    // site with `services.report.enabled = false` provides none.
    let report_catalogue = ctx.port::<dyn spot_http::api::ports::report::ReportCatalogPort>();
    // The session port a hosting module provided (WP-IAM-07.T3): the route registry mounts
    // `auth_session_list` and `auth_session_end` once, behind the router's bearer layer. Absent
    // stays absent, and a site with `services.session.enabled = false` provides none.
    let session_port = ctx.port::<dyn spot_http::api::ports::session::SessionPort>();
    // One services bundle per audience router: a router consumes its bundle, and the two share
    // every service behind it.
    let make_services = || {
        let services = ApiServices::new(
            meta_service.clone(),
            resources.clone(),
            lookup.clone(),
            auth.clone(),
            idempotency.clone(),
        )
        .with_permissions(permissions.clone())
        .with_files(files.clone())
        .with_rate_limits(rate_limits);
        let services = match &report_catalogue {
            Some(catalogue) => services
                .with_port::<dyn spot_http::api::ports::report::ReportCatalogPort>(
                    catalogue.clone(),
                ),
            None => services,
        };
        let services = match &session_port {
            Some(sessions) => services
                .with_port::<dyn spot_http::api::ports::session::SessionPort>(sessions.clone()),
            None => services,
        };
        match &printing {
            Some(printing) => services.with_print(printing.clone()),
            None => services,
        }
    };
    // The ports a service provided for a registry route group go into the API ports; a group that
    // is left out is logged and named in readiness (WP-CTL-11.T5).
    let services = forward_ports(make_services(), &ctx);
    // The consumer audience's router: only the routes the table marks for consumers (none yet),
    // and none of the module routes, realtime or open paths, which are staff surfaces.
    let consumer_router = router_for(
        &[spot_core::Audience::Consumer],
        forward_ports(make_services(), &ctx),
    );
    let (site_router, mounted) = router_with(services, routes::REGISTRY);
    let mut degraded = live.degraded();
    degraded.extend(skipped_group_notes(&mounted));
    live.set_degraded(degraded);
    let guest_functions = ctx.port::<services::function::GuestFunctions>().is_some();
    let assembled = ctx.finish(site_router);
    // One access-log line, one span and the cost counters per request.
    let site_router = assembled
        .router
        .layer(spot_telemetry::TelemetryLayer::new());
    Ok((
        SiteRuntime::Ready {
            router: site_router,
            consumer: consumer_router,
            realtime: realtime.as_ref().map(|hub| hub.router()),
            auth,
            dev_token,
            principals: permission.principals.clone(),
            open: assembled.open,
            guest_functions,
        },
        assembled.assembly,
        participant_names,
    ))
}

/// How the route group of a registry entry reaches a site. This is the audit of WP-CTL-11.T5:
/// every group in `spot_http::api::router::routes::REGISTRY` appears here, so a new group cannot
/// be added without deciding how its port gets in (`tests/host_ports.rs` fails for a group that
/// is not listed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GroupServing {
    /// The host forwards the port a service provided ([`forward_ports`]); the registry mounts the
    /// group. Absent while that service is off, and then named in readiness.
    Forwarded,
    /// The providing service builds its own `Ports`, mounts the group itself and merges the
    /// router into the site (`ctx.router_layer`): forwarding its port as well would mount every
    /// route twice. The registry pass skips it by design.
    Service,
    /// No service hosts a provider of the group's port yet: the group is not served.
    NoProvider,
}

/// The audit table: the registry group, and how its port reaches the site.
const GROUPS: &[(&str, GroupServing)] = &[
    // Mounted by services/apps.rs.
    ("apps", GroupServing::Service),
    // Mounted by services/connectors.rs.
    ("connectors", GroupServing::Service),
    // `dyn ConsumerSessions`: no service provides it yet.
    ("consumer_renew", GroupServing::NoProvider),
    // `dyn EventStreamPort`: services/events.rs.
    ("events", GroupServing::Service),
    // Mounted by services/graph.rs.
    ("graph", GroupServing::Service),
    // Mounted by services/identity_admin.rs and services/key_admin.rs.
    ("identity", GroupServing::Service),
    // Mounted by services/import.rs.
    ("import", GroupServing::Service),
    // Mounted by services/invitations.rs.
    ("invitation_accept", GroupServing::Service),
    // Mounted by services/journal_access.rs.
    ("journal_access", GroupServing::Service),
    // `dyn LinkInboundPort`: services/link_external.rs provides it with `ctx.provide`.
    ("link_inbound", GroupServing::Forwarded),
    // `dyn NoticeSource`: no service provides it yet.
    ("notices", GroupServing::NoProvider),
    // no service provides it yet.
    ("occupancy", GroupServing::NoProvider),
    // `dyn ProcessPort` and `dyn IdempotencyStore`: services/process_api.rs.
    ("process", GroupServing::Service),
    // Mounted by services/process_versions.rs.
    ("process_version", GroupServing::Service),
    // `dyn ReportCatalogPort`: services/report.rs.
    ("report", GroupServing::Service),
    // `dyn ScimPort`: services/scim.rs.
    ("scim", GroupServing::Service),
    // `dyn SessionPort`: services/session.rs.
    ("session", GroupServing::Service),
    // Mounted by services/sso.rs.
    ("sso", GroupServing::Service),
    // Mounted by services/sso_admin.rs.
    ("sso_admin", GroupServing::Service),
    // `dyn TaskPort`: no service provides it yet.
    ("task", GroupServing::NoProvider),
    // `dyn WebhookPort`: services/process_triggers.rs.
    ("webhook", GroupServing::Service),
    // Mounted by services/webhook_secret.rs.
    ("webhook_secret", GroupServing::Service),
];

/// Puts into the API `Ports` the ports a service provided for a group the registry mounts (the
/// [`GroupServing::Forwarded`] rows of [`GROUPS`]). A port nobody provided stays absent, so its
/// group is skipped and the path is not served.
fn forward_ports(services: ApiServices, ctx: &services::SiteCtx<'_>) -> ApiServices {
    match ctx.port::<dyn LinkInboundPort>() {
        Some(port) => services.with_port::<dyn LinkInboundPort>(port),
        None => services,
    }
}

/// One line of readiness for each registry group that was skipped and ought to be served, and a
/// log line for each group left out. Never a value: group names and port type names only.
fn skipped_group_notes(report: &routes::MountReport) -> Vec<String> {
    let mut notes = Vec::new();
    for (group, reason) in &report.skipped {
        let serving = GROUPS
            .iter()
            .find(|(name, _)| name == group)
            .map(|(_, serving)| *serving);
        let why = match reason {
            routes::SkipReason::MissingPort(port) => {
                format!("its port `{port}` is not provided")
            }
            routes::SkipReason::ServesNothing => "it serves no operation".to_owned(),
            routes::SkipReason::DuplicateOperation(operation) => {
                format!("operation `{operation}` is already served")
            }
        };
        match serving {
            Some(GroupServing::Service) => {
                tracing::debug!(group = *group, "route group is mounted by its own service");
            }
            Some(GroupServing::NoProvider) => {
                tracing::info!(group = *group, reason = %why, "route group not served: no service provides its port");
            }
            Some(GroupServing::Forwarded) => {
                tracing::info!(group = *group, reason = %why, "route group not served");
                notes.push(format!("api group `{group}` is not served: {why}"));
            }
            None => {
                tracing::warn!(group = *group, reason = %why, "route group is not classified in host.rs, so its port is not forwarded");
                notes.push(format!(
                    "api group `{group}` is not served: {why} (not classified in host.rs)"
                ));
            }
        }
    }
    notes
}

/// The throttle's admission port as a sized type, so `AuditedAdmission` (generic over a sized
/// admission) can wrap it.
struct PortAdmission(Arc<dyn spot_login::SignInAdmission>);

impl spot_login::SignInAdmission for PortAdmission {
    fn admit<'a>(
        &'a self,
        attempt: spot_login::SignInAttempt<'a>,
    ) -> spot_login::BoxFuture<
        'a,
        std::result::Result<spot_login::Admission, spot_login::AdmissionError>,
    > {
        self.0.admit(attempt)
    }
}

/// A small wrapper so dev can push a verifier before the store is built.
struct SiteKeys(Vec<config::Key>);

impl SiteKeys {
    fn new(keys: Vec<config::Key>) -> Self {
        Self(keys)
    }
    fn push(&mut self, key: config::Key) {
        self.0.push(key);
    }
    fn into_inner(self) -> Vec<config::Key> {
        self.0
    }
}

/// The framework's own modules, registered like any other: the orders are the documented
/// start order (see [`services::Registration`]). Their construction still lives here, because
/// the files that own the services (`src/services/jobs.rs`, `mail.rs` and the rest) are outside
/// this change; moving one body into its own file, with a `pub const ORDER` and a
/// `pub async fn register`, removes its entry from this table and changes nothing else.
/// Participants (500) are built before the engine exists, so they stay in `build_site`.
const BUILTIN: &[services::Registration] = &[
    services::Registration {
        name: "storage",
        order: 100,
        register: reg_storage,
    },
    services::Registration {
        name: "auth",
        order: 200,
        register: reg_auth,
    },
];

fn builtin() -> &'static [services::Registration] {
    BUILTIN
}

macro_rules! register_fn {
    ($wrapper:ident, $body:ident) => {
        fn $wrapper<'a, 'b>(ctx: &'a mut services::SiteCtx<'b>) -> services::RegisterFuture<'a> {
            Box::pin($body(ctx))
        }
    };
}

register_fn!(reg_storage, storage);
register_fn!(reg_auth, auth);

/// Storage: the pool answers, or the site cannot serve a single request.
async fn storage(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    let backend = ctx.env()?.backend.clone();
    ctx.add(services::Idle::new("storage", Level::Required, move || {
        let backend = backend.clone();
        Box::pin(async move {
            match backend.health().await.status {
                HealthStatus::Healthy => Ok(()),
                other => Err(format!("the database reports {other:?}")),
            }
        })
    }));
    Ok(())
}

/// Authentication. Credentials live in the site database since WP-PLT-02.T8 (`login_stores`
/// imports the staged ones from `site.toml` and removes them from the file), so an empty site
/// file is the normal state after the first start: this check no longer counts the file. The
/// sign-in stores' own required check (`login_stores`) covers a store that does not answer, so
/// nobody can sign in only when that check fails.
async fn auth(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    ctx.add(services::Idle::new("auth", Level::Required, move || {
        Box::pin(async move { Ok(()) })
    }));
    Ok(())
}

/// How often the prune of expired Idempotency-Key rows runs (and once at start).
const IDEMPOTENCY_PRUNE_EVERY: std::time::Duration = std::time::Duration::from_secs(10 * 60);

/// `[services.idempotency]` of a site (WP-PLT-05.T8): where the HTTP `Idempotency-Key` rows live
/// and how long a stored response is replayed.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct IdempotencySettings {
    /// Keep the keys in memory on a production site too: they do not survive a restart
    ///, so a site that sets this says so in writing.
    allow_memory_idempotency: bool,
    /// How long a stored response is replayed.
    retention: RetentionSettings,
    /// The document engine's command ledger (WP-PLT-05.T9).
    ledger: LedgerSettings,
}

/// `[services.idempotency.ledger]`: the command ledger of the document engine.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct LedgerSettings {
    /// `[services.idempotency.ledger.retention]`: `default` (7 days), `max` (30 days) and
    /// `[...retention.classes]`, a duration per caller class.
    retention: RetentionSettings,
}

/// `[services.idempotency.retention]`: durations like `"24h"` or `"7d"`; absent keys take the
/// documented defaults (24 hours, capped at 7 days).
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RetentionSettings {
    default: Option<String>,
    max: Option<String>,
    classes: BTreeMap<String, String>,
}

/// A whole number of seconds, minutes, hours, days or weeks (`"90s"`, `"15m"`, `"24h"`, `"7d"`,
/// `"2w"`). Zero, a missing unit, text and an overflow are `None`.
fn retention_duration(text: &str) -> Option<std::time::Duration> {
    let text = text.trim();
    let (digits, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let count: u64 = digits.parse().ok()?;
    let unit = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        "w" => 604_800,
        _ => return None,
    };
    count
        .checked_mul(unit)
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
}

/// The retention policy of `site`: a refused setting stops the start and names the site and the
/// key, never a value of a request; a class longer than `max` is clamped and logged once.
fn retention_policy(
    site: &str,
    settings: &RetentionSettings,
) -> Result<spot_idempotency_pg::RetentionPolicy> {
    let bad = |key: &str| {
        Error(format!(
            "site `{site}`: services.idempotency.retention.{key} is not a valid duration \
             (a whole number with s, m, h, d or w, like \"24h\")"
        ))
    };
    let one = |key: &str, text: &Option<String>, fallback: std::time::Duration| match text {
        Some(text) => retention_duration(text).ok_or_else(|| bad(key)),
        None => Ok(fallback),
    };
    let default = one(
        "default",
        &settings.default,
        spot_idempotency_pg::DEFAULT_RETENTION,
    )?;
    let max = one(
        "max",
        &settings.max,
        spot_idempotency_pg::DEFAULT_MAX_RETENTION,
    )?;
    let mut classes = BTreeMap::new();
    for (name, text) in &settings.classes {
        let keep = retention_duration(text).ok_or_else(|| bad(&format!("classes.{name}")))?;
        classes.insert(name.clone(), keep);
    }
    let policy =
        spot_idempotency_pg::RetentionPolicy::new(default, max, classes.clone()).map_err(|e| {
            Error(format!(
                "site `{site}`: services.idempotency.retention: {e}"
            ))
        })?;
    for (name, keep) in &classes {
        if *keep > max {
            tracing::info!(
                site,
                class = %name,
                max_secs = max.as_secs(),
                "the retention of an idempotency class was clamped to the maximum"
            );
        }
    }
    Ok(policy)
}

/// The Idempotency-Key store of one site: `PgIdempotencyStore` on the site's own pool (table
/// prepared here; a failure stops the start naming the site and a recorded security finding), with the prune
/// service added; or `MemoryIdempotencyStore` for the dev switch and for a site that sets
/// `allow_memory_idempotency = true`.
async fn idempotency_store(
    ctx: &mut services::SiteCtx<'_>,
    site: &str,
    dev: bool,
    backend: &PgBackend,
    clock: &Arc<dyn spot_core::Clock>,
    request_timeout: std::time::Duration,
    settings: &IdempotencySettings,
) -> Result<Arc<dyn spot_http::api::idempotency::IdempotencyStore>> {
    // Validated for every site, so an invalid section never starts anywhere.
    let retention = retention_policy(site, &settings.retention)?;
    if dev || settings.allow_memory_idempotency {
        tracing::warn!(
            site,
            "Idempotency-Keys are kept in memory and do not survive a restart"
        );
        return Ok(Arc::new(MemoryIdempotencyStore::new(Arc::clone(clock))));
    }
    // A pending reservation outlives the longest request: twice the request timeout, at least
    // 120 seconds.
    let lease = (request_timeout * 2).max(std::time::Duration::from_secs(120));
    let config = spot_idempotency_pg::PgIdempotencyConfig::new(lease, retention);
    let store =
        spot_idempotency_pg::PgIdempotencyStore::new(backend.pool(), Arc::clone(clock), config)
            .map_err(|e| {
                Error(format!(
                    "site `{site}`: the idempotency store cannot start: {e}"
                ))
            })?;
    store.prepare().await.map_err(|_| {
        Error(format!(
            "site `{site}`: the idempotency table cannot be created"
        ))
    })?;
    let store = Arc::new(store);
    ctx.add(Arc::new(IdempotencyPrune {
        store: store.clone(),
    }));
    Ok(store)
}

/// Deletes expired Idempotency-Key rows: once at start, then every ten minutes. The store runs
/// no timer of its own, so the host owns this one and stops it on shutdown.
struct IdempotencyPrune {
    store: Arc<dyn spot_http::api::idempotency::IdempotencyStore>,
}

impl IdempotencyPrune {
    /// Rounds of 1,000 rows, at most ten per tick.
    async fn rounds(&self) {
        for _ in 0..10 {
            match self.store.prune(1_000).await {
                Ok(deleted) if deleted >= 1_000 => continue,
                Ok(_) => break,
                Err(_) => {
                    tracing::warn!(
                        "the idempotency prune could not run; the next tick tries again"
                    );
                    break;
                }
            }
        }
    }
}

impl Service for IdempotencyPrune {
    fn name(&self) -> &'static str {
        "idempotency"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(IDEMPOTENCY_PRUNE_EVERY);
            loop {
                tokio::select! {
                    _ = tick.tick() => self.rounds().await,
                    _ = stop.changed() => break,
                }
            }
            Ok(())
        })
    }
}

/// The retention classes the host gives the entry points (WP-PLT-05.T9). A site may add any
/// other class name in its configuration; these are the ones the platform itself uses. The
/// entry points that take their handle from here are wired by the tasks that own those modules
/// (jobs, scripts, BPM, import), so some items have no caller yet.
#[allow(dead_code)]
pub(crate) mod ledger_classes {
    use std::sync::Arc;

    use spot_app_api::context::{DocumentService, Invocation, Outcome};
    use spot_app_api::prelude::{RecordId, Version};
    use spot_app_api::{AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch};
    use spot_document::engine::Engine;
    use spot_postgres::PgBackend;

    /// The native API router.
    pub(crate) const HTTP_API: &str = "http_api";
    /// The BPM Document node.
    pub(crate) const BPM: &str = "bpm";
    /// The import back half.
    pub(crate) const IMPORT: &str = "import";
    /// The job runner.
    pub(crate) const JOBS: &str = "jobs";
    /// The script runtime.
    pub(crate) const SCRIPTS: &str = "scripts";

    /// The site's engine, from which each entry point takes a handle under its own class. A
    /// class the site's policy does not know records under the default retention (the engine
    /// decides that). The host never passes a duration, only the class name.
    pub(crate) struct EntryEngines {
        engine: Arc<Engine<PgBackend>>,
    }

    impl EntryEngines {
        pub(crate) fn new(engine: Arc<Engine<PgBackend>>) -> Self {
            Self { engine }
        }

        /// The document service every write of which is recorded under `class`.
        pub(crate) fn service(&self, class: &'static str) -> Arc<ClassedEngine> {
            Arc::new(ClassedEngine {
                engine: Arc::clone(&self.engine),
                class,
            })
        }
    }

    /// An owned handle on the engine under one retention class: `Engine::for_class` borrows the
    /// engine, an entry point that outlives the call needs one that does not.
    pub(crate) struct ClassedEngine {
        engine: Arc<Engine<PgBackend>>,
        class: &'static str,
    }

    impl ClassedEngine {
        /// The retention class of this handle.
        pub(crate) fn class(&self) -> &'static str {
            self.class
        }
    }

    impl DocumentService for ClassedEngine {
        fn meta<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
            Box::pin(self.engine.meta(doctype))
        }

        fn load<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(self.engine.load(inv, doctype, id))
        }

        fn create<'a>(
            &'a self,
            inv: &'a Invocation,
            doc: Document,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(async move { self.engine.for_class(self.class).create(inv, doc).await })
        }

        fn update<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            patch: DocumentPatch,
            expected: Version,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(async move {
                self.engine
                    .for_class(self.class)
                    .update(inv, doctype, id, patch, expected)
                    .await
            })
        }

        fn post<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            expected: Version,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(async move {
                self.engine
                    .for_class(self.class)
                    .post(inv, doctype, id, expected)
                    .await
            })
        }

        fn cancel<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            expected: Version,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(async move {
                self.engine
                    .for_class(self.class)
                    .cancel(inv, doctype, id, expected)
                    .await
            })
        }

        fn correct<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            expected: Version,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(async move {
                self.engine
                    .for_class(self.class)
                    .correct(inv, doctype, id, expected)
                    .await
            })
        }

        fn delete<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            expected: Version,
        ) -> BoxFuture<'a, AppResult<Outcome<()>>> {
            Box::pin(async move {
                self.engine
                    .for_class(self.class)
                    .delete(inv, doctype, id, expected)
                    .await
            })
        }

        fn rename<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            new_name: &'a str,
            expected: Version,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(async move {
                self.engine
                    .for_class(self.class)
                    .rename(inv, doctype, id, new_name, expected)
                    .await
            })
        }

        fn call_method<'a>(
            &'a self,
            inv: &'a Invocation,
            doctype: &'a str,
            id: RecordId,
            method: &'a str,
            args: serde_json::Value,
            expected: Option<Version>,
        ) -> BoxFuture<'a, AppResult<Outcome<(Document, serde_json::Value)>>> {
            Box::pin(
                self.engine
                    .call_method(inv, doctype, id, method, args, expected),
            )
        }

        fn field_changed<'a>(
            &'a self,
            inv: &'a Invocation,
            draft: Document,
            field: &'a str,
        ) -> BoxFuture<'a, AppResult<Outcome<Document>>> {
            Box::pin(self.engine.field_changed(inv, draft, field))
        }
    }
}

/// `[services.idempotency]` of `site`, decoded once. An error names the table and, for an
/// unknown or missing key, the key; it never echoes a value.
fn idempotency_settings(site: &str, loaded: &config::Loaded) -> Result<IdempotencySettings> {
    let Some(table) = loaded.common.services.services_extra.get("idempotency") else {
        return Ok(IdempotencySettings::default());
    };
    table.clone().try_into::<IdempotencySettings>().map_err(|e| {
        let message = e.message();
        if message.starts_with("unknown field") || message.starts_with("missing field") {
            Error(format!("site `{site}`: services.idempotency: {message}"))
        } else {
            Error(format!(
                "site `{site}`: services.idempotency: a setting has the wrong type or an invalid value"
            ))
        }
    })
}

/// A caller class name: a lowercase letter, then lowercase letters, digits and `_`, at most 48.
fn class_name_is_valid(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 48
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// The command ledger's retention policy of `site` (`[services.idempotency.ledger.retention]`).
///
/// A refused setting stops the start and names the site and the key, never a value; a class
/// longer than `max` is clamped by the engine and logged once at `info`. Durations use the
/// host's own parser ([`retention_duration`]): zero, text, a missing unit and an overflow refuse.
fn ledger_policy(
    site: &str,
    settings: &RetentionSettings,
) -> Result<spot_document::engine::RetentionPolicy> {
    let at = |name: &str| format!("site `{site}`: services.idempotency.ledger.retention.{name}");
    let bad = |name: &str| {
        Error(format!(
            "{} is not a valid duration (a whole number above zero with s, m, h, d or w, like \"7d\")",
            at(name)
        ))
    };
    let one = |name: &str, text: &Option<String>, fallback: std::time::Duration| match text {
        Some(text) => retention_duration(text).ok_or_else(|| bad(name)),
        None => Ok(fallback),
    };
    let default = one(
        "default",
        &settings.default,
        spot_document::engine::DEFAULT_RETENTION,
    )?;
    let max = one(
        "max",
        &settings.max,
        spot_document::engine::DEFAULT_MAX_RETENTION,
    )?;
    let mut classes = BTreeMap::new();
    for (name, text) in &settings.classes {
        if !class_name_is_valid(name) {
            return Err(Error(format!(
                "{} is not a valid class name (lowercase letters, digits and _, starting with a letter)",
                at(&format!("classes.{name}"))
            )));
        }
        let keep = retention_duration(text).ok_or_else(|| bad(&format!("classes.{name}")))?;
        classes.insert(name.clone(), keep);
    }
    let policy =
        spot_document::engine::RetentionPolicy::new(default, max, classes).map_err(|e| {
            Error(format!(
                "site `{site}`: services.idempotency.ledger.retention: {e}"
            ))
        })?;
    for name in policy.clamped_classes() {
        tracing::info!(
            site,
            class = %name,
            max_secs = max.as_secs(),
            "the command ledger retention of a class was clamped to the maximum"
        );
    }
    Ok(policy)
}

/// Deletes command ledger rows whose own `retain_until` has passed: once at start, then every
/// ten minutes from the host's `Clock` (never the system clock). The engine runs no timer of its
/// own, so the host owns this one and stops it on shutdown.
struct CommandLedgerPrune {
    site: String,
    engine: Arc<Engine<PgBackend>>,
    clock: Arc<dyn spot_core::Clock>,
}

impl CommandLedgerPrune {
    /// One prune. `prune_commands` is a single call (it does not batch), so one per tick.
    async fn once(&self) {
        match self.engine.prune_commands(self.clock.now_utc()).await {
            Ok(0) => {}
            Ok(pruned) => {
                tracing::debug!(site = %self.site, pruned, "expired command ledger rows were removed");
            }
            Err(_) => {
                tracing::warn!(
                    site = %self.site,
                    "the command ledger prune could not run; the next tick tries again"
                );
            }
        }
    }
}

impl Service for CommandLedgerPrune {
    fn name(&self) -> &'static str {
        "command_ledger"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: tokio::sync::watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut tick = tokio::time::interval(IDEMPOTENCY_PRUNE_EVERY);
            loop {
                tokio::select! {
                    _ = tick.tick() => self.once().await,
                    _ = stop.changed() => break,
                }
            }
            Ok(())
        })
    }
}

/// Where edge sync reads the effective metadata: the site's live handle, which every swap points
/// at the metadata being served, and the generation this runtime was built from as the fallback
/// for the moment a node is diverged from its peers (it serves nothing then).
fn live_meta_source(
    live: &Arc<SiteLive>,
    generation: &Arc<spot_meta::EffectiveMeta>,
) -> Arc<dyn spot_edge_sync::MetaSource> {
    let live = live.clone();
    let generation = generation.clone();
    Arc::new(move || {
        live.current()
            .map(|served| Arc::clone(&served.meta))
            .unwrap_or_else(|_| Arc::clone(&generation))
    })
}

/// A request on a consumer listener: the site's consumer router and nothing else. The staff
/// special cases of [`dispatch`] (sign-in, password change, realtime, metrics, the open paths and
/// the module routes) do not exist here: a path the consumer router does not mount is the
/// unserved envelope, whatever the staff router would have done with it.
async fn consumer_dispatch(request: Request, runtimes: RuntimeMap) -> Response {
    let context = request.extensions().get::<SiteContext>().cloned();
    let request_id = context
        .as_ref()
        .map(|c| c.request_id.clone())
        .or_else(|| request.extensions().get::<RequestId>().cloned())
        .unwrap_or_else(RequestId::generate);
    let Some(context) = context else {
        return unavailable(
            "the site context is missing",
            &request_id,
            "site_unavailable",
        );
    };
    let runtime = {
        let guard = runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.get(context.site.as_str()).cloned()
    };
    match runtime {
        Some(SiteRuntime::Ready { consumer, .. }) => {
            match consumer.oneshot(scrubbed(request)).await {
                Ok(response) => response,
                Err(never) => match never {},
            }
        }
        Some(SiteRuntime::Unavailable { detail }) => {
            unavailable(&detail, &request_id, "site_unavailable")
        }
        None => unavailable("no such site", &request_id, "site_unavailable"),
    }
}

async fn dispatch(
    mut request: Request,
    runtimes: RuntimeMap,
    metrics: config::Metrics,
) -> Response {
    let context = request.extensions().get::<SiteContext>().cloned();
    let request_id = context
        .as_ref()
        .map(|c| c.request_id.clone())
        .or_else(|| request.extensions().get::<RequestId>().cloned())
        .unwrap_or_else(RequestId::generate);
    let Some(context) = context else {
        return unavailable(
            "the site context is missing",
            &request_id,
            "site_unavailable",
        );
    };
    let runtime = {
        let guard = runtimes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.get(context.site.as_str()).cloned()
    };
    let Some(runtime) = runtime else {
        return unavailable("no such site", &request_id, "site_unavailable");
    };
    let (router, auth, dev_token, principals, guest_functions) = match runtime {
        SiteRuntime::Ready {
            router,
            consumer: _,
            realtime,
            auth,
            dev_token,
            principals,
            open,
            guest_functions,
        } => {
            // The maintenance gate (ADR-22) comes first: while it is closed no request reaches
            // sign-in, an open path, the realtime upgrade or the router (probes are exempt).
            if let Some(refused) = services::maintenance_gate::refuse(
                context.site.as_str(),
                request.method(),
                request.uri().path(),
                request_id.as_str(),
            ) {
                return refused;
            }
            // A socket authenticates with its first message, so the upgrade itself carries no
            // bearer. Nothing else passes this way, and the route refuses anything that is
            // not a `spot.v1` upgrade.
            if request.uri().path() == "/api/v1/realtime"
                && let Some(realtime) = realtime
            {
                return match realtime.oneshot(request).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                };
            }
            // The explicit allow-list of routes a module declared reachable without a session
            // (a public webhook, an inbound connector): the exact method and pattern, rate
            // limited and audited. Everything else falls through to the authenticate-first
            // rule below, unchanged.
            request = match open.serve(scrubbed(request), &router).await {
                Ok(response) => return response,
                Err(request) => *request,
            };
            (router, auth, dev_token, principals, guest_functions)
        }
        SiteRuntime::Unavailable { detail } => {
            return unavailable(&detail, &request_id, "site_unavailable");
        }
    };
    if request.uri().path() == "/api/v1/auth/login" {
        return login_response(&auth, &context, request).await;
    }
    // A scrape from a co-located collector: metrics are on only when configured, and the
    // credential rule is the operator's (`require_auth = false` is the loopback default for
    // a local Prometheus; the host still resolves the site, so the numbers are the site's).
    if request.uri().path() == "/metrics" && metrics.enabled && !metrics.require_auth {
        return services::telemetry::metrics().await;
    }
    // The SCIM endpoint authenticates every request itself, as a SCIM key, and answers in the
    // SCIM error format: the native credential check below does not run in front of it.
    if crate::audit::PathClass::of_path(request.uri().path()) == crate::audit::PathClass::Scim {
        return match router.oneshot(scrubbed(request)).await {
            Ok(response) => response,
            Err(never) => match never {},
        };
    }
    // The dev credential is the one exception to the password-change gate below: it exists only
    // under `--dev` on loopback, lives in memory, and is never the one-time secret.
    let mut dev_credential = false;
    if !request.headers().contains_key(header::AUTHORIZATION)
        && let Some(token) = &dev_token
    {
        request
            .headers_mut()
            .insert(header::AUTHORIZATION, token.clone());
        dev_credential = true;
    }
    let Some(ctx) = request_ctx(&context, request.headers()) else {
        // A function call that carries no `Authorization` header at all goes on to the router
        // with no actor: the function service dispatches it as an anonymous caller, which
        // reaches only functions declared `guest` (WP-PLT-17.T3). A header that is present but
        // is not a usable bearer credential is never a guest: it is refused here, as before.
        if guest_functions && is_anonymous_function_call(&request) {
            let mut request = scrubbed(request);
            request.extensions_mut().insert(spot_http::api::GuestCall);
            return match router.oneshot(request).await {
                Ok(response) => response,
                Err(never) => match never {},
            };
        }
        return render(ApiError::from_code(Code::AuthRequired), &request_id);
    };
    let path_class = crate::audit::PathClass::of_path(request.uri().path());
    let principal = match auth.authenticate_on(&ctx, path_class).await {
        Ok(principal) => principal,
        Err(error) => return render(error, &request_id),
    };
    // The password-change gate, the only one: a caller that has not chosen a password yet can
    // change it, sign out and read its session, and nothing else. It runs before the password
    // handler and before the router, so no route can forget it.
    if !dev_credential
        && auth.is_restricted(&principal)
        && !gate::allowed_while_restricted(request.method(), request.uri().path())
    {
        return gate::refusal(request_id.as_str());
    }
    if request.uri().path() == gate::PASSWORD_PATH {
        return password_response(&auth, &ctx, request).await;
    }
    // The counters are process-wide, so `/metrics` is an operator surface: only an
    // administrator of the site this request resolved to (never a header the caller sent)
    // reads it. The refusal is the one an unauthorised API call gets and goes through the
    // router's telemetry layer, so it is counted without logging who asked.
    let metrics_access = request.uri().path() == "/metrics"
        && metrics.enabled
        && metrics.require_auth
        && is_site_administrator(&principals, principal.user_id).await;
    let actor = Actor::User {
        id: principal.user_id,
        via: None,
    };
    let site = request.extensions().get::<SiteContext>().cloned();
    let id = request.extensions().get::<RequestId>().cloned();
    request.extensions_mut().clear();
    if let Some(site) = site {
        request.extensions_mut().insert(site);
    }
    if let Some(id) = id {
        request.extensions_mut().insert(id);
    }
    if metrics_access {
        request
            .extensions_mut()
            .insert(services::telemetry::MetricsAccess);
    }
    REQUEST_ACTOR
        .scope(actor, async move {
            match router.oneshot(request).await {
                Ok(response) => response,
                Err(never) => match never {},
            }
        })
        .await
}

/// Whether `request` is `GET|POST /api/v1/method/{app}/{module}/{function}` with no
/// `Authorization` header: the one request that may proceed with no credential (a guest function
/// call). Any header, even an empty or malformed one, makes it a request with a credential that
/// has to resolve.
fn is_anonymous_function_call(request: &Request) -> bool {
    if request.headers().contains_key(header::AUTHORIZATION) {
        return false;
    }
    if !matches!(request.method().as_str(), "GET" | "POST") {
        return false;
    }
    let Some(rest) = request.uri().path().strip_prefix("/api/v1/method/") else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    parts.len() == 3 && parts.iter().all(|part| !part.is_empty())
}

/// `request` with every extension dropped except the site context and the request id, the only
/// two the host itself put there.
fn scrubbed(mut request: Request) -> Request {
    let site = request.extensions().get::<SiteContext>().cloned();
    let id = request.extensions().get::<RequestId>().cloned();
    request.extensions_mut().clear();
    if let Some(site) = site {
        request.extensions_mut().insert(site);
    }
    if let Some(id) = id {
        request.extensions_mut().insert(id);
    }
    request
}

/// Whether `user` holds `Tenant Administrator` (the role that holds every right) on this site,
/// by an assignment with no scope restriction: "Tenant Administrator for Company B" is not an
/// administrator of the site. An unknown user has no assignments and so is not one.
async fn is_site_administrator(principals: &Arc<dyn PrincipalSource>, user: RecordId) -> bool {
    let principal = principals.resolve(&user).await;
    principal.assignments.iter().any(|assignment| {
        assignment.scope.is_empty()
            && assignment
                .roles
                .iter()
                .any(|role| role.0 == spot_platform_app::role::TENANT_ADMINISTRATOR)
    })
}

fn request_ctx(context: &SiteContext, headers: &HeaderMap) -> Option<RequestCtx> {
    Some(RequestCtx {
        site: context.site.clone(),
        credential: Some(header_credential(headers)?),
        idempotency_key: None,
        language: language_of(headers),
        request_id: context.request_id.clone(),
        peer: context.peer,
    })
}

fn header_credential(headers: &HeaderMap) -> Option<Credential> {
    let raw = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let token = raw.strip_prefix("Bearer ")?.trim();
    (!token.is_empty()).then(|| Credential::new(token))
}

fn language_of(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(header::ACCEPT_LANGUAGE)
        .and_then(|v| v.to_str().ok())?;
    let first = raw.split(',').next()?.trim();
    (!first.is_empty()).then(|| first.to_owned())
}

fn render(error: ApiError, request_id: &RequestId) -> Response {
    // A throttled or temporarily unavailable answer says when to come back: whole seconds, at
    // least one (a `Retry-After: 0` invites a hot retry loop).
    let wait = spot_http::api::storage_error::retry_after_secs(&error);
    let mut response = (
        error.status(),
        axum::Json(error.to_envelope_json(request_id.as_str())),
    )
        .into_response();
    if let Some(seconds) = wait {
        response.headers_mut().insert(
            header::RETRY_AFTER,
            axum::http::HeaderValue::from(seconds.max(1)),
        );
    }
    response
}

fn unavailable(_detail: &str, request_id: &RequestId, reason: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(unavailable_body(request_id.as_str(), reason)),
    )
        .into_response()
}

/// The envelope of [`unavailable`]: rendered by `ApiError` like every other error, so
/// `retryable` is the registry's and `request_id` is inside `error`. The reason a site is
/// unavailable (a configuration or database error) goes to the operator at start-up and on
/// reload; an unauthenticated client only ever sees the fixed `reason` token.
fn unavailable_body(request_id: &str, reason: &str) -> Json {
    ApiError::new(Code::ServerUnavailable, "This site is not available.")
        .detail("reason", json!(reason))
        .to_envelope_json(request_id)
}

fn peer_source(request: &Request) -> String {
    request
        .extensions()
        .get::<axum::extract::connect_info::ConnectInfo<std::net::SocketAddr>>()
        .map(|info| info.0.ip().to_string())
        .unwrap_or_default()
}

fn session_body(session: &spot_http::api::service::Session) -> Json {
    json!({
        "token": session.token,
        "expires_at": session.expires_at,
        "user": {
            "id": session.user.id.to_string(),
            "email": session.user.email,
            "full_name": session.user.full_name,
        },
    })
}

async fn login_response(auth: &SiteAuth, context: &SiteContext, request: Request) -> Response {
    let request_id = context.request_id.clone();
    let headers = request.headers().clone();
    // The peer address is the throttle's source, so one client cannot spray many accounts.
    let source = peer_source(&request);
    let bytes = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return render(
                ApiError::new(Code::ApiInvalidRequest, "The login body is too large."),
                &request_id,
            );
        }
    };
    let body: Json = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return render(
                ApiError::new(Code::ApiInvalidJson, "The login body is not JSON."),
                &request_id,
            );
        }
    };
    let Some(user) = body.get("user").and_then(Json::as_str) else {
        return render(
            ApiError::new(Code::ApiInvalidRequest, "`user` is required."),
            &request_id,
        );
    };
    let Some(password) = body.get("password").and_then(Json::as_str) else {
        return render(
            ApiError::new(Code::ApiInvalidRequest, "`password` is required."),
            &request_id,
        );
    };
    let login = LoginBody {
        user: user.to_owned(),
        password: password.to_owned(),
        totp: body.get("totp").and_then(Json::as_str).map(str::to_owned),
        recovery_code: body
            .get("recovery_code")
            .and_then(Json::as_str)
            .map(str::to_owned),
    };
    let ctx = RequestCtx {
        site: context.site.clone(),
        credential: header_credential(&headers),
        idempotency_key: None,
        language: language_of(&headers),
        request_id: request_id.clone(),
        peer: context.peer,
    };
    let (session, first) = match auth.login_with_source(&ctx, &login, &source).await {
        Ok(result) => result,
        Err(error) => return render(error, &request_id),
    };
    let mut envelope = serde_json::Map::new();
    envelope.insert("data".to_owned(), session_body(&session));
    if first {
        envelope.insert(
            "messages".to_owned(),
            Json::Array(vec![json!({
                "level": "warning",
                "code": concat!("auth", ".password_change_required"),
                "message": "Choose a password to finish the first sign-in.",
            })]),
        );
    }
    (StatusCode::OK, axum::Json(Json::Object(envelope))).into_response()
}

async fn password_response(auth: &SiteAuth, ctx: &RequestCtx, request: Request) -> Response {
    let request_id = ctx.request_id.clone();
    let source = peer_source(&request);
    let bytes = match axum::body::to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(_) => {
            return render(
                ApiError::new(Code::ApiInvalidRequest, "The body is too large."),
                &request_id,
            );
        }
    };
    let body: Json = match serde_json::from_slice(&bytes) {
        Ok(body) => body,
        Err(_) => {
            return render(
                ApiError::new(Code::ApiInvalidJson, "The body is not JSON."),
                &request_id,
            );
        }
    };
    let (Some(current), Some(new)) = (
        body.get("current").and_then(Json::as_str),
        body.get("new").and_then(Json::as_str),
    ) else {
        return render(
            ApiError::new(Code::ApiInvalidRequest, "`current` and `new` are required."),
            &request_id,
        );
    };
    match auth.change_password(ctx, current, new, &source).await {
        // A first sign-in's restricted session was replaced: the new session is the answer.
        Ok(Some(session)) => (
            StatusCode::OK,
            axum::Json(json!({ "data": session_body(&session) })),
        )
            .into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => render(error, &request_id),
    }
}

/// The reload task: a portable signal file (`spot reload`) polled every 500 ms, plus SIGHUP on
/// Unix. Rebuilding a site keeps its session and throttle stores.
#[allow(clippy::too_many_arguments)]
fn spawn_reload(
    root: PathBuf,
    dev: bool,
    options: Options,
    stores: BTreeMap<String, LoginStores>,
    lives: BTreeMap<String, Arc<SiteLive>>,
    partitions: Arc<services::partition::Host>,
    runtimes: RuntimeMap,
    ops: Arc<AtomicUsize>,
    modules: &'static [services::Registration],
) {
    let seen = Arc::new(Mutex::new(reload_stamps(&root)));
    #[cfg(unix)]
    {
        let seen = seen.clone();
        let runtimes = runtimes.clone();
        let stores = stores.clone();
        let lives = lives.clone();
        let partitions = partitions.clone();
        let root = root.clone();
        let options = options.clone();
        let ops = ops.clone();
        tokio::spawn(async move {
            let mut signal =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
                    Ok(signal) => signal,
                    Err(_) => return,
                };
            while signal.recv().await.is_some() {
                reload(
                    &root,
                    dev,
                    &options,
                    &stores,
                    &lives,
                    &partitions,
                    &runtimes,
                    &ops,
                    &seen,
                    modules,
                )
                .await;
            }
        });
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let stamps = reload_stamps(&root);
            let changed = {
                let last = seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                stamps
                    .iter()
                    .filter(|(site, stamp)| last.get(*site) != Some(*stamp))
                    .map(|(site, _)| site.clone())
                    .collect::<Vec<_>>()
            };
            if !changed.is_empty() {
                reload(
                    &root,
                    dev,
                    &options,
                    &stores,
                    &lives,
                    &partitions,
                    &runtimes,
                    &ops,
                    &seen,
                    modules,
                )
                .await;
            }
        }
    });
}

fn reload_stamps(root: &Path) -> BTreeMap<String, std::time::SystemTime> {
    let mut stamps = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(root.join("sites")) else {
        return stamps;
    };
    for entry in entries.flatten() {
        let path = entry.path().join("reload");
        if let Ok(meta) = std::fs::metadata(&path)
            && let Ok(modified) = meta.modified()
            && let Some(site) = entry.file_name().to_str()
        {
            stamps.insert(site.to_owned(), modified);
        }
    }
    stamps
}

#[allow(clippy::too_many_arguments)]
async fn reload(
    root: &Path,
    dev: bool,
    options: &Options,
    stores: &BTreeMap<String, LoginStores>,
    lives: &BTreeMap<String, Arc<SiteLive>>,
    partitions: &Arc<services::partition::Host>,
    runtimes: &RuntimeMap,
    ops: &Arc<AtomicUsize>,
    seen: &Arc<Mutex<BTreeMap<String, std::time::SystemTime>>>,
    modules: &'static [services::Registration],
) {
    let Ok(loaded) = config::load(root, dev) else {
        eprintln!("reload skipped: configuration did not validate");
        return;
    };
    let stamps = reload_stamps(root);
    let mut rebuilt = Vec::new();
    for site in &loaded.sites {
        let (Some(site_stores), Some(site_live)) = (stores.get(&site.site), lives.get(&site.site))
        else {
            continue;
        };
        // A reload rebuilds the serving runtime only: the site's services keep running (their
        // metadata follower and job worker are per process, not per router).
        match build_site(
            root,
            &loaded,
            site,
            dev,
            options,
            site_stores,
            site_live,
            partitions,
            ops.clone(),
            false,
            modules,
        )
        .await
        {
            Ok((runtime, _, _)) => rebuilt.push((site.site.clone(), runtime)),
            Err(e) => {
                eprintln!("reload refused for site `{}`: {e}", site.site);
                rebuilt.push((
                    site.site.clone(),
                    SiteRuntime::Unavailable {
                        detail: e.to_string(),
                    },
                ));
            }
        }
    }
    {
        let mut guard = runtimes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for (site, runtime) in rebuilt {
            guard.insert(site, runtime);
        }
    }
    // The new runtimes are in front of requests: now the metadata they were built with is served.
    for site_live in lives.values() {
        site_live.commit();
    }
    acknowledge_reloads(root, &stamps);
    *seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = stamps;
}

/// Touches the site's reload marker. `spot reload` is the portable trigger for a running host.
pub fn request_reload(root: &Path, site: &str) -> Result<()> {
    let dir = root.join("sites").join(site);
    if !dir.is_dir() {
        return Err(Error(format!("no site `{site}`")));
    }
    let path = dir.join("reload");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    use std::io::Write as _;
    writeln!(file, "{}", time::OffsetDateTime::now_utc())?;
    file.sync_all()?;
    Ok(())
}

/// [`request_reload`], then waits (up to ten seconds) until a host that serves `root` has reloaded
/// the site, so a credential an operator command changed is usable when the command returns.
/// Returns at once when no host serves the root.
pub fn request_reload_and_wait(root: &Path, site: &str) -> Result<()> {
    request_reload(root, site)?;
    if !host_serving(root) {
        return Ok(());
    }
    let dir = root.join("sites").join(site);
    let Some(wanted) = std::fs::metadata(dir.join("reload"))
        .ok()
        .and_then(|m| m.modified().ok())
        .map(nanos)
    else {
        return Ok(());
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        let acked = std::fs::read_to_string(dir.join("reload.ack"))
            .ok()
            .and_then(|text| text.trim().parse::<u128>().ok());
        if acked.is_some_and(|at| at >= wanted) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod envelope_tests {
    use super::*;
    use std::collections::BTreeSet;

    /// The keys inside `error`, apart from the optional ones.
    fn required_keys(body: &Json) -> BTreeSet<String> {
        body["error"]
            .as_object()
            .expect("an error object")
            .keys()
            .filter(|k| !matches!(k.as_str(), "details" | "fields" | "explanation"))
            .cloned()
            .collect()
    }

    #[test]
    fn the_site_unavailable_body_is_the_envelope_with_the_fixed_reason() {
        let body = unavailable_body("req-1", "site_unavailable");
        let error = body["error"].as_object().expect("an error object");
        assert_eq!(error["code"], "server.unavailable");
        assert_eq!(error["message"], "This site is not available.");
        assert_eq!(
            error["retryable"],
            ApiError::from_code(Code::ServerUnavailable).retryable()
        );
        assert_eq!(error["request_id"], "req-1");
        assert_eq!(error["details"]["reason"], "site_unavailable");
        assert!(
            body.get("request_id").is_none(),
            "no top-level request_id: {body}"
        );
    }

    #[tokio::test]
    async fn the_three_hand_built_refusals_share_one_error_shape() {
        let telemetry = services::telemetry::refusal("req-1");
        let bytes = axum::body::to_bytes(telemetry.into_body(), 1 << 16)
            .await
            .expect("a body");
        let telemetry: Json = serde_json::from_slice(&bytes).expect("json");
        let gate = gate::refusal_body("req-1");
        let unavailable = unavailable_body("req-1", "site_unavailable");
        let shape = required_keys(&telemetry);
        assert_eq!(
            required_keys(&gate),
            shape,
            "gate::refusal drifted from the envelope"
        );
        assert_eq!(
            required_keys(&unavailable),
            shape,
            "unavailable drifted from the envelope"
        );
        for key in ["code", "message", "retryable", "request_id"] {
            assert!(shape.contains(key), "{key}");
        }
    }
}
