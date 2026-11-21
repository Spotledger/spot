//! The hub's catch-up feed as a module (WP-AI-07.T16, ADR-066). Guide:
//! `docs/running-spot/sync_catchup_wiring.md` and `sync_hub_edge_desk.md`.
//!
//! `sync_catchup_wiring` wires the hub's three catch-up calls (route, chunk, acknowledge) only when
//! it holds a [`CatchupFeed`], the hub host's view of a node's gap and of the plan for it. Before
//! this module nobody provided one, so no hub wired catch-up, and the edge desk's `resync` answered
//! `not supported`. On a **hub** role this module provides `dyn CatchupFeed`; an edge, a standalone
//! site and a disabled module provide none (the wiring then names the missing piece, as before).
//!
//! # Order, and why the feed binds late
//!
//! The wiring (1160) reads the feed port when it registers, but what the feed reads (the registry
//! and the outbox through the hub's stores, the sync policy) is built by `sync_hub_parts` (1250).
//! So this module registers at [`ORDER`] (1158, between the chunk server's 1155 and the wiring's
//! 1160) and provides a [`LazyFeed`]; a finaliser, which runs after every module has registered,
//! builds the real [`feed::HubFeed`] from the `PgHub` and `PolicyHandle` ports and binds it. Until
//! it is bound (and when a port is missing, which the readiness line names) every question is
//! answered `None`: the director refuses the call and nothing is planned on a guess.
//!
//! # What the feed does
//!
//! See [`feed`]: `view` reports the node's gap from the registry's recorded acknowledgements, the
//! stream head and the outbox; `plan` builds the coalesced plan with
//! `spot_sync_catchup::{scope_for_node, plan}` over the events the hub still holds. A gap the hub
//! cannot plan from what it holds (a node that never acknowledged anything, a prefix below the
//! outbox, more rows than `max_events`, an undecodable event) is reported as a resync, which the
//! director turns into a snapshot bootstrap, and `plan` answers `None`: never a plan from a partial
//! read.
//!
//! # Settings
//!
//! ```toml
//! [services.sync_catchup_feed]
//! enabled = true          # false provides no feed
//! max_events = 20000      # 100 to 50000: outbox rows read by one computation
//! cache_secs = 120        # 5 to 3600: an unused computation is dropped after this
//! max_nodes = 32          # 1 to 1024: nodes kept
//! snapshot_bytes = 0      # optional: the estimate of a full scoped snapshot, bytes (0 = unknown)
//! ```
//!
//! The planner's thresholds are the director's: `[services.sync_catchup]`
//! (`catchup_threshold_events`, `snapshot_share_percent`, `bundle_records`), so the route and the
//! plan agree. A reload provides the same feed again.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use spot_sync_catchup::{CatchUpPlan, Settings as PlannerSettings};
use spot_sync_catchup_host::CatchupConfig;
use spot_sync_hub_pg::PgHub;

use crate::services::node_role::{ActiveRole, Role};
use crate::services::sync_catchup_wiring::{CatchupFeed, NodeView};
use crate::services::sync_policy::PolicyHandle;
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

#[path = "sync_catchup_feed/feed.rs"]
pub mod feed;
#[path = "sync_catchup_feed/pg.rs"]
pub mod pg;

/// The module's name and its `[services.<name>]` table.
pub const NAME: &str = "sync_catchup_feed";

/// After `sync_catchup` (1150), `sync_catchup_serve` (1155) and before `sync_catchup_wiring`
/// (1160), whose port it provides. The feed's own inputs arrive later (see the module docs).
pub const ORDER: u32 = 1158;

/// `[services.sync_catchup_feed]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// `false` provides no feed.
    pub enabled: bool,
    /// The most outbox rows one computation reads (100 to 50,000).
    pub max_events: usize,
    /// How long an unused computation is kept, in seconds (5 to 3,600).
    pub cache_secs: u64,
    /// How many nodes are kept (1 to 1,024).
    pub max_nodes: usize,
    /// The operator's estimate of a full scoped snapshot, in bytes; absent or 0 is unknown (the
    /// cost rule then never prefers a snapshot to a coalesced plan).
    pub snapshot_bytes: Option<u64>,
}

impl Default for Settings {
    fn default() -> Self {
        let defaults = feed::FeedSettings::default();
        Self {
            enabled: true,
            max_events: defaults.max_events,
            cache_secs: 120,
            max_nodes: defaults.max_nodes,
            snapshot_bytes: None,
        }
    }
}

impl Settings {
    /// The bounds, checked.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the setting that is out of bounds.
    pub fn resolve(&self) -> Result<feed::FeedSettings> {
        if !(100..=50_000).contains(&self.max_events) {
            return Err(Error(
                "services.sync_catchup_feed.max_events must be between 100 and 50000".to_owned(),
            ));
        }
        if !(5..=3_600).contains(&self.cache_secs) {
            return Err(Error(
                "services.sync_catchup_feed.cache_secs must be between 5 and 3600".to_owned(),
            ));
        }
        if !(1..=1_024).contains(&self.max_nodes) {
            return Err(Error(
                "services.sync_catchup_feed.max_nodes must be between 1 and 1024".to_owned(),
            ));
        }
        Ok(feed::FeedSettings {
            max_events: self.max_events,
            cache_secs: i64::try_from(self.cache_secs).unwrap_or(i64::MAX),
            max_nodes: self.max_nodes,
            snapshot_bytes: self.snapshot_bytes.filter(|bytes| *bytes > 0),
        })
    }
}

/// The feed port, bound to the real feed once the hub's stores exist. Unbound it answers `None` to
/// everything.
#[derive(Default)]
pub struct LazyFeed {
    inner: Mutex<Option<Arc<dyn CatchupFeed>>>,
}

impl LazyFeed {
    /// An unbound feed.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Binds the real feed.
    pub fn bind(&self, feed: Arc<dyn CatchupFeed>) {
        *self.inner.lock().unwrap_or_else(PoisonError::into_inner) = Some(feed);
    }

    /// Whether a real feed is bound.
    #[must_use]
    pub fn is_bound(&self) -> bool {
        self.bound().is_some()
    }

    fn bound(&self) -> Option<Arc<dyn CatchupFeed>> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl CatchupFeed for LazyFeed {
    fn view(&self, node: &str) -> Option<NodeView> {
        // The lock is released before the call: the feed reads stores.
        self.bound()?.view(node)
    }

    fn plan(&self, node: &str) -> Option<CatchUpPlan> {
        self.bound()?.plan(node)
    }
}

fn provided() -> &'static Mutex<HashMap<String, Arc<LazyFeed>>> {
    static PROVIDED: std::sync::OnceLock<Mutex<HashMap<String, Arc<LazyFeed>>>> =
        std::sync::OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The feed a hub site provided at its first build (for a harness and for a reload).
#[must_use]
pub fn hosted(site: &str) -> Option<Arc<LazyFeed>> {
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(site)
        .cloned()
}

/// Says what is missing, in the log and in readiness.
fn not_ready(ctx: &mut SiteCtx<'_>, missing: &[&'static str]) {
    let reason = format!(
        "sync_catchup_serve.not_ready: {} not provided to the catch-up feed",
        missing.join(", ")
    );
    tracing::info!(
        code = "sync_catchup_serve.not_ready",
        missing = %missing.join(", "),
        "sync_catchup_feed is not ready"
    );
    ctx.add(Idle::new(NAME, Level::Optional, move || {
        let reason = reason.clone();
        Box::pin(async move { Err(reason) })
    }));
}

/// Builds the real feed from the ports that exist after every module registered, and binds it.
fn bind(
    ctx: &mut SiteCtx<'_>,
    lazy: &Arc<LazyFeed>,
    settings: feed::FeedSettings,
    planner: PlannerSettings,
) -> Result<()> {
    let hub = ctx.port::<PgHub>();
    let policy = ctx.port::<PolicyHandle>();
    let mut missing: Vec<&'static str> = Vec::new();
    if hub.is_none() {
        missing.push("the hub's stores");
    }
    if policy.is_none() {
        missing.push("the sync policy");
    }
    let (Some(hub), Some(policy)) = (hub, policy) else {
        not_ready(ctx, &missing);
        return Ok(());
    };
    let env = *ctx.env()?;
    let meta = env.live.meta_source();
    let epoch = meta
        .current()
        .map_or(0, |current| current.version.unwrap_or(0));
    let source = pg::PgSource::new(
        (*hub).clone(),
        &env.site.site,
        policy,
        pg::node_report(epoch),
    );
    let real = feed::HubFeed::new(
        &env.site.site,
        Arc::new(source),
        Arc::new(pg::MetaLinks::new(meta)),
        planner,
        settings,
        Arc::clone(ctx.clock()),
    );
    lazy.bind(Arc::new(real));
    Ok(())
}

/// Provides `dyn CatchupFeed` on a hub; nothing on any other role or when disabled.
///
/// # Errors
///
/// A setting that does not decode or is out of bounds, an invalid `[services.sync_catchup]`
/// planner setting, or the port provided twice.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    if !settings.enabled {
        return Ok(());
    }
    let resolved = settings.resolve()?;
    let site = ctx.label().to_owned();
    if !ctx.assembling() {
        // A reload keeps the running feed: the same one is provided again.
        if let Some(existing) = hosted(&site) {
            ctx.provide::<dyn CatchupFeed>(existing as Arc<dyn CatchupFeed>)?;
        }
        return Ok(());
    }
    let Some(active) = ctx.port::<ActiveRole>() else {
        return Ok(());
    };
    if !matches!(active.role(), Role::Hub) {
        return Ok(());
    }
    // The planner plans with the director's own thresholds, so the route and the plan agree.
    let catchup: CatchupConfig = ctx.settings_or_default("sync_catchup")?;
    let planner = catchup
        .planner()
        .map_err(|e| Error(format!("services.sync_catchup_feed: {}", e.code())))?;
    let lazy = LazyFeed::new();
    ctx.provide::<dyn CatchupFeed>(Arc::clone(&lazy) as Arc<dyn CatchupFeed>)?;
    provided()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(site, Arc::clone(&lazy));
    ctx.finally(move |ctx| bind(ctx, &lazy, resolved, planner));
    Ok(())
}
