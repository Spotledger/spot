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
//! to standby. Stopping releases the lease at once, so a restart does not wait for the time to
//! live.
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
use std::time::Duration;

use spot_coordinator::{Coordinator, Lease};
use spot_core::{Clock, SiteId};
use spot_jobs::model::InsertResult;
use spot_jobs::worker::JobHandlerRegistry;
use spot_jobs::{InMemoryJobStore, JobStore, NewJob, QueuePolicy, Worker};
use spot_postgres::PgBackend;
use spot_scheduler::{
    InMemoryHighWater, InMemoryLeaseStore, MissedRunPolicy, RunSlot, Schedule, Scheduler,
    SchedulerConfig, SlotSink,
};
use spot_storage_contract::ChangeFeed;
use tokio::sync::{mpsc, watch};

use crate::services::{CheckFuture, Level, RunFuture, Service};

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
struct Keeper {
    coordinator: Arc<dyn Coordinator>,
    name: String,
    holder: String,
    ttl: Duration,
    held: std::sync::Mutex<Option<Lease>>,
}

impl Keeper {
    fn new(coordinator: Arc<dyn Coordinator>, name: String, holder: String, ttl: Duration) -> Self {
        Self {
            coordinator,
            name,
            holder,
            ttl,
            held: std::sync::Mutex::new(None),
        }
    }

    fn handle(&self) -> Option<Lease> {
        self.held.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn put(&self, lease: Option<Lease>) {
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) = lease;
    }

    /// Takes the lease if nobody holds it. `false` means another node does, or the
    /// coordinator cannot be reached: either way this node must not consume.
    async fn acquire(&self) -> bool {
        match self
            .coordinator
            .acquire(&self.name, &self.holder, self.ttl)
            .await
        {
            Ok(lease) => {
                self.put(Some(lease));
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
    async fn still_held(&self) -> bool {
        let Some(lease) = self.handle() else {
            return false;
        };
        match self.coordinator.check_fence(&self.name, lease.token).await {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!(lease = %self.name, %e, "fencing token is stale; the lease is lost");
                self.put(None);
                false
            }
        }
    }

    /// Renews the lease. `false` means it is lost.
    async fn renew(&self) -> bool {
        let Some(lease) = self.handle() else {
            return false;
        };
        match self.coordinator.renew(&lease, self.ttl).await {
            Ok(renewed) => {
                self.put(Some(renewed));
                true
            }
            Err(e) => {
                tracing::warn!(lease = %self.name, %e, "lease renewal failed; the lease is lost");
                self.put(None);
                false
            }
        }
    }

    async fn release(&self) {
        let lease = self.held.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(lease) = lease {
            // Releasing lets another node take over at once instead of after the time to
            // live; a failure only means the lease lapses instead.
            let _ = self.coordinator.release(&lease).await;
        }
    }
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
    keeper: Keeper,
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
            keeper: Keeper::new(
                Arc::clone(&settings.coordinator),
                consumer_lease(settings.site.as_str()),
                settings.node.clone(),
                settings.consumer_ttl,
            ),
        })
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
        let worker = Worker::new(
            Arc::clone(&self.store),
            Arc::clone(&self.registry),
            policies,
            {
                let clock = Arc::clone(&self.clock);
                Arc::new(move || clock.now_utc())
            },
        );
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
                        break false;
                    }
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

/// The scheduler: the one node that plans manifest schedules and enqueues their runs.
pub struct Scheduling {
    site: SiteId,
    store: Arc<dyn JobStore>,
    node: String,
    schedules: Vec<(Arc<Schedule>, MissedRunPolicy)>,
    tick_interval: Duration,
    lease_ttl: Duration,
    clock: Arc<dyn Clock>,
    keeper: Keeper,
}

impl Scheduling {
    /// A scheduler service for one site's manifest schedules.
    pub fn new(settings: &Settings) -> Arc<Self> {
        Arc::new(Self {
            site: settings.site.clone(),
            store: Arc::clone(&settings.store),
            node: settings.node.clone(),
            schedules: settings.schedules.clone(),
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
        // lease decides which node leads, this local store only serves the planner.
        let leases = Arc::new(InMemoryLeaseStore::new());
        let (sender, mut receiver) = mpsc::unbounded_channel::<RunSlot>();
        let mut nodes = Vec::new();
        let mut sinks: Vec<Arc<dyn SlotSink>> = Vec::new();
        for (schedule, policy) in &self.schedules {
            nodes.push(Scheduler::new(
                SchedulerConfig {
                    site: self.site.to_string(),
                    node: self.node.clone(),
                    ttl: self.lease_ttl,
                    schedules: vec![(Arc::clone(schedule), *policy)],
                    app: String::new(),
                },
                Arc::clone(&leases),
                InMemoryHighWater::new(),
            ));
            sinks.push(spot_scheduler::scheduler::sink({
                let sender = sender.clone();
                move |slot: &RunSlot| sender.send(slot.clone()).is_ok()
            }));
        }
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
                    while let Ok(slot) = receiver.try_recv() {
                        match spot_jobs::enqueue_scheduled(&slot, &self.site) {
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
            if self.schedules.is_empty() {
                tracing::debug!("no manifest schedules; the scheduler ticks and plans nothing");
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
