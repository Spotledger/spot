//! The edge's sync service: runs `spot_sync_node_host::NodeLoop` on a site with `role = "edge"`
//! (WP-RTB-02.T5; ADR-062 sections 3, 4 and 7).
//!
//! Absent the edge role this module is skipped before `register` runs (`ROLES`). The hub URL,
//! the pinned hub fingerprint, the enrolment code file and the node id come from the one table
//! that already validates them, `[services.node_role]`; this module's own table
//! `[services.sync_node]` carries the loop's bounds:
//!
//! ```toml
//! [services.sync_node]
//! backoff_min_secs = 1          # default 1
//! backoff_max_secs = 300        # default 300
//! min_reconnect_secs = 5        # least time between two connection attempts
//! batch_records = 256
//! hub_source = "head-office#hub" # the hub's stream; default "<site>#hub"
//! outbox_generation = 1
//! identity_dir = "/var/lib/spot/sync-node"  # default "<config root>/sync-node/<site>"
//! ```
//!
//! # Offline is normal
//!
//! The service is [`Level::Optional`] and its check never fails because the hub is away: an edge
//! works without its hub (EDG-01). The operator's line (`sync_node: connected to hub-1 (lag
//! 0.2s)`, `sync_node: offline 3h12m, 418 changes queued`, `sync_node: not enrolled`) is the
//! [`SyncNodeReadiness`] port.
//!
//! # The apply path is a port
//!
//! The intake is built by an `IntakeFactory` port a host module provides (it holds the site's
//! ownership view, audit, quarantine and catalogue). With no such port the service says so in its
//! check and the loop does not run: nothing is applied on a guess.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_sync_http::{BreakerConfig, ClientIdentity, SeededJitter, SocketTransport, TokioSleeper};
use spot_sync_node_host::enrol::{HttpEnroller, Identity, IdentityPaths, identity_step};
use spot_sync_node_host::pg::{OwnWritesMapper, PgCursors, PgOutboxReader};
use spot_sync_node_host::{
    HubLink, Intake, IntakeFactory, IntakeRequest, LinkState, LoopParts, LoopSettings, NodeLoop,
    OutboxSender, PushSide, StatusHandle, Worker,
};
use spot_sync_protocol::{NodeId, ProtocolRange};
use tokio::sync::watch;

use crate::services::node_role;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name and its `[services.<name>]` table.
pub const NAME: &str = "sync_node";
/// After `node_role` (1005), which provides the role this condition reads.
pub const ORDER: u32 = 1200;
/// Edge only (the row in `node_role::SERVICE_SETS`).
pub const ROLES: &[&str] = &["edge"];

/// `[services.sync_node]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Shortest wait after a failed session, seconds.
    pub backoff_min_secs: Option<u64>,
    /// Longest wait after repeated failures, seconds.
    pub backoff_max_secs: Option<u64>,
    /// Least time between two connection attempts, seconds.
    pub min_reconnect_secs: Option<u64>,
    /// Most records per pull and push batch.
    pub batch_records: Option<usize>,
    /// The hub's stream; default `<site>#hub`.
    pub hub_source: Option<String>,
    /// The outbox generation the node's own stream reads.
    pub outbox_generation: Option<u64>,
    /// Where the identity (key, certificate, pin) lives.
    pub identity_dir: Option<String>,
}

/// What the settings and the site say, resolved and checked.
#[derive(Clone, Debug)]
pub struct Plan {
    /// This node.
    pub node: NodeId,
    /// `host:port` to dial.
    pub address: String,
    /// The TLS name (the host part).
    pub server_name: String,
    /// The pin from the configuration, if any.
    pub configured_pin: Option<String>,
    /// The enrolment code file, if configured.
    pub code_file: Option<String>,
    /// The identity directory.
    pub identity_dir: std::path::PathBuf,
    /// The loop's bounds.
    pub loop_settings: LoopSettings,
    /// The hub's stream.
    pub hub_source: SourceId,
    /// The tenant (the site label).
    pub tenant: String,
    /// The outbox generation.
    pub generation: u64,
}

/// Resolves and validates the plan.
///
/// # Errors
///
/// [`Error`] naming the setting that is missing or out of bounds. A hub URL that is not `https`
/// is refused: the hub is authenticated by TLS and a pinned certificate.
pub fn plan(
    settings: &Settings,
    role: &node_role::Settings,
    site: &str,
    partition_node: Option<&str>,
    root: &std::path::Path,
) -> Result<Plan> {
    let node_text = partition_node
        .ok_or_else(|| Error("sync_node: `[partition] node` names this node".to_owned()))?;
    let node = NodeId::new(node_text).map_err(|e| Error(format!("sync_node: node id: {e}")))?;
    let url = role
        .hub_url
        .as_deref()
        .ok_or_else(|| Error("sync_node: [services.node_role] hub_url is required".to_owned()))?;
    let rest = url
        .strip_prefix("https://")
        .ok_or_else(|| Error("sync_node: hub_url must be an https:// URL".to_owned()))?;
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.contains('[') || authority.contains('@') {
        return Err(Error("sync_node: hub_url has no usable host".to_owned()));
    }
    let (host, address) = match authority.rsplit_once(':') {
        Some((host, port)) if port.parse::<u16>().is_ok() => (host, authority.to_owned()),
        _ => (authority, format!("{authority}:443")),
    };
    let secs = |v: Option<u64>, default: u64| Duration::from_secs(v.unwrap_or(default));
    let mut loop_settings = LoopSettings {
        backoff_min: secs(settings.backoff_min_secs, 1),
        backoff_max: secs(settings.backoff_max_secs, 300),
        min_reconnect_interval: secs(settings.min_reconnect_secs, 5),
        ..LoopSettings::default()
    };
    if let Some(batch) = settings.batch_records {
        loop_settings.batch_records = batch;
        loop_settings.offer_records = loop_settings.offer_records.max(batch.saturating_mul(4));
    }
    let loop_settings = loop_settings
        .validated()
        .map_err(|e| Error(format!("{}: {}", e.code, e.message)))?;
    let hub_source_text = settings
        .hub_source
        .clone()
        .unwrap_or_else(|| format!("{site}#hub"));
    let hub_source = SourceId::new(&hub_source_text)
        .map_err(|e| Error(format!("sync_node: hub_source: {e}")))?;
    let identity_dir = settings.identity_dir.as_deref().map_or_else(
        || root.join("sync-node").join(site),
        std::path::PathBuf::from,
    );
    Ok(Plan {
        node,
        address,
        server_name: host.to_owned(),
        configured_pin: role.hub_fingerprint.clone(),
        code_file: role.enrol_code_file.clone(),
        identity_dir,
        loop_settings,
        hub_source,
        tenant: site.to_owned(),
        generation: settings.outbox_generation.unwrap_or(1),
    })
}

/// The operator's readiness line. Always `ok`: offline is normal.
pub struct SyncNodeReadiness {
    status: StatusHandle,
    clock: Arc<dyn Clock>,
}

impl ReadinessCheck for SyncNodeReadiness {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async move {
            CheckResult {
                ok: true,
                detail: self
                    .status
                    .snapshot()
                    .readiness(self.clock.now_utc().unix_timestamp()),
            }
        })
    }

    fn name(&self) -> &str {
        NAME
    }
}

type Starter = Box<
    dyn FnOnce(
            watch::Receiver<bool>,
        ) -> Pin<Box<dyn Future<Output = std::result::Result<(), String>> + Send>>
        + Send,
>;

/// The hosted service.
pub struct SyncNodeService {
    status: StatusHandle,
    unwired: Option<String>,
    starter: Mutex<Option<Starter>>,
}

impl SyncNodeService {
    /// A service that runs `starter` as its task. `unwired` makes the check say why nothing runs.
    pub fn new(status: StatusHandle, unwired: Option<String>, starter: Option<Starter>) -> Self {
        Self {
            status,
            unwired,
            starter: Mutex::new(starter),
        }
    }

    /// The status the readiness line reads.
    pub fn status(&self) -> &StatusHandle {
        &self.status
    }
}

impl Service for SyncNodeService {
    fn name(&self) -> &'static str {
        NAME
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        let unwired = self.unwired.clone();
        Box::pin(async move { unwired.map_or(Ok(()), Err) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        let starter = self
            .starter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        Box::pin(async move {
            match starter {
                Some(starter) => starter(stop).await,
                None => {
                    let mut stop = stop;
                    let _ = stop.changed().await;
                    Ok(())
                }
            }
        })
    }
}

/// Registers the service.
///
/// # Errors
///
/// [`Error`] for an invalid plan, a missing cursor table (run `spot migrate`) or a bridge that
/// cannot be built.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let role: node_role::Settings = ctx.settings_or_default("node_role")?;
    let clock = Arc::clone(ctx.clock());
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let partition_node = node_role::partition_node_of(env.site);
    let plan = plan(&settings, &role, &site, partition_node.as_deref(), env.root)?;
    let status = StatusHandle::new(plan.address.clone(), LinkState::Connecting);
    ctx.provide::<SyncNodeReadiness>(Arc::new(SyncNodeReadiness {
        status: status.clone(),
        clock: Arc::clone(&clock),
    }))?;
    let Some(factory) = ctx.port::<dyn IntakeFactory>() else {
        ctx.add(Arc::new(SyncNodeService::new(
            status,
            Some(
                "sync_node: the apply path is not wired (no module provides the intake factory)"
                    .to_owned(),
            ),
            None,
        )));
        return Ok(());
    };
    let pool_size = env.loaded.common.postgres.pool_max_size;
    let backend = env.backend.clone();
    let tenant = plan.tenant.clone();
    let bridge = crate::services::partition::on_plain_thread("sync_node bridge", move || {
        let largest = BridgeConfig::largest_in_flight(pool_size);
        if largest < 1 {
            return Err(Error(
                "sync_node: the PostgreSQL pool is too small for the sync bridge".to_owned(),
            ));
        }
        let bridge = Bridge::for_backend(
            BridgeConfig {
                name: "sync_node".to_owned(),
                max_in_flight: largest.min(4),
                workers: 2,
                queue_capacity: 16,
                default_deadline: Duration::from_secs(15),
                max_deadline: Duration::from_secs(60),
            },
            &backend,
        )
        .map_err(|e| Error(format!("{e}")))?;
        let bridge = Arc::new(bridge);
        PgCursors::new(Arc::clone(&bridge), tenant)
            .and_then(|c| c.require_table())
            .map_err(|e| Error(format!("sync_node: {}", e.message)))?;
        Ok(bridge)
    })?;
    let starter = starter(plan, factory, bridge, pool_size, status.clone(), clock);
    ctx.add(Arc::new(SyncNodeService::new(status, None, Some(starter))));
    Ok(())
}

fn starter(
    plan: Plan,
    factory: Arc<dyn IntakeFactory>,
    bridge: Arc<Bridge>,
    pool_size: usize,
    status: StatusHandle,
    clock: Arc<dyn Clock>,
) -> Starter {
    Box::new(move |stop| {
        Box::pin(async move {
            run_edge(plan, factory, bridge, pool_size, status, clock, stop)
                .await
                .map_err(|e| e.to_string())
        })
    })
}

async fn run_edge(
    plan: Plan,
    factory: Arc<dyn IntakeFactory>,
    bridge: Arc<Bridge>,
    pool_size: usize,
    status: StatusHandle,
    clock: Arc<dyn Clock>,
    mut stop: watch::Receiver<bool>,
) -> spot_sync_node_host::Result<()> {
    let address = plan.address.clone();
    let server_name = plan.server_name.clone();
    let transport_for =
        move |identity: &spot_sync_enrol::NodeIdentity,
              pin: &str|
              -> spot_sync_node_host::Result<Arc<dyn spot_sync_http::Transport>> {
            let tls = spot_sync_node_host::tls::pinned_client_config(pin, identity)?;
            Ok(Arc::new(SocketTransport::new(
                address.clone(),
                server_name.clone(),
                Arc::new(tls),
            )))
        };
    let enroller = HttpEnroller::new(Arc::new(transport_for.clone()));
    let paths = IdentityPaths {
        dir: plan.identity_dir.clone(),
        code_file: plan.code_file.as_ref().map(std::path::PathBuf::from),
        configured_pin: plan.configured_pin.clone(),
    };
    let now = clock.now_utc().unix_timestamp();
    let (identity, pin) = match identity_step(&paths, &plan.node, now, &enroller).await {
        Ok(Identity::Enrolled {
            identity,
            hub_fingerprint,
        }) => (identity, hub_fingerprint),
        Ok(Identity::NotEnrolled) => {
            status.not_enrolled();
            let _ = stop.changed().await;
            return Ok(());
        }
        Err(error) => {
            status.not_enrolled();
            tracing::warn!(code = error.code, "sync_node: enrolment did not complete");
            let _ = stop.changed().await;
            return Ok(());
        }
    };
    let transport = transport_for(&identity, &pin)?;
    let settings = plan.loop_settings;
    let link = HubLink::new(
        transport,
        ClientIdentity::new(plan.node.clone())
            .with_protocol(ProtocolRange::current(), factory.metadata_epoch()),
        Arc::clone(&clock),
        BreakerConfig::default(),
        plan.address.clone(),
        settings.request_deadline,
        settings.batch_records,
    )?;
    let cursors_bridge = Arc::clone(&bridge);
    let tenant = plan.tenant.clone();
    let hub_source = plan.hub_source.clone();
    let intake_request = IntakeRequest {
        bridge: Arc::clone(&bridge),
        pool_size,
        tenant: tenant.clone(),
        hub_source: hub_source.clone(),
        cursors: PgCursors::new_unchecked(Arc::clone(&bridge), tenant.clone()),
    };
    let intake: Arc<Worker<Box<dyn Intake>>> =
        Arc::new(Worker::spawn("sync-node-intake", move || {
            factory.build(intake_request)
        })?);
    let node_source = SourceId::new(plan.node.as_str()).map_err(|_| {
        spot_sync_node_host::Error::invalid_settings("the node id is not a source id")
    })?;
    let generation = plan.generation;
    let push: Arc<Worker<Box<dyn PushSide>>> =
        Arc::new(Worker::spawn("sync-node-push", move || {
            let cursors = PgCursors::new(Arc::clone(&cursors_bridge), tenant)?;
            let reader = PgOutboxReader::new(cursors_bridge, generation, OwnWritesMapper)?;
            Ok(
                Box::new(OutboxSender::new(reader, cursors.push(), node_source))
                    as Box<dyn PushSide>,
            )
        })?);
    let parts = LoopParts {
        settings,
        link,
        hub_source,
        intake: Arc::clone(&intake),
        push: Some(Arc::clone(&push)),
        clock,
        jitter: Box::new(SeededJitter::new(seed_of(plan.node.as_str()))),
        sleeper: Arc::new(TokioSleeper),
        status,
    };
    let result = NodeLoop::new(parts)?.run(stop).await;
    // A bundle in flight finishes first; then the threads end.
    intake.shutdown().await;
    push.shutdown().await;
    result
}

/// A per-node jitter seed (FNV-1a of the node id), so edges do not retry in lockstep. It is a
/// seed, not a secret and not a clock read.
fn seed_of(node: &str) -> u64 {
    node.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}
