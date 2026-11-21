//! The sync hub service: the hub half of sync inside `spot serve` (WP-RTB-02.T4; ADR-062
//! section 3). Guide: `docs/running-spot/sync_hub.md`; the join is documented in
//! the project process notes.
//!
//! The service is **off unless configured** and exists only on a site whose node role is `hub`
//! (`ROLES`): a hub site without `services.sync_hub.listen` refuses to start with a message that
//! says what to set. It accepts edge-initiated connections over mutual TLS (client certificates
//! are requested and verified against the configured client CA, and then resolved against the
//! live node directory on every request, never against the system trust store), answers
//! `hello`, `pull` and `push` of `spot-sync-http` through the guard of `spot-sync-hub-host`
//! (per-node and global limits, own-queue pulls only, tenant by pinned identity), and drives the
//! hub relay on its own thread, publishing each destination's queue into the serve cache.
//!
//! It is **required** on a hub site: if it cannot bind, load its certificate or find its parts,
//! the site is not ready.
//!
//! # What this service is handed
//!
//! The directory of enrolled nodes (WP-RTB-02.T3), the relay with its destinations and policy
//! (WP-RTB-02.T1/T8) and the per-tenant push sinks (WP-RTB-02.T2) are built by their own
//! modules, which provide one [`HubParts`] port at an `ORDER` below this module's. A missing
//! port is a start-up failure that names it, never a silent no-op.
//!
//! # The edge desk (WP-AI-07.T9)
//!
//! On a hub site the service also provides `dyn EdgeDesk` ([`edge_desk::HubEdgeDesk`]): every
//! enrolled edge with its expected and actual queue position (read from the serve cache this
//! service creates when it runs) and a resync of one edge through the catch-up path. The remedy
//! host's edge part reads it. Its roster is the enrolment directory (an edge's revocation is
//! visible), see [`edge_desk::DirectoryRoster`]. A non-hub role provides nothing (`ROLES`). See
//! [`edge_desk`] and the guide.
//!
//! Tables: the hub's durable state (`spot-sync-hub-pg`) is created by `spot migrate` (its DDL is
//! in `schema::extra_tables`), never by the host.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_sync_http::{NodeDirectory, ServerSettings, SyncServer, tls};
use spot_sync_hub_host::driver::{DriveSettings, HubCore, drive};
use spot_sync_hub_host::{
    Guard, GuardSettings, Limits, RefusalAudit, SeenSink, ServeCache, TenantSinks, guard_layer,
};
use spot_sync_protocol::NodeId;
use tokio::sync::watch;

use crate::services::world_access::edge::EdgeDesk;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "sync_hub/edge_desk.rs"]
pub mod edge_desk;

/// This module's start order: after `node_role` (1005), which decides the role `ROLES` reads.
pub const ORDER: u32 = 1300;

/// The roles this module runs in.
pub const ROLES: &[&str] = &["hub"];

/// What the other sync modules hand this service. Provided as a port.
pub struct HubParts {
    /// The enrolled nodes: certificate pinning, revocation, partitions.
    pub directory: Arc<dyn NodeDirectory>,
    /// The relay core, taken once by this service's driver thread.
    pub core: Mutex<Option<Box<dyn HubCore>>>,
    /// The apply sink of each tenant.
    pub sinks: Arc<TenantSinks>,
    /// The hub's own node id (`<hub site>#<name>`).
    pub hub_node: NodeId,
    /// The metadata epoch the hub advertises at `hello`.
    pub metadata_epoch: u64,
    /// Where refusals are audited.
    pub audit: Arc<dyn RefusalAudit>,
    /// Where `last_seen` is written, if anywhere.
    pub seen: Option<Arc<dyn SeenSink>>,
}

/// The certificate files of the listener.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    /// The hub's certificate chain (PEM), relative to the configuration root.
    pub cert: PathBuf,
    /// The hub's private key (PEM), relative to the configuration root; mode 0600.
    pub key: PathBuf,
    /// The CA that issued the edges' client certificates (PEM). Client certificates are
    /// verified against this and nothing else.
    pub client_ca: PathBuf,
}

/// `[services.sync_hub]`.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// The address to listen on, for example `0.0.0.0:7443`. Required on a hub site.
    pub listen: Option<String>,
    /// The listener's certificate files. Required with `listen`.
    pub tls: Option<TlsFiles>,
    /// Concurrent requests per node (1 to 16, default 2).
    pub per_node_connections: Option<u32>,
    /// Concurrent requests for the host (1 to 100000, default 2000).
    pub global_connections: Option<u32>,
    /// Push batches per node per second (1 to 1000, default 20).
    pub push_per_second: Option<u32>,
    /// The largest push body in MiB (1 to 256, default 64).
    pub max_body_mib: Option<u32>,
    /// The most records one batch may carry (1 to 65536, default 4096).
    pub max_batch_records: Option<u32>,
    /// The longest a pull waits, in seconds (1 to 60, default 25).
    pub long_poll_secs: Option<u32>,
    /// The heartbeat of a waiting pull, in seconds (1 to 30, default 5).
    pub heartbeat_secs: Option<u32>,
    /// The least time between two audit records for one node, in seconds (10 to 3600, default 60).
    pub audit_interval_secs: Option<u32>,
    /// How many queued items per destination the serve cache publishes (1 to 65536, default 4096).
    pub window: Option<u32>,
    /// The pause between relay rounds, in milliseconds (20 to 5000, default 200).
    pub drive_interval_ms: Option<u32>,
}

fn bounded(name: &str, value: Option<u32>, default: u32, min: u32, max: u32) -> Result<u32> {
    let value = value.unwrap_or(default);
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(Error(format!(
            "services.sync_hub.{name} must be between {min} and {max}"
        )))
    }
}

/// The settings with defaults applied and every bound checked.
#[derive(Clone, Debug)]
pub struct Resolved {
    /// The address to bind.
    pub listen: SocketAddr,
    /// The certificate files.
    pub tls: TlsFiles,
    /// The connection and rate limits.
    pub limits: Limits,
    /// The server's body, batch and timing limits.
    pub server: ServerSettings,
    /// The driver's pacing and window.
    pub drive: DriveSettings,
}

impl Settings {
    /// Checks every bound and applies the defaults.
    ///
    /// # Errors
    /// [`Error`] naming the key; a hub site without `listen` or `tls` is refused here.
    pub fn resolve(&self) -> Result<Resolved> {
        let listen = self.listen.as_deref().ok_or_else(|| {
            Error(
                "this site is a sync hub but services.sync_hub.listen is not set: set the \
                 address the edges dial, for example \"0.0.0.0:7443\""
                    .to_owned(),
            )
        })?;
        let listen: SocketAddr = listen
            .parse()
            .map_err(|_| Error("services.sync_hub.listen is not an address:port".to_owned()))?;
        let tls = self.tls.clone().ok_or_else(|| {
            Error("services.sync_hub.tls (cert, key, client_ca) is required".to_owned())
        })?;
        let limits = Limits {
            per_node_connections: bounded(
                "per_node_connections",
                self.per_node_connections,
                2,
                1,
                16,
            )?,
            push_per_second: bounded("push_per_second", self.push_per_second, 20, 1, 1_000)?,
            global_connections: bounded(
                "global_connections",
                self.global_connections,
                2_000,
                1,
                100_000,
            )?,
            audit_interval_ms: i64::from(bounded(
                "audit_interval_secs",
                self.audit_interval_secs,
                60,
                10,
                3_600,
            )?) * 1_000,
        };
        let max_body = usize::try_from(bounded("max_body_mib", self.max_body_mib, 64, 1, 256)?)
            .unwrap_or(64)
            .saturating_mul(1024 * 1024);
        let server = ServerSettings {
            max_batch_records: usize::try_from(bounded(
                "max_batch_records",
                self.max_batch_records,
                4_096,
                1,
                65_536,
            )?)
            .unwrap_or(4_096),
            max_push_bytes: max_body,
            long_poll: Duration::from_secs(u64::from(bounded(
                "long_poll_secs",
                self.long_poll_secs,
                25,
                1,
                60,
            )?)),
            heartbeat: Duration::from_secs(u64::from(bounded(
                "heartbeat_secs",
                self.heartbeat_secs,
                5,
                1,
                30,
            )?)),
        };
        let drive = DriveSettings {
            interval: Duration::from_millis(u64::from(bounded(
                "drive_interval_ms",
                self.drive_interval_ms,
                200,
                20,
                5_000,
            )?)),
            window: usize::try_from(bounded("window", self.window, 4_096, 1, 65_536)?)
                .unwrap_or(4_096),
        };
        Ok(Resolved {
            listen,
            tls,
            limits,
            server,
            drive,
        })
    }
}

fn load_acceptor(
    root: &Path,
    files: &TlsFiles,
) -> std::result::Result<tokio_rustls::TlsAcceptor, String> {
    use rustls::pki_types::pem::PemObject as _;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let join = |p: &Path| {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            root.join(p)
        }
    };
    let chain = CertificateDer::pem_file_iter(join(&files.cert))
        .map_err(|_| "the hub certificate file cannot be read".to_owned())?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|_| "the hub certificate file is not PEM certificates".to_owned())?;
    let key = PrivateKeyDer::from_pem_file(join(&files.key))
        .map_err(|_| "the hub key file cannot be read as a PEM private key".to_owned())?;
    let client_ca = CertificateDer::pem_file_iter(join(&files.client_ca))
        .map_err(|_| "the client CA file cannot be read".to_owned())?
        .next()
        .ok_or_else(|| "the client CA file holds no certificate".to_owned())?
        .map_err(|_| "the client CA file is not a PEM certificate".to_owned())?;
    let config = tls::server_config(chain, key, client_ca).map_err(|e| e.message)?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// The service.
pub struct SyncHub {
    resolved: Resolved,
    root: PathBuf,
    hub_site: String,
    parts: Arc<HubParts>,
    clock: Arc<dyn Clock>,
    prepared: Mutex<Option<(tokio::net::TcpListener, tokio_rustls::TlsAcceptor)>>,
    link: Arc<edge_desk::EdgeLink>,
}

impl SyncHub {
    /// The service over resolved settings, the configuration root (certificate paths are relative
    /// to it), the hub's site and the parts the other modules provided.
    #[must_use]
    pub fn new(
        resolved: Resolved,
        root: PathBuf,
        hub_site: String,
        parts: Arc<HubParts>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            resolved,
            root,
            hub_site,
            parts,
            clock,
            prepared: Mutex::new(None),
            link: Arc::new(edge_desk::EdgeLink::default()),
        }
    }

    /// The link the edge desk reads the running hub's serve cache through.
    #[must_use]
    pub fn edge_link(&self) -> Arc<edge_desk::EdgeLink> {
        Arc::clone(&self.link)
    }
}

impl Service for SyncHub {
    fn name(&self) -> &'static str {
        "sync_hub"
    }

    fn level(&self) -> Level {
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // Bind first: a port that is taken is the commonest fault and needs no certificate to
            // report.
            let listener = tokio::net::TcpListener::bind(self.resolved.listen)
                .await
                .map_err(|e| {
                    format!(
                        "the sync hub cannot listen on {}: {}",
                        self.resolved.listen,
                        e.kind()
                    )
                })?;
            let provided = self
                .parts
                .core
                .lock()
                .map_err(|_| "the hub core lock is poisoned".to_owned())?
                .is_some();
            if !provided {
                return Err("no relay core was provided to the sync hub".to_owned());
            }
            let acceptor = load_acceptor(&self.root, &self.resolved.tls)?;
            *self
                .prepared
                .lock()
                .map_err(|_| "the sync hub state is poisoned".to_owned())? =
                Some((listener, acceptor));
            tracing::info!(listen = %self.resolved.listen, "sync hub listening");
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let (listener, acceptor) = self
                .prepared
                .lock()
                .map_err(|_| "the sync hub state is poisoned".to_owned())?
                .take()
                .ok_or_else(|| "the sync hub was not checked before it ran".to_owned())?;
            let core = self
                .parts
                .core
                .lock()
                .map_err(|_| "the hub core lock is poisoned".to_owned())?
                .take()
                .ok_or_else(|| "the relay core was already taken".to_owned())?;

            // The relay and its PostgreSQL queue are synchronous and may not run on an async
            // thread: a plain thread drives them and publishes into the cache.
            let (cache, acks) = ServeCache::new();
            self.link.attach(Arc::clone(&cache));
            let halt = Arc::new(AtomicBool::new(false));
            let thread = {
                let (cache, halt, clock) = (
                    Arc::clone(&cache),
                    Arc::clone(&halt),
                    Arc::clone(&self.clock),
                );
                let settings = self.resolved.drive;
                std::thread::Builder::new()
                    .name("sync-hub-relay".to_owned())
                    .spawn(move || {
                        drive(core, &cache, acks, &*clock, &halt, settings, &|reason| {
                            tracing::warn!(reason, "sync hub relay round failed");
                        });
                    })
                    .map_err(|e| format!("the relay thread did not start: {}", e.kind()))?
            };

            let server = SyncServer::new(
                self.parts.hub_node.clone(),
                Arc::clone(&self.parts.directory),
                cache,
                self.parts.sinks.clone(),
                Arc::clone(&self.clock),
            )
            .with_settings(self.resolved.server)
            .with_identity(
                spot_sync_protocol::ProtocolRange::current(),
                self.parts.metadata_epoch,
            );
            let guard = Guard::new(
                GuardSettings {
                    limits: self.resolved.limits,
                    hub_site: self.hub_site.clone(),
                },
                Arc::clone(&self.parts.directory),
                Arc::clone(&self.clock),
                Arc::clone(&self.parts.audit),
                self.parts.seen.clone(),
            );
            let router = guard_layer(server.router(), guard);
            let mut stop = stop;
            let shutdown = async move {
                let _ = stop.wait_for(|stopped| *stopped).await;
            };
            let served = tls::serve(listener, acceptor, router, shutdown).await;
            halt.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = thread.join();
            self.link.detach();
            served.map_err(|e| e.message)
        })
    }
}

/// Registers the sync hub on a hub site; refuses to start a hub site without a listener.
///
/// # Errors
/// A missing or out-of-bounds setting, or a missing [`HubParts`] port, named.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    if !ctx.assembling() {
        // A reload keeps the running service; the desk it provided is provided again.
        if let Some(desk) = edge_desk::recall(ctx.label()) {
            ctx.provide::<dyn EdgeDesk>(desk)?;
        }
        return Ok(());
    }
    let mut settings = ctx.settings_or_default::<Settings>("sync_hub")?;
    // `services.node_role.listen` is the address the role hands this service: it applies when the
    // hub's own table names none.
    if settings.listen.is_none() {
        settings.listen = ctx
            .settings_or_default::<crate::services::node_role::Settings>(
                crate::services::node_role::NAME,
            )?
            .listen
            .filter(|address| !address.is_empty());
    }
    let resolved = settings.resolve()?;
    let parts = ctx.port::<HubParts>().ok_or_else(|| {
        Error(
            "the sync hub needs the enrolment directory, the relay and the tenant sinks: no \
             module provided them (WP-RTB-02.T3, T1, T2, T8)"
                .to_owned(),
        )
    })?;
    let env = *ctx.env()?;
    let hub = Arc::new(SyncHub::new(
        resolved,
        env.root.to_path_buf(),
        env.site.site.clone(),
        parts,
        Arc::clone(ctx.clock()),
    ));
    // The edge desk: positions from the serve cache this service creates when it runs, the
    // enrolled edges from a roster port (else the relay's own destinations) and the catch-up
    // path when the catch-up wiring provided one (it is optional: without it a resync answers
    // `not supported`).
    let link = hub.edge_link();
    let roster: Arc<dyn edge_desk::EdgeRoster> =
        if let Some(provided) = ctx.port::<dyn edge_desk::EdgeRoster>() {
            provided
        } else if let Some(directory) = ctx.port::<spot_sync_enrol::DurableDirectory>() {
            // The enrolment directory sees every enrolled edge and its revocation.
            Arc::new(edge_desk::DirectoryRoster::new(directory))
        } else {
            // A hub with no directory port: the relay's own destinations (no revocation is visible).
            Arc::new(edge_desk::RelayRoster::new(Arc::clone(&link)))
        };
    let catchup: Option<Arc<dyn edge_desk::Catchup>> = ctx
        .port::<crate::services::sync_catchup_wiring::CatchupHub>()
        .map(|wired| wired as Arc<dyn edge_desk::Catchup>);
    let desk: Arc<dyn EdgeDesk> = Arc::new(edge_desk::HubEdgeDesk::new(
        &env.site.site,
        link,
        roster,
        catchup,
    ));
    edge_desk::remember(ctx.label(), Arc::clone(&desk));
    ctx.provide::<dyn EdgeDesk>(desk)?;
    ctx.add(hub);
    Ok(())
}
