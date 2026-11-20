//! The job worker, the outbox intake and the scheduler, for one site.
//!
//! # Why these three are one service family
//!
//! A job only exists because something committed: a document hook's outbox event (ADR-51), a
//! schedule's due slot, or mail. The *intake* turns committed `sys_job_enqueued` outbox events
//! into rows, the *worker* claims and runs them, and the *scheduler* is the one producer that
//! is not a hook. The first two run under one coordinator lease and the scheduler under
//! another, so a second node on the same site is fenced out instead of doubling the work:
//!
//! - `serve:<site>:jobs`: the site's queue consumer (the worker and the intake);
//! - `scheduler:<site>`: the schedule leader.
//!
//! # The lease
//!
//! A node that does not hold the lease is a standby: it retries and consumes nothing. The holder
//! renews at a third of the lease's time to live, and before every intake batch it asks the
//! coordinator whether its fencing token is still the current one. A renewal that fails or a
//! stale token means another node may be consuming: the worker is stopped (running jobs get the
//! grace period, then are abandoned to the store's own claim fencing), and the node goes back
//! to standby. The worker also asks the same lease before every claim, through a claim gate that
//! caches its answer for a tenth of the time to live. Stopping releases the lease at
//! once, so a restart does not wait for the time to live.
//!
//! # The store
//!
//! The durable store (`spot-jobs-pg`, on the site's own pool) keeps jobs, their idempotency
//! keys, the outbox events already taken in and the intake position in the site's database.
//! A node killed mid-job loses nothing: the claim's lease lapses, the reaper returns the work to
//! the queue and a node (this one after a restart, or another) runs it again with a higher
//! attempt number. The in-memory store (`services.jobs.store = "memory"`, single-node profile
//! only) keeps nothing across a restart; the intake then replays the outbox from the start
//! (deduplicated by event id).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use spot_app_api::context::Invocation;
use spot_app_api::prelude::RecordId;
use spot_app_api::{AppError, AppResult, BoxFuture as AppBoxFuture, Document, ErrorClass, Message};
use spot_committed_delivery::{CommittedStepsHandler, JobStoreMarkers, RunnerFor, StepRunner};
use spot_coordinator::{Coordinator, Lease};
use spot_core::{Clock, SiteId};
use spot_document::engine::{CommittedStep, CommittedSteps, StepOutcome, StepSource};
use spot_effect_hold::{EffectClass, Gate, HoldSnapshot, QueueGate};
use spot_http::api::service::{Door, DoorTier};
use spot_jobs::model::InsertResult;
use spot_jobs::model::Job;
use spot_jobs::worker::{
    BoxFuture, ClaimGate, FinalAttemptHook, HandlerResult, JobContext, JobHandlerRegistry,
};
use spot_jobs::{InMemoryJobStore, JobStore, NewJob, QueuePolicy, Worker};
use spot_postgres::PgBackend;
use spot_scheduler::{
    InMemoryHighWater, InMemoryLeaseStore, MissedRunPolicy, RunSlot, Schedule, Scheduler,
    SchedulerConfig, SlotSink,
};
use spot_storage_contract::ChangeFeed;
use tokio::sync::{mpsc, watch};

use crate::live::SiteLive;
use crate::services::realtime::{OfflineSlot, finalise_door, refusal_error};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};

/// The claimed queue-consumer lease of one site.
pub fn consumer_lease(site: &str) -> String {
    format!("serve:{site}:jobs")
}

/// The schedule leader lease of one site.
pub fn scheduler_lease(site: &str) -> String {
    format!("scheduler:{site}")
}

/// How often the reaper looks for claims whose lease lapsed.
const REAP_INTERVAL: Duration = Duration::from_secs(2);

/// The hooks the worker calls once when a last attempt ends in a timeout, a cancellation or a
/// panic: one [`FinalAttemptHook`] that fans out to every subsystem that registered one.
///
/// The worker holds a single hook, so a second subsystem could not register without replacing
/// the first. This set is that one hook. It calls the registered hooks in registration order,
/// each on its own task, so a hook that panics or hangs up does not stop the ones after it (the
/// worker applies the same rule to the hook as a whole). Nothing is logged but a count: a hook
/// gets a job's arguments, and those hold tenant data.
#[derive(Default)]
pub struct FinalAttemptHooks {
    hooks: std::sync::Mutex<Vec<Arc<dyn FinalAttemptHook>>>,
}

impl FinalAttemptHooks {
    /// An empty set.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Adds `hook` after the ones already registered.
    pub fn register(&self, hook: Arc<dyn FinalAttemptHook>) {
        self.hooks
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(hook);
    }

    /// How many hooks are registered.
    pub fn len(&self) -> usize {
        self.hooks.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// Whether no hook is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl FinalAttemptHook for FinalAttemptHooks {
    fn on_dead<'a>(&'a self, job: &'a Job, reason: &'a str) -> BoxFuture<'a, ()> {
        // Copy the list out: the guard must not be held across an await.
        let hooks: Vec<Arc<dyn FinalAttemptHook>> =
            self.hooks.lock().unwrap_or_else(|p| p.into_inner()).clone();
        Box::pin(async move {
            for (index, hook) in hooks.into_iter().enumerate() {
                let (job, reason) = (job.clone(), reason.to_owned());
                // A task per hook: a panic (while building the future or while running it)
                // comes back as a join error instead of unwinding through the worker.
                let outcome = tokio::spawn(async move { hook.on_dead(&job, &reason).await }).await;
                if outcome.is_err() {
                    tracing::error!(
                        code = "jobs.final_attempt_hook_failed",
                        hook = index,
                        "a final-attempt hook panicked; the next hooks still run"
                    );
                }
            }
        })
    }
}

/// Builds the worker the queue consumer runs: the handlers, the queue policies, the clock and
/// the final-attempt hooks of the site. A separate function so a test runs the worker the host
/// runs, without a database.
pub fn build_worker(
    store: Arc<dyn JobStore>,
    registry: Arc<JobHandlerRegistry>,
    policies: HashMap<String, QueuePolicy>,
    clock: Arc<dyn Clock>,
    hooks: Arc<FinalAttemptHooks>,
) -> Worker {
    Worker::new(store, registry, policies, {
        Arc::new(move || clock.now_utc())
    })
    .with_final_attempt_hook(hooks)
}

// ---- internal schedules: a service's own schedule and handler ----

/// A service's handler for one of its internal schedules. It gets the job like any handler of the
/// registry (so the claim gate, the side-effect hold and the metrics apply unchanged) and answers
/// the worker's verdict.
pub type InternalHandler =
    Arc<dyn Fn(JobContext) -> BoxFuture<'static, HandlerResult> + Send + Sync>;

/// What a service registers to run something on a schedule.
pub struct InternalSchedule {
    /// The registering service (a storage-style name). It namespaces the schedule: the job is
    /// called `<service>:<name>`.
    pub service: String,
    /// The schedule's name, a storage-style name unique within the service.
    pub name: String,
    /// The six-field cron expression, seconds first (`0 * * * * *` is every minute), in UTC.
    pub cron: String,
    /// What a run missed while no node led does.
    pub missed_run_policy: MissedRunPolicy,
    /// The queue: `default`, `short` or `long`.
    pub queue: String,
    /// The longest one run may take.
    pub budget: Duration,
    /// The handler, run by the worker.
    pub handler: InternalHandler,
}

/// The registration port of the jobs module, provided with
/// `ctx.provide::<dyn InternalSchedules>(..)` (start order [`ORDER`], 600). A service whose own
/// `ORDER` is larger fetches it with `ctx.port::<dyn InternalSchedules>()` from its `register`
/// (build phase) and calls [`InternalSchedules::register`]. The port is absent on a reload build
/// (the running scheduler keeps what it planned): a module skips quietly when it is `None`.
///
/// The scheduler plans an internal schedule exactly like a manifest schedule (the leader lease,
/// the high-water mark per schedule, `enqueue_scheduled`), and the worker runs its handler
/// through the registry that holds the manifest handlers.
pub trait InternalSchedules: Send + Sync {
    /// Adds `schedule`.
    ///
    /// # Errors
    ///
    /// A service, schedule or queue name that is malformed, a cron expression that does not
    /// parse, a schedule of that service and name registered already, or a registration after the
    /// site's modules were all registered.
    fn register(&self, schedule: InternalSchedule) -> crate::Result<()>;

    /// The job names (`<service>:<name>`) registered, in registration order. A health text shows
    /// these and nothing else.
    fn names(&self) -> Vec<String>;
}

/// One internal schedule after validation: what the scheduler plans and the worker runs.
#[derive(Clone)]
struct PlannedInternal {
    service: String,
    job: String,
    schedule: Arc<Schedule>,
    policy: MissedRunPolicy,
    handler: InternalHandler,
}

/// The internal schedules of one site. Built per site; nothing is shared between sites.
#[derive(Default)]
pub struct InternalScheduleRegistry {
    planned: std::sync::Mutex<Vec<PlannedInternal>>,
    sealed: std::sync::atomic::AtomicBool,
}

impl InternalScheduleRegistry {
    /// An empty, open registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<PlannedInternal>> {
        self.planned.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Closes the registry (a later registration is refused), checks what it holds against
    /// `manifest` (the manifest schedules planned) and the handlers already in `handlers`, and
    /// registers each handler under its job name. Returns what the scheduler plans: the service
    /// (the app part of the job name), the schedule and its missed-run policy. The jobs module
    /// calls this once every module registered.
    ///
    /// # Errors
    ///
    /// A job name that a manifest schedule (its planned name or its step) or an earlier handler
    /// already uses: the site does not start.
    pub fn install(
        &self,
        manifest: &[(Arc<Schedule>, MissedRunPolicy)],
        handlers: &mut JobHandlerRegistry,
    ) -> crate::Result<Vec<(String, Arc<Schedule>, MissedRunPolicy)>> {
        self.sealed.store(true, Ordering::Release);
        let planned = self.lock().clone();
        install_internal(planned, manifest, handlers)
    }
}

impl InternalSchedules for InternalScheduleRegistry {
    fn register(&self, schedule: InternalSchedule) -> crate::Result<()> {
        use spot_app_api::prelude::Name;
        let InternalSchedule {
            service,
            name,
            cron,
            missed_run_policy,
            queue,
            budget,
            handler,
        } = schedule;
        let job = format!("{service}:{name}");
        if self.sealed.load(Ordering::Acquire) {
            return Err(crate::Error(format!(
                "the internal schedule `{job}` was registered after every module registered; register from `register`"
            )));
        }
        if Name::new(&service).is_err() || Name::new(&name).is_err() {
            return Err(crate::Error(format!(
                "the internal schedule `{job}` needs a storage-style service and schedule name"
            )));
        }
        let queue = match queue.as_str() {
            "default" => spot_manifest::Queue::Default,
            "short" => spot_manifest::Queue::Short,
            "long" => spot_manifest::Queue::Long,
            other => {
                return Err(crate::Error(format!(
                    "the internal schedule `{job}` names the queue `{other}`; queues are default, short and long"
                )));
            }
        };
        let cron = spot_manifest::Cron::parse(&cron)
            .map_err(|e| crate::Error(format!("the internal schedule `{job}`: {e}")))?;
        let step = spot_app_api::StepName::new(&job)
            .map_err(|e| crate::Error(format!("the internal schedule `{job}`: {e}")))?;
        let zone = spot_scheduler::TimeZone::parse("UTC")
            .map_err(|e| crate::Error(format!("the internal schedule `{job}`: {e}")))?;
        let planned = Schedule::build(name, cron, step, queue, budget, zone)
            .map_err(|e| crate::Error(format!("the internal schedule `{job}`: {e}")))?;
        let mut all = self.lock();
        if all.iter().any(|p| p.job == job) {
            return Err(crate::Error(format!(
                "the internal schedule `{job}` is registered twice"
            )));
        }
        all.push(PlannedInternal {
            service,
            job,
            schedule: Arc::new(planned),
            policy: missed_run_policy,
            handler,
        });
        Ok(())
    }

    fn names(&self) -> Vec<String> {
        self.lock().iter().map(|p| p.job.clone()).collect()
    }
}

/// Checks the internal schedules against what the manifests declare and what the registry
/// already holds, and registers their handlers. Returns what the scheduler plans, with the app
/// part of each job name (the service).
///
/// # Errors
///
/// A job name that a manifest schedule (its planned name or its step) or an earlier handler
/// already uses: the site does not start.
fn install_internal(
    planned: Vec<PlannedInternal>,
    manifest: &[(Arc<Schedule>, MissedRunPolicy)],
    registry: &mut JobHandlerRegistry,
) -> crate::Result<Vec<(String, Arc<Schedule>, MissedRunPolicy)>> {
    let mut out = Vec::new();
    for item in planned {
        let clash = manifest.iter().any(|(s, _)| {
            s.step.as_str() == item.job
                || spot_scheduler::scheduler::schedule_key("", &s.name) == item.job
        });
        if clash || registry.get(&item.job).is_some() {
            return Err(crate::Error(format!(
                "the internal schedule `{}` of `{}` clashes with a schedule or a job handler that is already declared",
                item.job, item.service
            )));
        }
        let (handler, service, job) = (
            Arc::clone(&item.handler),
            item.service.clone(),
            item.job.clone(),
        );
        registry.register(item.job.clone(), move |ctx: JobContext| {
            let handler = Arc::clone(&handler);
            let (service, job) = (service.clone(), job.clone());
            Box::pin(async move {
                // Audited by name: who it ran as (the registering service), never a caller.
                tracing::info!(
                    target: "spot::audit::internal_schedule",
                    service = %service,
                    job = %job,
                    attempt = ctx.attempt,
                    "internal schedule run"
                );
                Ok(handler(ctx).await)
            })
        });
        out.push((item.service, item.schedule, item.policy));
    }
    Ok(out)
}

/// Settings the job services are built from.
pub struct Settings {
    /// The site.
    pub site: SiteId,
    /// The site's pooled database: the change feed the intake reads.
    pub backend: PgBackend,
    /// The durable store, or the in-memory one when the host runs the memory store.
    pub store: Arc<dyn JobStore>,
    /// The queues this node claims.
    pub queues: Vec<String>,
    /// Outbox intake interval.
    pub intake_interval: Duration,
    /// How long a stopping worker waits for its running job.
    pub grace: Duration,
    /// This node's identity in leases and worker claims. It is unique per process, so two
    /// processes of one site never share a holder name.
    pub node: String,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// The coordinator the leases are held in.
    pub coordinator: Arc<dyn Coordinator>,
    /// The consumer lease's time to live.
    pub consumer_ttl: Duration,
    /// Every manifest schedule of the site, with its missed-run policy.
    pub schedules: Vec<(Arc<Schedule>, MissedRunPolicy)>,
    /// The scheduler's tick interval.
    pub tick_interval: Duration,
    /// The schedule leader lease's time to live.
    pub lease_ttl: Duration,
}

impl Settings {
    /// The in-memory store a host that runs the memory store uses.
    pub fn memory_store() -> Arc<dyn JobStore> {
        Arc::new(InMemoryJobStore::new())
    }
}

/// One named lease and the handle this node holds on it.
///
/// The keeper never holds its guard across an await: the handle is cloned out, used, and put
/// back.
pub struct Keeper {
    coordinator: Arc<dyn Coordinator>,
    name: String,
    holder: String,
    ttl: Duration,
    held: std::sync::Mutex<Option<Lease>>,
    view: Arc<LeaseView>,
}

/// What the claim gate sees of a [`Keeper`]'s lease: written by the keeper, read by the
/// gate.
///
/// The keeper marks the view *lost*, and bumps the generation, the moment it learns the lease is
/// gone (a failed or stale renewal, a stale fence check, a release): before it logs and before
/// it does anything else. The gate honours a cached `true` only under the generation it was
/// computed under, answers `false` for a lost view without a coordinator call, and a lost view
/// stays lost until the keeper takes the lease again (a new token), not until a cache period
/// passes. The view starts lost: a keeper that never held the lease permits nothing.
///
/// A claim already past the gate when the loss lands is not covered here; the lease token the
/// job store checks on the claim (job fencing) covers it.
pub struct LeaseView {
    generation: std::sync::atomic::AtomicU64,
    state: std::sync::Mutex<ViewState>,
}

struct ViewState {
    lost: bool,
    /// The generation, the monotonic time and the answer of the last fence check.
    cached: Option<(u64, Duration, bool)>,
}

/// What the gate does for one claim attempt.
enum Consult {
    /// The lease is lost: refuse.
    Closed,
    /// A fresh answer under the current generation says the lease is held.
    Permit,
    /// Ask the coordinator; remember this generation, read before the call.
    Ask(u64),
}

impl LeaseView {
    fn new() -> Self {
        Self {
            generation: std::sync::atomic::AtomicU64::new(0),
            state: std::sync::Mutex::new(ViewState {
                lost: true,
                cached: None,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ViewState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// The lease is gone: refuse everything, drop the cache, invalidate answers in flight.
    pub fn mark_lost(&self) {
        let mut state = self.lock();
        state.lost = true;
        state.cached = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// The keeper took the lease (a new token): permit again, under a new generation.
    fn reacquired(&self) {
        let mut state = self.lock();
        state.lost = false;
        state.cached = None;
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// `true` while the lease is lost.
    pub fn is_lost(&self) -> bool {
        self.lock().lost
    }

    fn consult(&self, now: Duration, period: Duration) -> Consult {
        let state = self.lock();
        if state.lost {
            return Consult::Closed;
        }
        let generation = self.generation();
        if let Some((cached_generation, at, true)) = state.cached
            && cached_generation == generation
            // A clock reading before the cached one is not trusted: ask again (fail closed).
            && now >= at
            && now - at < period
        {
            return Consult::Permit;
        }
        Consult::Ask(generation)
    }

    /// Records the answer of a fence check started under `generation`. A loss that landed during
    /// the call discards the answer: `false` is returned whatever the call said.
    fn finish(&self, generation: u64, answer: bool, now: Duration) -> bool {
        let mut state = self.lock();
        if state.lost || self.generation() != generation {
            return false;
        }
        if answer {
            state.cached = Some((generation, now, true));
        }
        answer
    }

    /// A renewal just succeeded: the lease is current as of `now`.
    fn refreshed(&self, now: Duration) {
        let mut state = self.lock();
        if !state.lost {
            let generation = self.generation();
            state.cached = Some((generation, now, true));
        }
    }
}

impl Keeper {
    /// A keeper for the lease `name` that `holder` takes for `ttl` in `coordinator`.
    pub fn new(
        coordinator: Arc<dyn Coordinator>,
        name: String,
        holder: String,
        ttl: Duration,
    ) -> Self {
        Self {
            coordinator,
            name,
            holder,
            ttl,
            held: std::sync::Mutex::new(None),
            view: Arc::new(LeaseView::new()),
        }
    }

    /// What the claim gate reads: lost as soon as this keeper knows the lease is.
    pub fn view(&self) -> Arc<LeaseView> {
        Arc::clone(&self.view)
    }

    fn handle(&self) -> Option<Lease> {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn put(&self, lease: Option<Lease>) {
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) = lease;
    }

    /// Takes the lease if nobody holds it. `false` means another node does, or the
    /// coordinator cannot be reached: either way this node must not consume.
    pub async fn acquire(&self) -> bool {
        match self
            .coordinator
            .acquire(&self.name, &self.holder, self.ttl)
            .await
        {
            Ok(lease) => {
                self.put(Some(lease));
                self.view.reacquired();
                true
            }
            Err(e) => {
                tracing::debug!(lease = %self.name, %e, "lease not acquired; standing by");
                false
            }
        }
    }

    /// Confirms the fencing token is still the current one. `false` means the lease is lost:
    /// stop consuming.
    pub async fn still_held(&self) -> bool {
        let Some(lease) = self.handle() else {
            return false;
        };
        match self.coordinator.check_fence(&self.name, lease.token).await {
            Ok(()) => true,
            Err(e) => {
                // Lost first: nothing, not even the log line, comes before the gate closes.
                self.view.mark_lost();
                tracing::warn!(lease = %self.name, %e, "fencing token is stale; the lease is lost");
                self.put(None);
                false
            }
        }
    }

    /// Renews the lease. `false` means it is lost.
    pub async fn renew(&self) -> bool {
        let Some(lease) = self.handle() else {
            return false;
        };
        match self.coordinator.renew(&lease, self.ttl).await {
            Ok(renewed) => {
                self.put(Some(renewed));
                true
            }
            Err(e) => {
                self.view.mark_lost();
                tracing::warn!(lease = %self.name, %e, "lease renewal failed; the lease is lost");
                self.put(None);
                false
            }
        }
    }

    /// Gives the lease up, so another node takes over at once.
    pub async fn release(&self) {
        self.view.mark_lost();
        let lease = self.held.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(lease) = lease {
            // Releasing lets another node take over at once instead of after the time to
            // live; a failure only means the lease lapses instead.
            let _ = self.coordinator.release(&lease).await;
        }
    }
}

/// The claim gate over a [`Keeper`]: the worker claims a job only while this node's
/// fencing token is still the current one in the coordinator.
///
/// The worker asks once per claim tick (10 a second) and `check_fence` is a coordinator call, a
/// database round trip in the PostgreSQL profile. The answer is therefore cached, both `true` and
/// `false`, for [`claim_gate_period`]: a tenth of the lease's time to live, at least 50 ms and at
/// most 500 ms. A lease lost between two checks is noticed within that period, so the window in
/// which a deposed node can still claim is at most 500 ms and at most a tenth of the lease, far
/// below the third of the time to live the renew loop alone leaves open. The cache is read from
/// the injected clock's monotonic time, so a test moves it without sleeping. A refused check
/// also drops the keeper's handle, so the gate stays closed (without more coordinator calls)
/// until the node takes the lease again; a failed check call is a refusal (fail closed).
pub struct LeaseClaimGate {
    keeper: Arc<Keeper>,
    view: Arc<LeaseView>,
    clock: Arc<dyn Clock>,
    period: Duration,
}

/// How long [`LeaseClaimGate`] keeps an answer for a lease with time to live `ttl`.
pub fn claim_gate_period(ttl: Duration) -> Duration {
    (ttl / 10).clamp(Duration::from_millis(50), Duration::from_millis(500))
}

impl LeaseClaimGate {
    /// A gate over `keeper`, timed by `clock`.
    pub fn new(keeper: Arc<Keeper>, clock: Arc<dyn Clock>) -> Self {
        let period = claim_gate_period(keeper.ttl);
        let view = keeper.view();
        Self {
            keeper,
            view,
            clock,
            period,
        }
    }

    /// How long an answer is kept.
    pub fn period(&self) -> Duration {
        self.period
    }

    /// A renewal just succeeded: the lease is current as of now.
    pub fn renewed(&self) {
        self.view.refreshed(self.clock.monotonic());
    }

    /// The lease was lost or given up: the gate closes until the keeper takes it again.
    pub fn cleared(&self) {
        self.view.mark_lost();
    }
}

impl ClaimGate for LeaseClaimGate {
    fn permit<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // No handle (never acquired, released, lost): closed, whatever the cache says. The
            // view is already lost then (every path that drops the handle marks it first), so
            // the gate does not mark it again: a re-acquire landing between this read and a
            // second mark would leave a held lease with a gate that stays shut.
            if self.keeper.handle().is_none() {
                return false;
            }
            match self.view.consult(self.clock.monotonic(), self.period) {
                Consult::Closed => false,
                Consult::Permit => true,
                Consult::Ask(generation) => {
                    // The generation was read before the call: a loss that lands while the
                    // coordinator answers discards the answer.
                    let answer = self.keeper.still_held().await;
                    self.view.finish(generation, answer, self.clock.monotonic())
                }
            }
        })
    }
}

/// The side-effect hold's queue gate for this site, when the `effect_hold` module provided its
/// service (WP-MIG-08.T5). `None` when the hold is disabled: jobs then behave as before. Call it
/// from the finaliser that builds [`Jobs`] (every module has registered by then) and pass the
/// result to [`Jobs::hold_queue_gate`].
pub fn queue_gate_from(ctx: &SiteCtx<'_>) -> Option<Arc<QueueGate>> {
    ctx.port::<spot_effect_hold::HoldService>()
        .map(|hold| Arc::new(QueueGate::new(hold.gate())))
}

/// The claim gate while the side-effect hold may be active (WP-MIG-08.T5): every queue must be
/// claimable (a held site claims none) **and** the lease must be held.
///
/// The hold's own answer is a read of a cached flag, asked first and without a ledger call, so a
/// held site does not even consult the coordinator. Jobs that are not claimed are not failed and
/// do not count an attempt: they keep their position and run in order after the release.
pub struct HoldClaimGate {
    lease: Arc<dyn ClaimGate>,
    held: Arc<QueueGate>,
    queues: Vec<String>,
}

impl HoldClaimGate {
    /// A gate over the lease gate, the hold's queue gate and the queues the worker consumes.
    pub fn new(lease: Arc<dyn ClaimGate>, held: Arc<QueueGate>, queues: Vec<String>) -> Self {
        Self {
            lease,
            held,
            queues,
        }
    }
}

impl ClaimGate for HoldClaimGate {
    fn permit<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            // Fail closed: a queue the hold does not release keeps the whole worker idle.
            if !self
                .queues
                .iter()
                .all(|queue| self.held.is_claimable(queue))
            {
                return false;
            }
            self.lease.permit().await
        })
    }
}

/// Guards the handler registered under `job_name` for an external effect of `class` (mail, a
/// webhook delivery) against the site being held (WP-MIG-08.T5). Returns `false` when no handler
/// is registered under that name (nothing is changed).
///
/// The queue gate ([`HoldClaimGate`]) is the primary control: a held site claims no job. This is
/// the second line, for a job claimed in the instant before the hold began: the gate is consulted
/// **before** the handler (and so before the network call), reads only the gate's cached state
/// (it records nothing, so a release never has to replay a job), and a held answer or an unknown
/// state returns a retry: the effect does not leave, and the job tries again after its backoff.
/// A class the hold never holds (a fiscal transmission) always passes.
pub fn guard_job_handler(
    registry: &mut JobHandlerRegistry,
    job_name: &str,
    class: EffectClass,
    gate: Arc<Gate>,
) -> bool {
    let Some(inner) = registry.get(job_name) else {
        return false;
    };
    registry.register(job_name.to_owned(), move |ctx: JobContext| {
        let inner = Arc::clone(&inner);
        let gate = Arc::clone(&gate);
        let class = class.clone();
        Box::pin(async move {
            let open = gate.policies().is_never_hold(&class)
                || matches!(gate.snapshot(), HoldSnapshot::Open);
            if !open {
                return Ok(HandlerResult::Retry(
                    "the site's external effects are held".to_owned(),
                ));
            }
            (**inner)(ctx).await
        })
    });
    true
}

/// The job worker and the outbox intake of one site.
pub struct Jobs {
    site: SiteId,
    backend: PgBackend,
    store: Arc<dyn JobStore>,
    registry: Arc<JobHandlerRegistry>,
    queues: Vec<String>,
    intake_interval: Duration,
    grace: Duration,
    node: String,
    clock: Arc<dyn Clock>,
    keeper: Arc<Keeper>,
    hooks: Arc<FinalAttemptHooks>,
    /// The side-effect hold's queue gate (WP-MIG-08.T5): while the site is held no queue is claimed.
    hold: std::sync::Mutex<Option<Arc<QueueGate>>>,
}

impl Jobs {
    /// A service over `settings`, with the given handlers (mail owns one; an app may).
    pub fn new(settings: &Settings, registry: Arc<JobHandlerRegistry>) -> Arc<Self> {
        Arc::new(Self {
            site: settings.site.clone(),
            backend: settings.backend.clone(),
            store: Arc::clone(&settings.store),
            registry,
            queues: settings.queues.clone(),
            intake_interval: settings.intake_interval,
            grace: settings.grace,
            node: settings.node.clone(),
            clock: Arc::clone(&settings.clock),
            keeper: Arc::new(Keeper::new(
                Arc::clone(&settings.coordinator),
                consumer_lease(settings.site.as_str()),
                settings.node.clone(),
                settings.consumer_ttl,
            )),
            hooks: FinalAttemptHooks::new(),
            hold: std::sync::Mutex::new(None),
        })
    }

    /// Attaches the side-effect hold's queue gate (WP-MIG-08.T5). While the site is held the worker
    /// claims nothing: held jobs stay queued in their original order, are not failed and do not
    /// count an attempt, and run normally once the hold is released. Without a gate the worker
    /// behaves as before. Call it before the service starts: the worker is built when the node
    /// takes the consumer lease.
    pub fn hold_queue_gate(&self, gate: Arc<QueueGate>) {
        if let Ok(mut slot) = self.hold.lock() {
            *slot = Some(gate);
        }
    }

    /// Registers a hook the worker calls once when a job's last attempt ends in a timeout, a
    /// cancellation or a panic (mail writes its bounces this way). Hooks run in registration
    /// order and a failing one does not stop the others. Call it before the service starts: the
    /// worker is built when the node takes the consumer lease.
    pub fn register_final_attempt_hook(&self, hook: Arc<dyn FinalAttemptHook>) {
        self.hooks.register(hook);
    }

    /// The store the worker and the scheduler share.
    pub fn store(&self) -> &Arc<dyn JobStore> {
        &self.store
    }

    /// Takes one batch of committed `sys_job_enqueued` outbox events in as jobs.
    ///
    /// The document engine writes the event as a payload document (`data.job`, `data.args`);
    /// `spot_jobs::process_batch` reads a flatter shape no producer writes, so the host decodes
    /// with the engine's own reader. Each event is inserted with its id as the deduplication
    /// key, and the stored position moves only after every job of the batch is in: a crash
    /// between the two replays the batch and inserts nothing twice. An event that cannot become
    /// a job is logged and skipped (the position still moves, so it cannot block the feed).
    async fn take_in(&self) -> Result<u32, String> {
        let now = self.clock.now_utc();
        let position = self
            .store
            .intake_position()
            .await
            .map_err(|e| e.to_string())?;
        let records = self
            .backend
            .read_outbox(position, 100)
            .await
            .map_err(|e| e.to_string())?;
        let mut last = position;
        let mut jobs = Vec::new();
        for record in &records {
            last = record.position;
            match spot_document::engine::job_from_event(&record.event) {
                Ok(None) => {}
                Ok(Some(request)) if NewJob::validate_job_name(&request.job) => {
                    let mut job = NewJob::new(
                        request.job,
                        request.args,
                        "default".to_owned(),
                        self.site.clone(),
                        request.not_before.unwrap_or(now),
                    )
                    .with_outbox_event_id(record.event.id.to_string());
                    if let Some(key) = request.idempotency_key {
                        job = job.with_idempotency_key(key);
                    }
                    jobs.push(job);
                }
                Ok(Some(_)) | Err(_) => {
                    tracing::warn!(site = %self.site, event = %record.event.id, "an outbox job event was refused");
                }
            }
        }
        let mut inserted = 0;
        for job in jobs {
            if matches!(
                self.store
                    .insert_if_new(job)
                    .await
                    .map_err(|e| e.to_string())?,
                InsertResult::Inserted(_)
            ) {
                inserted += 1;
            }
        }
        if last != position {
            self.store
                .set_intake_position(last)
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(inserted)
    }

    /// Runs the queue as the lease holder until the stop signal or the loss of the lease.
    /// `true` means the node was stopped, `false` that the lease was lost.
    async fn lead(&self, stop: &mut watch::Receiver<bool>) -> bool {
        let policies: HashMap<String, QueuePolicy> = self
            .queues
            .iter()
            .map(|queue| (queue.clone(), QueuePolicy::default()))
            .collect();
        let worker = build_worker(
            Arc::clone(&self.store),
            Arc::clone(&self.registry),
            policies,
            Arc::clone(&self.clock),
            Arc::clone(&self.hooks),
        );
        let gate = Arc::new(LeaseClaimGate::new(
            Arc::clone(&self.keeper),
            Arc::clone(&self.clock),
        ));
        let held = self.hold.lock().ok().and_then(|slot| slot.clone());
        let claim: Arc<dyn ClaimGate> = match held {
            Some(held) => Arc::new(HoldClaimGate {
                lease: gate.clone(),
                held,
                queues: self.queues.clone(),
            }),
            None => gate.clone(),
        };
        let worker = worker.with_claim_gate(claim);
        let (work_stop, work_rx) = watch::channel(false);
        let worker_id = format!("{}:{}", self.node, self.site);
        let queues = self.queues.clone();
        let grace = self.grace;
        let worker_task = tokio::spawn(async move {
            worker
                .run(&worker_id, queues, work_rx, grace)
                .await
                .map_err(|e| e.to_string())
        });
        let mut intake = tokio::time::interval(self.intake_interval);
        intake.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut renew =
            tokio::time::interval((self.keeper.ttl / 3).max(Duration::from_millis(100)));
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut reap = tokio::time::interval(REAP_INTERVAL);
        reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = renew.tick() => {
                    if !self.keeper.renew().await {
                        gate.cleared();
                        break false;
                    }
                    gate.renewed();
                }
                _ = reap.tick() => {
                    let now = self.clock.now_utc();
                    match self.store.reap_expired(now).await {
                        Ok(reaped) if !reaped.is_empty() => {
                            tracing::info!(site = %self.site, count = reaped.len(), "returned claimed jobs whose lease lapsed");
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!(site = %self.site, %e, "reaping lapsed claims failed"),
                    }
                }
                _ = intake.tick() => {
                    // A stale token means another node may be taking this site's events in:
                    // do not take one more batch.
                    if !self.keeper.still_held().await {
                        gate.cleared();
                        break false;
                    }
                    match self.take_in().await {
                        Ok(0) => {}
                        Ok(count) => {
                            tracing::info!(site = %self.site, inserted = count, "outbox intake enqueued jobs");
                        }
                        Err(e) => tracing::warn!(site = %self.site, %e, "outbox intake failed"),
                    }
                }
            }
        };
        let _ = work_stop.send(true);
        match tokio::time::timeout(self.grace, worker_task).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => {
                tracing::warn!(site = %self.site, %e, "job worker stopped with an error")
            }
            Ok(Err(join)) => tracing::warn!(site = %self.site, %join, "job worker task failed"),
            Err(_) => tracing::warn!(site = %self.site, "job worker did not drain in time"),
        }
        gate.cleared();
        self.keeper.release().await;
        stopped
    }
}

impl Service for Jobs {
    fn name(&self) -> &'static str {
        "jobs"
    }

    fn level(&self) -> Level {
        // Without a queue consumer no committed job ever runs; that is not a degraded site.
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            self.store
                .list(spot_jobs::store::JobFilter {
                    site: Some(self.site.clone()),
                    limit: Some(1),
                    ..spot_jobs::store::JobFilter::default()
                })
                .await
                .map_err(|e| format!("the job store is not usable: {e}"))?;
            self.keeper
                .coordinator
                .current(&self.keeper.name)
                .await
                .map_err(|e| format!("the coordinator is not reachable: {e}"))?;
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            // The standby retry is as short as the renewal, so a lease that lapsed is taken
            // within about a third of its time to live.
            let standby = (self.keeper.ttl / 3).max(Duration::from_millis(100));
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if self.keeper.acquire().await {
                    tracing::info!(site = %self.site, node = %self.node, "consuming the job queue");
                    if self.lead(&mut stop).await {
                        return Ok(());
                    }
                    tracing::warn!(site = %self.site, node = %self.node, "no longer the queue consumer; standing by");
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

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.keeper.release().await;
            Ok(())
        })
    }
}

// ---- dynamic schedules: a service whose schedules change while the site runs ----

/// One schedule a [`ScheduleSource`] plans now, and the job each of its slots enqueues.
///
/// Unlike an internal schedule (fixed when the site is assembled) a source may return a
/// different set on every call; the scheduler follows it at its next tick.
#[derive(Clone)]
pub struct DynamicSchedule {
    /// The registering service (the app part of the schedule's deduplication key).
    pub service: String,
    /// The schedule; its name is unique among everything the site schedules.
    pub schedule: Arc<Schedule>,
    /// What a run missed while no node led does.
    pub policy: MissedRunPolicy,
    /// The job handler name every slot of this schedule enqueues (`<app>:<job>`). The handler is
    /// registered with `SiteCtx::job_handlers` like any other.
    pub job: String,
    /// The job's own arguments (a JSON object); the schedule's budget is added as `budget_ms`.
    pub args: serde_json::Value,
}

/// A service's current dynamic schedules. Cheap and synchronous: the source keeps the answer it
/// last computed and returns it.
pub trait ScheduleSource: Send + Sync {
    /// The schedules in force now.
    fn schedules(&self) -> Vec<DynamicSchedule>;
}

/// The sources of one site, provided by the jobs module as a port (`ctx.port::<ScheduleSources>()`)
/// on the assembling build. A source is registered under an id (a second registration of the id
/// replaces the first, so a reload never doubles one).
#[derive(Default)]
pub struct ScheduleSources {
    sources: std::sync::Mutex<std::collections::BTreeMap<String, Arc<dyn ScheduleSource>>>,
}

impl ScheduleSources {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers (or replaces) the source called `id`.
    pub fn set(&self, id: &str, source: Arc<dyn ScheduleSource>) {
        self.sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(id.to_owned(), source);
    }

    /// Every source's schedules, in source-id order.
    #[must_use]
    pub fn current(&self) -> Vec<DynamicSchedule> {
        let sources: Vec<Arc<dyn ScheduleSource>> = self
            .sources
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .cloned()
            .collect();
        sources.iter().flat_map(|s| s.schedules()).collect()
    }
}

/// One planned dynamic schedule as the leader runs it.
struct DynamicNode {
    signature: String,
    node: Scheduler<InMemoryLeaseStore, InMemoryHighWater>,
    sink: Arc<dyn SlotSink>,
}

/// What a dynamic schedule's slots enqueue, found by `(service, schedule name)`.
#[derive(Clone)]
struct DynamicTarget {
    job: String,
    args: serde_json::Value,
}

/// How late a tick may reach a schedule's firing and still run it under every missed-run policy
/// (`spot_scheduler::DEFAULT_SKIP_GRACE` is a minute, as long as a cron firing of every minute: no
/// firing of it would ever count as missed, so `skip` would replay a long outage's last firing).
/// The host knows its tick: five of them, at least a second.
fn skip_grace(tick: Duration) -> Duration {
    tick.saturating_mul(5).max(Duration::from_secs(1))
}

/// The leader's view of the dynamic schedules: its nodes, and every target seen this lease (a
/// slot planned just before a schedule was removed still finds its job).
#[derive(Default)]
struct DynamicNodes {
    nodes: Vec<DynamicNode>,
    targets: HashMap<(String, String), DynamicTarget>,
}

fn dynamic_signature(planned: &DynamicSchedule) -> String {
    format!(
        "{}:{}|{}|{}|{:?}|{:?}|{}|{}",
        planned.service,
        planned.schedule.name,
        planned.schedule.cron.as_str(),
        planned.schedule.zone.name(),
        planned.schedule.queue,
        planned.policy,
        planned.job,
        planned.args
    )
}

impl DynamicNodes {
    /// Follows `wanted`: a schedule whose signature is unchanged keeps its node (and so its
    /// high-water mark), a new or changed one gets a fresh node, a removed one is dropped.
    fn sync(
        &mut self,
        wanted: Vec<DynamicSchedule>,
        build: impl Fn(&DynamicSchedule) -> DynamicNode,
    ) {
        let signatures: Vec<String> = wanted.iter().map(dynamic_signature).collect();
        let current: Vec<&str> = self.nodes.iter().map(|n| n.signature.as_str()).collect();
        if signatures
            .iter()
            .map(String::as_str)
            .eq(current.iter().copied())
        {
            return;
        }
        let mut old: HashMap<String, DynamicNode> = std::mem::take(&mut self.nodes)
            .into_iter()
            .map(|n| (n.signature.clone(), n))
            .collect();
        for (planned, signature) in wanted.iter().zip(signatures) {
            self.targets.insert(
                (planned.service.clone(), planned.schedule.name.clone()),
                DynamicTarget {
                    job: planned.job.clone(),
                    args: planned.args.clone(),
                },
            );
            let node = old.remove(&signature).unwrap_or_else(|| build(planned));
            self.nodes.push(node);
        }
    }

    /// The job a slot of a dynamic schedule enqueues, or `None` for any other slot.
    fn job_for(&self, slot: &RunSlot, site: &SiteId) -> Option<spot_jobs::Result<NewJob>> {
        let target = self
            .targets
            .get(&(slot.slot_ref.app.clone(), slot.schedule.clone()))?;
        let queue = slot.slot_ref.queue.as_str();
        if !matches!(queue, "default" | "short" | "long") {
            return Some(Err(spot_jobs::Error::UnknownQueue));
        }
        let mut args = match &target.args {
            serde_json::Value::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        args.insert(
            "budget_ms".to_owned(),
            serde_json::Value::from(slot.slot_ref.budget_ms),
        );
        let scheduled_at =
            time::OffsetDateTime::from_unix_timestamp(slot.scheduled_for.as_second())
                .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        Some(Ok(NewJob::new(
            target.job.clone(),
            serde_json::Value::Object(args),
            queue.to_owned(),
            site.clone(),
            scheduled_at,
        )
        .with_idempotency_key(slot.deduplication_key.clone())))
    }
}

/// The scheduler: the one node that plans manifest schedules and enqueues their runs.
pub struct Scheduling {
    site: SiteId,
    store: Arc<dyn JobStore>,
    node: String,
    schedules: Vec<(Arc<Schedule>, MissedRunPolicy)>,
    /// The internal schedules of services, with the service that registered each (the app part of
    /// the job name).
    internal: Vec<(String, Arc<Schedule>, MissedRunPolicy)>,
    /// The sources of schedules that change while the site runs.
    dynamic: Arc<ScheduleSources>,
    tick_interval: Duration,
    lease_ttl: Duration,
    clock: Arc<dyn Clock>,
    keeper: Keeper,
}

impl Scheduling {
    /// A scheduler service for one site's manifest schedules.
    pub fn new(settings: &Settings) -> Arc<Self> {
        Self::with_internal(settings, Vec::new())
    }

    /// As [`Scheduling::new`], also planning the internal schedules of services (`(service,
    /// schedule, policy)`): each runs under the same leader lease and high-water mark per
    /// schedule as a manifest schedule, and enqueues the job `<service>:<name>`.
    pub fn with_internal(
        settings: &Settings,
        internal: Vec<(String, Arc<Schedule>, MissedRunPolicy)>,
    ) -> Arc<Self> {
        Self::with_sources(settings, internal, Arc::new(ScheduleSources::new()))
    }

    /// As [`Scheduling::with_internal`], also following the `dynamic` sources: their schedules
    /// are re-read at every tick, so one added, changed or removed while the site runs is
    /// planned from the next tick (a changed schedule starts a fresh high-water mark).
    pub fn with_sources(
        settings: &Settings,
        internal: Vec<(String, Arc<Schedule>, MissedRunPolicy)>,
        dynamic: Arc<ScheduleSources>,
    ) -> Arc<Self> {
        Arc::new(Self {
            site: settings.site.clone(),
            store: Arc::clone(&settings.store),
            node: settings.node.clone(),
            schedules: settings.schedules.clone(),
            internal,
            dynamic,
            tick_interval: settings.tick_interval,
            lease_ttl: settings.lease_ttl,
            clock: Arc::clone(&settings.clock),
            keeper: Keeper::new(
                Arc::clone(&settings.coordinator),
                scheduler_lease(settings.site.as_str()),
                settings.node.clone(),
                settings.lease_ttl,
            ),
        })
    }

    /// Plans and enqueues due slots while this node leads. `true` on a stop, `false` when the
    /// lease was lost.
    async fn lead(&self, stop: &mut watch::Receiver<bool>) -> bool {
        // One node per schedule keeps each schedule's high-water mark apart; the coordinator
        // lease decides which node leads, this local store only serves the planner. Each node
        // has its own store: sharing one lets a node that renewed first after a pause take the
        // lease under a new token and lock every other node out for good.
        let (sender, mut receiver) = mpsc::unbounded_channel::<RunSlot>();
        let mut nodes = Vec::new();
        let mut sinks: Vec<Arc<dyn SlotSink>> = Vec::new();
        let planned: Vec<(String, Arc<Schedule>, MissedRunPolicy)> = self
            .schedules
            .iter()
            .map(|(schedule, policy)| {
                // The job is named `<app>:<schedule>`; a manifest step is `<app>:<step>`.
                let app = schedule
                    .step
                    .as_str()
                    .split_once(':')
                    .map_or_else(String::new, |(app, _)| app.to_owned());
                (app, Arc::clone(schedule), *policy)
            })
            .chain(self.internal.iter().cloned())
            .collect();
        for (app, schedule, policy) in &planned {
            nodes.push(Scheduler::new(
                SchedulerConfig {
                    site: self.site.to_string(),
                    node: self.node.clone(),
                    ttl: self.lease_ttl,
                    schedules: vec![(Arc::clone(schedule), *policy)],
                    app: app.clone(),
                },
                Arc::new(InMemoryLeaseStore::new()),
                InMemoryHighWater::new(),
            )
            .with_skip_grace(skip_grace(self.tick_interval)));
            sinks.push(spot_scheduler::scheduler::sink({
                let sender = sender.clone();
                move |slot: &RunSlot| sender.send(slot.clone()).is_ok()
            }));
        }
        let mut dynamic = DynamicNodes::default();
        let mut ticker = tokio::time::interval(self.tick_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut renew = tokio::time::interval((self.lease_ttl / 3).max(Duration::from_millis(100)));
        renew.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let stopped = loop {
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break true;
                    }
                }
                _ = renew.tick() => {
                    if !self.keeper.renew().await {
                        break false;
                    }
                }
                _ = ticker.tick() => {
                    if !self.keeper.still_held().await {
                        break false;
                    }
                    for (node, sink) in nodes.iter_mut().zip(sinks.iter()) {
                        match node.tick(self.clock.as_ref(), sink.as_ref()) {
                            Ok(slots) => {
                                for slot in slots {
                                    tracing::info!(
                                        site = %self.site,
                                        schedule = slot.key(),
                                        "schedule fired"
                                    );
                                }
                            }
                            Err(e) => tracing::debug!(
                                site = %self.site,
                                %e,
                                "scheduler tick did not lead"
                            ),
                        }
                    }
                    dynamic.sync(self.dynamic.current(), |planned| {
                        let node = Scheduler::new(
                            SchedulerConfig {
                                site: self.site.to_string(),
                                node: self.node.clone(),
                                ttl: self.lease_ttl,
                                schedules: vec![(Arc::clone(&planned.schedule), planned.policy)],
                                app: planned.service.clone(),
                            },
                            Arc::new(InMemoryLeaseStore::new()),
                            InMemoryHighWater::new(),
                        )
                        .with_skip_grace(skip_grace(self.tick_interval));
                        let sink = spot_scheduler::scheduler::sink({
                            let sender = sender.clone();
                            move |slot: &RunSlot| sender.send(slot.clone()).is_ok()
                        });
                        DynamicNode {
                            signature: dynamic_signature(planned),
                            node,
                            sink,
                        }
                    });
                    for planned in &mut dynamic.nodes {
                        match planned.node.tick(self.clock.as_ref(), planned.sink.as_ref()) {
                            Ok(slots) => {
                                for slot in slots {
                                    tracing::info!(
                                        site = %self.site,
                                        schedule = slot.key(),
                                        "schedule fired"
                                    );
                                }
                            }
                            Err(e) => tracing::debug!(
                                site = %self.site,
                                %e,
                                "dynamic scheduler tick did not lead"
                            ),
                        }
                    }
                    while let Ok(slot) = receiver.try_recv() {
                        let enqueued = dynamic
                            .job_for(&slot, &self.site)
                            .unwrap_or_else(|| spot_jobs::enqueue_scheduled(&slot, &self.site));
                        match enqueued {
                            Ok(job) => {
                                // The slot's deduplication key makes a second insert
                                // harmless: a duplicate is reported, not fatal.
                                if let Err(e) = self.store.insert_if_new(job).await {
                                    tracing::warn!(
                                        site = %self.site,
                                        %e,
                                        "scheduled job was not enqueued"
                                    );
                                }
                            }
                            Err(e) => tracing::warn!(
                                site = %self.site,
                                %e,
                                "scheduled slot was refused"
                            ),
                        }
                    }
                }
            }
        };
        self.keeper.release().await;
        stopped
    }
}

impl Service for Scheduling {
    fn name(&self) -> &'static str {
        "scheduler"
    }

    fn level(&self) -> Level {
        // A schedule that never fires is a silent data problem, not a degraded extra.
        Level::Required
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async {
            if self.schedules.is_empty() && self.internal.is_empty() {
                tracing::debug!("no schedules; the scheduler ticks and plans nothing");
            }
            // Names only: what a service schedules is its own business.
            let names: Vec<String> = self
                .internal
                .iter()
                .map(|(service, schedule, _)| format!("{service}:{}", schedule.name))
                .collect();
            if !names.is_empty() {
                tracing::debug!(internal = %names.join(", "), "internal schedules planned");
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let standby = (self.lease_ttl / 3).max(Duration::from_millis(100));
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if self.keeper.acquire().await && self.lead(&mut stop).await {
                    return Ok(());
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

    fn shutdown(&self) -> RunFuture<'_> {
        Box::pin(async move {
            self.keeper.release().await;
            Ok(())
        })
    }
}

/// This module's start order: the one it had in the host's table before it moved here.
pub const ORDER: u32 = 600;

/// Jobs, intake and schedules. The queue and the coordinator are durable: jobs live in the
/// site's database (a killed node loses none), and the cluster profile coordinates through
/// the database too. The single-node profile keeps the in-process coordinator, which
/// refuses a second node in this process, and may run the memory store when `common.toml`
/// says so.
///
/// The worker needs every handler the other modules contribute (mail adds `send_mail`), so the
/// services are built in a finaliser that runs after all modules registered, and placed at
/// this module's order.
pub async fn register(ctx: &mut SiteCtx<'_>) -> crate::Result<()> {
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let (root, loaded, site, backend) = (env.root, env.loaded, env.site, env.backend);
    let clock = Arc::clone(ctx.clock());
    let services_config = &loaded.common.services;
    let node = process_node(&site.site);
    // One coordinator per site: the cluster-redis plugin's, when the host provided it before the
    // modules registered (WP-PLT-19.T3); otherwise the PostgreSQL or in-process one below.
    let coordinator: Arc<dyn spot_coordinator::Coordinator> =
        if let Some(provided) = ctx.port::<dyn spot_coordinator::Coordinator>() {
            provided
        } else if services_config.profile == "cluster" {
            let coordinator = spot_coordinator_pg::PgCoordinator::new(
                backend.pool(),
                spot_coordinator_pg::PgCoordinatorConfig::default(),
            );
            coordinator.prepare().await.map_err(crate::error)?;
            Arc::new(coordinator)
        } else {
            let node_id = spot_coordinator::NodeId::new(node.clone()).map_err(crate::error)?;
            spot_coordinator::InProcessCoordinator::start(
                Arc::new(spot_coordinator::InMemoryPresence::new()),
                node_id,
                Arc::clone(&clock),
            )
            .await
            .map_err(crate::error)?
        };
    // Lease events and the held gauge on `/metrics`, only when that service is on.
    let coordinator: Arc<dyn spot_coordinator::Coordinator> = if services_config.metrics.enabled {
        Arc::new(crate::services::metered_coordinator::MeteredCoordinator::new(coordinator))
    } else {
        coordinator
    };
    let store: Arc<dyn spot_jobs::JobStore> = if services_config.jobs.store == "postgres" {
        let store = spot_jobs_pg::PgJobStore::new(backend.pool());
        store.prepare().await.map_err(crate::error)?;
        Arc::new(store)
    } else {
        Settings::memory_store()
    };
    let schedules = manifest_schedules(root, loaded, site)?;
    // Services that start later (a larger ORDER) register schedules of their own here.
    let internal = Arc::new(InternalScheduleRegistry::new());
    ctx.provide::<dyn InternalSchedules>(Arc::clone(&internal) as Arc<dyn InternalSchedules>)?;
    // Services whose schedules change while the site runs (script schedules) add a source.
    let sources = Arc::new(ScheduleSources::new());
    ctx.provide::<ScheduleSources>(Arc::clone(&sources))?;
    // The narrow ports later modules take (job submit, outbox lease, system reader); each is
    // built from this site's own store, coordinator, feed and engine.
    crate::services::ports::provide(
        ctx,
        crate::services::ports::PortParts {
            site: SiteId::new(&site.site).map_err(crate::error)?,
            store: Arc::clone(&store),
            queues: services_config.jobs.queues.clone(),
            clock: Arc::clone(&clock),
            coordinator: Arc::clone(&coordinator),
            feed: backend.clone(),
            node: node.clone(),
            ttl: std::time::Duration::from_millis(services_config.jobs.lease_ttl_ms),
            loader: Arc::new(crate::services::ports::LiveLoader(Arc::clone(env.live))),
        },
    )?;
    // The committed-steps handler keeps its done markers in the same store the worker claims from.
    let committed_store = Arc::clone(&store);
    let settings = Settings {
        site: SiteId::new(&site.site).map_err(crate::error)?,
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
    // The committed-steps runner asks the offline decision for the user a step acts as; the slot
    // is filled once every module has registered (empty: unchanged).
    let offline = Arc::new(OfflineSlot::default());
    install_committed_steps(
        ctx.job_handlers(),
        Arc::new(LiveRunners::with_offline(
            SiteId::new(&site.site).map_err(crate::error)?,
            Arc::clone(env.live),
            Arc::clone(&offline),
        )),
        committed_store,
    );
    finalise_door(ctx, "offline_door_jobs", move |admission| {
        offline.set(admission);
    });
    ctx.finally(move |ctx| {
        let mut registry = ctx.take_job_handlers();
        let planned = internal.install(&settings.schedules, &mut registry)?;
        let handlers = Arc::new(registry);
        let jobs = Jobs::new(&settings, handlers);
        if let Some(mail) = ctx.port::<crate::services::mail::Mail>() {
            jobs.register_final_attempt_hook(mail.final_attempt_hook());
        }
        ctx.add(jobs);
        ctx.add(Scheduling::with_sources(&settings, planned, sources));
        Ok(())
    });
    Ok(())
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
    root: &std::path::Path,
    loaded: &crate::config::Loaded,
    site: &crate::config::Site,
) -> crate::Result<
    Vec<(
        Arc<spot_scheduler::Schedule>,
        spot_scheduler::MissedRunPolicy,
    )>,
> {
    let catalogue = crate::apps::catalogue(root, loaded)?;
    let mut schedules = Vec::new();
    for app in &site.apps {
        let Some(source) = catalogue.iter().find(|source| &source.name == app) else {
            continue;
        };
        let manifest = crate::appdir::manifest(&source.dir)?;
        let zone = spot_scheduler::TimeZone::parse("UTC")
            .map_err(|e| crate::Error(format!("{}: {e}", source.dir.display())))?;
        for sub in &manifest.schedules {
            let schedule = spot_scheduler::Schedule::new(sub, zone.clone())
                .map_err(|e| crate::Error(format!("{}: {e}", source.dir.display())))?;
            // `skip` plans nothing for any firing in a tick's window, the on-time one included,
            // so it would never fire. `run_once` plans the latest firing of the window: one run
            // per tick that reaches a firing, and never a burst (the high-water mark is kept in
            // memory, so a restarted node does not replay what it missed while down).
            schedules.push((Arc::new(schedule), spot_scheduler::MissedRunPolicy::RunOnce));
        }
    }
    Ok(schedules)
}

/// The step runner of the site this host serves, resolved when a step runs.
///
/// The host builds the document engine after the job service is assembled, so the engine cannot
/// be held here: the runner reads the site's live handle on every call. While the runtime is
/// being built or swapped there is no engine, and the answer is a retryable `Unavailable`
/// error, which the handler turns into `Retry` (no marker, nothing skipped); it never fails the
/// job for it.
pub struct LiveRunners {
    site: SiteId,
    runner: Arc<LiveRunner>,
}

/// The offline decision for the user a job's invocation acts as (WP-IAM-09.T5): `None` admits.
///
/// A user actor is asked for the user whose rights applied and, when a person acted as that user,
/// for the person too; a platform component (a system job) needs no user; a consumer is not in the
/// offline directory. The refusal names no user and carries only its class.
#[must_use]
pub fn offline_refusal(slot: &OfflineSlot, invocation: &Invocation) -> Option<AppError> {
    slot.admit_actor(Door::Jobs, &invocation.actor, DoorTier::Three, None)
        .err()
        .map(refusal_error)
}

impl LiveRunners {
    /// The runner of `site`, read from its live handle.
    #[must_use]
    pub fn new(site: SiteId, live: Arc<SiteLive>) -> Self {
        Self {
            site,
            runner: Arc::new(LiveRunner {
                live,
                offline: Arc::new(OfflineSlot::default()),
            }),
        }
    }

    /// The same runner with its steps decided by `slot` before they run (the host passes the slot
    /// its finaliser fills; an empty slot admits).
    #[must_use]
    pub fn with_offline(site: SiteId, live: Arc<SiteLive>, slot: Arc<OfflineSlot>) -> Self {
        Self {
            site,
            runner: Arc::new(LiveRunner {
                live,
                offline: slot,
            }),
        }
    }
}

impl RunnerFor for LiveRunners {
    fn runner(&self, site: &SiteId) -> Option<Arc<dyn StepRunner>> {
        (*site == self.site).then(|| Arc::clone(&self.runner) as Arc<dyn StepRunner>)
    }
}

/// The engine of the running runtime, as a committed-steps runner.
struct LiveRunner {
    live: Arc<SiteLive>,
    offline: Arc<OfflineSlot>,
}

fn engine_unavailable() -> AppError {
    AppError::new(
        ErrorClass::Unavailable,
        "committed_delivery.step_unavailable",
        "The document engine is not running yet.",
    )
}

impl CommittedSteps for LiveRunner {
    fn run<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> AppBoxFuture<'a, Vec<Message>> {
        Box::pin(async move { self.run_checked(invocation, step).await.messages })
    }

    fn run_checked<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> AppBoxFuture<'a, StepOutcome> {
        Box::pin(async move {
            // A step acting for a user is decided offline before the engine sees it.
            if let Some(failure) = offline_refusal(&self.offline, invocation) {
                return StepOutcome {
                    messages: Vec::new(),
                    failure: Some(failure),
                };
            }
            match self.live.engine() {
                Some(engine) => engine.runner().run_checked(invocation, step).await,
                None => StepOutcome {
                    messages: Vec::new(),
                    failure: Some(engine_unavailable()),
                },
            }
        })
    }
}

impl StepSource for LiveRunner {
    fn current<'a>(
        &'a self,
        doctype: &'a str,
        id: RecordId,
    ) -> AppBoxFuture<'a, AppResult<Option<Document>>> {
        Box::pin(async move {
            match self.live.engine() {
                Some(engine) => engine.runner().current(doctype, id).await,
                None => Err(engine_unavailable()),
            }
        })
    }
}

/// Registers the `sys:committed_steps` handler on `registry`, with its done markers in the job
/// context row of `store` (the store the worker claims from), so a retry after a crash resumes at
/// the first unmarked step. `InMemoryMarkers` is never used by the host.
pub fn install_committed_steps(
    registry: &mut JobHandlerRegistry,
    runners: Arc<dyn RunnerFor>,
    store: Arc<dyn JobStore>,
) -> Arc<CommittedStepsHandler> {
    let markers = Arc::new(JobStoreMarkers::new(Arc::clone(&store)));
    let handler = Arc::new(CommittedStepsHandler::new(runners, markers, store));
    handler.register(registry);
    handler
}
