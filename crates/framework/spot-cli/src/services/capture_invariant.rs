//! The capture invariant, hosted: a loop that compares the site's change feed with its journal's
//! `doc_state` events and raises `CAPTURE_MISMATCH` when a write was missed (WP-JRN-02.T3,
//! ADR-061 sections 3 and 8).
//!
//! # Why
//!
//! The document engine writes one `doc_state` event for every document a transaction wrote
//! (WP-JRN-02.T2), and storage's own change feed is the independent witness that it did. A write
//! that bypassed the engine leaves a feed entry with no event, which a rebuild from the journal
//! would silently lose. `spot-capture-invariant` is the pure comparison and
//! `spot-capture-invariant-pg` reads both streams; this module runs them on a timer and turns
//! the result into a condition an operator sees within minutes.
//!
//! # A pass
//!
//! Every `services.capture_invariant.interval_seconds` (default 60, never below 10) the service
//! builds the checker's exclusions from the document engine's declared capture exclusions
//! ([`spot_document::engine::Engine::capture_exclusions`], read again on every pass, so a newly
//! excluded DocType is not reported), reads both streams in one read-only snapshot from the
//! persisted watermark (`spot_capture_watermark`) and compares them. The rules of settling,
//! the lag and resuming are in `spot_capture_invariant_pg::scan`'s documentation. In short:
//!
//! - a **settled** finding (every position it names was published at least
//!   `services.capture_invariant.lag_seconds` before the snapshot) of kind missing event, orphan
//!   event or version mismatch raises `CAPTURE_MISMATCH` once per record, on the subject
//!   `<source>:<entity>/<id>` with the parameters `source` (`change_feed` or `doc_state`) and
//!   `record` (`entity/id`); never a value. The condition engine de-duplicates a second raise;
//! - the watermark is held back below the earliest open finding, so the finding is found again by
//!   every pass (and after a restart), and **clears** only when a later pass finds the entry
//!   matched: a clean pass past the previously failing position;
//! - `OutOfWindow` findings (an entry left the checker's window unmatched) raise only when the
//!   window is larger than the **measured lag**, the entries published in the last `lag_seconds`.
//!   A smaller window may simply be too small: the entry is counted and logged at warn level
//!   with the numbers, and raises nothing. Size `window` above the busiest `lag_seconds` of the
//!   site (the default is 50,000 versions);
//! - duplicate `doc_state` events and entries younger than the lag do not raise: they are counted
//!   (the log line, [`PassSummary`]) and the young ones are judged by a later pass.
//!
//! # Failure behaviour
//!
//! The service is **optional**: it never stops the site. A pass that cannot read the streams (or
//! cannot reach the document engine) changes nothing, keeps the watermark and counts a failure;
//! after [`DEGRADED_AFTER_FAILURES`] consecutive failed passes [`CaptureInvariantService::
//! readiness_text`] reports `degraded`, not `failed`, and the first good pass resets it. A pass
//! runs in its own task, so a panic in it is a failed pass and not a stopped service. A raise
//! that fails (the conditions service is down) fails the pass before the watermark is saved.
//!
//! # Tenancy and secrets
//!
//! One database is one site: the pass reads the site's own pool and never names another site.
//! The snapshot is read-only (the database refuses a write), the readers select no payload but
//! the operation, and nothing logged or raised carries a document's content: only `entity/id`,
//! counts and the stable error codes.
//!
//! # CI
//!
//! [`CaptureGate::run_once`] compares the whole feed with the whole journal with no lag, for a
//! harness that has stopped writing; a non-empty report is a failing assertion.
//!
//! # Tables
//!
//! `spot_capture_watermark` is created by `spot migrate` (it is in `schema::extra_tables`); this
//! module never creates a table. A site that has not run it starts with this service degraded and
//! says so.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
pub use spot_capture_invariant::{Exclusions, Finding, FindingKind, Report};
pub use spot_capture_invariant_pg::{
    CaptureGate, CapturePgError, Scan, ScanConfig, Watermarks, schema as capture_schema,
};
use spot_conditions::{ConditionParams, Millis};
use spot_core::Clock;
use spot_document::engine::ExclusionKind;
use tokio::sync::watch;

use crate::live::SiteLive;
use crate::services::conditions::ConditionsHandle;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The id of the condition this service raises.
pub const CONDITION_ID: &str = "CAPTURE_MISMATCH";

/// Start order: after the conditions module (1000), whose handle this service raises through.
pub const ORDER: u32 = 1050;

/// The shortest interval between passes.
pub const MIN_INTERVAL_SECONDS: u64 = 10;

/// Consecutive failed passes before readiness says `degraded`.
pub const DEGRADED_AFTER_FAILURES: u32 = 3;

/// `[services.capture_invariant]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the service runs. Off by default: a site opts in once `spot migrate` has created
    /// the watermark table (the host refuses to start with it missing).
    pub enabled: bool,
    /// Seconds between passes; values below [`MIN_INTERVAL_SECONDS`] are raised to it.
    pub interval_seconds: u64,
    /// Seconds a position must have been published before a finding about it is judged. It must
    /// exceed the longest gap between the publication of a write's feed entry and its event.
    pub lag_seconds: u64,
    /// The checker's window, in document versions. At least 1.
    pub window: usize,
    /// Entries read per statement. At least 1.
    pub page_size: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            interval_seconds: 60,
            lag_seconds: spot_capture_invariant_pg::DEFAULT_LAG_SECS,
            window: spot_capture_invariant_pg::DEFAULT_WINDOW,
            page_size: spot_capture_invariant_pg::DEFAULT_PAGE_SIZE,
        }
    }
}

impl Settings {
    /// The interval between passes, never below [`MIN_INTERVAL_SECONDS`].
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_seconds.max(MIN_INTERVAL_SECONDS))
    }

    /// Refuses a window or a page size of 0.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the key.
    pub fn validate(&self) -> Result<()> {
        if self.window == 0 {
            return Err(Error(
                "services.capture_invariant.window must be at least 1".to_owned(),
            ));
        }
        if self.page_size == 0 {
            return Err(Error(
                "services.capture_invariant.page_size must be at least 1".to_owned(),
            ));
        }
        if i64::try_from(self.lag_seconds).is_err() {
            return Err(Error(
                "services.capture_invariant.lag_seconds is too large".to_owned(),
            ));
        }
        Ok(())
    }

    fn scan(&self) -> ScanConfig {
        ScanConfig {
            page_size: self.page_size,
            window: self.window,
            lag_secs: self.lag_seconds,
        }
    }
}

/// Where a pass gets the capture exclusions of the site's engine.
pub trait ExclusionSource: Send + Sync {
    /// The exclusions in force now.
    ///
    /// # Errors
    ///
    /// A reason a pass cannot judge: the engine is not running. The pass then fails (it never
    /// judges with an empty list, which would report every excluded DocType as a miss).
    fn exclusions(&self) -> std::result::Result<Exclusions, String>;
}

/// The exclusions of the site's running document engine.
pub struct EngineExclusions(Arc<SiteLive>);

impl EngineExclusions {
    /// The exclusions of the engine `live` currently serves.
    pub fn new(live: Arc<SiteLive>) -> Self {
        Self(live)
    }
}

impl ExclusionSource for EngineExclusions {
    fn exclusions(&self) -> std::result::Result<Exclusions, String> {
        let engine = self
            .0
            .engine()
            .ok_or_else(|| "the document engine is not running".to_owned())?;
        let mut out = Exclusions::new();
        for exclusion in engine.capture_exclusions() {
            out = match exclusion.kind {
                ExclusionKind::Derived => out.derived(
                    &exclusion.doctype,
                    &exclusion.owner,
                    &exclusion.reason_code,
                    None,
                ),
                ExclusionKind::Local => {
                    out.local(&exclusion.doctype, &exclusion.owner, &exclusion.reason_code)
                }
            }
            .map_err(|e| e.to_string())?;
        }
        Ok(out)
    }
}

/// How a pass makes sure everything committed has a position before it reads.
///
/// The storage feed assigns positions when a reader asks (`read_outbox`, `read_changes`): until
/// then a committed entry is *pending* and invisible to a reader of the published positions. A
/// running host publishes the outbox through the jobs and events services, but nothing else
/// asks for the change feed, so this service does, once per pass, through the backend's own
/// publisher. That is the storage's act on its own tables, not a write by this service.
pub trait FeedPublisher: Send + Sync {
    /// Publishes what is pending in both logs. A failure is not fatal: the pass reads what is
    /// published.
    fn publish(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>>;
}

/// The publisher of the site's backend.
pub struct BackendPublisher(spot_postgres::PgBackend);

impl BackendPublisher {
    /// Publishes through `backend`.
    pub fn new(backend: spot_postgres::PgBackend) -> Self {
        Self(backend)
    }
}

impl FeedPublisher for BackendPublisher {
    fn publish(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            use spot_storage_contract::{ChangeFeed, FeedPosition};
            // Publication commits before the read validates the bookmark, so even a refused read
            // (a resync-required answer) has published.
            let _ = self.0.read_changes(FeedPosition::START, 1).await;
            let _ = self.0.read_outbox(FeedPosition::START, 1).await;
        })
    }
}

/// Publishes nothing: a database whose positions a test sets itself.
pub struct NoPublisher;

impl FeedPublisher for NoPublisher {
    fn publish(&self) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// What one pass did. Counts only: never an id, an entity or a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PassSummary {
    /// Findings raised (or re-raised) this pass.
    pub raised: usize,
    /// Conditions cleared this pass.
    pub cleared: usize,
    /// Findings about entries younger than the lag.
    pub deferred: usize,
    /// Settled findings that raise nothing (duplicate events, out-of-window entries of a window
    /// not larger than the lag).
    pub noted: usize,
    /// Findings open after the pass.
    pub open: usize,
    /// Feed entries read.
    pub feed_entries: u64,
    /// Events read.
    pub state_events: u64,
    /// The watermark after the pass.
    pub watermark: Watermarks,
}

#[derive(Default)]
struct State {
    watermark: Watermarks,
    loaded: bool,
    seeded: bool,
    open: BTreeSet<String>,
    failures: u32,
    passes: u64,
    last_error: Option<&'static str>,
}

struct Inner {
    site: String,
    backend: spot_postgres::PgBackend,
    exclusions: Arc<dyn ExclusionSource>,
    publisher: Arc<dyn FeedPublisher>,
    conditions: Arc<ConditionsHandle>,
    clock: Arc<dyn Clock>,
    settings: Settings,
    state: Mutex<State>,
}

/// The capture invariant of one site.
pub struct CaptureInvariantService {
    inner: Arc<Inner>,
}

/// Builds the service over `backend`'s pool. Exposed so a test (or a direct host) can drive it through the
/// plain [`Service`] trait and [`CaptureInvariantService::run_pass`] without a `SiteCtx`.
pub fn build(
    site: String,
    backend: spot_postgres::PgBackend,
    exclusions: Arc<dyn ExclusionSource>,
    publisher: Arc<dyn FeedPublisher>,
    conditions: Arc<ConditionsHandle>,
    clock: Arc<dyn Clock>,
    settings: Settings,
) -> Arc<CaptureInvariantService> {
    Arc::new(CaptureInvariantService {
        inner: Arc::new(Inner {
            site,
            backend,
            exclusions,
            publisher,
            conditions,
            clock,
            settings,
            state: Mutex::new(State::default()),
        }),
    })
}

/// The subject, source and record of the condition for `finding`.
fn condition_of(finding: &Finding) -> (String, &'static str, String) {
    let source = if finding.feed_position.is_some() {
        "change_feed"
    } else {
        "doc_state"
    };
    let record = format!("{}/{}", finding.entity, finding.id);
    (format!("{source}:{record}"), source, record)
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn now(&self) -> Millis {
        let millis = self.clock.now_utc().unix_timestamp_nanos() / 1_000_000;
        Millis::new(u64::try_from(millis.max(0)).unwrap_or(0))
    }

    /// Reads the watermark and the conditions already open (a restart), once.
    async fn initialise(&self) -> std::result::Result<(), &'static str> {
        if !self.lock().loaded {
            let watermark = Watermarks::load(&self.backend.pool())
                .await
                .map_err(|e| e.code())?;
            let mut state = self.lock();
            state.watermark = watermark;
            state.loaded = true;
        }
        if !self.lock().seeded {
            // A condition raised before a restart whose finding was fixed meanwhile must still
            // clear: the open set of this module is seeded from the conditions engine.
            let open = self
                .conditions
                .open()
                .await
                .map_err(|_| "capture_invariant.conditions_unavailable")?;
            let mut state = self.lock();
            state.open = open
                .iter()
                .filter(|o| o.id.as_str() == CONDITION_ID)
                .map(|o| o.subject.as_str().to_owned())
                .collect();
            state.seeded = true;
        }
        Ok(())
    }

    async fn pass(&self) -> std::result::Result<PassSummary, &'static str> {
        self.initialise().await?;
        let exclusions = self.exclusions.exclusions().map_err(|reason| {
            tracing::warn!(site = %self.site, %reason, "capture invariant: no exclusions to judge with");
            "capture_invariant.engine_unavailable"
        })?;
        self.publisher.publish().await;
        let from = self.lock().watermark;
        let scan = spot_capture_invariant_pg::scan(&self.backend.pool(), exclusions, from, &self.settings.scan())
            .await
            .map_err(|e| e.code())?;

        let now = self.now();
        let mut current: BTreeSet<String> = BTreeSet::new();
        for finding in &scan.raise {
            let (subject, source, record) = condition_of(finding);
            let params = ConditionParams::new()
                .with("source", source)
                .with("record", record);
            match self
                .conditions
                .raise(CONDITION_ID, &subject, params, now)
                .await
            {
                Ok(Ok(_)) => {
                    current.insert(subject);
                }
                Ok(Err(error)) => {
                    tracing::warn!(site = %self.site, code = error.code(), "capture invariant: a raise was refused");
                    return Err("capture_invariant.raise_refused");
                }
                Err(_) => return Err("capture_invariant.conditions_unavailable"),
            }
        }
        let previous: Vec<String> = self.lock().open.iter().cloned().collect();
        let mut cleared = 0;
        let mut still_open = current.clone();
        for subject in previous {
            if current.contains(&subject) {
                continue;
            }
            match self.conditions.observe_cleared(CONDITION_ID, &subject, now).await {
                Ok(Ok(_)) => cleared += 1,
                // Keep it: the next pass tries again.
                _ => {
                    still_open.insert(subject);
                }
            }
        }
        if !scan.noted.is_empty() || scan.deferred > 0 {
            tracing::warn!(
                site = %self.site,
                noted = scan.noted.len(),
                deferred = scan.deferred,
                lag_entries = scan.lag_entries,
                window = self.settings.window,
                window_exceeds_lag = scan.window_exceeds_lag,
                "capture invariant: findings that raise nothing this pass"
            );
        }
        {
            let mut state = self.lock();
            state.open = still_open;
            state.watermark = scan.next;
        }
        let saved = scan.next.save(&self.backend.pool()).await;
        let summary = PassSummary {
            raised: scan.raise.len(),
            cleared,
            deferred: scan.deferred,
            noted: scan.noted.len(),
            open: self.lock().open.len(),
            feed_entries: scan.report.feed_entries,
            state_events: scan.report.state_events,
            watermark: scan.next,
        };
        saved.map_err(|e| e.code())?;
        Ok(summary)
    }
}

impl CaptureInvariantService {
    /// One pass, in its own task: a panic in it is a failed pass, never a stopped service.
    ///
    /// # Errors
    ///
    /// The stable code of why the pass failed. A failed pass changes nothing but the failure
    /// count (and, for a failed watermark write, the in-memory watermark, which is still right).
    pub async fn run_pass(&self) -> std::result::Result<PassSummary, &'static str> {
        let inner = Arc::clone(&self.inner);
        let result = tokio::spawn(async move { inner.pass().await })
            .await
            .unwrap_or(Err("capture_invariant.pass_panicked"));
        let mut state = self.inner.lock();
        state.passes += 1;
        match &result {
            Ok(_) => {
                state.failures = 0;
                state.last_error = None;
            }
            Err(code) => {
                state.failures = state.failures.saturating_add(1);
                state.last_error = Some(code);
                tracing::warn!(
                    site = %self.inner.site,
                    code,
                    failures = state.failures,
                    "capture invariant: a pass failed"
                );
            }
        }
        result
    }

    /// `capture_invariant: ok (watermark g<generation>:s<seq>, N findings open)`, or
    /// `capture_invariant: degraded (...)` after [`DEGRADED_AFTER_FAILURES`] failed passes in a
    /// row. Never names a record.
    pub fn readiness_text(&self) -> String {
        let state = self.inner.lock();
        if state.failures >= DEGRADED_AFTER_FAILURES {
            return format!(
                "capture_invariant: degraded ({} passes in a row could not read the streams: {})",
                state.failures,
                state.last_error.unwrap_or("unknown")
            );
        }
        format!(
            "capture_invariant: ok (watermark g{}:s{}, {} findings open)",
            state.watermark.generation,
            state.watermark.feed,
            state.open.len()
        )
    }

    /// Consecutive failed passes.
    pub fn failures(&self) -> u32 {
        self.inner.lock().failures
    }

    /// The watermark in memory.
    pub fn watermark(&self) -> Watermarks {
        self.inner.lock().watermark
    }
}

impl Service for CaptureInvariantService {
    fn name(&self) -> &'static str {
        "capture_invariant"
    }

    fn level(&self) -> Level {
        // It must never stop trading: a failing capture check degrades the site and says so.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // Only the table: the conditions service has not started yet, so nothing is asked of
            // it here (a call now would wait for an actor that is not running).
            Watermarks::load(&self.inner.backend.pool()).await.map(|_| ()).map_err(|e| {
                format!(
                    "the capture watermark could not be read ({}); the table is created by `spot migrate`",
                    e.code()
                )
            })
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut ticker = tokio::time::interval(self.inner.settings.interval());
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            remove_provided(&self.inner.site);
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => {
                        // A failed pass is counted and logged inside; the loop goes on.
                        let _ = self.run_pass().await;
                    }
                }
            }
        })
    }
}

/// What this module provides a site's ports on every build, kept across a reload (the running
/// service is not rebuilt, so the reloaded ports map needs the same instance).
fn provided() -> &'static Mutex<std::collections::HashMap<String, Arc<CaptureInvariantService>>> {
    static PROVIDED: OnceLock<Mutex<std::collections::HashMap<String, Arc<CaptureInvariantService>>>> =
        OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// The service a running site hosts, by site name: for a harness that drives a pass itself
/// ([`CaptureInvariantService::run_pass`]) instead of waiting for the interval. `None` before the
/// site assembled it and after it stopped.
pub fn hosted(site: &str) -> Option<Arc<CaptureInvariantService>> {
    provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .get(site)
        .cloned()
}

fn remove_provided(site: &str) {
    provided()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(site);
}

/// Registers the service: a finaliser builds it once the conditions module has provided its
/// handle. Provides [`CaptureInvariantService`] as a port (its readiness text and failure
/// count). A reload adds nothing and re-provides the running instance.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("capture_invariant")?;
    settings.validate()?;
    if !settings.enabled {
        return Ok(());
    }
    if !ctx.assembling() {
        let site = ctx.label().to_owned();
        let existing = provided()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&site)
            .cloned();
        if let Some(service) = existing {
            ctx.provide::<CaptureInvariantService>(service)?;
        }
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let live = Arc::clone(env.live);
    let backend = env.backend.clone();
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    ctx.finally(move |ctx| {
        let conditions = ctx.require_port::<ConditionsHandle>()?;
        let service = build(
            site.clone(),
            backend.clone(),
            Arc::new(EngineExclusions(live)),
            Arc::new(BackendPublisher(backend)),
            conditions,
            clock,
            settings,
        );
        ctx.add(Arc::clone(&service) as Arc<dyn Service>);
        ctx.provide::<CaptureInvariantService>(Arc::clone(&service))?;
        provided()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(site, service);
        Ok(())
    });
    Ok(())
}
