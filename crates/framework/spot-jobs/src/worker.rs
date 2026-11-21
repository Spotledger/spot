//! The job worker: claiming, executing, and completing jobs with panic isolation.

use crate::model::{Claim, Job, JobState};
use crate::policy::QueuePolicy;
use crate::store::JobStore;
use crate::Result;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{watch, Semaphore};
use tokio::time::interval;
use tracing::Instrument;

/// A boxed future, the convention used by ports in this crate (see [`JobStore`]).
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A job execution context passed to handlers.
#[derive(Clone, Debug)]
pub struct JobContext {
    /// The job being executed.
    pub job: Job,
    /// The current attempt number (1-indexed).
    pub attempt: u32,
    /// A clock for the handler to use.
    pub clock: OffsetDateTime,
}

impl JobContext {
    /// Build a context for a job. The job's own `max_attempts` is the limit the context reports.
    pub fn new(job: Job, attempt: u32, clock: OffsetDateTime) -> Self {
        Self {
            job,
            attempt,
            clock,
        }
    }

    /// The maximum number of attempts the engine enforces for this job. It is the stored job's
    /// own limit, which the worker sets from the queue policy it enforces, never a limit a
    /// handler-side policy copy supplies.
    pub fn max_attempts(&self) -> u32 {
        self.job.max_attempts
    }

    /// Whether this attempt is the last one the engine will run (`attempt >= max_attempts`).
    pub fn is_last_attempt(&self) -> bool {
        self.attempt >= self.job.max_attempts
    }

    /// The time budget in milliseconds a scheduled job carries in its arguments
    /// (`{"budget_ms": <u64>}`, written by [`enqueue_scheduled`](crate::enqueue_scheduled)), or
    /// `None` for a job without one. A method rather than a field, so existing handlers and
    /// test fixtures that build a context by hand keep compiling.
    pub fn budget_ms(&self) -> Option<u64> {
        job_budget_ms(&self.job)
    }
}

/// The budget in the job's args, when it has a valid one (a non-negative whole number).
fn job_budget_ms(job: &Job) -> Option<u64> {
    job.args
        .get(crate::intake::ARG_BUDGET_MS)
        .and_then(serde_json::Value::as_u64)
}

/// Result of a job handler execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HandlerResult {
    /// Job succeeded.
    Ok,
    /// Retry the job (with the given reason).
    Retry(String),
    /// Permanently fail the job; don't retry.
    Fail(String),
}

/// A job handler: async function that executes a job.
pub type JobHandler = Box<
    dyn Fn(
            JobContext,
        )
            -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HandlerResult>> + Send>>
        + Send
        + Sync,
>;

/// Maps job names to handlers.
pub struct JobHandlerRegistry {
    handlers: std::collections::HashMap<String, Arc<JobHandler>>,
}

impl JobHandlerRegistry {
    /// Create a new registry.
    pub fn new() -> Self {
        Self {
            handlers: std::collections::HashMap::new(),
        }
    }

    /// Register a handler for a job name.
    pub fn register<F>(&mut self, name: impl Into<String>, handler: F)
    where
        F: Fn(
                JobContext,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<HandlerResult>> + Send>>
            + Send
            + Sync
            + 'static,
    {
        self.handlers
            .insert(name.into(), Arc::new(Box::new(handler)));
    }

    /// Get a handler.
    pub fn get(&self, name: &str) -> Option<Arc<JobHandler>> {
        self.handlers.get(name).cloned()
    }
}

impl Default for JobHandlerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// A port the worker calls once when a last attempt ends in a timeout, cancellation or panic
/// caught by the worker, so a handler-side crate can write its bounce. The hook's failure (if
/// any) does not change the job's outcome.
///
/// The hook gets the reason as a code only (`job.timeout`, `job.handler_panicked` or
/// `job.handler_error: handler cancelled`). A hook that panics is caught by the worker and the
/// job still completes as dead.
pub trait FinalAttemptHook: Send + Sync {
    /// Called once before the job is marked dead.
    fn on_dead<'a>(&'a self, job: &'a Job, reason: &'a str) -> BoxFuture<'a, ()>;
}

/// A port the worker asks before every claim: whether this host still holds the right to consume
/// the queues it was asked to serve.
///
/// The store's claim fencing plus the renew/stop loop is the last line of defence, not the first:
/// a lease can be lost without the loop noticing (a long pause, a network partition), and for up
/// to a third of the lease two hosts then act on one job. This gate lets the host veto a claim at
/// the moment of the claim. `true` means the host still holds the right to consume the queues and
/// the worker may claim; `false` means do not claim now.
///
/// The worker asks once per claim attempt, skips that tick on `false` and keeps running: the host
/// decides when to stop the worker, because it may regain the lease. Jobs already running are not
/// affected. A gate that panics is caught by the worker and counted as `false`, the same way a
/// panicking [`FinalAttemptHook`] is caught.
///
/// The worker awaits the answer inline in its claim tick, so `permit` must answer from cached lease
/// state (a flag the host's lease renewer keeps current) and return at once. A gate that blocks on
/// the network or the store would stall claiming and delay shutdown.
///
/// The other half of the contract -- deciding `true` or `false` from the coordinator lease -- is
/// the host's (WP-SEC-12.T5); this crate only asks.
pub trait ClaimGate: Send + Sync {
    /// `true` when this host still holds the right to consume the queues, so a claim may be made.
    fn permit<'a>(&'a self) -> BoxFuture<'a, bool>;
}

/// What the claim gate answered for one claim attempt.
enum GateVerdict {
    /// No gate is attached, or the gate permits a claim on this tick.
    Permitted,
    /// The gate refuses a claim on this tick.
    Refused,
    /// The gate panicked, which counts as a refusal.
    Panicked,
}

/// The job worker: claims and executes jobs.
pub struct Worker {
    store: Arc<dyn JobStore>,
    registry: Arc<JobHandlerRegistry>,
    /// Queue policies indexed by name.
    policies: Arc<std::collections::HashMap<String, QueuePolicy>>,
    /// Clock used by the worker.
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    /// Concurrency limiters per queue.
    semaphores: Arc<std::collections::HashMap<String, Arc<Semaphore>>>,
    /// Optional hook called once on a dead last attempt (timeout/cancellation/panic).
    final_attempt_hook: Option<Arc<dyn FinalAttemptHook>>,
    /// Optional gate asked before every claim; `None` claims on every tick, as before.
    claim_gate: Option<Arc<dyn ClaimGate>>,
}

impl Worker {
    /// Create a new worker with an injected clock.
    pub fn new(
        store: Arc<dyn JobStore>,
        registry: Arc<JobHandlerRegistry>,
        policies: std::collections::HashMap<String, QueuePolicy>,
        clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    ) -> Self {
        // Create semaphores for each queue
        let mut semaphores = std::collections::HashMap::new();
        for (name, policy) in &policies {
            semaphores.insert(
                name.clone(),
                Arc::new(Semaphore::new(policy.concurrency as usize)),
            );
        }

        Self {
            store,
            registry,
            policies: Arc::new(policies),
            clock,
            semaphores: Arc::new(semaphores),
            final_attempt_hook: None,
            claim_gate: None,
        }
    }

    /// Attach an optional final-attempt hook.
    pub fn with_final_attempt_hook(mut self, hook: Arc<dyn FinalAttemptHook>) -> Self {
        self.final_attempt_hook = Some(hook);
        self
    }

    /// Attach an optional [`ClaimGate`], asked before every claim. Without one the worker keeps
    /// claiming on every tick, exactly as before.
    pub fn with_claim_gate(mut self, gate: Arc<dyn ClaimGate>) -> Self {
        self.claim_gate = Some(gate);
        self
    }

    /// Ask the claim gate, when one is attached, whether this tick may claim.
    ///
    /// The gate runs in its own task so a panic is caught as a `JoinError` -- the same approach
    /// the final-attempt hook uses -- and counts as a refusal.
    async fn ask_claim_gate(&self) -> GateVerdict {
        let Some(gate) = self.claim_gate.clone() else {
            return GateVerdict::Permitted;
        };
        match tokio::spawn(async move { gate.permit().await }).await {
            Ok(true) => GateVerdict::Permitted,
            Ok(false) => GateVerdict::Refused,
            Err(err) if err.is_panic() => GateVerdict::Panicked,
            // The task was aborted from outside; it never answered, so do not claim.
            Err(_) => GateVerdict::Refused,
        }
    }

    /// Run the worker loop with bounded concurrency per queue.
    ///
    /// The worker will claim and execute jobs until the shutdown signal is set.
    /// Panic isolation ensures a handler panic or timeout fails that attempt, never the worker.
    pub async fn run(
        &self,
        worker_id: &str,
        queues: Vec<String>,
        shutdown: watch::Receiver<bool>,
        grace_period: Duration,
    ) -> Result<()> {
        let mut shutdown = shutdown;
        let mut claim_interval = interval(Duration::from_millis(100));
        let mut in_flight = tokio::task::JoinSet::new();
        // Whether the claim gate last permitted claims. A refusal is logged once when it starts
        // and a recovery once when it ends, not on every 100 ms tick.
        let mut claim_permitted = true;

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    // Shutdown signal received
                    // Wait for in-flight jobs within grace period
                    let start = (self.clock)();
                    while !in_flight.is_empty() {
                        let elapsed = ((self.clock)() - start).unsigned_abs();
                        if elapsed > grace_period {
                            // Grace period expired; abandon remaining
                            break;
                        }
                        let _ = in_flight.join_next().await;
                    }
                    break;
                }
                _ = claim_interval.tick() => {
                    // Ask the host first: when it no longer holds the right to consume the
                    // queues, skip this tick's claim and stay running. The host decides when to
                    // stop the worker, because it may regain the lease; jobs already running are
                    // untouched. A panicking gate counts as a refusal (and is said so once).
                    let verdict = self.ask_claim_gate().await;
                    if !matches!(verdict, GateVerdict::Permitted) {
                        if claim_permitted {
                            claim_permitted = false;
                            match verdict {
                                GateVerdict::Panicked => tracing::warn!(
                                    worker = %worker_id,
                                    "claim gate panicked; treating it as a refusal and skipping claims until it permits again"
                                ),
                                _ => tracing::warn!(
                                    worker = %worker_id,
                                    "claim gate refused; this host no longer holds the right to consume the queues, skipping claims until it permits again"
                                ),
                            }
                        }
                        continue;
                    }
                    if !claim_permitted {
                        claim_permitted = true;
                        tracing::info!(
                            worker = %worker_id,
                            "claim gate permits claims again; resuming claims"
                        );
                    }

                    // Try to claim more jobs
                    if let Ok(claimed) = self.store.claim(
                        &queues,
                        worker_id,
                        Duration::from_secs(30),
                        (self.clock)(),
                        10,
                    ).await {
                        for claimed_job in claimed {
                            let store = self.store.clone();
                            let registry = self.registry.clone();
                            let policies = self.policies.clone();
                            let clock = self.clock.clone();
                            let semaphores = self.semaphores.clone();
                            let hook = self.final_attempt_hook.clone();
                            let job = claimed_job.job.clone();
                            let claim = claimed_job.claim.clone();

                            in_flight.spawn(async move {
                                Self::execute_job_with_hook(
                                    store,
                                    registry,
                                    policies,
                                    clock,
                                    semaphores,
                                    job,
                                    claim,
                                    hook,
                                )
                                .await
                            });
                        }
                    }
                }
                _ = in_flight.join_next(), if !in_flight.is_empty() => {
                    // A job finished; the loop continues to claim more
                }
            }
        }

        Ok(())
    }

    /// Execute a job with heartbeat and timeout.
    ///
    /// This method executes a single job, managing its lifecycle including heartbeat renewal,
    /// timeout enforcement, and result mapping to job states.
    pub async fn execute_job(
        store: Arc<dyn JobStore>,
        registry: Arc<JobHandlerRegistry>,
        policies: Arc<std::collections::HashMap<String, QueuePolicy>>,
        clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
        semaphores: Arc<std::collections::HashMap<String, Arc<Semaphore>>>,
        job: Job,
        claim: Claim,
    ) -> Result<()> {
        Self::execute_job_with_hook(
            store, registry, policies, clock, semaphores, job, claim, None,
        )
        .await
    }

    /// [`execute_job`](Self::execute_job) with an optional [`FinalAttemptHook`], called once when
    /// the last attempt ends in a timeout, cancellation or panic the worker caught.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute_job_with_hook(
        store: Arc<dyn JobStore>,
        registry: Arc<JobHandlerRegistry>,
        policies: Arc<std::collections::HashMap<String, QueuePolicy>>,
        clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
        semaphores: Arc<std::collections::HashMap<String, Arc<Semaphore>>>,
        job: Job,
        claim: Claim,
        final_attempt_hook: Option<Arc<dyn FinalAttemptHook>>,
    ) -> Result<()> {
        // Get the policy for this queue
        let policy = policies.get(&job.queue).cloned();
        let policy = policy.unwrap_or_else(QueuePolicy::default);

        // Acquire semaphore permit for concurrency control
        let semaphore = semaphores
            .get(&job.queue)
            .cloned()
            .unwrap_or_else(|| Arc::new(Semaphore::new(policy.concurrency as usize)));
        let _permit = semaphore.acquire().await.ok();

        // The engine enforces the queue policy's max_attempts for this job; stamp it onto the
        // job so the context and the handler see the engine's own limit (never a handler-side
        // policy copy).
        let mut job = job;
        job.max_attempts = policy.max_attempts;

        let now = clock();
        let ctx = JobContext::new(job.clone(), job.attempt, now);

        // Get the handler
        let Some(handler) = registry.get(&job.name) else {
            let _ = store
                .complete(
                    &claim,
                    JobState::Dead,
                    Some("job.handler_not_found".to_string()),
                )
                .await;
            return Ok(());
        };

        // Execute with panic isolation and timeout
        // A scheduled job's own budget can only shorten the queue's timeout, never extend it. A
        // run cut short by its budget is a timeout *failure that is retried*; a run cut short
        // by the queue's own timeout keeps its existing outcome.
        let budget = job_budget_ms(&job).map(Duration::from_millis);
        let (timeout_duration, budget_applies) = match budget {
            Some(b) if b <= policy.timeout => (b, true),
            _ => (policy.timeout, false),
        };
        // One span per attempt, covering the handler run: its name, queue and attempt number,
        // never its arguments (they may hold tenant data). The cost scope follows the handler
        // across awaits, so storage and cache work it does lands on this attempt.
        let span = tracing::info_span!(
            "job.run",
            job = %job.name,
            queue = %job.queue,
            attempt = job.attempt,
            db_statements = tracing::field::Empty,
            cache_hits = tracing::field::Empty,
            cache_misses = tracing::field::Empty,
            outbox_events = tracing::field::Empty,
        );
        let is_last_attempt = ctx.is_last_attempt();
        let (result, cost) = spot_telemetry::cost::scope(
            Self::execute_with_heartbeat(
                store.clone(),
                claim.clone(),
                handler,
                ctx,
                timeout_duration,
                clock.clone(),
            )
            .instrument(span.clone()),
        )
        .await;
        span.record("db_statements", cost.db_statements);
        span.record("cache_hits", cost.cache_hits);
        span.record("cache_misses", cost.cache_misses);
        span.record("outbox_events", cost.outbox_events);

        // Map the result
        let mut worker_caught = false;
        let (new_state, error_msg) = match result {
            Ok(HandlerResult::Ok) => (JobState::Succeeded, None),
            Err(HandlerError::Timeout) if budget_applies => {
                worker_caught = true;
                Self::retry_or_dead(&policy, &job, "job.timeout".to_string(), clock())
            }
            Ok(HandlerResult::Retry(reason)) => Self::retry_or_dead(&policy, &job, reason, clock()),
            Ok(HandlerResult::Fail(reason)) => (JobState::Dead, Some(reason)),
            Err(HandlerError::Panicked) => {
                worker_caught = true;
                (JobState::Dead, Some("job.handler_panicked".to_string()))
            }
            Err(HandlerError::Timeout) => {
                worker_caught = true;
                (JobState::Dead, Some("job.timeout".to_string()))
            }
            Err(HandlerError::Cancelled) => {
                worker_caught = true;
                (
                    JobState::Dead,
                    Some("job.handler_error: handler cancelled".to_string()),
                )
            }
            Err(HandlerError::Other(reason)) => (
                JobState::Dead,
                Some(format!("job.handler_error: {}", reason)),
            ),
        };

        // Final-attempt hook: when a worker-caught failure (timeout/cancellation/panic) lands
        // on the last attempt, notify once before dead-lettering. Hook failures are ignored.
        if is_last_attempt && worker_caught && matches!(new_state, JobState::Dead) {
            if let Some(hook) = final_attempt_hook {
                let reason = error_msg.clone().unwrap_or_default();
                let hook_job = job.clone();
                // Own task: a panicking hook is caught as a `JoinError` and the job still
                // completes. The result is ignored on purpose.
                let _ = tokio::spawn(async move { hook.on_dead(&hook_job, &reason).await }).await;
            }
        }

        let _ = store.complete(&claim, new_state, error_msg).await;

        Ok(())
    }

    /// The state after a failed attempt that may be retried: `Retrying` with the queue's
    /// backoff while attempts remain, `Dead` once they are used up.
    fn retry_or_dead(
        policy: &QueuePolicy,
        job: &Job,
        reason: String,
        now: OffsetDateTime,
    ) -> (JobState, Option<String>) {
        // `next_attempt` is 1-indexed, as the backoff expects.
        let next_attempt = job.attempt + 1;
        // The engine enforces the job's own max_attempts (stamped from the queue policy).
        if next_attempt > job.max_attempts {
            (JobState::Dead, Some(reason))
        } else {
            let retry_at = policy
                .backoff
                .delay_for_attempt(next_attempt, job.id.0, now);
            (JobState::Retrying { at: retry_at }, Some(reason))
        }
    }

    async fn execute_with_heartbeat(
        store: Arc<dyn JobStore>,
        claim: Claim,
        handler: Arc<JobHandler>,
        ctx: JobContext,
        timeout_duration: Duration,
        clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    ) -> std::result::Result<HandlerResult, HandlerError> {
        // Spawn a heartbeat task
        let store_for_heartbeat = store.clone();
        let claim_for_heartbeat = claim.clone();
        let clock_for_heartbeat = clock.clone();
        let mut heartbeat_interval = interval(Duration::from_secs(5));
        let _heartbeat_guard = AbortOnDrop(tokio::spawn(async move {
            loop {
                heartbeat_interval.tick().await;
                let _ = store_for_heartbeat
                    .heartbeat(
                        &claim_for_heartbeat,
                        Duration::from_secs(30),
                        clock_for_heartbeat(),
                    )
                    .await;
            }
        }));

        // Run the handler in its own task: a panic is caught as a `JoinError` and fails only
        // this attempt. The guard aborts the task if this future is dropped (cancellation
        // safety, charter rule 13), so a dropped worker never leaves a handler running.
        let mut handler_task = AbortOnDrop(tokio::spawn(handler(ctx)));
        let result = tokio::select! {
            joined = &mut handler_task.0 => map_join(joined),
            _ = tokio::time::sleep(timeout_duration) => Err(HandlerError::Timeout),
        };

        result
    }
}

/// Maps how the handler task ended to the worker's outcome: a panic and an abort from outside
/// are told apart by type, never by message.
fn map_join(
    joined: std::result::Result<Result<HandlerResult>, tokio::task::JoinError>,
) -> std::result::Result<HandlerResult, HandlerError> {
    match joined {
        Ok(Ok(handler_res)) => Ok(handler_res),
        Ok(Err(_)) => Err(HandlerError::Other("handler error".to_string())),
        Err(e) if e.is_panic() => Err(HandlerError::Panicked),
        Err(_) => Err(HandlerError::Cancelled),
    }
}

/// Aborts the wrapped task when dropped.
struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Error from handler execution.
enum HandlerError {
    Panicked,
    Timeout,
    /// The handler task was aborted from outside before it finished.
    Cancelled,
    Other(String),
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::{InsertResult, JobId, NewJob};
    use crate::store::InMemoryJobStore;
    use serde_json::json;
    use spot_core::SiteId;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc as StdArc;
    use std::time::Duration as StdDuration;
    use time::OffsetDateTime;

    #[test]
    fn test_handler_registry() {
        let mut registry = JobHandlerRegistry::new();

        registry.register("app:test", |_ctx| {
            Box::pin(async move { Ok::<_, crate::error::Error>(HandlerResult::Ok) })
        });

        assert!(registry.get("app:test").is_some());
        assert!(registry.get("app:other").is_none());
    }

    #[tokio::test]
    async fn shutdown_drains_inflight_within_grace_and_leaves_rest_pending() {
        let store = Arc::new(InMemoryJobStore::new());
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        // Insert multiple jobs (more than can execute in the grace period)
        for i in 0..20 {
            let job = NewJob::new(
                "app:work".to_string(),
                json!({ "id": i }),
                "default".to_string(),
                site.clone(),
                now,
            );
            let _ = store.insert_if_new(job).await.unwrap();
        }

        // Create a handler that takes time
        let mut registry = JobHandlerRegistry::new();
        let execution_count = StdArc::new(AtomicU32::new(0));
        let exec_count_clone = execution_count.clone();
        registry.register("app:work", move |_ctx| {
            let count = exec_count_clone.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(StdDuration::from_millis(500)).await;
                Ok::<_, crate::error::Error>(HandlerResult::Ok)
            })
        });

        let registry = Arc::new(registry);
        let mut policies = std::collections::HashMap::new();
        // Use lower concurrency to ensure not all jobs are claimed at once
        policies.insert(
            "default".to_string(),
            QueuePolicy::custom("default").with_concurrency(1).build(),
        );

        let clock = Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime);
        let worker = Arc::new(Worker::new(
            store.clone(),
            registry.clone(),
            policies,
            clock.clone(),
        ));

        // Create shutdown signal
        let (tx, rx) = watch::channel(false);

        // Run worker in a task
        let worker_handle = tokio::spawn({
            let worker = worker.clone();
            async move {
                let _ = worker
                    .run(
                        "test-worker",
                        vec!["default".to_string()],
                        rx,
                        StdDuration::from_secs(2),
                    )
                    .await;
            }
        });

        // Let it process for a bit (less than 500ms so first job is still running)
        tokio::time::sleep(StdDuration::from_millis(300)).await;

        // Send shutdown
        let _ = tx.send(true);

        // Wait for worker to shut down
        let _ = tokio::time::timeout(StdDuration::from_secs(5), worker_handle).await;

        // Verify some jobs were executed
        let count = execution_count.load(Ordering::SeqCst);
        assert!(count > 0, "At least one job should be executed");
        assert!(
            count < 20,
            "Not all jobs should be executed within grace period, but got {}",
            count
        );

        // Verify remaining jobs are still pending
        let all_jobs = store
            .list(crate::store::JobFilter {
                site: Some(site),
                ..Default::default()
            })
            .await
            .unwrap();
        let pending = all_jobs
            .iter()
            .filter(|j| j.state == JobState::Pending)
            .count();
        let succeeded = all_jobs
            .iter()
            .filter(|j| j.state == JobState::Succeeded)
            .count();
        let running = all_jobs
            .iter()
            .filter(|j| j.state == JobState::Running)
            .count();
        let retrying = all_jobs
            .iter()
            .filter(|j| matches!(j.state, JobState::Retrying { .. }))
            .count();
        assert!(
            pending > 0 || running > 0,
            "Some jobs should remain pending or running after shutdown. Succeeded: {}, Pending: {}, Running: {}, Retrying: {}",
            succeeded,
            pending,
            running,
            retrying
        );
    }

    /// Rule 13: dropping the worker future at any await leaves the job either properly
    /// completed or held by a lease that will lapse and be reaped (never lost, never stuck).
    #[tokio::test(start_paused = true)]
    async fn cancel_at_every_await_leaves_job_completed_or_lease_lapsing() {
        use std::future::Future;
        use std::task::{Context, Poll};

        let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = Arc::new(move || now);
        let waker = futures_util::task::noop_waker();
        let mut ran_to_completion = false;

        for cancel_after in 0..40usize {
            let store = Arc::new(InMemoryJobStore::with_clock(clock.clone()));
            let job = NewJob::new(
                "app:cancellable".to_string(),
                json!({}),
                "default".to_string(),
                SiteId::new("test-site").unwrap(),
                now,
            );
            let InsertResult::Inserted(job_id) = store.insert_if_new(job).await.unwrap() else {
                panic!("job not inserted");
            };
            let claimed = store
                .claim(
                    &["default".to_string()],
                    "w1",
                    StdDuration::from_secs(30),
                    now,
                    1,
                )
                .await
                .unwrap()
                .remove(0);

            let mut registry = JobHandlerRegistry::new();
            registry.register("app:cancellable", |_ctx| {
                Box::pin(async move {
                    for _ in 0..3 {
                        tokio::task::yield_now().await;
                    }
                    Ok::<_, crate::error::Error>(HandlerResult::Ok)
                })
            });
            let mut policies = std::collections::HashMap::new();
            policies.insert("default".to_string(), QueuePolicy::default());
            let mut semaphores = std::collections::HashMap::new();
            semaphores.insert("default".to_string(), Arc::new(Semaphore::new(1)));

            let mut fut = Box::pin(Worker::execute_job(
                store.clone(),
                Arc::new(registry),
                Arc::new(policies),
                clock.clone(),
                Arc::new(semaphores),
                claimed.job.clone(),
                claimed.claim.clone(),
            ));
            let mut finished = false;
            for _ in 0..cancel_after {
                let mut cx = Context::from_waker(&waker);
                if let Poll::Ready(r) = fut.as_mut().poll(&mut cx) {
                    r.unwrap();
                    finished = true;
                    break;
                }
                // Let the spawned handler and heartbeat tasks make progress.
                tokio::task::yield_now().await;
            }
            drop(fut);
            // Aborted tasks need a turn to unwind.
            for _ in 0..4 {
                tokio::task::yield_now().await;
            }

            let after = store.get(job_id).await.unwrap().unwrap();
            match after.state {
                JobState::Succeeded | JobState::Dead | JobState::Retrying { .. } => {}
                JobState::Running => {
                    // Still held: the lease must lapse and the reaper must hand it back.
                    let reaped = store
                        .reap_expired(now + time::Duration::hours(1))
                        .await
                        .unwrap();
                    assert_eq!(reaped, vec![job_id], "cancel after {cancel_after} polls");
                    let again = store.get(job_id).await.unwrap().unwrap();
                    assert!(
                        matches!(again.state, JobState::Retrying { .. }),
                        "cancel after {cancel_after}: {:?}",
                        again.state
                    );
                }
                other => panic!("cancel after {cancel_after} polls left {other:?}"),
            }
            ran_to_completion |= finished;
        }
        assert!(
            ran_to_completion,
            "the scripted run must also finish when not cancelled"
        );
    }

    #[tokio::test]
    async fn handler_retry_uses_backoff() {
        let store = Arc::new(InMemoryJobStore::new());
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:retry".to_string(),
            json!({}),
            "default".to_string(),
            site,
            now,
        );

        let job_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };

        let mut registry = JobHandlerRegistry::new();
        registry.register("app:retry", |_ctx| {
            Box::pin(async move {
                Ok::<_, crate::error::Error>(HandlerResult::Retry("test retry".to_string()))
            })
        });

        let registry = Arc::new(registry);
        let mut policies = std::collections::HashMap::new();
        policies.insert("default".to_string(), QueuePolicy::default());

        let clock = Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime);

        // Claim and execute
        let claimed = store
            .claim(
                &["default".to_string()],
                "test-worker",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();

        assert_eq!(claimed.len(), 1);
        let claim = claimed[0].claim.clone();
        let claimed_job = claimed[0].job.clone();

        Worker::execute_job(
            store.clone(),
            registry.clone(),
            Arc::new(std::collections::HashMap::from([(
                "default".to_string(),
                QueuePolicy::default(),
            )])),
            clock.clone(),
            Arc::new(std::collections::HashMap::new()),
            claimed_job,
            claim,
        )
        .await
        .unwrap();

        let job = store.get(job_id).await.unwrap().unwrap();
        assert!(
            matches!(job.state, JobState::Retrying { .. }),
            "Job should be in Retrying state"
        );
        if let JobState::Retrying { at } = job.state {
            let delay = at - now;
            assert!(delay.whole_seconds() > 0, "Retry should be in the future");
        }
    }

    #[tokio::test]
    async fn handler_fail_goes_dead() {
        let store = Arc::new(InMemoryJobStore::new());
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:fail".to_string(),
            json!({}),
            "default".to_string(),
            site,
            now,
        );

        let job_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };

        let mut registry = JobHandlerRegistry::new();
        registry.register("app:fail", |_ctx| {
            Box::pin(async move {
                Ok::<_, crate::error::Error>(HandlerResult::Fail("permanent failure".to_string()))
            })
        });

        let registry = Arc::new(registry);
        let mut policies = std::collections::HashMap::new();
        policies.insert("default".to_string(), QueuePolicy::default());

        let clock = Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime);

        // Claim and execute
        let claimed = store
            .claim(
                &["default".to_string()],
                "test-worker",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();

        assert_eq!(claimed.len(), 1);
        let claim = claimed[0].claim.clone();
        let claimed_job = claimed[0].job.clone();

        Worker::execute_job(
            store.clone(),
            registry.clone(),
            Arc::new(std::collections::HashMap::from([(
                "default".to_string(),
                QueuePolicy::default(),
            )])),
            clock.clone(),
            Arc::new(std::collections::HashMap::new()),
            claimed_job,
            claim,
        )
        .await
        .unwrap();

        let job = store.get(job_id).await.unwrap().unwrap();
        assert!(
            matches!(job.state, JobState::Dead),
            "Job should be in Dead state"
        );
        assert!(job.last_error.is_some(), "Job should have an error message");
    }

    #[tokio::test]
    async fn lease_heartbeat_extends_during_long_handler() {
        let store = Arc::new(InMemoryJobStore::new());
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:slow".to_string(),
            json!({}),
            "default".to_string(),
            site,
            now,
        );

        let job_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };

        let mut registry = JobHandlerRegistry::new();
        registry.register("app:slow", |_ctx| {
            Box::pin(async move {
                tokio::time::sleep(StdDuration::from_millis(200)).await;
                Ok::<_, crate::error::Error>(HandlerResult::Ok)
            })
        });

        let registry = Arc::new(registry);
        let mut policies = std::collections::HashMap::new();
        // Set a longer timeout so the job completes
        policies.insert(
            "default".to_string(),
            QueuePolicy::custom("default")
                .with_timeout(StdDuration::from_secs(5))
                .build(),
        );

        let clock = Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime);

        // Claim and execute
        let claimed = store
            .claim(
                &["default".to_string()],
                "test-worker",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();

        assert_eq!(claimed.len(), 1);
        let original_claim = claimed[0].claim.clone();
        let claimed_job = claimed[0].job.clone();

        let policies_map = Arc::new(std::collections::HashMap::from([(
            "default".to_string(),
            QueuePolicy::custom("default")
                .with_timeout(StdDuration::from_secs(5))
                .build(),
        )]));

        Worker::execute_job(
            store.clone(),
            registry.clone(),
            policies_map,
            clock.clone(),
            Arc::new(std::collections::HashMap::new()),
            claimed_job,
            original_claim.clone(),
        )
        .await
        .unwrap();

        let job = store.get(job_id).await.unwrap().unwrap();
        assert!(
            matches!(job.state, JobState::Succeeded),
            "Job should have succeeded"
        );
    }

    /// Runs one claimed job whose handler sleeps `handler_ms` against a queue with `timeout`
    /// and `max_attempts`; returns the job after the attempt.
    async fn run_sleeper(
        args: serde_json::Value,
        handler_ms: u64,
        timeout: StdDuration,
        max_attempts: u32,
        attempt: u32,
    ) -> Job {
        let store = Arc::new(InMemoryJobStore::new());
        let now = OffsetDateTime::now_utc();
        let job = NewJob::new(
            "app:sched".to_string(),
            args,
            "default".to_string(),
            SiteId::new("test-site").unwrap(),
            now,
        );
        let job_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };
        let mut registry = JobHandlerRegistry::new();
        registry.register("app:sched", move |_ctx| {
            Box::pin(async move {
                tokio::time::sleep(StdDuration::from_millis(handler_ms)).await;
                Ok::<_, crate::error::Error>(HandlerResult::Ok)
            })
        });
        let policy = QueuePolicy::custom("default")
            .with_timeout(timeout)
            .with_max_attempts(max_attempts)
            .build();
        let claimed = store
            .claim(
                &["default".to_string()],
                "test-worker",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();
        Worker::execute_job(
            store.clone(),
            Arc::new(registry),
            Arc::new(std::collections::HashMap::from([(
                "default".to_string(),
                policy,
            )])),
            Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime),
            Arc::new(std::collections::HashMap::new()),
            Job {
                attempt,
                ..claimed[0].job.clone()
            },
            claimed[0].claim.clone(),
        )
        .await
        .unwrap();
        store.get(job_id).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_handler_past_its_budget_is_timed_out_and_retried_per_policy() {
        let job = run_sleeper(
            json!({"step": "app:s", "budget_ms": 30}),
            2_000,
            StdDuration::from_secs(10),
            3,
            1,
        )
        .await;
        assert!(
            matches!(job.state, JobState::Retrying { .. }),
            "{:?}",
            job.state
        );
        assert_eq!(job.last_error.as_deref(), Some("job.timeout"));
    }

    #[tokio::test]
    async fn a_budget_timeout_on_the_last_attempt_dead_letters() {
        // attempt 1 of max 1: no retry left.
        let job = run_sleeper(
            json!({"step": "app:s", "budget_ms": 30}),
            2_000,
            StdDuration::from_secs(10),
            1,
            1,
        )
        .await;
        assert!(matches!(job.state, JobState::Dead), "{:?}", job.state);
        assert_eq!(job.last_error.as_deref(), Some("job.timeout"));
    }

    #[tokio::test]
    async fn a_handler_inside_its_budget_succeeds() {
        let job = run_sleeper(
            json!({"step": "app:s", "budget_ms": 5_000}),
            10,
            StdDuration::from_secs(10),
            3,
            1,
        )
        .await;
        assert!(matches!(job.state, JobState::Succeeded), "{:?}", job.state);
    }

    #[tokio::test]
    async fn a_budget_never_extends_the_queue_timeout() {
        // Queue timeout 50 ms, budget 1 hour: the queue timeout still ends the run, with its
        // existing (dead) outcome.
        let job = run_sleeper(
            json!({"step": "app:s", "budget_ms": 3_600_000}),
            2_000,
            StdDuration::from_millis(50),
            3,
            1,
        )
        .await;
        assert!(matches!(job.state, JobState::Dead), "{:?}", job.state);
    }

    #[tokio::test]
    async fn a_job_without_a_budget_keeps_the_queue_timeout() {
        let job = run_sleeper(json!(null), 2_000, StdDuration::from_millis(50), 3, 1).await;
        assert!(matches!(job.state, JobState::Dead), "{:?}", job.state);
    }

    /// A hook that counts its calls and records the reason it was given.
    struct CountingHook {
        calls: AtomicU32,
        reason: std::sync::Mutex<Option<String>>,
        panics: bool,
    }

    impl CountingHook {
        fn new(panics: bool) -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicU32::new(0),
                reason: std::sync::Mutex::new(None),
                panics,
            })
        }
    }

    impl FinalAttemptHook for CountingHook {
        fn on_dead<'a>(&'a self, _job: &'a Job, reason: &'a str) -> BoxFuture<'a, ()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.reason.lock().unwrap() = Some(reason.to_string());
            let panics = self.panics;
            Box::pin(async move {
                if panics {
                    panic!("hook boom");
                }
            })
        }
    }

    /// What a hooked run of one claimed job looked like from outside.
    struct HookedRun {
        job: Job,
        /// `(ctx.max_attempts(), ctx.is_last_attempt())` as the handler saw them, if it ran.
        seen: Option<(u32, bool)>,
    }

    /// Runs one claimed job on a queue whose policy has `policy_max` attempts, with the job
    /// itself carrying `job_max` and being on `attempt`. `handler` is `"panic"`, `"hang"`
    /// (outlives the queue timeout) or `"ok"`; the handler records what its context says.
    async fn run_hooked(
        handler: &'static str,
        policy_max: u32,
        job_max: u32,
        attempt: u32,
        hook: Option<Arc<dyn FinalAttemptHook>>,
    ) -> HookedRun {
        let store = Arc::new(InMemoryJobStore::new());
        let now = OffsetDateTime::now_utc();
        let new_job = NewJob::new(
            "app:hooked".to_string(),
            json!({}),
            "default".to_string(),
            SiteId::new("test-site").unwrap(),
            now,
        );
        let job_id = match store.insert_if_new(new_job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("not inserted"),
        };
        let seen = StdArc::new(std::sync::Mutex::new(None));
        let seen_in_handler = seen.clone();
        let mut registry = JobHandlerRegistry::new();
        registry.register("app:hooked", move |ctx| {
            *seen_in_handler.lock().unwrap() = Some((ctx.max_attempts(), ctx.is_last_attempt()));
            Box::pin(async move {
                match handler {
                    "panic" => panic!("boom"),
                    "hang" => {
                        tokio::time::sleep(StdDuration::from_secs(60)).await;
                        Ok::<_, crate::error::Error>(HandlerResult::Ok)
                    }
                    _ => Ok(HandlerResult::Ok),
                }
            })
        });
        let policy = QueuePolicy::custom("default")
            .with_timeout(if handler == "hang" {
                StdDuration::from_millis(50)
            } else {
                StdDuration::from_secs(10)
            })
            .with_max_attempts(policy_max)
            .build();
        let claimed = store
            .claim(
                &["default".to_string()],
                "w",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();
        let claimed_job = Job {
            attempt,
            max_attempts: job_max,
            ..claimed[0].job.clone()
        };
        Worker::execute_job_with_hook(
            store.clone(),
            Arc::new(registry),
            Arc::new(std::collections::HashMap::from([(
                "default".to_string(),
                policy,
            )])),
            Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime),
            Arc::new(std::collections::HashMap::new()),
            claimed_job,
            claimed[0].claim.clone(),
            hook,
        )
        .await
        .unwrap();
        let job = store.get(job_id).await.unwrap().unwrap();
        let seen = *seen.lock().unwrap();
        HookedRun { job, seen }
    }

    #[tokio::test]
    async fn the_hook_fires_once_on_a_last_attempt_panic_and_the_job_is_dead() {
        let hook = CountingHook::new(false);
        let run = run_hooked("panic", 1, 1, 1, Some(hook.clone())).await;
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            hook.reason.lock().unwrap().as_deref(),
            Some("job.handler_panicked")
        );
        assert!(
            matches!(run.job.state, JobState::Dead),
            "{:?}",
            run.job.state
        );
    }

    #[tokio::test]
    async fn the_hook_fires_once_on_a_last_attempt_timeout() {
        let hook = CountingHook::new(false);
        let run = run_hooked("hang", 2, 2, 2, Some(hook.clone())).await;
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
        assert_eq!(hook.reason.lock().unwrap().as_deref(), Some("job.timeout"));
        assert!(
            matches!(run.job.state, JobState::Dead),
            "{:?}",
            run.job.state
        );
    }

    #[tokio::test]
    async fn an_aborted_handler_task_maps_to_cancelled_and_a_panic_to_panicked() {
        let aborted = tokio::spawn(async {
            std::future::pending::<()>().await;
            Ok(HandlerResult::Ok)
        });
        aborted.abort();
        assert!(matches!(
            map_join(aborted.await),
            Err(HandlerError::Cancelled)
        ));
        let panicked = tokio::spawn(async {
            panic!("boom");
            #[allow(unreachable_code)]
            Ok(HandlerResult::Ok)
        });
        assert!(matches!(
            map_join(panicked.await),
            Err(HandlerError::Panicked)
        ));
    }

    #[tokio::test]
    async fn the_hook_does_not_fire_on_an_earlier_attempt() {
        for handler in ["panic", "hang"] {
            let hook = CountingHook::new(false);
            // Attempt 1 of 2: a hang is a dead timeout on the queue timeout, but it is not
            // the last attempt, so no bounce is written.
            let run = run_hooked(handler, 2, 2, 1, Some(hook.clone())).await;
            assert_eq!(hook.calls.load(Ordering::SeqCst), 0, "{handler}");
            assert!(matches!(run.job.state, JobState::Dead), "{handler}");
        }
    }

    #[tokio::test]
    async fn the_hook_does_not_fire_on_success() {
        let hook = CountingHook::new(false);
        let run = run_hooked("ok", 1, 1, 1, Some(hook.clone())).await;
        assert_eq!(hook.calls.load(Ordering::SeqCst), 0);
        assert!(matches!(run.job.state, JobState::Succeeded));
    }

    #[tokio::test]
    async fn a_panicking_hook_does_not_leave_the_job_claimed() {
        let hook = CountingHook::new(true);
        let run = run_hooked("panic", 1, 1, 1, Some(hook.clone())).await;
        assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
        assert!(
            matches!(run.job.state, JobState::Dead),
            "{:?}",
            run.job.state
        );
    }

    #[tokio::test]
    async fn the_handler_sees_the_engines_limit_not_the_jobs_stored_default() {
        // The job says 3, the queue policy (what the engine enforces) says 1.
        let run = run_hooked("ok", 1, 3, 1, None).await;
        assert_eq!(run.seen, Some((1, true)));
        // And the other way round: the policy says 5, the job says 1.
        let run = run_hooked("ok", 5, 1, 1, None).await;
        assert_eq!(run.seen, Some((5, false)));
    }

    #[tokio::test]
    async fn the_handler_flag_is_true_exactly_on_the_final_attempt() {
        for max in [1u32, 2, 5] {
            for attempt in 1..=max {
                let run = run_hooked("ok", max, 3, attempt, None).await;
                assert_eq!(
                    run.seen,
                    Some((max, attempt == max)),
                    "max {max} attempt {attempt}"
                );
            }
        }
    }

    #[tokio::test]
    async fn is_last_attempt_follows_the_jobs_own_limit() {
        let now = OffsetDateTime::UNIX_EPOCH;
        for max in [1u32, 2, 5] {
            for attempt in 1..=max + 1 {
                let job = Job {
                    id: JobId(1),
                    site: SiteId::new("test-site").unwrap(),
                    name: "app:x".to_string(),
                    args: json!(null),
                    queue: "default".to_string(),
                    state: JobState::Pending,
                    attempt,
                    max_attempts: max,
                    not_before: now,
                    idempotency_key: None,
                    claim: None,
                    last_error: None,
                    created_at: now,
                    context: json!(null),
                };
                let ctx = JobContext::new(job, attempt, now);
                assert_eq!(ctx.max_attempts(), max);
                assert_eq!(
                    ctx.is_last_attempt(),
                    attempt >= max,
                    "max {max} attempt {attempt}"
                );
            }
        }
    }

    #[test]
    fn the_context_reads_the_budget_from_the_args() {
        let make = |args: serde_json::Value| {
            let now = OffsetDateTime::UNIX_EPOCH;
            let job = crate::model::Job {
                id: crate::model::JobId(1),
                site: SiteId::new("test-site").unwrap(),
                name: "app:sched".to_string(),
                args,
                queue: "default".to_string(),
                state: JobState::Pending,
                attempt: 1,
                max_attempts: 3,
                not_before: now,
                idempotency_key: None,
                claim: None,
                last_error: None,
                created_at: now,
                context: json!(null),
            };
            JobContext {
                attempt: 1,
                job,
                clock: now,
            }
        };
        assert_eq!(make(json!({"budget_ms": 750})).budget_ms(), Some(750));
        assert_eq!(make(json!(null)).budget_ms(), None);
        assert_eq!(make(json!({"budget_ms": -1})).budget_ms(), None);
        assert_eq!(make(json!({"budget_ms": "x"})).budget_ms(), None);
    }
}
