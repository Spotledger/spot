//! The conditions service: `ConditionEngine` hosted per site, durable through
//! `spot-conditions-pg`, with a 5-second tick and a cloneable async handle (WP-RTB-01.T2,
//! ADR-067 sections 1, 2 and 6).
//!
//! # Why this module exists
//!
//! `spot-conditions` is a pure in-memory engine and `spot-sync-conditions` is the catalogue of
//! the journal and sync conditions; nothing hosted either before this task. Every detector of
//! the journal and sync work (`CAPTURE_MISMATCH`, `RPO_BREACH`, `JOURNAL_GAP`, `NODE_SILENT`,
//! ...) raises through the [`ConditionsHandle`] this module builds, instead of keeping its own
//! engine, so every condition of a site shares one lifecycle, one store and one health surface.
//!
//! # Shape
//!
//! One [`ConditionsService`] task per site owns the engine, exactly as `services/jobs.rs`'s
//! worker owns its queue: a single writer reached over a channel, never a lock held across an
//! await. [`ConditionsHandle::raise`], [`ConditionsHandle::observe_cleared`],
//! [`ConditionsHandle::open`], [`ConditionsHandle::history`] and [`ConditionsHandle::health`]
//! each send one command and await its reply. The task also runs a tick every
//! `services.conditions.tick_seconds` (default 5), which advances the engine (debounce windows
//! closing, escalation) and retries writing whatever the store has not taken yet.
//!
//! # Start-up
//!
//! [`ConditionsService::check`] builds the registry from [`spot_sync_conditions::catalogue`]
//! plus whatever other modules registered on [`ConditionRegistrations`] during their own
//! `register(ctx)` (see that type's docs for the exact window), prepares the store's tables and
//! loads the open set, then restores the engine with
//! [`ConditionEngine::restore`](spot_conditions::ConditionEngine::restore). A site that cannot
//! read its own open conditions at start-up does not start this service (`Level::Optional`): the
//! site still serves and readiness says `conditions` is degraded with the reason (for example
//! "run `spot migrate`" when the tables are missing), and every call on the handle then fails with
//! an error instead of answering from an engine that never started. Optional is chosen because
//! the brief names no Required service: an outage of the conditions store must not stop the site
//! serving. The tables are created by `spot migrate` (they are in `schema::extra_tables`); this
//! module never creates one.
//!
//! # Failure behaviour (the brief's point 4)
//!
//! A transition always applies to the in-memory engine at once, whatever the store is doing: a
//! detector's `raise` never waits for PostgreSQL. The audit events each command produces are
//! queued, and only the tick (every `tick_seconds`) and the final flush at shutdown write the
//! queue, in order; once the store answers, the queue drains in the order the transitions
//! happened. A write that fails as a whole is retried one event at a time: an event that can
//! never be written (a stored value out of range, `conditions_pg.corrupt`) is set aside, counted
//! and logged by id, so it cannot block every later transition; an event that fails because the
//! store is down stops the pass and keeps its place. While the store is down the pending
//! queue may grow past [`PENDING_SOFT_CAP`] (10,000); beyond it, the oldest **info** transition
//! is dropped first (counted by [`ConditionsHandle::dropped_info`]) — a red transition is never
//! dropped. Past [`PENDING_HARD_CAP`] (100,000) the service reports failed in its readiness text
//! rather than lose anything silently.
//!
//! # Open question on the unknown-condition refusal code
//!
//! The brief names the refusal of a raise for an unregistered id or subject kind
//! `conditions.unknown_condition`. [`spot_conditions::ConditionEngine::raise`] already refuses
//! exactly that case with [`spot_conditions::ConditionsError::UnknownId`], catalogued as
//! `conditions.unknown_id` (`crates/framework/spot-errors/catalogue/conditions.toml`, not owned
//! by this task). This module forwards that existing, already-catalogued error rather than
//! introduce a second code with the same meaning in a catalogue file this task does not own; see
//! `## Open questions` in the task report.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
// Public: every item here is used by this module's own internal code below, and is also the
// host-level surface a caller that depends only on `spot-cli` needs to build and drive
// [`ConditionsService`]/[`ConditionsHandle`] directly — `harness/rust/l3-journal-sync/tests/
// conditions.rs` in particular, which owns no dependency on `spot-conditions`,
// `spot-conditions-pg` or `spot-storage-contract` of its own. Nothing here is a *new*
// dependency of `spot-cli`: every one of these crates is already a direct dependency (see
// `Cargo.toml`); this only widens what is reachable through `spot-cli`'s own public surface.
pub use spot_conditions::{
    AuditEvent, AuditSink, ClearOutcome, ConditionDef, ConditionEngine, ConditionParams,
    ConditionsError, HealthSummary, HealthThresholds, HistoryEntry, Millis, Open, RaiseOutcome,
    Registry, Severity,
};
pub use spot_conditions_pg::{ConditionsPgError, PgConditionStore};
pub use spot_core::{Clock, ManualClock};
pub use spot_storage_contract::feed::FeedPosition;
pub use spot_storage_contract::{Catalog, ChangeFeed, Collation, StorageBackend};
pub use time::OffsetDateTime;
use tokio::sync::{mpsc, oneshot, watch};

use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

/// The durable side this service writes through: [`PgConditionStore`] in production. A trait so a
/// test can inject a store outage (or a poison event) without touching the database.
pub trait ConditionStore: Send + Sync {
    /// Every stored open condition.
    fn load_open(&self) -> StoreFuture<'_, Vec<Open>>;
    /// Writes `events` in one transaction, all or none.
    fn apply<'a>(&'a self, events: &'a [AuditEvent]) -> StoreFuture<'a, ()>;
}

/// A store call's answer.
pub type StoreFuture<'a, T> = std::pin::Pin<
    Box<dyn std::future::Future<Output = std::result::Result<T, ConditionsPgError>> + Send + 'a>,
>;

impl ConditionStore for PgConditionStore {
    fn load_open(&self) -> StoreFuture<'_, Vec<Open>> {
        Box::pin(PgConditionStore::load_open(self))
    }

    fn apply<'a>(&'a self, events: &'a [AuditEvent]) -> StoreFuture<'a, ()> {
        Box::pin(PgConditionStore::apply(self, events))
    }
}

/// This module's start order (framework services use 100 to 900; new modules use 1000 and
/// above). A module that registers an extra condition definition through
/// [`ConditionRegistrations`] must use an `ORDER` **greater** than this one, so its ordinary
/// `register(ctx)` call runs after this module has provided the port (see
/// [`ConditionRegistrations`]'s docs for why this is the exact and only requirement).
pub const ORDER: u32 = 1000;

/// Transitions kept in memory while the store cannot be written before the oldest `info` one is
/// dropped. High enough that an ordinary short outage never loses anything.
pub const PENDING_SOFT_CAP: usize = 10_000;
/// The hard ceiling: past this the service reports failed rather than keep growing unbounded.
pub const PENDING_HARD_CAP: usize = 100_000;

/// Where a module that is not this one adds its own condition definitions to the registry this
/// site's engine is built from.
///
/// # The window
///
/// A module registers a definition by calling [`ConditionRegistrations::register`] during its
/// own, ordinary `register(ctx)` call (not a finaliser): this module provides the port
/// immediately when **its own** `register(ctx)` runs, and because every module's `register(ctx)`
/// executes in `ORDER` sequence, any module whose `ORDER` is greater than
/// [`ORDER`] (this module's) sees the port already provided. This module then reads every
/// registration back in **its own finaliser** ([`register`]), which runs only after the whole
/// site has finished its ordinary registration pass, so it sees contributions from every
/// qualifying module regardless of the exact order among them.
///
/// Registering twice under the same id, or an id the catalogue already holds, fails the site's
/// start-up with [`ConditionsError::DuplicateId`] (surfaced through the module's finaliser as a
/// plain start-up error: two definitions of the same id would silently rewrite the meaning of a
/// condition that may already be open).
#[derive(Default)]
pub struct ConditionRegistrations(Mutex<Vec<ConditionDef>>);

impl ConditionRegistrations {
    /// Adds `def` to the set this site's engine is built from.
    pub fn register(&self, def: ConditionDef) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(def);
    }

    fn take(&self) -> Vec<ConditionDef> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

/// `[services.conditions]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// How often the engine ticks: closes debounce windows, escalates, retries the pending
    /// write queue.
    pub tick_seconds: u64,
    /// How long an `amber` condition must have been open before it joins the `needs you` list
    /// (ADR-067 section 6). A `red` condition is on it at once regardless of this setting.
    pub amber_needs_person_after_seconds: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            tick_seconds: 5,
            amber_needs_person_after_seconds: 300,
        }
    }
}

impl Settings {
    fn tick(&self) -> Duration {
        Duration::from_secs(self.tick_seconds.max(1))
    }

    fn thresholds(&self) -> HealthThresholds {
        HealthThresholds::new(Millis::new(self.amber_needs_person_after_seconds.saturating_mul(1_000)))
    }
}

/// A sink that only moves events into a shared buffer, drained by the actor after the call that
/// produced them: the engine stays pure and reads no clock, this only collects what it already
/// decided to record. Shared (not owned) so the actor can drain it without a mutable accessor on
/// the engine itself.
#[derive(Clone, Default)]
struct CollectSink(Arc<Mutex<Vec<AuditEvent>>>);

impl CollectSink {
    fn drain(&self) -> Vec<AuditEvent> {
        std::mem::take(&mut *self.0.lock().unwrap_or_else(|p| p.into_inner()))
    }
}

impl AuditSink for CollectSink {
    fn record(&mut self, event: AuditEvent) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).push(event);
    }
}

/// The transitions the store has not written yet, and the counters the readiness text and
/// [`ConditionsHandle::dropped_info`] report.
#[derive(Default)]
struct Pending {
    queue: VecDeque<AuditEvent>,
    dropped_info: u64,
    /// Events set aside because the store can never take them (see the module docs).
    rejected: u64,
    failed: bool,
}

impl Pending {
    /// Appends `events`, then enforces [`PENDING_SOFT_CAP`] (drop the oldest `info` transition)
    /// and [`PENDING_HARD_CAP`] (mark failed, never drop a transition to make room).
    fn push(&mut self, events: Vec<AuditEvent>) {
        self.queue.extend(events);
        while self.queue.len() > PENDING_SOFT_CAP {
            let Some(index) = self
                .queue
                .iter()
                .position(|event| event.severity() == Severity::Info)
            else {
                break;
            };
            self.queue.remove(index);
            self.dropped_info += 1;
        }
        self.failed = self.queue.len() > PENDING_HARD_CAP;
    }

    fn is_degraded(&self) -> bool {
        !self.queue.is_empty() || self.rejected > 0
    }
}

/// One command the actor task processes, each with its own reply channel.
enum Command {
    Raise {
        id: String,
        subject: String,
        params: ConditionParams,
        now: Millis,
        reply: oneshot::Sender<std::result::Result<RaiseOutcome, ConditionsError>>,
    },
    ObserveCleared {
        id: String,
        subject: String,
        now: Millis,
        reply: oneshot::Sender<std::result::Result<ClearOutcome, ConditionsError>>,
    },
    Open {
        reply: oneshot::Sender<Vec<Open>>,
    },
    History {
        subject: String,
        reply: oneshot::Sender<Vec<HistoryEntry>>,
    },
    Health {
        thresholds: HealthThresholds,
        reply: oneshot::Sender<HealthSummary>,
    },
    Readiness {
        reply: oneshot::Sender<String>,
    },
}

fn channel_closed() -> Error {
    Error("the conditions service is not running".to_owned())
}

/// The cloneable async API other services hold: a handle to the one engine task of this site.
///
/// Cloning is cheap (one channel sender); every call sends one [`Command`] and awaits its reply,
/// so no two callers can observe the engine mid-transition and no lock is ever held across an
/// await.
#[derive(Clone)]
pub struct ConditionsHandle {
    sender: mpsc::Sender<Command>,
}

impl ConditionsHandle {
    /// Raises `id` on `subject` with `params`, at `now`.
    ///
    /// Always applies to the in-memory engine first: a store outage never blocks this call (see
    /// the module's "Failure behaviour" docs). Refused with
    /// [`ConditionsError::UnknownId`] for an id this site's registry does not hold.
    ///
    /// # Errors
    ///
    /// The engine's own [`ConditionsError`], or [`Error`] if the service has stopped.
    pub async fn raise(
        &self,
        id: &str,
        subject: &str,
        params: ConditionParams,
        now: Millis,
    ) -> Result<std::result::Result<RaiseOutcome, ConditionsError>> {
        let (reply, rx) = oneshot::channel();
        self.sender
            .send(Command::Raise {
                id: id.to_owned(),
                subject: subject.to_owned(),
                params,
                now,
                reply,
            })
            .await
            .map_err(|_| channel_closed())?;
        rx.await.map_err(|_| channel_closed())
    }

    /// Records that `id` on `subject` is no longer abnormal, at `now`.
    ///
    /// # Errors
    ///
    /// The engine's own [`ConditionsError`], or [`Error`] if the service has stopped.
    pub async fn observe_cleared(
        &self,
        id: &str,
        subject: &str,
        now: Millis,
    ) -> Result<std::result::Result<ClearOutcome, ConditionsError>> {
        let (reply, rx) = oneshot::channel();
        self.sender
            .send(Command::ObserveCleared {
                id: id.to_owned(),
                subject: subject.to_owned(),
                now,
                reply,
            })
            .await
            .map_err(|_| channel_closed())?;
        rx.await.map_err(|_| channel_closed())
    }

    /// Every condition open now.
    ///
    /// # Errors
    ///
    /// [`Error`] if the service has stopped.
    pub async fn open(&self) -> Result<Vec<Open>> {
        let (reply, rx) = oneshot::channel();
        self.sender
            .send(Command::Open { reply })
            .await
            .map_err(|_| channel_closed())?;
        rx.await.map_err(|_| channel_closed())
    }

    /// The transitions recorded for `subject`, oldest first.
    ///
    /// # Errors
    ///
    /// [`Error`] if the service has stopped.
    pub async fn history(&self, subject: &str) -> Result<Vec<HistoryEntry>> {
        let (reply, rx) = oneshot::channel();
        self.sender
            .send(Command::History {
                subject: subject.to_owned(),
                reply,
            })
            .await
            .map_err(|_| channel_closed())?;
        rx.await.map_err(|_| channel_closed())
    }

    /// The calm-by-default health summary at the engine's current instant, against `thresholds`.
    ///
    /// # Errors
    ///
    /// [`Error`] if the service has stopped.
    pub async fn health(&self, thresholds: HealthThresholds) -> Result<HealthSummary> {
        let (reply, rx) = oneshot::channel();
        self.sender
            .send(Command::Health { thresholds, reply })
            .await
            .map_err(|_| channel_closed())?;
        rx.await.map_err(|_| channel_closed())
    }

    /// `conditions: calm`, `conditions: N open (R red)`, optionally prefixed `degraded:` while
    /// the store has not taken every transition yet, or `failed:` past
    /// [`PENDING_HARD_CAP`]. Never names a condition, a subject or a parameter.
    ///
    /// # Errors
    ///
    /// [`Error`] if the service has stopped.
    pub async fn readiness_text(&self) -> Result<String> {
        let (reply, rx) = oneshot::channel();
        self.sender
            .send(Command::Readiness { reply })
            .await
            .map_err(|_| channel_closed())?;
        rx.await.map_err(|_| channel_closed())
    }
}

/// The read port for the Admin Center and the management operations (WP-RTB-04): the
/// `HealthSummary` shape `spot-sync-health` consumes, over this handle.
///
/// This service performs no authorisation of its own: the caller's own permission decides
/// whether it may see a site's health, and this port exposes nothing on HTTP. `tenant` is
/// compared against the site this handle serves (one database is one site, ADR-34); a mismatch is
/// an error, never any other site's data and never a calm default: a process only ever holds the
/// engine of the sites it serves.
pub trait ConditionsView: Send + Sync {
    /// The health summary of `tenant`'s conditions.
    ///
    /// # Errors
    ///
    /// [`Error`] when `tenant` is not the one this handle serves, or the conditions service is
    /// not running: the caller shows "unknown", never a calm default for a service that is down.
    fn health<'a>(
        &'a self,
        tenant: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HealthSummary>> + Send + 'a>>;
}

/// [`ConditionsView`] over a handle and the settings its thresholds come from.
struct SiteView {
    site: String,
    handle: ConditionsHandle,
    thresholds: HealthThresholds,
}

impl ConditionsView for SiteView {
    fn health<'a>(
        &'a self,
        tenant: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HealthSummary>> + Send + 'a>>
    {
        Box::pin(async move {
            if tenant != self.site {
                return Err(Error(
                    "services.conditions: this site does not serve that tenant".to_owned(),
                ));
            }
            self.handle.health(self.thresholds).await
        })
    }
}

/// Readiness text for a transition queue the engine has not written yet.
fn readiness_text(summary: &HealthSummary, pending: &Pending) -> String {
    let calm = if summary.is_calm() {
        "conditions: calm".to_owned()
    } else {
        format!(
            "conditions: {} open ({} red)",
            summary.counts.total(),
            summary.counts.red
        )
    };
    if pending.failed {
        format!(
            "failed: {} transitions could not be written (dropped {} info); {calm}",
            pending.queue.len(),
            pending.dropped_info
        )
    } else if pending.is_degraded() {
        format!(
            "degraded: {} transitions waiting for the store (dropped {} info, rejected {}); {calm}",
            pending.queue.len(),
            pending.dropped_info,
            pending.rejected
        )
    } else {
        calm
    }
}

/// The running state the actor task owns: nothing here is ever reached from two tasks at once.
struct Actor {
    site: String,
    engine: ConditionEngine<CollectSink>,
    sink: CollectSink,
    store: Arc<dyn ConditionStore>,
    pending: Pending,
    clock: Arc<dyn Clock>,
    thresholds: HealthThresholds,
}

impl Actor {
    fn now(&self) -> Millis {
        // The engine's instant is milliseconds since the Unix epoch on the site's own clock
        // (ADR-068 rule 4: the library reads no clock; the host is where one is read).
        let micros = self.clock.now_utc().unix_timestamp_nanos() / 1_000_000;
        Millis::new(u64::try_from(micros.max(0)).unwrap_or(0))
    }

    fn drain(&mut self) {
        let events = self.sink.drain();
        if !events.is_empty() {
            self.pending.push(events);
        }
    }

    /// Writes the pending queue in order. Tries the whole queue in one transaction; if that
    /// fails, one event at a time so a poison event is set aside and a store outage stops the
    /// pass with the failed event still at the head of the queue.
    async fn flush(&mut self) {
        if self.pending.queue.is_empty() {
            return;
        }
        let batch: Vec<AuditEvent> = self.pending.queue.iter().cloned().collect();
        let Err(error) = self.store.apply(&batch).await else {
            self.pending.queue.clear();
            self.pending.failed = false;
            return;
        };
        tracing::warn!(
            site = %self.site,
            code = error.code(),
            pending = self.pending.queue.len(),
            "the condition store did not take the pending transitions; trying them one at a time"
        );
        while let Some(event) = self.pending.queue.front().cloned() {
            match self.store.apply(std::slice::from_ref(&event)).await {
                Ok(()) => {
                    self.pending.queue.pop_front();
                }
                Err(ConditionsPgError::Corrupt) => {
                    tracing::error!(
                        site = %self.site,
                        id = %event.id().as_str(),
                        "a condition transition can never be written and was set aside"
                    );
                    self.pending.queue.pop_front();
                    self.pending.rejected += 1;
                }
                Err(error) => {
                    tracing::warn!(
                        site = %self.site,
                        code = error.code(),
                        pending = self.pending.queue.len(),
                        "the condition store is not taking transitions; retrying next tick"
                    );
                    break;
                }
            }
        }
        self.pending.failed = self.pending.queue.len() > PENDING_HARD_CAP;
    }

    async fn handle(&mut self, command: Command) {
        match command {
            Command::Raise {
                id,
                subject,
                params,
                now,
                reply,
            } => {
                let outcome = self.engine.raise(&id, &subject, params, now);
                if let Err(error) = &outcome {
                    tracing::warn!(site = %self.site, id = %id, code = error.code(), "a raise was refused");
                }
                self.drain();
                let _ = reply.send(outcome);
            }
            Command::ObserveCleared {
                id,
                subject,
                now,
                reply,
            } => {
                let outcome = self.engine.observe_cleared(&id, &subject, now);
                self.drain();
                let _ = reply.send(outcome);
            }
            Command::Open { reply } => {
                let _ = reply.send(self.engine.open());
            }
            Command::History { subject, reply } => {
                let _ = reply.send(self.engine.history(&subject).to_vec());
            }
            Command::Health { thresholds, reply } => {
                let summary = self.engine.health(self.now(), &thresholds);
                let _ = reply.send(summary);
            }
            Command::Readiness { reply } => {
                let summary = self.engine.health(self.now(), &self.thresholds);
                let _ = reply.send(readiness_text(&summary, &self.pending));
            }
        }
    }

    /// One last write of whatever the queue holds when the service stops, so a transition
    /// raised just before shutdown is not left to the next start.
    async fn final_flush(&mut self) {
        self.drain();
        self.flush().await;
    }

    async fn tick(&mut self) {
        self.engine.tick(self.now());
        self.drain();
        self.flush().await;
    }
}

/// What [`ConditionsService::check`] builds for `run` to take and drive.
struct Started {
    registry: Registry,
    open: Vec<Open>,
}

/// The conditions service of one site.
pub struct ConditionsService {
    site: String,
    store: Arc<dyn ConditionStore>,
    clock: Arc<dyn Clock>,
    debounce: Millis,
    settings: Settings,
    registrations: Arc<ConditionRegistrations>,
    receiver: Mutex<Option<mpsc::Receiver<Command>>>,
    started: Mutex<Option<Started>>,
}

impl ConditionsService {
    fn registry(&self) -> Result<Registry> {
        let mut registry = spot_sync_conditions::catalogue()
            .map_err(|e| Error(format!("services.conditions: the catalogue is invalid: {e}")))?;
        for def in self.registrations.take() {
            registry.register(def).map_err(|e| {
                Error(format!(
                    "services.conditions: a registered condition conflicts with the catalogue: {}",
                    e.code()
                ))
            })?;
        }
        Ok(registry)
    }
}

impl Service for ConditionsService {
    fn name(&self) -> &'static str {
        "conditions"
    }

    fn level(&self) -> Level {
        // The brief names no Required service: a conditions-store outage degrades the site and
        // says so, it does not stop it serving (see the module docs).
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let result = self.check_inner().await;
            if result.is_err() {
                // The service will not run: close the command channel so every call on the
                // handle fails at once instead of queueing for a task that never starts.
                drop(self.receiver.lock().unwrap_or_else(|p| p.into_inner()).take());
            }
            result
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let result = self.run_inner(stop).await;
            provided()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&self.site);
            result
        })
    }
}

impl ConditionsService {
    async fn check_inner(&self) -> std::result::Result<(), String> {
        {
            let open = self.store.load_open().await.map_err(|e| {
                format!(
                    "the open conditions could not be read ({}); the tables are created by `spot migrate`",
                    e.code()
                )
            })?;
            let registry = self.registry().map_err(|e| e.0)?;
            // Proves the registry and the stored open set agree before the site starts serving,
            // the same check `run` relies on, but surfaced as a clear start-up refusal.
            ConditionEngine::restore(registry.clone(), CollectSink::default(), self.debounce, open.clone())
                .map_err(|e| {
                    format!(
                        "the stored open conditions do not match this build's registry: {}",
                        e.code()
                    )
                })?;
            *self.started.lock().unwrap_or_else(|p| p.into_inner()) = Some(Started { registry, open });
            Ok(())
        }
    }

    async fn run_inner(&self, stop: watch::Receiver<bool>) -> std::result::Result<(), String> {
        {
            let Some(started) = self.started.lock().unwrap_or_else(|p| p.into_inner()).take() else {
                return Err("the conditions service started without passing its check".to_owned());
            };
            let Some(mut receiver) = self.receiver.lock().unwrap_or_else(|p| p.into_inner()).take()
            else {
                return Err("the conditions service's command channel was already taken".to_owned());
            };
            let engine = ConditionEngine::restore(
                started.registry,
                CollectSink::default(),
                self.debounce,
                started.open,
            )
            .map_err(|e| e.to_string())?;
            let sink = engine.sink().clone();
            let mut actor = Actor {
                site: self.site.clone(),
                engine,
                sink,
                store: Arc::clone(&self.store),
                pending: Pending::default(),
                clock: Arc::clone(&self.clock),
                thresholds: self.settings.thresholds(),
            };
            let mut ticker = tokio::time::interval(self.settings.tick());
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut stop = stop;
            loop {
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            actor.final_flush().await;
                            return Ok(());
                        }
                    }
                    _ = ticker.tick() => {
                        actor.tick().await;
                    }
                    command = receiver.recv() => {
                        match command {
                            Some(command) => actor.handle(command).await,
                            None => {
                                actor.final_flush().await;
                                return Ok(());
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Builds the service and its handle from `settings`, the site's identity, clock and
/// `PgConditionStore`. The engine itself is not built here (see [`ConditionsService::check`]):
/// this only wires the channel and the registrations collector other modules write to.
///
/// Exposed so a direct host (a test, or a future caller that does not go through
/// [`register`]) can build and drive the service through the plain [`Service`] trait without
/// a `SiteCtx`.
pub fn build(
    site: String,
    store: Arc<dyn ConditionStore>,
    clock: Arc<dyn Clock>,
    settings: Settings,
    registrations: Arc<ConditionRegistrations>,
) -> (Arc<ConditionsService>, ConditionsHandle) {
    let (sender, receiver) = mpsc::channel(256);
    let service = Arc::new(ConditionsService {
        site,
        store,
        clock,
        debounce: spot_conditions::DEFAULT_DEBOUNCE_MILLIS,
        settings,
        registrations,
        receiver: Mutex::new(Some(receiver)),
        started: Mutex::new(None),
    });
    (service, ConditionsHandle { sender })
}

/// What this module provides a site's ports on every build, kept alive across a reload.
///
/// `SiteCtx::provide` only reaches the ports map the *current* build produces: on a reload
/// this module adds nothing new (the service and its one task keep running, as
/// [`ConditionsService::run`]'s `select!` loop never ends on its own), but the reloaded site's
/// new ports map still needs a live reference to the same handle, or the last clone of the
/// handle this process holds disappears with the old ports map — closing the command channel
/// out from under the still-running task, which then reads `None` from it and stops. A process
/// table keyed by site label, the same pattern `services/report.rs` uses, is how this module
/// re-provides the same handle and view on every reload instead of a fresh one.
#[derive(Clone)]
struct Provided {
    handle: Arc<ConditionsHandle>,
    view: Arc<dyn ConditionsView>,
}

fn provided() -> &'static Mutex<HashMap<String, Provided>> {
    static PROVIDED: OnceLock<Mutex<HashMap<String, Provided>>> = OnceLock::new();
    PROVIDED.get_or_init(Mutex::default)
}

/// Registers the conditions service: provides [`ConditionRegistrations`] at once, so any module
/// with a greater `ORDER` can add to it during its own `register(ctx)`, then defers building the
/// service to a finaliser so this module sees every contribution before the registry is sealed
/// (see [`ConditionRegistrations`]'s docs). A reload re-provides the handle and view this
/// process already built for the site (see [`Provided`]) and adds nothing new.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("conditions")?;
    if !ctx.assembling() {
        // Only this build's own module (with a greater `ORDER`) can still use this: the
        // finaliser that would read it already ran on the assembling build.
        ctx.provide::<ConditionRegistrations>(Arc::new(ConditionRegistrations::default()))?;
        let site = ctx.label().to_owned();
        let Some(existing) = provided()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&site)
            .cloned()
        else {
            // This process never assembled the service for this site (a test context with no
            // `SiteEnv`, for example): nothing to re-provide.
            return Ok(());
        };
        ctx.provide::<ConditionsHandle>(existing.handle)?;
        ctx.provide::<dyn ConditionsView>(existing.view)?;
        return Ok(());
    }
    let registrations = Arc::new(ConditionRegistrations::default());
    ctx.provide::<ConditionRegistrations>(Arc::clone(&registrations))?;
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let backend = env.backend.clone();
    let catalog = Arc::clone(&env.permission.catalog);
    let clock: Arc<dyn Clock> = Arc::clone(ctx.clock());
    ctx.finally(move |ctx| {
        let thresholds = settings.thresholds();
        let store = Arc::new(PgConditionStore::new(backend, catalog));
        let (service, handle) = build(site.clone(), store, clock, settings, registrations);
        ctx.add(service);
        let handle_port = Arc::new(handle.clone());
        ctx.provide::<ConditionsHandle>(Arc::clone(&handle_port))?;
        let view: Arc<dyn ConditionsView> = Arc::new(SiteView {
            site: site.clone(),
            handle,
            thresholds,
        });
        ctx.provide::<dyn ConditionsView>(Arc::clone(&view))?;
        provided().lock().unwrap_or_else(|p| p.into_inner()).insert(
            site,
            Provided {
                handle: handle_port,
                view,
            },
        );
        Ok(())
    });
    Ok(())
}

#[cfg(test)]
#[path = "conditions/tests.rs"]
mod tests;
