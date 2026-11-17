//! The periodic sync loops hosted (WP-RTB-02.T6 and T11). The steps and the runner are
//! `spot_sync_loops`; this module assembles them from the sources the other sync modules provide,
//! ticks them, sends what they raise to the conditions service and exposes the health view.
//! Guide: `docs/running-spot/sync_loops.md`.
//!
//! # Two homes
//!
//! * **Edge** (this file, `ROLES = ["edge"]`): obligations are the hub's, so the node's steps are
//!   freshness and stamps, reconcile, healing, closure repair, quarantine and series.
//! * **Hub**: obligations, liveness, certificates and series. The hub row of the role table is
//!   `sync_hub_loops` (`node_role::SERVICE_SETS`); a one-file module `services/sync_hub_loops.rs`
//!   re-exports [`HUB_NAME`], [`HUB_ORDER`], [`HUB_ROLES`] as `NAME`, `ORDER`, `ROLES` and calls
//!   [`register_hub`] (the registry generates one entry per file, so a module is a file).
//!
//! # A step is wired when its source is provided
//!
//! Each step needs a source from the module that owns the store (the ledger, the registry, the
//! repair list, the tracker). A source a module provides as a [`Take`] port is moved into its step
//! once. A step with **no** source is left out and listed as not wired on the health view: nothing is
//! faked, and a clean `calm` line means the wired steps found nothing. The digest request of
//! `reconcile` has no wire kind yet, so that step is always added: without a provider it says
//! `not available` on every run (see `spot_sync_loops::reconcile`).
//!
//! # Conditions
//!
//! Steps raise and clear through a buffer; after each tick the ticker sends the buffer to the
//! conditions handle (`raise` / `observe_cleared`), so no lock is held across an await and the
//! conditions service keeps its debounce and flap guard. An id the site's registry does not hold is
//! logged by id and dropped.
//!
//! # Settings
//!
//! ```toml
//! [services.sync_loops]
//! max_items = 200            # most subjects one step examines per run (1 to 10000)
//! reconcile_scopes = ["branch_01"]
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use serde::Deserialize;
use spot_conditions::{ConditionParams, Millis};
use spot_core::Clock;
use spot_http::health::{CheckResult, ReadinessCheck};
use spot_sync_loops::certificate::{CertificateSource, CertificateStep, Windows as CertWindows};
use spot_sync_loops::closure::{
    ClosureRepairStep, RepairPull, RepairSource, Windows as RepairWindows,
};
use spot_sync_loops::freshness::{
    FreshnessSource, FreshnessStep, StampSink, Windows as FreshWindows,
};
use spot_sync_loops::healing::{Budget, Healer, HealingSource, HealingStep};
use spot_sync_loops::liveness::{LivenessStep, NodeFactSource, Thresholds};
use spot_sync_loops::obligations::{
    ObligationSource, ObligationsStep, Responder, Windows as ObWindows,
};
use spot_sync_loops::quarantine::{
    OpenItem, QuarantineSource, QuarantineStep, Windows as QuarantineWindows,
};
use spot_sync_loops::reconcile::{DigestPort, NoDigest, ReconcileStep};
use spot_sync_loops::series::{SampleSource, SeriesStep};
use spot_sync_loops::{
    ConditionSink, Decision, DecisionSink, Loops, Raised, Severity, SharedDecisions, SyncHealthView,
};
use tokio::sync::watch;

use crate::services::conditions::ConditionsHandle;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The module's name.
pub const NAME: &str = "sync_loops";
/// After the sync modules.
pub const ORDER: u32 = 1220;
/// Edge only (the row in `node_role::SERVICE_SETS`).
pub const ROLES: &[&str] = &["edge"];
/// The hub's home: the name of the one-file module that calls [`register_hub`].
pub const HUB_NAME: &str = "sync_hub_loops";
/// The hub home's start order (after `sync_hub` at 1300 provides what the hub steps read).
pub const HUB_ORDER: u32 = 1320;
/// The hub home's roles.
pub const HUB_ROLES: &[&str] = &["hub"];
/// The seconds between ticks; each step has its own, longer interval.
pub const TICK_SECS: u64 = 5;

/// Stable code: a step was left out because no module provides its source (a message, on the
/// health view).
pub const STEP_NOT_WIRED: &str = "sync_loops_host.step_not_wired";

/// A source a module provides once: the step takes it.
pub type Take<T> = Mutex<Option<Box<T>>>;

/// `[services.sync_loops]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Most subjects one step examines per run (1 to 10000, default 200).
    pub max_items: Option<usize>,
    /// The scopes the reconcile step compares, round-robin.
    pub reconcile_scopes: Vec<String>,
}

impl Settings {
    /// The bound, checked.
    ///
    /// # Errors
    ///
    /// [`Error`] when `max_items` is out of bounds.
    pub fn max_items(&self) -> Result<usize> {
        let value = self.max_items.unwrap_or(200);
        if (1..=10_000).contains(&value) {
            Ok(value)
        } else {
            Err(Error(
                "services.sync_loops.max_items must be between 1 and 10000".to_owned(),
            ))
        }
    }
}

/// The sources the other modules provide. A `None` is a step that is not wired.
#[derive(Default)]
pub struct Sources {
    /// Freshness ages and the stale stamp.
    pub freshness: Option<(Box<dyn FreshnessSource>, Box<dyn StampSink>)>,
    /// What healing can mend and how.
    pub healing: Option<(Box<dyn HealingSource>, Box<dyn Healer>)>,
    /// The repair list and the repair pull.
    pub closure: Option<(Box<dyn RepairSource>, Box<dyn RepairPull>)>,
    /// Open quarantine items.
    pub quarantine: Option<Box<dyn QuarantineSource>>,
    /// The digest request. `None` leaves reconcile saying "not available".
    pub digest: Option<Box<dyn DigestPort>>,
    /// Expiring node certificates (hub).
    pub certificates: Option<Box<dyn CertificateSource>>,
    /// The numbers to sample.
    pub samples: Option<Box<dyn SampleSource>>,
    /// Open obligations and the ladder's responses (hub).
    pub obligations: Option<(Box<dyn ObligationSource>, Box<dyn Responder>)>,
    /// Node facts (hub).
    pub nodes: Option<Box<dyn NodeFactSource>>,
}

/// What [`assemble`] built.
pub struct Assembled {
    /// The runner.
    pub loops: Loops,
    /// The steps that were wired, in order.
    pub wired: Vec<&'static str>,
    /// The steps left out because no source was provided.
    pub not_wired: Vec<&'static str>,
}

/// Builds the runner from `sources`. Pure: every input is an argument.
#[must_use]
pub fn assemble(
    sources: Sources,
    decisions: &SharedDecisions,
    view: &SyncHealthView,
    settings: &Settings,
    max_items: usize,
) -> Assembled {
    let mut loops = Loops::new(max_items);
    let mut wired = Vec::new();
    let mut not_wired = Vec::new();
    let mut add = |loops: Loops, name: &'static str| {
        wired.push(name);
        loops
    };
    match sources.obligations {
        Some((source, responder)) => {
            loops = add(
                loops.with_step(Box::new(ObligationsStep::new(
                    source,
                    responder,
                    ObWindows::default(),
                ))),
                "obligations",
            );
        }
        None => not_wired.push("obligations"),
    }
    match sources.nodes {
        Some(source) => {
            loops = add(
                loops.with_step(Box::new(LivenessStep::new(source, Thresholds::default()))),
                "liveness",
            );
        }
        None => not_wired.push("liveness"),
    }
    match sources.freshness {
        Some((source, stamps)) => {
            loops = add(
                loops.with_step(Box::new(FreshnessStep::new(
                    source,
                    stamps,
                    FreshWindows::default(),
                    decisions.clone(),
                ))),
                "freshness",
            );
        }
        None => not_wired.push("freshness"),
    }
    // Always added: without a provider it says so on every run, it never reports clean.
    let digest: Box<dyn DigestPort> = sources.digest.unwrap_or_else(|| Box::new(NoDigest));
    loops = add(
        loops.with_step(Box::new(ReconcileStep::new(
            digest,
            settings.reconcile_scopes.clone(),
        ))),
        "reconcile",
    );
    match sources.healing {
        Some((source, healer)) => {
            loops = add(
                loops.with_step(Box::new(HealingStep::new(
                    source,
                    healer,
                    Budget::default(),
                    decisions.clone(),
                ))),
                "healing",
            );
        }
        None => not_wired.push("healing"),
    }
    match sources.closure {
        Some((source, pull)) => {
            loops = add(
                loops.with_step(Box::new(ClosureRepairStep::new(
                    source,
                    pull,
                    RepairWindows::default(),
                    decisions.clone(),
                ))),
                "closure_repair",
            );
        }
        None => not_wired.push("closure_repair"),
    }
    match sources.quarantine {
        Some(source) => {
            loops = add(
                loops.with_step(Box::new(QuarantineStep::new(
                    source,
                    QuarantineWindows::default(),
                ))),
                "quarantine",
            );
        }
        None => not_wired.push("quarantine"),
    }
    match sources.certificates {
        Some(source) => {
            loops = add(
                loops.with_step(Box::new(CertificateStep::new(
                    source,
                    CertWindows::default(),
                ))),
                "certificate",
            );
        }
        None => not_wired.push("certificate"),
    }
    match sources.samples {
        Some(source) => {
            loops = add(
                loops.with_step(Box::new(SeriesStep::new(source, view.clone()))),
                "series",
            );
        }
        None => not_wired.push("series"),
    }
    Assembled {
        loops,
        wired,
        not_wired,
    }
}

/// What a tick raised or cleared, buffered until the ticker can send it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// Raise (or keep open).
    Raise(Raised),
    /// Clear.
    Clear(&'static str, String),
}

/// A [`ConditionSink`] that buffers.
#[derive(Default)]
pub struct BufferSink(pub Vec<Op>);

impl ConditionSink for BufferSink {
    fn raise(&mut self, raised: &Raised) {
        self.0.push(Op::Raise(raised.clone()));
    }

    fn clear(&mut self, condition: &'static str, subject: &str) {
        self.0.push(Op::Clear(condition, subject.to_owned()));
    }
}

/// Decisions go to the log until a durable `sys_sync_decision` writer is provided (the site has
/// no audit chain yet): identities and fixed words only.
pub struct LogDecisions;

impl DecisionSink for LogDecisions {
    fn record(&mut self, decision: &Decision) {
        tracing::info!(
            topic = spot_sync_loops::SYS_SYNC_DECISION,
            step = decision.step,
            action = decision.action,
            subject = %decision.subject,
            at = decision.at,
            ok = decision.ok,
            "sync loop decision"
        );
    }
}

/// An open quarantine list refreshed by the ticker (the store's call is async; a step is not).
#[derive(Clone, Default)]
pub struct QuarantineCache(Arc<Mutex<Vec<OpenItem>>>);

impl QuarantineCache {
    /// Replaces the list.
    pub fn set(&self, items: Vec<OpenItem>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = items;
    }
}

impl QuarantineSource for QuarantineCache {
    fn open(&mut self, max: usize) -> std::result::Result<Vec<OpenItem>, String> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .take(max)
            .cloned()
            .collect())
    }
}

/// The readiness line. Always ok: open conditions are information, not a failure.
pub struct SyncLoopsReadiness {
    view: SyncHealthView,
    not_wired: Vec<&'static str>,
}

impl ReadinessCheck for SyncLoopsReadiness {
    fn check(&self) -> Pin<Box<dyn Future<Output = CheckResult> + Send + '_>> {
        Box::pin(async move {
            let mut detail = self.view.line();
            for (step, reason) in self.view.failed() {
                detail.push_str(&format!("; {step}: {reason}"));
            }
            if !self.not_wired.is_empty() {
                detail.push_str(&format!("; not wired: {}", self.not_wired.join(", ")));
            }
            CheckResult { ok: true, detail }
        })
    }

    fn name(&self) -> &str {
        NAME
    }
}

struct Ticker {
    name: &'static str,
    loops: Mutex<Loops>,
    view: SyncHealthView,
    conditions: Option<ConditionsHandle>,
    quarantine: Option<(QuarantineCache, Arc<dyn spot_quarantine::QuarantineStore>)>,
    clock: Arc<dyn Clock>,
}

impl Ticker {
    /// Refreshes the quarantine cache from the (async) store.
    async fn refresh_quarantine(&self, now: i64) {
        let Some((cache, store)) = &self.quarantine else {
            return;
        };
        match store.list(&spot_quarantine::ItemFilter::all()).await {
            Ok(items) => {
                let now_micros = now.saturating_mul(1_000_000);
                cache.set(
                    items
                        .iter()
                        .filter(|item| item.state.is_open())
                        .map(|item| OpenItem {
                            subject: item.id.to_string(),
                            age_secs: now_micros.saturating_sub(item.first_seen) / 1_000_000,
                        })
                        .collect(),
                );
            }
            Err(error) => {
                tracing::warn!(
                    code = error.code(),
                    "sync loops: the quarantine list was not read"
                );
            }
        }
    }

    async fn send(&self, ops: Vec<Op>, now: i64) {
        let Some(handle) = &self.conditions else {
            return;
        };
        let at = Millis::new(u64::try_from(now.saturating_mul(1000)).unwrap_or(0));
        for op in ops {
            let result = match &op {
                Op::Raise(raised) => handle
                    .raise(
                        raised.condition,
                        &raised.subject,
                        ConditionParams::new(),
                        at,
                    )
                    .await
                    .map(|inner| inner.map(|_| ())),
                Op::Clear(condition, subject) => handle
                    .observe_cleared(condition, subject, at)
                    .await
                    .map(|inner| inner.map(|_| ())),
            };
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    // An id the registry does not hold, or an engine refusal: by code, once per tick.
                    tracing::warn!(
                        step_module = self.name,
                        code = error.code(),
                        "sync loops: a condition was refused"
                    );
                }
                Err(_) => return,
            }
        }
    }
}

impl Service for Ticker {
    fn name(&self) -> &'static str {
        self.name
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            loop {
                tokio::select! {
                    () = tokio::time::sleep(Duration::from_secs(TICK_SECS)) => {
                        let now = self.clock.now_utc().unix_timestamp();
                        self.refresh_quarantine(now).await;
                        let (report, summary, ops) = {
                            let mut loops = self.loops.lock().unwrap_or_else(PoisonError::into_inner);
                            let mut buffer = BufferSink::default();
                            let report = loops.tick(now, &mut buffer);
                            (report, loops.summary(), buffer.0)
                        };
                        for (step, reason) in &report.failed {
                            tracing::warn!(step, reason = %reason, "a sync loop step failed");
                        }
                        self.view.update(now, report, summary);
                        self.send(ops, now).await;
                    }
                    _ = stop.changed() => return Ok(()),
                }
            }
        })
    }
}

/// The sources the ports provide. Each [`Take`] port is taken once.
fn from_ports(ctx: &SiteCtx<'_>) -> (Sources, Option<Arc<dyn spot_quarantine::QuarantineStore>>) {
    fn take<T: ?Sized + Send + 'static>(port: Option<Arc<Take<T>>>) -> Option<Box<T>> {
        port?.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
    let mut sources = Sources::default();
    if let (Some(source), Some(stamps)) = (
        take(ctx.port::<Take<dyn FreshnessSource>>()),
        take(ctx.port::<Take<dyn StampSink>>()),
    ) {
        sources.freshness = Some((source, stamps));
    }
    if let (Some(source), Some(healer)) = (
        take(ctx.port::<Take<dyn HealingSource>>()),
        take(ctx.port::<Take<dyn Healer>>()),
    ) {
        sources.healing = Some((source, healer));
    }
    if let (Some(source), Some(pull)) = (
        take(ctx.port::<Take<dyn RepairSource>>()),
        take(ctx.port::<Take<dyn RepairPull>>()),
    ) {
        sources.closure = Some((source, pull));
    }
    sources.digest = take(ctx.port::<Take<dyn DigestPort>>());
    sources.certificates = take(ctx.port::<Take<dyn CertificateSource>>());
    sources.samples = take(ctx.port::<Take<dyn SampleSource>>());
    if let (Some(source), Some(responder)) = (
        take(ctx.port::<Take<dyn ObligationSource>>()),
        take(ctx.port::<Take<dyn Responder>>()),
    ) {
        sources.obligations = Some((source, responder));
    }
    sources.nodes = take(ctx.port::<Take<dyn NodeFactSource>>());
    let store = ctx.port::<dyn spot_quarantine::QuarantineStore>();
    (sources, store)
}

fn register_with(ctx: &mut SiteCtx<'_>, name: &'static str, edge: bool) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let max_items = settings.max_items()?;
    if !ctx.assembling() {
        return Ok(());
    }
    let (mut sources, store) = from_ports(ctx);
    let cache = QuarantineCache::default();
    if edge {
        if store.is_some() {
            sources.quarantine = Some(Box::new(cache.clone()));
        }
    } else {
        // The hub's steps are obligations, liveness, certificates and series.
        sources.freshness = None;
        sources.healing = None;
        sources.closure = None;
        sources.quarantine = None;
    }
    let decisions = SharedDecisions::new(Box::new(LogDecisions));
    let view = SyncHealthView::new();
    let assembled = assemble(sources, &decisions, &view, &settings, max_items);
    ctx.provide::<SyncHealthView>(Arc::new(view.clone()))?;
    ctx.provide::<SyncLoopsReadiness>(Arc::new(SyncLoopsReadiness {
        view: view.clone(),
        not_wired: assembled.not_wired.clone(),
    }))?;
    ctx.add(Arc::new(Ticker {
        name,
        loops: Mutex::new(assembled.loops),
        view,
        conditions: ctx.port::<ConditionsHandle>().map(|h| (*h).clone()),
        quarantine: if edge {
            store.map(|s| (cache, s))
        } else {
            None
        },
        clock: Arc::clone(ctx.clock()),
    }));
    Ok(())
}

/// Registers the edge's loops.
///
/// # Errors
///
/// [`Error`] for a bound out of range or a port that cannot be provided twice.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    register_with(ctx, NAME, true)
}

/// Registers the hub's loops: called by the one-file module `sync_hub_loops`.
///
/// # Errors
///
/// As [`register`].
pub fn register_hub(ctx: &mut SiteCtx<'_>) -> Result<()> {
    register_with(ctx, HUB_NAME, false)
}

/// The open conditions by severity word, for a command or a test.
#[must_use]
pub fn tally(ops: &[Op]) -> BTreeMap<&'static str, usize> {
    let mut out = BTreeMap::new();
    for op in ops {
        if let Op::Raise(raised) = op {
            let word = match raised.severity {
                Severity::Info => "info",
                Severity::Amber => "amber",
                Severity::Red => "red",
            };
            *out.entry(word).or_insert(0) += 1;
        }
    }
    out
}
