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
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::service::{
    ApiError, BoxFuture, Credential, LoginBody, LookupService, RequestCtx,
};
use spot_http::api::{
    idempotency::MemoryIdempotencyStore,
    registry::Code,
    router::{ApiServices, router},
};
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_http::request::RequestId;
use spot_http::site::SiteContext;
use spot_http::{ServerBuilder, ServerHandle, StaticSites};
use spot_login::LoginConfig;
use spot_postgres::PgBackend;
use spot_storage_contract::{HealthStatus, RecordId, StorageBackend};
use tower::ServiceExt;

use crate::services::{self, Level, Services};

use crate::gate;
use crate::live::{LivePermissions, LiveReady, LiveResources, SiteLive};
use crate::login::{self, Credentials, LoginStores, SiteAuth, SiteDirectory};
use crate::permissions::{self, GuardedEngine};
use crate::{Error, Result, appdir, apps, config, error, users};

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

struct NotConfigured;

impl LookupService for NotConfigured {
    fn search<'a>(
        &'a self,
        _: &'a RequestCtx,
        _: &'a str,
        _: &'a str,
        _: &'a LookupRequest,
    ) -> BoxFuture<'a, std::result::Result<LookupPage, ApiError>> {
        Box::pin(async {
            Err(ApiError::new(
                Code::ApiInvalidRequest,
                "No lookup provider is configured.",
            ))
        })
    }
}

#[derive(Clone)]
enum SiteRuntime {
    Ready {
        router: Router,
        /// The realtime socket route, authenticated by its own first message.
        realtime: Option<Router>,
        auth: Arc<SiteAuth>,
        dev_token: Option<HeaderValue>,
        /// Who holds which roles on this site: `/metrics` is for its administrators.
        principals: Arc<dyn PrincipalSource>,
        /// The routes modules declared reachable without a session.
        open: Arc<services::OpenPaths>,
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
    let loaded = config::load(root, dev)?;
    if loaded.sites.is_empty() {
        return Err(Error("no sites configured".into()));
    }
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
                readiness.push(assembly.health_check());
                // What the site's participants are, and whether it is a partitioned node: `ok`
                // always, because a site that cannot check ownership refuses to start instead.
                participant_checks.push(services::participants::ParticipantsReady::new(
                    &site.site,
                    site.partition.as_ref(),
                    &participant_names,
                ));
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
    let mut builder = ServerBuilder::new(loaded.server, {
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
    let handle = builder
        .site_routes(dispatch)
        .start(shutdown)
        .await
        .map_err(error)?;
    spawn_reload(
        root.to_path_buf(),
        dev,
        options,
        stores,
        lives.clone(),
        partitions,
        runtimes,
        ops.clone(),
    );
    Ok(Running {
        handle,
        ops,
        services: assemblies,
        lives,
    })
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
    let engine = Engine::new(
        backend.clone(),
        permission.meta.clone(),
        installed.seams.clone(),
        clock.clone(),
        Arc::new(V7Ids),
    )
    .map_err(error)?
    .with_authorizer(permission.authorizer.clone())
    .with_app_permissions(Arc::new(spot_app_permissions::SnapshotAppPermissions::new(
        permission.authorizer.clone(),
    )))
    .with_controllers(installed.controllers.clone());
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
    let deps = services::participants::Deps {
        // No audit chain yet: `spot-audit-store` publishes no DDL the migrator can plan and no
        // durable appender store, so an audit event written here would never be chained and would
        // be pruned with the feed retention. The site installs no audit participant and readiness
        // says `docaudit: off`; see the task's open questions.
        audit: None,
        controllers: controllers.clone(),
        // A site with no `[partition]` gets `None` and installs no owner check; one with a
        // profile that somehow has no registry is refused (`cli.partition_registry_missing`).
        partitions: partitions.clone(),
        meta: meta_source,
    };
    let participants = services::participants::build(&site_id, site, &deps)?;
    live.set_participants_factory({
        let live = live.clone();
        let site = site.clone();
        let site_id = site_id.clone();
        Arc::new(move |meta: &Arc<spot_meta::EffectiveMeta>| {
            let new_deps = services::participants::Deps {
                audit: None,
                controllers: controllers.clone(),
                partitions: partitions.clone(),
                meta: live_meta_source(&live, meta),
            };
            services::participants::build(&site_id, &site, &new_deps)
                .map(|installed| installed.participants)
                .map_err(|e| e.to_string())
        })
    });
    let participant_names = participants.names.clone();
    engine.set_participants(participants.participants);
    engine.prepare().await.map_err(error)?;
    let engine = Arc::new(engine);
    live.attach(
        permission.meta.clone(),
        permission.model.clone(),
        permission.apps.clone(),
        permission.decision.clone(),
        engine.clone(),
        permission.authorizer.clone(),
        permission.principals.clone(),
    );
    let credentials = Arc::new(Credentials::load(root, site, clock.clone())?);
    let directory = Arc::new(SiteDirectory::new(
        backend.clone(),
        permission.catalog.clone(),
        credentials.clone(),
    ));
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
    let auth = Arc::new(SiteAuth::new(
        options.login_config(),
        directory,
        credentials,
        keys.into_inner(),
        clock.clone(),
        stores,
    )?);
    live.set_auth(auth.clone());
    let guarded = Arc::new(GuardedEngine::new(
        engine,
        permission.authorizer.clone(),
        ops,
    ));
    // Files: one service per site for the life of the process (the memory provider keeps its
    // bytes in it); it asks the running site for the engine and the sign-in service.
    let files = {
        let settings = files_settings(root, loaded, site, live, &backend, &clock)?;
        live.files_with(|| services::files::Files::new(&settings))
    };
    // Print: the template folder, the fonts and the renderer, built once per site.
    let printing = print_settings(root, loaded, site, live, &clock)
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
    if let Some(hub) = &realtime {
        ctx.provide(hub.clone())?;
    }
    if let Some(host) = &secrets {
        ctx.provide(host.clone())?;
    }
    let extra = crate::live::extra_modules();
    services::register_all(&mut ctx, &[builtin(), services::REGISTRY, &extra]).await?;
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
    let services = ApiServices::new(
        Arc::new(MetaResources::live(live.meta_source())),
        Arc::new(LiveResources {
            live: live.clone(),
            engine: guarded,
            clock: clock.clone(),
            actor: Arc::new(|_| actor()),
            delegates,
        }),
        Arc::new(NotConfigured),
        auth.clone(),
        Arc::new(MemoryIdempotencyStore::new(Arc::clone(&clock))),
    )
    .with_permissions(Arc::new(LivePermissions {
        live: live.clone(),
        principals: permission.principals.clone(),
        clock: clock.clone(),
        actor: Arc::new(|_| actor()),
    }))
    .with_files(files);
    let services = match &printing {
        Some(printing) => services.with_print(printing.clone()),
        None => services,
    };
    let site_router = router(services);
    let assembled = ctx.finish(site_router);
    // One access-log line, one span and the cost counters per request.
    let site_router = assembled
        .router
        .layer(spot_telemetry::TelemetryLayer::new());
    Ok((
        SiteRuntime::Ready {
            router: site_router,
            realtime: realtime.as_ref().map(|hub| hub.router()),
            auth,
            dev_token,
            principals: permission.principals.clone(),
            open: assembled.open,
        },
        assembled.assembly,
        participant_names,
    ))
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
    services::Registration {
        name: "metadata",
        order: 300,
        register: reg_metadata,
    },
    services::Registration {
        name: "customise",
        order: 400,
        register: reg_customise,
    },
    services::Registration {
        name: "jobs",
        order: 600,
        register: reg_jobs,
    },
    services::Registration {
        name: "mail",
        order: 700,
        register: reg_mail,
    },
    services::Registration {
        name: "realtime",
        order: 800,
        register: reg_realtime,
    },
    services::Registration {
        name: "files",
        order: 900,
        register: reg_files,
    },
    services::Registration {
        name: "print",
        order: 900,
        register: reg_print,
    },
    services::Registration {
        name: "telemetry",
        order: 900,
        register: reg_telemetry,
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
register_fn!(reg_metadata, metadata);
register_fn!(reg_customise, customise);
register_fn!(reg_jobs, jobs);
register_fn!(reg_mail, mail);
register_fn!(reg_realtime, realtime);
register_fn!(reg_files, files);
register_fn!(reg_print, print);
register_fn!(reg_telemetry, telemetry);

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

/// Authentication: at least one credential exists, or nobody can sign in.
async fn auth(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    let site = ctx.env()?.site;
    let keys = site.api_keys.len();
    let credentials = site.credentials.len();
    ctx.add(services::Idle::new("auth", Level::Required, move || {
        Box::pin(async move {
            if keys + credentials == 0 {
                return Err("no API key and no password verifier is configured".into());
            }
            Ok(())
        })
    }));
    Ok(())
}

/// Metadata: the live snapshot, its version store and the additive schema plan. Adopting a
/// new version rebuilds the site runtime through this marker, which is the same path
/// `spot reload` takes. Provides the `LiveMetadata` port.
async fn metadata(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let catalog = crate::schema::catalog(&env.permission.meta)?;
    let notify: Arc<dyn Fn() + Send + Sync> = {
        let root = env.root.to_path_buf();
        let name = env.site.site.clone();
        Arc::new(move || {
            if let Err(e) = request_reload(&root, &name) {
                eprintln!("metadata reload refused for site `{name}`: {e}");
            }
        })
    };
    let metadata = services::metadata::LiveMetadata::new(services::metadata::Settings {
        root: env.root.to_path_buf(),
        loaded: env.loaded.clone(),
        site: env.site.clone(),
        backend: Some(env.backend.clone()),
        catalog: Some(catalog),
        notify,
        clock: Arc::clone(ctx.clock()),
    })
    .await?;
    ctx.add(metadata.clone());
    ctx.provide(metadata)
}

/// Runtime customisation: DocTypes, custom fields and property overrides made while the
/// site runs, served at once and followed by the other nodes. Its check recovers a change a
/// crash left half done, before the site accepts a request.
async fn customise(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let metadata = ctx.require_port::<services::metadata::LiveMetadata>()?;
    let service = services::customise::Customise::new(services::customise::Settings {
        live: env.live.clone(),
        metadata,
        backend: env.backend.clone(),
        catalog: env.permission.catalog.clone(),
        principals: env.permission.principals.clone(),
        site: env.site.site.clone(),
        poll: std::time::Duration::from_millis(env.loaded.common.services.metadata.poll_ms),
    })?;
    ctx.add(service);
    Ok(())
}

/// Jobs, intake and schedules. The queue and the coordinator are durable: jobs live in the
/// site's database (a killed node loses none), and the cluster profile coordinates through
/// the database too. The single-node profile keeps the in-process coordinator, which
/// refuses a second node in this process, and may run the memory store when `common.toml`
/// says so.
///
/// The worker needs every handler the other modules contribute (mail adds `send_mail`), so the
/// services are built in a finaliser that runs after all modules registered, and placed at
/// this module's order.
async fn jobs(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let (root, loaded, site, backend) = (env.root, env.loaded, env.site, env.backend);
    let clock = Arc::clone(ctx.clock());
    let services_config = &loaded.common.services;
    let node = process_node(&site.site);
    let coordinator: Arc<dyn spot_coordinator::Coordinator> =
        if services_config.profile == "cluster" {
            let coordinator = spot_coordinator_pg::PgCoordinator::new(
                backend.pool(),
                spot_coordinator_pg::PgCoordinatorConfig::default(),
            );
            coordinator.prepare().await.map_err(error)?;
            Arc::new(coordinator)
        } else {
            let node_id = spot_coordinator::NodeId::new(node.clone()).map_err(error)?;
            spot_coordinator::InProcessCoordinator::start(
                Arc::new(spot_coordinator::InMemoryPresence::new()),
                node_id,
                Arc::clone(&clock),
            )
            .await
            .map_err(error)?
        };
    let store: Arc<dyn spot_jobs::JobStore> = if services_config.jobs.store == "postgres" {
        let store = spot_jobs_pg::PgJobStore::new(backend.pool());
        store.prepare().await.map_err(error)?;
        Arc::new(store)
    } else {
        services::jobs::Settings::memory_store()
    };
    let schedules = manifest_schedules(root, loaded, site)?;
    // The narrow ports later modules take (job submit, outbox lease, system reader); each is
    // built from this site's own store, coordinator, feed and engine.
    services::ports::provide(
        ctx,
        services::ports::PortParts {
            site: SiteId::new(&site.site).map_err(error)?,
            store: Arc::clone(&store),
            queues: services_config.jobs.queues.clone(),
            clock: Arc::clone(&clock),
            coordinator: Arc::clone(&coordinator),
            feed: backend.clone(),
            node: node.clone(),
            ttl: std::time::Duration::from_millis(services_config.jobs.lease_ttl_ms),
            loader: Arc::new(services::ports::LiveLoader(Arc::clone(env.live))),
        },
    )?;
    let settings = services::jobs::Settings {
        site: SiteId::new(&site.site).map_err(error)?,
        backend: backend.clone(),
        store,
        queues: services_config.jobs.queues.clone(),
        intake_interval: std::time::Duration::from_millis(services_config.jobs.intake_poll_ms),
        grace: std::time::Duration::from_millis(services_config.jobs.grace_ms),
        node,
        clock: Arc::clone(&clock),
        coordinator,
        consumer_ttl: std::time::Duration::from_millis(services_config.jobs.lease_ttl_ms),
        schedules,
        tick_interval: std::time::Duration::from_millis(services_config.scheduler.tick_ms),
        lease_ttl: std::time::Duration::from_millis(services_config.scheduler.lease_ttl_ms),
    };
    crate::natives::add_job_handlers(ctx.job_handlers());
    ctx.finally(move |ctx| {
        let handlers = Arc::new(ctx.take_job_handlers());
        ctx.add(services::jobs::Jobs::new(&settings, handlers));
        ctx.add(services::jobs::Scheduling::new(&settings));
        Ok(())
    });
    Ok(())
}

/// Mail: the sender over the site's settings, the site's file store and (with the durable
/// job store) the site's PostgreSQL sent log. Optional, so a server that cannot be reached
/// degrades the site, not stops it.
///
/// The sent log is PostgreSQL's by default whenever the job store is: a restart between the
/// server's acceptance and the job's completion must not send a message twice. The table is
/// `spot_mail_sent_log`, a platform table `spot migrate` knows; `prepare()` creates it when
/// missing and is safe to repeat.
async fn mail(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let (site, backend, live) = (env.site, env.backend, env.live);
    let services_config = &env.loaded.common.services;
    tracing::info!(
        site = %site.site,
        "{}",
        services_config
            .mail
            .sent_log_description(&services_config.jobs.store)
    );
    let sent_log: Option<Arc<dyn spot_mail_queue::SentLog>> = if services_config
        .mail
        .sent_log_in_postgres(&services_config.jobs.store)
    {
        let log = spot_mail_queue_pg::PgSentLog::new(backend.pool());
        log.prepare().await.map_err(error)?;
        Some(Arc::new(log))
    } else {
        None
    };
    let files = ctx.require_port::<services::files::Files>()?;
    let mail_site = SiteId::new(&site.site).map_err(error)?;
    let clock = Arc::clone(ctx.clock());
    let mail = live.mail_with(|| {
        if sent_log.is_none() {
            // Once per process (this closure runs for the first build only).
            tracing::warn!(
                site = %site.site,
                "the mail sent log is in memory: a restart can send an accepted message again \
                 (use the PostgreSQL job store, or services.mail.sent_log = \"postgres\")"
            );
        }
        services::mail::Mail::new(services::mail::Settings {
            site: mail_site,
            mail: services_config.mail.clone(),
            files: files.store(),
            sent_log,
            clock,
        })
    });
    mail.register(ctx.job_handlers());
    ctx.add(mail.service());
    Ok(())
}

/// Realtime: optional, so a feed that cannot be read degrades the site, not stops it.
async fn realtime(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    if let Some(hub) = ctx.env()?.live.realtime() {
        ctx.add(hub.service());
    }
    Ok(())
}

/// Files: the provider's folder takes writes; the cluster profile cannot serve without it.
async fn files(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    let env = *ctx.env()?;
    let settings = files_settings(
        env.root,
        env.loaded,
        env.site,
        env.live,
        env.backend,
        ctx.clock(),
    )?;
    ctx.add(services::files::FilesCheck::new(&settings));
    Ok(())
}

/// Print: optional, so a renderer that cannot make a page degrades the site, not stops it.
async fn print(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    let live = ctx.env()?.live;
    ctx.add(services::print::PrintCheck::new(
        live.print(),
        Arc::clone(ctx.clock()),
    ));
    Ok(())
}

/// Metrics: the `/metrics` route (when enabled) and the optional recorder service, so a
/// recorder that cannot install degrades the site, not stops it.
async fn telemetry(ctx: &mut services::SiteCtx<'_>) -> Result<()> {
    let metrics = ctx.env()?.loaded.common.services.metrics.clone();
    if metrics.enabled {
        ctx.router_layer(|router| router.merge(services::telemetry::router()));
    }
    ctx.add(services::telemetry::Telemetry::new(
        metrics.enabled,
        metrics.require_auth,
    ));
    Ok(())
}
/// What the file service of `site` is built from.
fn files_settings(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    live: &Arc<SiteLive>,
    backend: &PgBackend,
    clock: &Arc<dyn spot_core::Clock>,
) -> Result<services::files::Settings> {
    let files = &loaded.common.services.files;
    Ok(services::files::Settings {
        site: SiteId::new(&site.site).map_err(error)?,
        live: live.clone(),
        backend: backend.clone(),
        clock: clock.clone(),
        provider: files.provider.clone(),
        root: root.join("sites").join(&site.site).join(&files.root),
        max_bytes: files.max_bytes,
        required: loaded.common.services.profile == "cluster",
    })
}

/// What the print service of `site` is built from; `None` when printing is switched off.
fn print_settings(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    live: &Arc<SiteLive>,
    clock: &Arc<dyn spot_core::Clock>,
) -> Option<services::print::Settings> {
    let print = &loaded.common.services.print;
    if !print.enabled {
        return None;
    }
    Some(services::print::Settings {
        site: SiteId::new(&site.site).ok()?,
        live: live.clone(),
        clock: clock.clone(),
        templates: root
            .join("sites")
            .join(&site.site)
            .join(&print.template_root),
    })
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

/// This host's identity in leases and worker claims: the site, and a random part chosen at
/// every start, so a second host on the same site never shares a holder name with this one
/// (not even inside one process).
fn process_node(site: &str) -> String {
    let id = spot_storage_contract::RecordId::new_v7().to_string();
    let part = id.rsplit('-').next().unwrap_or(&id);
    format!("node-{site}-{part}")
}

/// Every manifest schedule of the site's installed apps, with the policy a missed run gets.
fn manifest_schedules(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
) -> Result<
    Vec<(
        Arc<spot_scheduler::Schedule>,
        spot_scheduler::MissedRunPolicy,
    )>,
> {
    let catalogue = apps::catalogue(root, loaded)?;
    let mut schedules = Vec::new();
    for app in &site.apps {
        let Some(source) = catalogue.iter().find(|source| &source.name == app) else {
            continue;
        };
        let manifest = appdir::manifest(&source.dir)?;
        let zone = spot_scheduler::TimeZone::parse("UTC")
            .map_err(|e| Error(format!("{}: {e}", source.dir.display())))?;
        for sub in &manifest.schedules {
            let schedule = spot_scheduler::Schedule::new(sub, zone.clone())
                .map_err(|e| Error(format!("{}: {e}", source.dir.display())))?;
            // A run missed while no node led is skipped, not replayed: a catch-up burst on a
            // restarted node is worse than one missed tick for every schedule this platform
            // ships today.
            schedules.push((Arc::new(schedule), spot_scheduler::MissedRunPolicy::Skip));
        }
    }
    Ok(schedules)
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
    let (router, auth, dev_token, principals) = match runtime {
        SiteRuntime::Ready {
            router,
            realtime,
            auth,
            dev_token,
            principals,
            open,
        } => {
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
            (router, auth, dev_token, principals)
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
    (
        error.status(),
        axum::Json(error.to_envelope_json(request_id.as_str())),
    )
        .into_response()
}

fn unavailable(_detail: &str, request_id: &RequestId, reason: &str) -> Response {
    // The reason a site is unavailable (a configuration or database error) goes to the operator
    // at start-up and on reload; it never reaches an unauthenticated client.
    let body = json!({
        "error": {
            "code": "server.unavailable",
            "message": "This site is not available.",
            "details": { "reason": reason },
        },
        "request_id": request_id.as_str(),
    });
    (StatusCode::SERVICE_UNAVAILABLE, axum::Json(body)).into_response()
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
