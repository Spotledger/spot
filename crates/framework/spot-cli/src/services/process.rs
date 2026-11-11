//! The process engine (`spot-bpm-engine`) hosted for one site: durable Business Process Cases
//! over PostgreSQL, with case-step jobs, durable resume on a schedule, and timers routed to the
//! scheduler through a fenced port.
//!
//! # Where the pieces come from
//!
//! - **The engine** (`spot-bpm-engine`) is pure orchestration over traits; this module gives it
//!   its PostgreSQL [`CaseStore`](spot_bpm_engine::CaseStore) ([`PgCaseStore`]) and its
//!   [`TimerPort`](spot_bpm_engine::TimerPort) (a [`FencedTimerPort`] over an in-process index,
//!   guarded by a scheduler lease).
//! - **Process definitions** are durable in `spot-bpm-definitions`/`spot-bpm-defs-pg`
//!   ([`PgDefinitionStore`]): every published version is loaded into the engine's
//!   `ProcessRegistry` at start, and [`ProcessHost::reload_definitions`] rebuilds it after a
//!   publish (WP-BPM-09.T4 calls that).
//! - **Node handlers** for kinds this crate does not ship (Decision, Document, Integration,
//!   forms, messages) are registered by their own hosting modules, through the
//!   [`HandlerBuilder`] port this module provides *before* it finalises the engine: a module
//!   with a smaller `ORDER` than its own (1100 to 1230, see [`ORDER`]) calls
//!   [`HandlerBuilder::register`] during its own `register()`, and this module's finaliser
//!   drains every registration into the one `HandlerRegistry` the engine runs with. A process
//!   that uses a kind nothing registered is refused at load (named in the readiness detail),
//!   never failed mid-Run.
//! - **Case-step jobs** run through the site's own job worker (a lower `ORDER` than this
//!   module): [`register`] adds a handler for [`spot_bpm_engine::CASE_JOB_NAME`] that calls
//!   [`Engine::step`](spot_bpm_engine::Engine::step) and re-enqueues once more when the turn
//!   made progress but the Case is not finished (the engine only runs the nodes ready *at the
//!   start* of one `step`, so a chain of several nodes needs several steps). The idempotency key
//!   is the engine's own, so a double enqueue — ours racing the resume scan below — is dropped
//!   by the job store, never run twice.
//! - **Durable resume**: at start and then every `resume_interval_secs` (default 15, bounded 5
//!   to 300), under a held lease, [`ProcessHost`] lists every Case the store reports runnable
//!   (`list_runnable`: a ready node, a due timer or a waiting child) and enqueues a case-step
//!   job for each. A Case killed mid-step is found this way: the engine's own contract re-runs
//!   the node left `started` with `is_retry == true`.
//!
//! # Configuration
//!
//! `[services.process]`: `enabled` (default `true`) and `resume_interval_secs` (default `15`,
//! clamped `5..=300`). `enabled = false` makes the whole module inert: `register` adds nothing,
//! creates no table and provides no port.
//!
//! # Health and metrics
//!
//! The readiness check loads every published process exactly once (the first poll that finds
//! the engine built); a version that no longer compiles is a logged finding by name and
//! number, never a reason to stop loading the rest, and never the version's text. Every later
//! poll re-checks handler coverage against the registry already loaded — a read, never a
//! reload — so `check()` never re-mutates the live registry on a schedule;
//! [`ProcessHost::reload_definitions`] is the only other reload path, run after a publish.
//! Counters (Runs started, completed, failed, steps run, the resume backlog) carry no label at
//! all, so no run id, process name or error text can ever reach one. Logs carry ids; metric
//! labels never do.
//!
//! **Pinned, retired versions:** [`ProcessHost::reload_definitions`] and the readiness check
//! load every published version plus each retired version the store's `running_pins` still lists
//! (`spot_bpm_definitions::load_into`), so a Case pinned to a later-retired version resumes on
//! its pin after a restart. `ProcessRegistry::latest` returns published versions only, so a new
//! start never lands on a retired version (WP-BPM-09.T5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_bpm_definitions::{LoadReport, load_into};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{
    CASE_JOB_NAME, CaseId, CaseJob, CaseStore, CaseUpdate, Engine, EngineError, HandlerRegistry,
    NodeHandler,
};
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_core::{Clock, SiteId};
use spot_dag_exec::RunStatus;
use spot_jobs::worker::{HandlerResult, JobContext};
use spot_scheduler::{InMemoryLeaseStore, Lease, LeaseStore};
use time::OffsetDateTime;
use tokio::sync::{OnceCell, watch};

use crate::services::ports::{JobSpec, JobSubmit, OutboxLease};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result, error};

/// Start order: after the framework's own services and after every module 1000 to 1099. Node
/// handler modules (Decision, Document, Integration, forms, messages) take 1110 to 1230, so
/// their smaller-is-earlier `ORDER` than theirs lets them find this module's [`HandlerBuilder`]
/// port already provided.
pub const ORDER: u32 = 1100;

/// The queue every case-step job and resume enqueue uses: every site's worker claims `default`
/// (`services.jobs.queues`), so the process engine needs no extra operator configuration.
const QUEUE: &str = "default";

/// The consumer name the resume loop's lease is held under (`outbox:<site>:process`, through
/// the jobs module's [`OutboxLease`] port).
const RESUME_CONSUMER: &str = "process";

/// The in-process lease name the timer port is fenced by. Scoped to this process only: see the
/// module-level note on [`build_timer_port`].
const TIMER_LEASE: &str = "process_timer";

/// How long the in-process timer lease lives. It is renewed by nothing and expires in no
/// deployment that restarts before this; a lease that did expire would simply make
/// [`FencedTimerPort`] stop firing anything, caught at once by the next resume turn.
const TIMER_LEASE_TTL: Duration = Duration::from_secs(10 * 365 * 24 * 3600);

/// `[services.process]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all. `false`: no table, no port, no job handler.
    pub enabled: bool,
    /// How often the resume scan runs, in seconds. Bounded 5 to 300.
    pub resume_interval_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            resume_interval_secs: 15,
        }
    }
}

impl Settings {
    fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(5..=300).contains(&self.resume_interval_secs) {
            return Err("resume_interval_secs");
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Metrics: counters only, never a label (ids, names and error text stay in logs, not here).
// ---------------------------------------------------------------------------------------------

const RUNS_STARTED_TOTAL: &str = "spot_bpm_runs_started_total";
const RUNS_COMPLETED_TOTAL: &str = "spot_bpm_runs_completed_total";
const RUNS_FAILED_TOTAL: &str = "spot_bpm_runs_failed_total";
const STEPS_RUN_TOTAL: &str = "spot_bpm_steps_run_total";
const RESUME_BACKLOG: &str = "spot_bpm_resume_backlog";

/// The engine's counters, label-free. The atomics are the test and health surface; the
/// `metrics` calls are the operator one.
#[derive(Debug, Default)]
struct Metrics {
    runs_started: AtomicU64,
    runs_completed: AtomicU64,
    runs_failed: AtomicU64,
    steps_run: AtomicU64,
    resume_backlog: AtomicU64,
}

impl Metrics {
    fn run_started(&self) {
        self.runs_started.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(RUNS_STARTED_TOTAL).increment(1);
    }
    fn run_completed(&self) {
        self.runs_completed.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(RUNS_COMPLETED_TOTAL).increment(1);
    }
    fn run_failed(&self) {
        self.runs_failed.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(RUNS_FAILED_TOTAL).increment(1);
    }
    fn step_ran(&self) {
        self.steps_run.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(STEPS_RUN_TOTAL).increment(1);
    }
    fn backlog(&self, size: usize) {
        self.resume_backlog.store(size as u64, Ordering::Relaxed);
        metrics::gauge!(RESUME_BACKLOG).set(size as f64);
    }
}

/// A snapshot of the counters, for tests and the readiness detail.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Runs started through [`ProcessHost::start_run`].
    pub runs_started: u64,
    /// Runs that reached a completed status.
    pub runs_completed: u64,
    /// Runs that reached a failed status.
    pub runs_failed: u64,
    /// `Engine::step` calls run by the case-step job handler.
    pub steps_run: u64,
    /// The most recent resume scan's backlog size.
    pub resume_backlog: u64,
}

// ---------------------------------------------------------------------------------------------
// The engine type this host builds, and the fenced timer port over it.
// ---------------------------------------------------------------------------------------------

/// The concrete engine every site hosts: a durable PostgreSQL Case store and a timer port
/// fenced by a scheduler lease.
pub type ProcessEngine = Engine<PgCaseStore, FencedTimerPort>;

/// `at` as the scheduler's timestamp type.
fn to_jiff(at: OffsetDateTime) -> Result<jiff::Timestamp> {
    let nanos = i32::try_from(at.nanosecond()).unwrap_or(0);
    jiff::Timestamp::new(at.unix_timestamp(), nanos)
        .map_err(|e| Error(format!("the instant does not convert to a lease timestamp: {e}")))
}

/// Builds the timer port over a fresh lease held on `leases`, named `name`.
///
/// # Note on real fencing
///
/// [`FencedTimerPort`] protects against a *stale* holder of the given lease acting after a
/// *new* holder took over — the mechanics the tests below prove. What it cannot do by itself is
/// make that protection span two real operating-system processes: `spot_scheduler::LeaseStore`
/// has only an in-memory reference implementation today (`InMemoryLeaseStore`); a
/// PostgreSQL-backed one does not exist yet in this workspace (tracked at `WP-PLT-18`, not a
/// dependency of this task). Each process therefore builds its own lease store and is always
/// its own first holder, so the fencing here is real *within* one process (it protects a lease
/// a reload or a restart cycles through) but does not yet fence two live nodes of a cluster
/// apart from each other. See `## Open questions` in this task's report.
fn build_timer_port(
    leases: Arc<dyn LeaseStore>,
    name: &str,
    holder: &str,
    now: OffsetDateTime,
) -> Result<(Lease, FencedTimerPort)> {
    let at = to_jiff(now)?;
    let lease = leases
        .acquire(name, holder, TIMER_LEASE_TTL, at)
        .map_err(|e| Error(format!("the timer lease could not be acquired: {e}")))?;
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let port = FencedTimerPort::new(Arc::clone(&leases), lease.clone(), inner);
    Ok((lease, port))
}

// ---------------------------------------------------------------------------------------------
// HandlerBuilder: the pre-engine port other node-kind modules register handlers through.
// ---------------------------------------------------------------------------------------------

/// Collects node handlers from every module with a larger `ORDER` than this one's, before the
/// engine is built. Provided as a port; a module calls [`HandlerBuilder::register`] from its own
/// `register()`, synchronously, before any finaliser runs (so before the engine exists at all).
pub struct HandlerBuilder(Mutex<Vec<Arc<dyn NodeHandler>>>);

impl Default for HandlerBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HandlerBuilder {
    /// An empty builder, seeded with nothing (callers see this crate's own handlers too, once
    /// [`HandlerBuilder::build_registry`] runs, through [`HandlerRegistry::logic_default`]).
    #[must_use]
    pub fn new() -> Self {
        Self(Mutex::new(Vec::new()))
    }

    /// Registers one handler. A later registration for the same
    /// [`NodeKind`](spot_bpm_engine::NodeKind) replaces an earlier one, exactly as
    /// [`HandlerRegistry::register_arc`] does.
    pub fn register(&self, handler: Arc<dyn NodeHandler>) {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(handler);
    }

    /// Drains every registration into a fresh registry seeded with this crate's own handlers
    /// (Logic nodes, Trigger, Wait).
    fn build_registry(&self) -> HandlerRegistry {
        let mut registry = HandlerRegistry::logic_default();
        let pending = std::mem::take(
            &mut *self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for handler in pending {
            registry.register_arc(handler);
        }
        registry
    }
}

// ---------------------------------------------------------------------------------------------
// ProcessHost: the port other modules use once the engine is built.
// ---------------------------------------------------------------------------------------------

/// Everything the engine needs reproduced on a clean Case id: the FNV-1a hash the engine itself
/// uses for its own derived ids (`spot-bpm-engine`'s sub-process child ids), so a business key
/// maps to one Case id deterministically with no randomness and no clock.
fn case_id_of(business_key: &str) -> Result<CaseId> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in business_key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    CaseId::new(&format!("case_{hash:016x}"))
        .map_err(|e| Error(format!("the derived case id is invalid: {e}")))
}

/// The node kinds of `definition` that no handler in `handlers` covers, as tokens (never the
/// definition's text or config). Excludes [`spot_bpm_engine::NodeKind::SubProcess`], which the
/// engine advances itself without consulting the handler registry.
fn missing_handler_kinds(
    definition: &spot_bpm_engine::ProcessDefinition,
    handlers: &HandlerRegistry,
) -> Vec<String> {
    let mut missing: Vec<String> = definition
        .version
        .nodes
        .iter()
        .map(|node| spot_bpm_engine::NodeKind::from_model(&node.kind))
        .filter(|kind| *kind != spot_bpm_engine::NodeKind::SubProcess)
        .filter(|kind| !handlers.contains(kind))
        .map(|kind| kind.token().to_owned())
        .collect();
    missing.sort();
    missing.dedup();
    missing
}

/// Classifies an [`EngineError`] from one `step` into what the job worker should do: a
/// transient store problem is retried by the job engine, a structural one (a missing process,
/// a missing handler) is not — the next resume turn or an operator publish fixes it, not a
/// retry loop. Carries the stable code only, never the error's full text.
fn classify(error: &EngineError) -> HandlerResult {
    match error {
        EngineError::StoreFailed { .. } | EngineError::VersionConflict { .. } => {
            HandlerResult::Retry(error.code().to_owned())
        }
        _ => HandlerResult::Fail(error.code().to_owned()),
    }
}

/// The process engine of one site, once built: `start_run`, `reload_definitions` and the engine
/// handle the case-step job handler and the resume loop both use.
pub struct ProcessHost {
    site: SiteId,
    engine: OnceLock<Arc<ProcessEngine>>,
    definitions: Arc<PgDefinitionStore>,
    submit: Arc<dyn JobSubmit>,
    clock: Arc<dyn Clock>,
    metrics: Metrics,
}

impl ProcessHost {
    fn new(
        site: SiteId,
        definitions: Arc<PgDefinitionStore>,
        submit: Arc<dyn JobSubmit>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            site,
            engine: OnceLock::new(),
            definitions,
            submit,
            clock,
            metrics: Metrics::default(),
        }
    }

    /// Builds a host whose engine is already finished: `register`'s finaliser uses this once
    /// every node handler module has registered, and tests use it directly (no `SiteCtx`
    /// needed).
    #[must_use]
    pub fn build(
        site: SiteId,
        store: PgCaseStore,
        timers: FencedTimerPort,
        handlers: HandlerRegistry,
        definitions: Arc<PgDefinitionStore>,
        submit: Arc<dyn JobSubmit>,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        let host = Arc::new(Self::new(site, definitions, submit, clock));
        host.set_engine(Arc::new(Engine::new(store, timers, handlers)));
        host
    }

    /// Set once, from the finaliser, after every module's handlers are collected.
    fn set_engine(&self, engine: Arc<ProcessEngine>) {
        let _ = self.engine.set(engine);
    }

    /// The built engine, once `register`'s finaliser (or [`ProcessHost::build`]) ran.
    ///
    /// # Errors
    /// The engine has not finished starting yet.
    pub fn engine(&self) -> Result<Arc<ProcessEngine>> {
        self.engine
            .get()
            .cloned()
            .ok_or_else(|| Error("the process engine has not finished starting".into()))
    }

    /// A snapshot of the counters.
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            runs_started: self.metrics.runs_started.load(Ordering::Relaxed),
            runs_completed: self.metrics.runs_completed.load(Ordering::Relaxed),
            runs_failed: self.metrics.runs_failed.load(Ordering::Relaxed),
            steps_run: self.metrics.steps_run.load(Ordering::Relaxed),
            resume_backlog: self.metrics.resume_backlog.load(Ordering::Relaxed),
        }
    }

    /// Loads every published, compiling version into the engine's registry again (the publish
    /// path, WP-BPM-09.T4, calls this after a publish).
    ///
    /// # Errors
    /// The definition store's failure. A version that fails to compile is in the report's
    /// `failures`, never an error here: the rest still load.
    pub async fn reload_definitions(&self) -> Result<LoadReport> {
        let engine = self.engine()?;
        load_into(engine.processes(), &*self.definitions)
            .await
            .map_err(|e| Error(format!("loading process definitions failed: {}", e.code())))
    }

    /// Starts a Case of the named process's latest published version (or `version`, when given),
    /// deriving the Case id from `business_key` so the same key never starts two Cases. Enqueues
    /// the first case-step job at once (best-effort: the resume scan finds it within
    /// `resume_interval_secs` either way, so a failed enqueue here is not fatal).
    ///
    /// # Errors
    /// [`EngineError::ProcessNotFound`] when the named (version) is not loaded, or the store's
    /// failure.
    pub async fn start_run(
        &self,
        process: &str,
        version: Option<u32>,
        input: Json,
        initiator: Option<String>,
        business_key: &str,
        now: OffsetDateTime,
    ) -> Result<CaseId> {
        let engine = self.engine()?;
        let definition = match version {
            Some(version) => engine.processes().get(process, version),
            None => engine.processes().latest(process),
        }
        .ok_or_else(|| {
            Error(format!(
                "process `{process}` has no loaded version to start"
            ))
        })?;
        let case_id = case_id_of(business_key)?;
        let missing = missing_handler_kinds(&definition, engine.handlers());
        if let Some(kind) = missing.first() {
            return Err(Error(format!(
                "process `{process}` version {} uses node kind `{kind}`, which no handler is registered for",
                definition.number()
            )));
        }
        let record = engine
            .start_definition(&definition, case_id.clone(), business_key.to_owned(), initiator, now)
            .await
            .map_err(|e| Error(format!("starting the case failed: {}", e.code())))?;
        // `start_definition` persists the fresh Case with empty variables (`CaseRecord::new`'s
        // default); a non-empty `input` must be written back through the store, or every Run
        // starts with empty variables no matter what the caller passed. Skip the extra round
        // trip when there is nothing to change.
        if input != record.variables {
            engine
                .store()
                .commit(
                    &case_id,
                    record.run.version,
                    CaseUpdate::new().with_variables(input),
                )
                .await
                .map_err(|e| Error(format!("persisting the run's input failed: {}", e.code())))?;
        }
        self.metrics.run_started();
        self.enqueue_step(&case_id, now).await;
        Ok(case_id)
    }

    /// Enqueues one case-step job. Logged and dropped on failure: never fatal to the caller,
    /// since the resume scan will find the Case regardless.
    async fn enqueue_step(&self, case: &CaseId, now: OffsetDateTime) {
        let job = CaseJob::new(case.clone(), self.site.clone(), now);
        let spec = JobSpec {
            name: CASE_JOB_NAME.to_owned(),
            queue: QUEUE.to_owned(),
            args: serde_json::json!({ "case": case.as_str() }),
            idempotency_key: job.idempotency_key(),
            not_before: Some(now),
        };
        if let Err(e) = self.submit.submit(spec).await {
            tracing::debug!(site = %self.site, %e, "case-step enqueue did not land; the resume scan will catch it");
        }
    }

    /// Runs one case-step job: `Engine::step`, then re-enqueues once more when the turn made
    /// progress but the Case has not finished (the engine runs only the nodes ready at the
    /// start of the turn).
    async fn run_case_step(&self, job: &JobContext) -> HandlerResult {
        let Some(case_text) = job.job.args.get("case").and_then(Json::as_str) else {
            return HandlerResult::Fail("bpm_engine.config_invalid".to_owned());
        };
        let Ok(case) = CaseId::new(case_text) else {
            return HandlerResult::Fail("bpm_engine.config_invalid".to_owned());
        };
        let Ok(engine) = self.engine() else {
            return HandlerResult::Retry("process engine is not ready yet".to_owned());
        };
        let now = self.clock.now_utc();
        match engine.step(&case, now).await {
            Ok(report) => {
                self.metrics.step_ran();
                if report.finished {
                    if report.status == RunStatus::Failed {
                        self.metrics.run_failed();
                    } else {
                        self.metrics.run_completed();
                    }
                } else if report.made_progress() {
                    self.enqueue_step(&case, now).await;
                }
                HandlerResult::Ok
            }
            Err(e) => classify(&e),
        }
    }

    /// Every Case the store reports runnable at `now`, enqueued (idempotent). Returns how many
    /// were listed, for the resume loop's backlog gauge.
    ///
    /// # Errors
    /// The store's failure.
    pub async fn resume_once(&self, now: OffsetDateTime, limit: u32) -> Result<usize> {
        let engine = self.engine()?;
        let ids = engine
            .store()
            .list_runnable(now, limit)
            .await
            .map_err(|e| Error(format!("listing runnable cases failed: {}", e.code())))?;
        self.metrics.backlog(ids.len());
        for id in &ids {
            self.enqueue_step(id, now).await;
        }
        Ok(ids.len())
    }
}

// ---------------------------------------------------------------------------------------------
// ProcessService: the hosted service (readiness, the resume loop, shutdown).
// ---------------------------------------------------------------------------------------------

/// How many runnable cases one resume scan lists at most.
const RESUME_PAGE: u32 = 500;

struct ProcessService {
    host: Arc<ProcessHost>,
    outbox: Arc<dyn OutboxLease>,
    resume_interval: Duration,
    clock: Arc<dyn Clock>,
    /// The `(name, version)` pairs loaded by the one `load_into` call this service ever makes
    /// (the first successful [`ProcessService::check`]), so every later readiness poll only
    /// re-reads the already-loaded registry (no mutation) instead of reloading the definition
    /// store on every poll. [`ProcessHost::reload_definitions`] is the only other path that
    /// reloads, after a publish.
    startup: OnceCell<Vec<(String, u32)>>,
}

impl Service for ProcessService {
    fn name(&self) -> &'static str {
        "process"
    }

    fn level(&self) -> Level {
        // Durable processes that pause are not a reason to take document serving down.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            let engine = self
                .host
                .engine()
                .map_err(|e| format!("the process engine has not finished starting: {e}"))?;
            // Load the definition store exactly once (the first readiness poll that finds the
            // engine built), caching the loaded `(name, version)` pairs. Every later poll
            // re-validates handler coverage over the registry already loaded — a read, never a
            // mutation — instead of calling `load_into` (which re-mutates the live registry)
            // again on every poll. `ProcessHost::reload_definitions` is the only other reload
            // path, run after a publish (WP-BPM-09.T4), not from here.
            let loaded = self
                .startup
                .get_or_try_init(|| async {
                    let report = load_into(engine.processes(), &*self.host.definitions)
                        .await
                        .map_err(|e| format!("the definition store is not usable: {}", e.code()))?;
                    if let Some(failure) = report.failures.first() {
                        return Err(format!(
                            "process `{}` version {} did not load ({} total failure(s), {:?}); the rest loaded",
                            failure.name,
                            failure.version,
                            report.failures.len(),
                            failure.kind
                        ));
                    }
                    Ok(report.loaded)
                })
                .await?;
            for (name, version) in loaded {
                let Some(definition) = engine.processes().get(name, *version) else {
                    continue;
                };
                if let Some(kind) = missing_handler_kinds(&definition, engine.handlers()).first() {
                    return Err(format!(
                        "process `{name}` version {version} uses node kind `{kind}`, which no handler is registered for"
                    ));
                }
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let standby = (self.resume_interval / 3).max(Duration::from_millis(500));
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                match self.outbox.acquire(RESUME_CONSUMER).await {
                    Ok(lease) => {
                        if self.resume_lead(&mut stop, &lease).await {
                            return Ok(());
                        }
                    }
                    Err(_) => {
                        tracing::debug!(
                            "the process resume lease is held elsewhere; standing by"
                        );
                    }
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(standby) => {}
                }
            }
        })
    }
}

impl ProcessService {
    /// Runs the resume scan as the lease holder until the stop signal or the loss of the lease.
    /// `true` means the node was stopped, `false` that the lease was lost (the caller retries).
    async fn resume_lead(
        &self,
        stop: &mut watch::Receiver<bool>,
        lease: &crate::services::ports::ConsumerLease,
    ) -> bool {
        let mut ticker = tokio::time::interval(self.resume_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = ticker.tick() => {
                    match self.outbox.still_held(lease).await {
                        Ok(true) => {
                            let now = self.clock.now_utc();
                            if let Err(e) = self.host.resume_once(now, RESUME_PAGE).await {
                                tracing::warn!(%e, "the process resume scan failed");
                            }
                        }
                        Ok(false) | Err(_) => break false,
                    }
                }
            }
        };
        let _ = self.outbox.release(lease).await;
        stopped
    }
}

// ---------------------------------------------------------------------------------------------
// register(): wires it all together, per site, under `ctx.assembling()` only.
// ---------------------------------------------------------------------------------------------

/// Registers the process engine: its stores, its node handler builder port, its case-step job
/// handler and its resume service. Inert when `services.process.enabled = false`.
///
/// # Errors
/// A setting that does not decode or is out of range; a store that fails to prepare; a port
/// another module already provided.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.process.{key}: out of range")))?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(error)?;
    let pool = env.backend.pool();
    let clock = Arc::clone(ctx.clock());

    let case_store = PgCaseStore::new(pool.clone()).with_tenant(site.as_str());
    case_store.prepare().await.map_err(error)?;
    let definitions = Arc::new(PgDefinitionStore::new(pool).with_tenant(site.as_str()));
    definitions.prepare().await.map_err(error)?;

    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let holder = format!("process-{}", spot_storage_contract::RecordId::new_v7());
    let lease_name = format!("{TIMER_LEASE}:{}", site.as_str());
    let now = clock.now_utc();
    let (_lease, timer_port) = build_timer_port(leases, &lease_name, &holder, now)?;

    let submit = ctx.require_port::<dyn JobSubmit>()?;
    let outbox = ctx.require_port::<dyn OutboxLease>()?;

    let builder = Arc::new(HandlerBuilder::new());
    ctx.provide(Arc::clone(&builder))?;

    let host = Arc::new(ProcessHost::new(
        site,
        Arc::clone(&definitions),
        submit,
        Arc::clone(&clock),
    ));
    ctx.provide(Arc::clone(&host))?;

    let registry_handler = Arc::clone(&host);
    ctx.job_handlers().register(CASE_JOB_NAME, move |job| {
        let host = Arc::clone(&registry_handler);
        Box::pin(async move { Ok(host.run_case_step(&job).await) })
    });

    ctx.add(Arc::new(ProcessService {
        host: Arc::clone(&host),
        outbox,
        resume_interval: Duration::from_secs(settings.resume_interval_secs),
        clock,
        startup: OnceCell::new(),
    }));

    ctx.finally(move |_ctx| {
        let registry = builder.build_registry();
        let engine = Arc::new(Engine::new(case_store, timer_port, registry));
        host.set_engine(engine);
        Ok(())
    });
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn settings_default_to_enabled_with_a_fifteen_second_resume() {
        let settings = Settings::default();
        assert!(settings.enabled);
        assert_eq!(settings.resume_interval_secs, 15);
        assert!(settings.validate().is_ok());
    }

    #[test]
    fn settings_refuse_an_out_of_range_interval_and_unknown_keys() {
        let too_short = Settings {
            resume_interval_secs: 1,
            ..Settings::default()
        };
        assert_eq!(too_short.validate(), Err("resume_interval_secs"));
        let too_long = Settings {
            resume_interval_secs: 301,
            ..Settings::default()
        };
        assert_eq!(too_long.validate(), Err("resume_interval_secs"));
        let bad: std::result::Result<Settings, _> = toml::from_str("enabedl = true");
        assert!(bad.is_err());
    }

    #[test]
    fn case_ids_are_deterministic_and_distinct_by_business_key() {
        let a = case_id_of("order-1").unwrap();
        let b = case_id_of("order-1").unwrap();
        let c = case_id_of("order-2").unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn classify_separates_transient_from_structural_failures() {
        let transient = EngineError::StoreFailed {
            detail: "d".to_owned(),
        };
        assert!(matches!(classify(&transient), HandlerResult::Retry(_)));
        let structural = EngineError::ProcessNotFound {
            key: "p".to_owned(),
            version: 1,
        };
        assert!(matches!(classify(&structural), HandlerResult::Fail(_)));
    }

    #[test]
    fn the_handler_builder_collects_handlers_on_top_of_the_logic_default_set() {
        let builder = HandlerBuilder::new();
        builder.register(Arc::new(spot_bpm_engine::TriggerHandler));
        let registry = builder.build_registry();
        assert!(registry.contains(&spot_bpm_engine::NodeKind::Trigger));
        assert!(registry.contains(&spot_bpm_engine::NodeKind::Wait));
    }

    #[test]
    fn to_jiff_round_trips_an_ordinary_instant() {
        let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let converted = to_jiff(at).unwrap();
        assert_eq!(converted.as_second(), 1_700_000_000);
    }
}
