//! Offline operation of an edge, hosted (WP-RTB-03.T2): leased series ranges, the identity lease
//! and the shared-balance policy. The decisions live in `spot_edge_offline_host`; this module
//! reads `[services.edge_offline]`, validates it and publishes the readiness line.
//!
//! ```toml
//! [services.edge_offline]
//! identity_lease_hours = 24            # default 24, hard cap 168
//! series = [{ name = "INV", block = 1000 }]
//! balances_default = "online-only"     # online-only | allocated-quota | accept-and-reconcile
//! ```
//!
//! # What this module hosts (WP-RTB-03.T7, the edge half)
//!
//! * **Durable leased ranges** (`spot_leased_range`, created by `spot migrate`, never by the host)
//!   behind a [`RangeBook`] port: the naming path wraps its allocator in
//!   `SwitchedSequences` over `RangeBook::sequences()`. Needs `[partition] node` and at least one
//!   configured series; the host refuses to start the module with the table missing.
//! * **The carrier loop** ([`CarrierLoop`]): lease requests and permission snapshot requests on
//!   the sync connection, through a `dyn CarrierLink` port the node-loop owner provides. Without
//!   that port the module runs, the ranges already held keep working and the readiness line says
//!   `carrier not wired`.
//! * **The offline identity keeper** ([`SnapshotKeeper`]), opened from an `IdentityParts` port
//!   (the trust anchor and the sealed persistence, provided by the module that holds the sealing
//!   key). Without it the line says `no snapshot` and every offline authorisation is a denial.
//! * **The shared-balance gate** (`Mutex<EdgeBalances>` port), default online only.
//! * The readiness line: lease headroom, the series, the snapshot age.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_edge_offline_host::balance::BalancePolicy;
use spot_edge_offline_host::carrier_loop::{
    CarrierLink, CarrierLoop, CarrierSettings, Instants, SharedKeeper,
};
use spot_edge_offline_host::edge_balances::EdgeBalances;
use spot_edge_offline_host::identity::{
    BoxedPersistence, IdentityParts, NodeClock, SnapshotKeeper,
};
use spot_edge_offline_host::lease::{IdentityLease, Standing, lease_length};
use spot_edge_offline_host::ranges::{RangeBook, partition_node};
use spot_edge_offline_host::readiness::{SeriesState, line_full};
use spot_edge_offline_host::series::SeriesPolicy;
use spot_edge_offline_host::store_pg::{PgRangeStore, STRIDE};
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_partition::{HighWaterStore, LowWater, MemorySeriesStore};
use spot_pg_bridge::{Bridge, BridgeConfig};
use tokio::sync::watch;

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name.
pub const NAME: &str = "edge_offline";
/// After the sync modules.
pub const ORDER: u32 = 1210;

/// One configured series.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SeriesSetting {
    /// The series name.
    pub name: String,
    /// The numbers one lease grants.
    pub block: Option<u64>,
}

/// `[services.edge_offline]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Identity lease, hours.
    pub identity_lease_hours: Option<u64>,
    /// Leased series.
    pub series: Vec<SeriesSetting>,
    /// The default balance policy.
    pub balances_default: Option<String>,
}

/// The resolved plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The lease length.
    pub lease: Duration,
    /// The series.
    pub series: Vec<SeriesPolicy>,
    /// The default balance policy.
    pub balance: BalancePolicy,
}

/// Validates the settings.
///
/// # Errors
///
/// [`Error`] for a duplicate or empty series name, a zero block or more than 64 series.
pub fn plan(settings: &Settings) -> Result<Plan> {
    if settings.series.len() > 64 {
        return Err(Error("edge_offline: at most 64 series".to_owned()));
    }
    let mut series: Vec<SeriesPolicy> = Vec::new();
    for s in &settings.series {
        let block = s.block.unwrap_or(1_000);
        if s.name.trim().is_empty() || block == 0 {
            return Err(Error(
                "edge_offline: a series needs a name and a block above zero".to_owned(),
            ));
        }
        if series.iter().any(|p| p.name == s.name) {
            return Err(Error(format!(
                "edge_offline: series {} is configured twice",
                s.name
            )));
        }
        series.push(SeriesPolicy {
            name: s.name.clone(),
            block,
        });
    }
    Ok(Plan {
        lease: lease_length(settings.identity_lease_hours),
        series,
        balance: settings
            .balances_default
            .as_deref()
            .map_or(BalancePolicy::OnlineOnly, BalancePolicy::parse),
    })
}

/// What the carrier loop publishes for readiness.
#[derive(Default)]
pub struct OfflineState {
    /// The identity lease clock of the earlier task; used only when no keeper is wired.
    pub lease: Option<IdentityLease>,
    /// Numbers left per series.
    pub remaining: Vec<(String, u64)>,
    /// The keeper's standing and the snapshot age, when a keeper is wired.
    pub identity: Option<(Standing, Option<Duration>)>,
    /// Whether a carrier link is wired.
    pub carrier_wired: bool,
}

/// The readiness line port. Always ok: offline is normal.
pub struct EdgeOfflineReadiness {
    plan: Plan,
    state: Arc<Mutex<OfflineState>>,
    clock: Arc<dyn Clock>,
}

impl ReadinessCheck for EdgeOfflineReadiness {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async move {
            let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
            let (standing, age) = match state.identity {
                Some((standing, age)) => (standing, age),
                None => (
                    state.lease.map_or(Standing::Lapsed, |l| {
                        l.standing(
                            self.clock.now_utc().unix_timestamp(),
                            self.clock.monotonic(),
                        )
                    }),
                    None,
                ),
            };
            let series: Vec<SeriesState> = self
                .plan
                .series
                .iter()
                .map(|p| SeriesState {
                    name: p.name.clone(),
                    remaining: state
                        .remaining
                        .iter()
                        .find(|(n, _)| *n == p.name)
                        .map_or(0, |(_, r)| *r),
                    block: p.block,
                })
                .collect();
            CheckResult {
                ok: true,
                detail: line_full(standing, &series, age, state.carrier_wired),
            }
        })
    }

    fn name(&self) -> &str {
        NAME
    }
}

/// The node clock the identity store takes, from the injected clock.
fn node_clock(clock: &dyn Clock, boot_id: u64) -> NodeClock {
    NodeClock {
        mono_ms: u64::try_from(clock.monotonic().as_millis()).unwrap_or(u64::MAX),
        boot_id,
    }
}

/// What the running service holds.
struct Parts {
    ranges: Option<Arc<RangeBook>>,
    keeper: Option<SharedKeeper>,
    link: Option<Arc<dyn CarrierLink>>,
    state: Arc<Mutex<OfflineState>>,
    clock: Arc<dyn Clock>,
    boot_id: u64,
}

impl Parts {
    /// A second handle on the same parts, for the run future.
    fn share(&self) -> Parts {
        Parts {
            ranges: self.ranges.clone(),
            keeper: self.keeper.clone(),
            link: self.link.clone(),
            state: Arc::clone(&self.state),
            clock: Arc::clone(&self.clock),
            boot_id: self.boot_id,
        }
    }

    /// Publishes the figures the readiness line reads.
    fn publish(&self) {
        let now = node_clock(self.clock.as_ref(), self.boot_id);
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(ranges) = &self.ranges {
            state.remaining = ranges.remaining_all();
        }
        if let Some(keeper) = &self.keeper {
            let keeper = keeper.lock().unwrap_or_else(|p| p.into_inner());
            state.identity = Some((keeper.standing(now), keeper.snapshot_age(now)));
        }
        state.carrier_wired = self.link.is_some();
    }
}

struct EdgeOffline {
    parts: Parts,
    unwired: Option<String>,
}

impl Service for EdgeOffline {
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
        Box::pin(run_loop(self.parts.share(), stop))
    }
}

/// Publishes the readiness figures and, with a link and a range book, drives the carrier.
async fn run_loop(
    parts: Parts,
    mut stop: watch::Receiver<bool>,
) -> std::result::Result<(), String> {
    let mut carrier = match (&parts.link, &parts.ranges) {
        (Some(link), Some(ranges)) => Some(CarrierLoop::new(
            Arc::clone(link),
            Arc::clone(ranges),
            parts.keeper.clone(),
            CarrierSettings::default(),
        )),
        _ => None,
    };
    loop {
        if *stop.borrow() {
            return Ok(());
        }
        let wake = if let Some(carrier) = carrier.as_mut() {
            let now = Instants {
                mono: parts.clock.monotonic(),
                node: node_clock(parts.clock.as_ref(), parts.boot_id),
            };
            let tick = carrier.tick(now).await;
            if !tick.codes.is_empty() {
                tracing::info!(codes = ?tick.codes, "edge_offline: the carrier saw refusals or failures");
            }
            tick.next_wake
        } else {
            Duration::from_secs(30)
        };
        if let Some(keeper) = &parts.keeper {
            let now = node_clock(parts.clock.as_ref(), parts.boot_id);
            let saved = keeper
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .checkpoint(now);
            if let Err(error) = saved {
                tracing::warn!(
                    code = error.code(),
                    "edge_offline: the identity lease was not checkpointed"
                );
            }
        }
        parts.publish();
        tokio::select! {
            () = tokio::time::sleep(wake) => {}
            _ = stop.changed() => {}
        }
    }
}

/// Builds the range book on a plain thread: the bridge, the store, the table check, the book.
fn build_ranges(
    env: &crate::services::SiteEnv<'_>,
    node: &str,
    series: Vec<SeriesPolicy>,
) -> Result<Arc<RangeBook>> {
    let node = partition_node(node).map_err(|e| Error(format!("edge_offline: {e}")))?;
    if series.is_empty() {
        let store: Arc<dyn HighWaterStore> = Arc::new(MemorySeriesStore::new());
        return RangeBook::open(node, series, store, LowWater::half())
            .map(Arc::new)
            .map_err(|e| Error(format!("edge_offline: {e}")));
    }
    let tenant = env.site.site.clone();
    let pool_size = env.loaded.common.postgres.pool_max_size;
    let backend = env.backend.clone();
    crate::services::partition::on_plain_thread("edge_offline bridge", move || {
        let largest = BridgeConfig::largest_in_flight(pool_size);
        if largest < 1 {
            return Err(Error(
                "edge_offline: the PostgreSQL pool is too small for the ranges bridge".to_owned(),
            ));
        }
        let bridge = Bridge::for_backend(
            BridgeConfig {
                name: "edge_offline".to_owned(),
                max_in_flight: largest.min(2),
                workers: 1,
                queue_capacity: 16,
                default_deadline: Duration::from_secs(10),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .map_err(|e| Error(format!("{e}")))?;
        let store = PgRangeStore::new(Arc::new(bridge), tenant, STRIDE);
        store
            .require_table()
            .map_err(|why| Error(format!("edge_offline: {why}")))?;
        let store: Arc<dyn HighWaterStore> = Arc::new(store);
        RangeBook::open(node, series, store, LowWater::half())
            .map(Arc::new)
            .map_err(|e| Error(format!("edge_offline: {e}")))
    })
}

/// Registers the module when `[services.edge_offline]` is configured; absent, it is off.
///
/// # Errors
///
/// [`Error`] for invalid settings, a missing `spot_leased_range` table (run `spot migrate`) or a
/// bridge that cannot be built.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let Some(settings) = ctx.settings::<Settings>(NAME).ok() else {
        return Ok(());
    };
    let plan = plan(&settings)?;
    let state = Arc::new(Mutex::new(OfflineState::default()));
    let clock = Arc::clone(ctx.clock());
    ctx.provide::<EdgeOfflineReadiness>(Arc::new(EdgeOfflineReadiness {
        plan: plan.clone(),
        state: Arc::clone(&state),
        clock: Arc::clone(&clock),
    }))?;
    ctx.provide::<Mutex<OfflineState>>(Arc::clone(&state))?;
    if !ctx.assembling() {
        return Ok(());
    }
    let boot_id = u64::try_from(clock.now_utc().unix_timestamp()).unwrap_or(0);
    let env = *ctx.env()?;
    let node = crate::services::node_role::partition_node_of(env.site);

    let mut unwired = None;
    let mut ranges = None;
    match (&node, plan.series.is_empty()) {
        (Some(node), _) => {
            let book = build_ranges(&env, node, plan.series.clone())?;
            ctx.provide::<RangeBook>(Arc::clone(&book))?;
            ranges = Some(book);
        }
        (None, false) => {
            unwired = Some(
                "edge_offline: series are configured but the site has no [partition] node"
                    .to_owned(),
            );
        }
        (None, true) => {}
    }

    let node_name = node.clone().unwrap_or_else(|| env.site.site.clone());
    ctx.provide::<Mutex<EdgeBalances>>(Arc::new(Mutex::new(EdgeBalances::for_node(
        &node_name,
        plan.balance,
        BTreeMap::new(),
    ))))?;

    let keeper: Option<SharedKeeper> = ctx
        .port::<IdentityParts>()
        .and_then(|parts| parts.open(node_clock(clock.as_ref(), boot_id)))
        .map(|keeper: SnapshotKeeper<BoxedPersistence>| Arc::new(Mutex::new(keeper)));
    if let Some(keeper) = &keeper {
        ctx.provide::<Mutex<SnapshotKeeper<BoxedPersistence>>>(Arc::clone(keeper))?;
    }
    let link = ctx.port::<dyn CarrierLink>();

    let parts = Parts {
        ranges,
        keeper,
        link,
        state,
        clock,
        boot_id,
    };
    parts.publish();
    ctx.add(Arc::new(EdgeOffline { parts, unwired }));
    Ok(())
}
