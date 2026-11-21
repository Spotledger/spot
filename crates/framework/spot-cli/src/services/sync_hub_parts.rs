//! The hub's parts: the module that hands the sync hub what it needs (WP-RTB-02.T13; ADR-062
//! section 3). Guide: `docs/running-spot/sync_hub.md`.
//!
//! `sync_hub` (WP-RTB-02.T4) is the listener; it is handed one [`HubParts`] port. This module, on a
//! `role = "hub"` site, builds it from the site's database, on the plain thread the blocking
//! stores need:
//!
//! | Part | From |
//! | --- | --- |
//! | node directory | `spot_sync_enrol::DurableDirectory` over the enrolment store (WP-RTB-02.T3), loaded once before anything is served, then refreshed by [`DirectoryRefresh`] |
//! | enrolment endpoint | `spot_sync_hub_host::enrol` over `EnrolService::redeem` ([`EnrolRoute`] port) |
//! | `last_seen` | the hub's node registry (`spot-sync-hub-pg`), written from a queue on its own thread |
//! | relay core, tenant sinks, metadata epoch | a [`RelayAssembly`] port (the module that knows the site's streams, images and policy: WP-RTB-02.T1 and T8) |
//! | refusal audit | the log (a durable audit is owed: the site has no audit chain yet) |
//!
//! Besides [`HubParts`] and [`EnrolRoute`] the module provides two ports for the modules that read
//! what it built: `DurableDirectory` (its snapshot lists the enrolled nodes and their status, which
//! the edge desk's roster uses) and `PgHub` (the registry, relay state and outbox reads, which the
//! catch-up feed uses). Neither carries a secret.
//!
//! # Fail closed
//!
//! Every part is required. A part this module cannot build (a table missing: run `spot migrate`; a
//! port nobody provided; a directory that could not load) is a start-up failure that names the
//! part, never a default: a hub with no directory would authenticate nobody, one with an empty
//! default would be worse.
//!
//! # Settings
//!
//! ```toml
//! [services.sync_hub_parts]
//! hub_name = "hub"              # the hub's node id is "<site>#<hub_name>"
//! directory_refresh_secs = 2    # 1 to 5: the directory refuses everyone after 30 s of failures
//! enrol_per_second = 5          # the enrolment endpoint's whole-endpoint throttle
//! enrol_queued = 8              # redemptions that may wait for the database
//! ```
//!
//! The catalogue of the hub's settings is `apps/platform/sync_hub_host.toml`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use serde::Deserialize;
use spot_core::Clock;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_sync_enrol::{
    DirectoryConfig, DurableDirectory, EnrolError, EnrolService, EnrolmentStore, Ids,
    PgEnrolmentStore, RandomSource,
};
use spot_sync_http::NodeDirectory;
use spot_sync_hub_host::enrol::{EnrolSettings, Redeemer, Refusal, enrol_router};
use spot_sync_hub_host::{HubCore, RefusalAudit, SeenSink, TenantSinks};
use spot_sync_hub_pg::{PgHub, PgNodeRegistry};
use spot_sync_protocol::NodeId;
use tokio::sync::watch;

use crate::services::sync_hub::HubParts;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name and its `[services.<name>]` table.
pub const NAME: &str = "sync_hub_parts";
/// After `node_role` (1005) and before `sync_hub` (1300), which takes the port.
pub const ORDER: u32 = 1250;
/// Hub only (the row in `node_role::SERVICE_SETS`).
pub const ROLES: &[&str] = &["hub"];

/// Stable code: a part of the hub could not be built.
pub const PART_UNAVAILABLE: &str = "sync_hub_parts.part_unavailable";
/// Stable code: a part of the hub was not provided by any module.
pub const PART_MISSING: &str = "sync_hub_parts.part_missing";

/// What the module that knows the site's streams, images and policy provides: the relay core (the
/// `HubRelay` over the hub's PostgreSQL queue, with its destinations and the redaction and policy
/// of WP-RTB-02.T8), the per-tenant push sinks and the metadata epoch the hub advertises.
pub trait RelayAssembly: Send + Sync + 'static {
    /// The metadata epoch the hub advertises at `hello`.
    fn metadata_epoch(&self) -> u64;

    /// The push sink of each tenant this hub serves.
    fn sinks(&self) -> Arc<TenantSinks>;

    /// Builds the relay core over the hub's durable stores. Called once, on a plain thread, after
    /// the hub's tables were checked.
    ///
    /// # Errors
    /// A message naming what could not be built (never a value).
    fn core(&self, hub: &PgHub) -> std::result::Result<Box<dyn HubCore>, String>;
}

/// The enrolment route, as a port: `sync_hub` merges it into the listener's router **outside** the
/// client-certificate requirement, once.
pub struct EnrolRoute {
    router: Mutex<Option<Router>>,
}

impl EnrolRoute {
    /// Takes the route (once).
    pub fn take(&self) -> Option<Router> {
        self.router
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// `[services.sync_hub_parts]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The hub's name inside its site: its node id is `<site>#<hub_name>`.
    pub hub_name: Option<String>,
    /// Seconds between directory refreshes (1 to 5).
    pub directory_refresh_secs: Option<u64>,
    /// Enrolment requests per second, whole endpoint (1 to 100).
    pub enrol_per_second: Option<u32>,
    /// Enrolment requests that may wait for the database (1 to 64).
    pub enrol_queued: Option<usize>,
}

/// The settings with defaults applied and bounds checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The hub's name inside its site.
    pub hub_name: String,
    /// The directory refresh pause.
    pub refresh: Duration,
    /// The enrolment endpoint's limits.
    pub enrol: EnrolSettings,
}

impl Settings {
    /// Applies the defaults and checks every bound.
    ///
    /// # Errors
    /// [`Error`] naming the setting that is out of bounds.
    pub fn resolve(&self) -> Result<Resolved> {
        let refresh = self.directory_refresh_secs.unwrap_or(2);
        if !(1..=5).contains(&refresh) {
            return Err(Error(
                "services.sync_hub_parts.directory_refresh_secs must be between 1 and 5".to_owned(),
            ));
        }
        let per_second = self.enrol_per_second.unwrap_or(5);
        if !(1..=100).contains(&per_second) {
            return Err(Error(
                "services.sync_hub_parts.enrol_per_second must be between 1 and 100".to_owned(),
            ));
        }
        let queued = self.enrol_queued.unwrap_or(8);
        if !(1..=64).contains(&queued) {
            return Err(Error(
                "services.sync_hub_parts.enrol_queued must be between 1 and 64".to_owned(),
            ));
        }
        Ok(Resolved {
            hub_name: self.hub_name.clone().unwrap_or_else(|| "hub".to_owned()),
            refresh: Duration::from_secs(refresh),
            enrol: EnrolSettings { per_second, queued },
        })
    }
}

/// The site clock as the enrolment crate's Unix-seconds clock.
struct UnixSeconds(Arc<dyn Clock>);

impl spot_sync_enrol::Clock for UnixSeconds {
    fn now(&self) -> i64 {
        self.0.now_utc().unix_timestamp()
    }
}

/// Refusals go to the log: node ids and fingerprints are public, nothing else is passed.
struct LogAudit;

impl RefusalAudit for LogAudit {
    fn refused(&self, who: &str, reason: &str) {
        tracing::warn!(who, reason, "the sync hub refused a node");
    }
}

/// `last_seen` reports queued to a plain thread (the registry's store is blocking and refuses to
/// be called on a runtime thread).
struct SeenQueue {
    tx: mpsc::SyncSender<String>,
}

impl SeenSink for SeenQueue {
    fn seen(&self, node: &str, _at_ms: i64) {
        // Full queue: the next report (at most one per node per ten seconds) carries it.
        let _ = self.tx.try_send(node.to_owned());
    }
}

fn start_seen_thread(registry: PgNodeRegistry, clock: Arc<dyn Clock>) -> Result<SeenQueue> {
    let (tx, rx) = mpsc::sync_channel::<String>(1024);
    std::thread::Builder::new()
        .name("sync-hub-seen".to_owned())
        .spawn(move || {
            while let Ok(node) = rx.recv() {
                let Ok(id) = NodeId::new(&node) else { continue };
                let at = clock.now_utc().unix_timestamp();
                if let Err(error) = registry.touch(&id, at) {
                    tracing::warn!(code = error.code(), "sync hub: last_seen was not recorded");
                }
            }
        })
        .map_err(|_| {
            Error(format!(
                "{PART_UNAVAILABLE}: the last_seen thread did not start"
            ))
        })?;
    Ok(SeenQueue { tx })
}

/// Enrolment over `EnrolService::redeem`.
pub struct EnrolRedeemer(Arc<EnrolService>);

impl EnrolRedeemer {
    /// A redeemer over `service`.
    #[must_use]
    pub fn new(service: Arc<EnrolService>) -> Self {
        Self(service)
    }
}

/// The endpoint's answer for an enrolment failure: the same text for every wrong code.
#[must_use]
pub fn refusal_of(error: &EnrolError) -> Refusal {
    let code = error.code();
    let (status, message) = match code {
        spot_sync_enrol::error::CODE_INVALID => (403, "the enrolment code is not valid".to_owned()),
        spot_sync_enrol::error::LOCKED_OUT => (429, error.to_string()),
        spot_sync_enrol::error::CERT_INVALID => (400, error.to_string()),
        _ => (503, "the hub could not process the enrolment".to_owned()),
    };
    Refusal {
        code: code.to_owned(),
        message,
        status,
    }
}

impl Redeemer for EnrolRedeemer {
    fn redeem(
        &self,
        code: &str,
        node: &str,
        certificate: &[u8],
        source: &str,
    ) -> std::result::Result<(), Refusal> {
        let node = NodeId::new(node).map_err(|_| Refusal {
            code: "sync_http.invalid_request".to_owned(),
            message: "the enrolment request is not valid".to_owned(),
            status: 400,
        })?;
        self.0
            .redeem(code, &node, certificate, source)
            .map(|_| ())
            .map_err(|error| refusal_of(&error))
    }
}

/// Keeps the directory's snapshot fresh: the directory refuses everyone after 30 s without a
/// successful refresh, so a hub that cannot reach its database stops admitting nodes (fail
/// closed) and recovers by itself when the database is back.
pub struct DirectoryRefresh {
    directory: Arc<DurableDirectory>,
    every: Duration,
}

impl Service for DirectoryRefresh {
    fn name(&self) -> &'static str {
        "sync_hub_directory"
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        // The first load happened when the parts were built: a directory that could not load is
        // a registration failure, not a late one.
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let halt = Arc::new(AtomicBool::new(false));
            let (directory, every, flag) =
                (Arc::clone(&self.directory), self.every, Arc::clone(&halt));
            std::thread::Builder::new()
                .name("sync-hub-directory".to_owned())
                .spawn(move || {
                    while !flag.load(Ordering::SeqCst) {
                        if directory.needs_refresh()
                            && let Err(error) = directory.refresh()
                        {
                            tracing::warn!(
                                code = error.code(),
                                "sync hub: the node directory did not refresh"
                            );
                        }
                        std::thread::sleep(every);
                    }
                })
                .map_err(|_| "the directory refresh thread did not start".to_owned())?;
            let mut stop = stop;
            let _ = stop.wait_for(|stopped| *stopped).await;
            halt.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
}

/// What the plain thread built.
pub struct Built {
    /// The port `sync_hub` takes.
    pub parts: HubParts,
    /// The directory, loaded once.
    pub directory: Arc<DurableDirectory>,
    /// The enrolment service (administration creates codes through it).
    pub service: Arc<EnrolService>,
    /// The enrolment route.
    pub enrol: Router,
    /// The hub's durable stores (the registry, the relay state, the outbox reads), for the modules
    /// that read them after this one is built (the catch-up feed, WP-AI-07.T16).
    pub hub: PgHub,
}

/// What [`build`] needs.
pub struct Inputs {
    /// The site's database.
    pub backend: spot_postgres::PgBackend,
    /// The size of the site's pool.
    pub pool_size: usize,
    /// The tenant (the site label).
    pub tenant: String,
    /// The hub's node id.
    pub hub_node: NodeId,
    /// The settings, resolved.
    pub resolved: Resolved,
    /// The site clock.
    pub clock: Arc<dyn Clock>,
    /// The entropy source for enrolment codes.
    pub random: Arc<dyn RandomSource>,
    /// The id source for audit records.
    pub ids: Arc<dyn Ids>,
    /// Relay core, sinks and metadata epoch.
    pub assembly: Arc<dyn RelayAssembly>,
}

fn unavailable(part: &str, detail: &str) -> Error {
    Error(format!("{PART_UNAVAILABLE}: {part}: {detail}"))
}

/// Builds every part. Blocking: call it on a plain thread (the stores refuse a runtime thread).
///
/// # Errors
///
/// `sync_hub_parts.part_unavailable` naming the part (a missing table says to run `spot migrate`).
pub fn build(inputs: Inputs) -> Result<Built> {
    let Inputs {
        backend,
        pool_size,
        tenant,
        hub_node,
        resolved,
        clock,
        random,
        ids,
        assembly,
    } = inputs;
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(unavailable(
            "database",
            "the PostgreSQL pool is too small for the sync hub bridge",
        ));
    }
    let bridge = Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "sync_hub".to_owned(),
                max_in_flight: largest.min(4),
                workers: 2,
                queue_capacity: 32,
                default_deadline: Duration::from_secs(15),
                max_deadline: Duration::from_secs(60),
            },
            &backend,
        )
        .map_err(|e| Error(format!("{e}")))?,
    );
    let hub = PgHub::new(Arc::clone(&bridge), pool_size, tenant.clone())
        .map_err(|e| unavailable("hub store", e.code()))?;
    // The tables are created by `spot migrate`, never here.
    hub.db()
        .require_tables()
        .map_err(|e| unavailable("hub tables (run `spot migrate`)", e.code()))?;

    let enrol_store: Arc<dyn EnrolmentStore> = Arc::new(
        PgEnrolmentStore::new(Arc::clone(&bridge), pool_size, tenant)
            .map_err(|e| unavailable("enrolment store", e.code()))?,
    );
    let enrol_clock: Arc<dyn spot_sync_enrol::Clock> = Arc::new(UnixSeconds(Arc::clone(&clock)));
    let directory = Arc::new(DurableDirectory::new(
        Arc::clone(&enrol_store),
        Arc::clone(&enrol_clock),
        DirectoryConfig::default(),
    ));
    // Loaded before anything is served: until then the directory authenticates nobody.
    directory
        .refresh()
        .map_err(|e| unavailable("node directory (run `spot migrate`?)", e.code()))?;
    let service = Arc::new(EnrolService::new(
        enrol_store,
        Arc::clone(&directory),
        random,
        ids,
        enrol_clock,
    ));
    let enrol = enrol_router(
        Arc::new(EnrolRedeemer::new(Arc::clone(&service))),
        resolved.enrol,
        Arc::clone(&clock),
    )
    .map_err(|_| unavailable("enrolment endpoint", "its thread did not start"))?;

    let seen = start_seen_thread(hub.nodes(), clock)?;
    let core = assembly
        .core(&hub)
        .map_err(|why| unavailable("relay core", &why))?;
    let directory_port: Arc<dyn NodeDirectory> = Arc::clone(&directory) as Arc<dyn NodeDirectory>;
    let parts = HubParts {
        directory: directory_port,
        core: Mutex::new(Some(core)),
        sinks: assembly.sinks(),
        hub_node,
        metadata_epoch: assembly.metadata_epoch(),
        audit: Arc::new(LogAudit),
        seen: Some(Arc::new(seen)),
    };
    Ok(Built {
        parts,
        directory,
        service,
        enrol,
        hub,
    })
}

/// Builds the hub's parts and provides [`HubParts`] and [`EnrolRoute`].
///
/// # Errors
///
/// [`Error`] naming the setting out of bounds, the port nobody provided
/// (`sync_hub_parts.part_missing`) or the part that could not be built
/// (`sync_hub_parts.part_unavailable`).
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let resolved = settings.resolve()?;
    if !ctx.assembling() {
        return Ok(());
    }
    let clock = Arc::clone(ctx.clock());
    let random = ctx.port::<dyn RandomSource>();
    let ids = ctx.port::<dyn Ids>();
    let assembly = ctx.port::<dyn RelayAssembly>();
    let mut missing: Vec<&str> = Vec::new();
    if random.is_none() {
        missing.push("random source");
    }
    if ids.is_none() {
        missing.push("id source");
    }
    if assembly.is_none() {
        missing.push("relay assembly (relay core, tenant sinks, metadata epoch)");
    }
    let (Some(random), Some(ids), Some(assembly)) = (random, ids, assembly) else {
        return Err(Error(format!(
            "{PART_MISSING}: the sync hub is missing: {}",
            missing.join(", ")
        )));
    };
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let hub_node = NodeId::new(&format!("{site}#{}", resolved.hub_name))
        .map_err(|e| Error(format!("services.sync_hub_parts.hub_name: {e}")))?;
    let refresh = resolved.refresh;
    let inputs = Inputs {
        backend: env.backend.clone(),
        pool_size: env.loaded.common.postgres.pool_max_size,
        tenant: site,
        hub_node,
        resolved,
        clock,
        random,
        ids,
        assembly,
    };
    let built =
        crate::services::partition::on_plain_thread("sync hub parts", move || build(inputs))?;
    ctx.provide::<HubParts>(Arc::new(built.parts))?;
    // The directory itself and the hub's stores are ports for the modules that start after this
    // one and need more than `HubParts` shows: the edge desk's roster reads the directory's
    // enrolled nodes (revocation included), the catch-up feed reads the registry and the outbox.
    ctx.provide::<DurableDirectory>(Arc::clone(&built.directory))?;
    ctx.provide::<PgHub>(Arc::new(built.hub))?;
    ctx.provide::<EnrolRoute>(Arc::new(EnrolRoute {
        router: Mutex::new(Some(built.enrol)),
    }))?;
    ctx.add(Arc::new(DirectoryRefresh {
        directory: built.directory,
        every: refresh,
    }));
    Ok(())
}
