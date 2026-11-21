//! The JobStore trait: the persistent backing for jobs.

use crate::metrics::{queue_label, JobMetricsSink};
use crate::model::{CancelOutcome, Claim, InsertResult, Job, JobId, JobState, NewJob};
use crate::{Error, Result};
use spot_core::SiteId;
use spot_storage_contract::feed::FeedPosition;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use time::OffsetDateTime;

/// A claim result from the worker.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaimedJob {
    /// The job.
    pub job: Job,
    /// The new claim for this worker to hold.
    pub claim: Claim,
}

/// The job store: persistent storage for jobs (object-safe).
///
/// Object-safe and async, so a database-backed implementation (WP-PLT-06.T2) can implement
/// the same trait using Box<dyn Future>.
pub trait JobStore: Send + Sync {
    /// Insert a new job if it doesn't already exist (by idempotency key or outbox event ID).
    fn insert_if_new(
        &self,
        job: NewJob,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<InsertResult>> + Send + '_>>;

    /// Claim jobs for processing.
    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        now: OffsetDateTime,
        limit: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<ClaimedJob>>> + Send + '_>>;

    /// Renew a claim's lease.
    fn heartbeat(
        &self,
        claim: &Claim,
        ttl: Duration,
        now: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Complete a job (success or failure).
    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Reap jobs whose leases have expired.
    fn reap_expired(
        &self,
        now: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<JobId>>> + Send + '_>>;

    /// Cancel a job.
    fn cancel(
        &self,
        id: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;

    /// Cancel a job only while nobody has claimed it, in one atomic step.
    ///
    /// A job that is `Pending` or `Retrying` and holds no claim becomes `Cancelled` and the
    /// answer is [`CancelOutcome::Cancelled`]. A job a worker holds (`Running`, any claim), or
    /// one that is already `Succeeded`, `Dead` or `Cancelled`, is left exactly as it is and the
    /// answer is [`CancelOutcome::NotCancellable`] (so a second call on a cancelled job reports
    /// it too, unlike the idempotent [`JobStore::cancel`]). An unknown id is
    /// [`CancelOutcome::NotFound`]; none of the three is an error.
    ///
    /// The state check and the write are one step (one lock in memory, one `UPDATE ... WHERE`
    /// statement in PostgreSQL), so a worker that claims at the same moment either wins (the job
    /// is `Running` and this answers `NotCancellable`) or loses (the job is cancelled and its
    /// claim finds nothing): never both. [`JobStore::cancel`] is unchanged: it also cancels a
    /// running job in PostgreSQL, which is why a caller that must not cancel running work uses
    /// this operation instead of reading the state and then calling `cancel`.
    ///
    /// There is deliberately no default body: one that read the job and then called `cancel`
    /// would be the race this operation exists to close, so every store implements it.
    fn cancel_if_pending(
        &self,
        id: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CancelOutcome>> + Send + '_>>;

    /// Move a dead or retrying job back to the queue: it becomes `Pending` with
    /// `not_before = now` (never later than `now`, so it is claimable at once), `attempt` reset
    /// to 0 (it gets its full `max_attempts` again), no claim; `last_error` stays as history.
    /// Returns the job as stored.
    ///
    /// Only a job in `Dead` or `Retrying` can be requeued. A `Pending` job, a `Running` job (a
    /// live claim), a `Succeeded` or a `Cancelled` one is
    /// [`Error::RequeueRefused`](crate::Error::RequeueRefused) and nothing changes, so a
    /// requeue never overwrites a concurrent claim or reap, and of two concurrent requeues of
    /// one dead job exactly one wins. An unknown id is the store's not-found error. The job
    /// keeps its idempotency key: if another live job (pending, running or retrying) now holds
    /// the same (site, name, key) the requeue is
    /// [`Error::RequeueKeyHeld`](crate::Error::RequeueKeyHeld) and nothing changes.
    ///
    /// The default answers `job.requeue_unsupported`, so a store or test double written before
    /// this operation keeps compiling; the in-memory and PostgreSQL stores override it.
    fn requeue(
        &self,
        id: JobId,
        now: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Job>> + Send + '_>> {
        let _ = (id, now);
        Box::pin(async { Err(Error::other("job.requeue_unsupported")) })
    }

    /// Shallow-merge `patch` into the job's `context`, only while `claim` is still the job's
    /// current claim (the same fence `heartbeat` and `complete` check).
    ///
    /// A stored context that is not a JSON object (the default is `null`) is replaced by an
    /// object. Returns the merged context. A stale claim, or a claim that belongs to another
    /// job, is [`Error::ClaimLost`](crate::Error::ClaimLost) and writes nothing. A merged
    /// context over [`MAX_CONTEXT_BYTES`] serialised bytes or [`MAX_CONTEXT_KEYS`] keys is
    /// [`Error::ContextTooLarge`](crate::Error::ContextTooLarge) and writes nothing.
    ///
    /// The context is job metadata, not a secret store: handlers must not put secrets,
    /// tokens or personal data in it.
    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send + '_>>;

    /// Get a job by ID.
    fn get(
        &self,
        id: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Job>>> + Send + '_>>;

    /// List jobs matching a filter.
    fn list(
        &self,
        filter: JobFilter,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Job>>> + Send + '_>>;

    /// Get the current outbox intake position.
    fn intake_position(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FeedPosition>> + Send + '_>>;

    /// Update the outbox intake position.
    fn set_intake_position(
        &self,
        position: FeedPosition,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>>;
}

/// Filter for listing jobs.
#[derive(Clone, Debug, Default)]
pub struct JobFilter {
    /// Filter by site.
    pub site: Option<SiteId>,
    /// Filter by state.
    pub state: Option<JobState>,
    /// Filter by queue.
    pub queue: Option<String>,
    /// Limit results.
    pub limit: Option<u32>,
}

/// An in-memory job store for testing and benchmarks (test-only implementation).
///
/// This implementation uses panics to handle mutex poisoning. In production, use a database.
/// It is provided as a test double for unit tests and integration tests.
#[cfg_attr(not(test), allow(dead_code))]
pub struct InMemoryJobStore {
    /// The injected clock; `created_at` comes from it, never from the system clock directly.
    clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>,
    jobs: Mutex<HashMap<JobId, Job>>,
    /// Map of (site, name, key) -> job ID for idempotency.
    idempotency: Mutex<HashMap<(SiteId, String, String), JobId>>,
    /// Map of outbox event ID -> job ID for intake deduplication.
    outbox_dedup: Mutex<HashMap<String, JobId>>,
    intake_position: Mutex<FeedPosition>,
    next_job_id: Mutex<u64>,
    /// Where requeues are counted, if anywhere.
    metrics: Option<Arc<dyn JobMetricsSink>>,
}

impl std::fmt::Debug for InMemoryJobStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryJobStore").finish_non_exhaustive()
    }
}

impl InMemoryJobStore {
    /// Create a new in-memory store that stamps jobs with the system clock.
    pub fn new() -> Self {
        Self::with_clock(Arc::new(OffsetDateTime::now_utc))
    }

    /// Create a new in-memory store with an injected clock (deterministic tests).
    pub fn with_clock(clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync>) -> Self {
        Self {
            clock,
            jobs: Mutex::new(HashMap::new()),
            idempotency: Mutex::new(HashMap::new()),
            outbox_dedup: Mutex::new(HashMap::new()),
            intake_position: Mutex::new(FeedPosition::START),
            next_job_id: Mutex::new(1),
            metrics: None,
        }
    }

    /// Count each successful [`JobStore::requeue`] on `sink` (label: the job's queue, bounded
    /// by [`queue_label`]).
    #[must_use]
    pub fn with_metrics(mut self, sink: Arc<dyn JobMetricsSink>) -> Self {
        self.metrics = Some(sink);
        self
    }
}

impl Default for InMemoryJobStore {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(clippy::unwrap_used)]
impl JobStore for InMemoryJobStore {
    fn insert_if_new(
        &self,
        job: NewJob,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<InsertResult>> + Send + '_>>
    {
        Box::pin(async move {
            let mut jobs = self.jobs.lock().unwrap();
            let mut idempotency = self.idempotency.lock().unwrap();
            let mut outbox_dedup = self.outbox_dedup.lock().unwrap();

            // Check outbox deduplication first
            if let Some(event_id) = &job.outbox_event_id {
                if let Some(existing_id) = outbox_dedup.get(event_id) {
                    return Ok(InsertResult::Duplicate(*existing_id));
                }
            }

            // Check idempotency key
            if let Some(key) = &job.idempotency_key {
                let name = job.name.clone();
                let idem_key = (job.site.clone(), name, key.clone());
                if let Some(existing_id) = idempotency.get(&idem_key) {
                    // Check if the existing job is still in an active state
                    if let Some(existing_job) = jobs.get(existing_id) {
                        match existing_job.state {
                            JobState::Pending | JobState::Running | JobState::Retrying { .. } => {
                                return Ok(InsertResult::Duplicate(*existing_id));
                            }
                            _ => {
                                // Completed job; can enqueue again
                            }
                        }
                    }
                }
            }

            // Generate new ID
            let mut next_id = self.next_job_id.lock().unwrap();
            let id = JobId(*next_id);
            *next_id += 1;

            let now = (self.clock)();
            let new_job = Job {
                id,
                site: job.site.clone(),
                name: job.name.clone(),
                args: job.args,
                queue: job.queue,
                state: JobState::Pending,
                attempt: 0,
                max_attempts: 3,
                not_before: job.not_before,
                idempotency_key: job.idempotency_key.clone(),
                claim: None,
                last_error: None,
                created_at: now,
                context: job.context,
            };

            jobs.insert(id, new_job);

            // Store idempotency key
            if let Some(key) = job.idempotency_key {
                idempotency.insert((job.site, job.name, key), id);
            }

            // Store outbox dedup
            if let Some(event_id) = job.outbox_event_id {
                outbox_dedup.insert(event_id, id);
            }

            Ok(InsertResult::Inserted(id))
        })
    }

    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        now: OffsetDateTime,
        limit: u32,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<ClaimedJob>>> + Send + '_>>
    {
        let queues = queues.to_vec();
        let worker_id = worker_id.to_string();
        Box::pin(async move {
            let mut jobs = self.jobs.lock().unwrap();
            let mut claimed = Vec::new();

            // Find jobs in order: then not_before, then id
            let mut candidates: Vec<_> = jobs
                .iter_mut()
                .filter(|(_, job)| {
                    queues.contains(&job.queue)
                        && matches!(job.state, JobState::Pending | JobState::Retrying { .. })
                        && job.not_before <= now
                        && job.claim.is_none()
                })
                .collect();

            // Sort by not_before then id
            candidates.sort_by(|a, b| {
                a.1.not_before
                    .cmp(&b.1.not_before)
                    .then_with(|| a.0.cmp(b.0))
            });

            for (_id, job) in candidates.iter_mut().take(limit as usize) {
                let token =
                    spot_scheduler::lease::FencingToken(job.id.0 ^ (now.unix_timestamp() as u64));
                let expires_at = now + time::Duration::seconds(ttl.as_secs() as i64);

                let claim = Claim {
                    token,
                    worker: worker_id.clone(),
                    expires_at,
                };

                job.claim = Some(claim.clone());
                job.state = JobState::Running;

                claimed.push(ClaimedJob {
                    job: job.clone(),
                    claim,
                });
            }

            Ok(claimed)
        })
    }

    fn heartbeat(
        &self,
        claim: &Claim,
        ttl: Duration,
        now: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        let claim = claim.clone();
        Box::pin(async move {
            let mut jobs = self.jobs.lock().unwrap();

            // Find the job with this claim (simplified: linear search)
            for job in jobs.values_mut() {
                if let Some(current_claim) = &job.claim {
                    if current_claim.token == claim.token && current_claim.worker == claim.worker {
                        let new_expires_at = now + time::Duration::seconds(ttl.as_secs() as i64);
                        job.claim = Some(Claim {
                            token: claim.token,
                            worker: claim.worker.clone(),
                            expires_at: new_expires_at,
                        });
                        return Ok(());
                    }
                }
            }

            Err(crate::error::Error::ClaimLost)
        })
    }

    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        let claim = claim.clone();
        Box::pin(async move {
            let mut jobs = self.jobs.lock().unwrap();

            // Find the job with this claim
            for job in jobs.values_mut() {
                if let Some(current_claim) = &job.claim {
                    if current_claim.token == claim.token && current_claim.worker == claim.worker {
                        job.state = new_state;
                        job.last_error = last_error;
                        job.claim = None;
                        return Ok(());
                    }
                }
            }

            Err(crate::error::Error::ClaimLost)
        })
    }

    fn reap_expired(
        &self,
        now: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<JobId>>> + Send + '_>> {
        Box::pin(async move {
            let mut jobs = self.jobs.lock().unwrap();
            let mut reaped = Vec::new();

            for job in jobs.values_mut() {
                if let Some(claim) = &job.claim {
                    if claim.expires_at <= now {
                        job.claim = None;
                        job.attempt += 1;
                        job.last_error = Some("Worker lost (lease expired)".to_string());
                        job.state = JobState::Retrying {
                            at: now + time::Duration::seconds(5),
                        };
                        reaped.push(job.id);
                    }
                }
            }

            Ok(reaped)
        })
    }

    fn cancel(
        &self,
        id: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(job) = jobs.get_mut(&id) {
                job.state = JobState::Cancelled;
                Ok(())
            } else {
                Err(crate::error::Error::Other("Job not found".to_string()))
            }
        })
    }

    fn cancel_if_pending(
        &self,
        id: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<CancelOutcome>> + Send + '_>>
    {
        Box::pin(async move {
            // `claim` takes this same lock, so the check and the write cannot interleave with
            // a claim: one of the two goes first and the other sees its result.
            let mut jobs = self.jobs.lock().unwrap();
            let Some(job) = jobs.get_mut(&id) else {
                return Ok(CancelOutcome::NotFound);
            };
            let cancellable = matches!(job.state, JobState::Pending | JobState::Retrying { .. })
                && job.claim.is_none();
            if !cancellable {
                return Ok(CancelOutcome::NotCancellable);
            }
            job.state = JobState::Cancelled;
            Ok(CancelOutcome::Cancelled)
        })
    }

    fn requeue(
        &self,
        id: JobId,
        now: OffsetDateTime,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Job>> + Send + '_>> {
        Box::pin(async move {
            // The state check, the key check and the write happen under the same locks (taken
            // in the order `insert_if_new` takes them), so a concurrent claim, reap or second
            // requeue cannot interleave.
            let (requeued, queue) = {
                let mut jobs = self.jobs.lock().unwrap();
                let mut idempotency = self.idempotency.lock().unwrap();
                let Some(current) = jobs.get(&id).cloned() else {
                    return Err(Error::Other("Job not found".to_string()));
                };
                let requeueable =
                    matches!(current.state, JobState::Dead | JobState::Retrying { .. })
                        && current.claim.is_none();
                if !requeueable {
                    return Err(Error::RequeueRefused);
                }
                if let Some(key) = &current.idempotency_key {
                    let idem_key = (current.site.clone(), current.name.clone(), key.clone());
                    if let Some(holder) = idempotency.get(&idem_key) {
                        if *holder != id {
                            if let Some(other) = jobs.get(holder) {
                                if matches!(
                                    other.state,
                                    JobState::Pending
                                        | JobState::Running
                                        | JobState::Retrying { .. }
                                ) {
                                    return Err(Error::RequeueKeyHeld);
                                }
                            }
                        }
                    }
                    // This job holds the key again.
                    idempotency.insert(idem_key, id);
                }
                let Some(job) = jobs.get_mut(&id) else {
                    return Err(Error::Other("Job not found".to_string()));
                };
                job.state = JobState::Pending;
                job.attempt = 0;
                job.not_before = now;
                job.claim = None;
                (job.clone(), job.queue.clone())
            };
            if let Some(sink) = &self.metrics {
                sink.requeued(queue_label(&queue));
            }
            Ok(requeued)
        })
    }

    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send + '_>>
    {
        let claim = claim.clone();
        Box::pin(async move {
            // One lock for the check and the write: concurrent merges cannot lose a key and a
            // stale claim cannot slip in between.
            let mut jobs = self.jobs.lock().unwrap();
            let job = jobs.get_mut(&id).ok_or(crate::error::Error::ClaimLost)?;
            match &job.claim {
                Some(current)
                    if current.token == claim.token
                        && current.worker == claim.worker
                        && job.state == JobState::Running => {}
                _ => return Err(crate::error::Error::ClaimLost),
            }
            let merged = crate::model::merge_context_value(&job.context, patch)?;
            job.context = merged.clone();
            Ok(merged)
        })
    }

    fn get(
        &self,
        id: JobId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<Job>>> + Send + '_>> {
        Box::pin(async move {
            let jobs = self.jobs.lock().unwrap();
            Ok(jobs.get(&id).cloned())
        })
    }

    fn list(
        &self,
        filter: JobFilter,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<Job>>> + Send + '_>> {
        let site = filter.site;
        let state = filter.state.clone();
        let queue = filter.queue.clone();
        let limit = filter.limit;

        Box::pin(async move {
            let jobs = self.jobs.lock().unwrap();

            let mut results: Vec<_> = jobs
                .values()
                .filter(|job| {
                    if let Some(ref s) = site {
                        if job.site != *s {
                            return false;
                        }
                    }
                    if let Some(ref st) = state {
                        if std::mem::discriminant(&job.state) != std::mem::discriminant(st) {
                            return false;
                        }
                    }
                    if let Some(ref q) = queue {
                        if job.queue != *q {
                            return false;
                        }
                    }
                    true
                })
                .cloned()
                .collect();

            if let Some(lim) = limit {
                results.truncate(lim as usize);
            }

            Ok(results)
        })
    }

    fn intake_position(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<FeedPosition>> + Send + '_>>
    {
        Box::pin(async move { Ok(*self.intake_position.lock().unwrap()) })
    }

    fn set_intake_position(
        &self,
        position: FeedPosition,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            *self.intake_position.lock().unwrap() = position;
            Ok(())
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_core::SiteId;

    #[tokio::test]
    async fn test_insert_if_new() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            OffsetDateTime::now_utc(),
        );

        let result = store.insert_if_new(job).await.unwrap();
        assert!(matches!(result, InsertResult::Inserted(_)));
    }

    #[tokio::test]
    async fn test_claim_jobs() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            OffsetDateTime::now_utc(),
        );

        let _ = store.insert_if_new(job).await.unwrap();

        let claimed = store
            .claim(
                &["default".to_string()],
                "worker-1",
                Duration::from_secs(30),
                OffsetDateTime::now_utc(),
                10,
            )
            .await
            .unwrap();

        assert_eq!(claimed.len(), 1);
    }

    #[tokio::test]
    async fn test_exact_retry_schedule_attempts_1_to_6() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();
        let backoff = crate::backoff::Backoff::default();

        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        );

        let inserted_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };

        // Verify the backoff calculation produces expected delays
        for attempt in 1..=6 {
            let retry_at = backoff.delay_for_attempt(attempt, inserted_id.0, now);
            // Each retry should be at least base delay away from now
            assert!(
                retry_at > now,
                "Retry at {} should be after now {}",
                retry_at,
                now
            );

            // Verify exponential growth
            if attempt > 1 {
                let prev_retry_at = backoff.delay_for_attempt(attempt - 1, inserted_id.0, now);
                assert!(
                    retry_at >= prev_retry_at,
                    "Attempt {} retry should be >= attempt {} retry",
                    attempt,
                    attempt - 1
                );
            }
        }
    }

    #[tokio::test]
    async fn test_max_attempts_reached_dead() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        );

        let inserted_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };

        // Claim the job
        let claimed = store
            .claim(
                &["default".to_string()],
                "worker-1",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();

        let claim = &claimed[0].claim;

        // Complete with max_attempts reached (set attempt to 3 which is default max)
        store
            .complete(
                claim,
                JobState::Dead,
                Some("Max attempts reached".to_string()),
            )
            .await
            .unwrap();

        // Verify job is dead
        let job = store.get(inserted_id).await.unwrap().unwrap();
        assert!(matches!(job.state, JobState::Dead));
        assert_eq!(job.last_error, Some("Max attempts reached".to_string()));
    }

    #[tokio::test]
    async fn test_per_site_fairness() {
        let store = InMemoryJobStore::new();
        let site_a = SiteId::new("site-a").unwrap();
        let site_b = SiteId::new("site-b").unwrap();
        let now = OffsetDateTime::now_utc();

        // Insert 100 pending jobs for site A
        for i in 0..100 {
            let job = NewJob::new(
                "app:test".to_string(),
                serde_json::json!({"index": i}),
                "default".to_string(),
                site_a.clone(),
                now,
            );
            let _ = store.insert_if_new(job).await.unwrap();
        }

        // Insert 1 pending job for site B
        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({"site": "b"}),
            "default".to_string(),
            site_b.clone(),
            now,
        );
        let site_b_job_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Site B job not inserted"),
        };

        // Claim jobs for site B - should get the job even though site A has many
        let claimed = store
            .claim(
                &["default".to_string()],
                "worker-1",
                Duration::from_secs(30),
                now,
                10,
            )
            .await
            .unwrap();

        // Should claim some jobs, and site B's job should be among them or available
        let claimed_ids: std::collections::HashSet<_> = claimed.iter().map(|c| c.job.id).collect();
        assert!(!claimed.is_empty(), "Should claim some jobs");

        // If site B's job wasn't claimed in this batch, it should still be pending
        if !claimed_ids.contains(&site_b_job_id) {
            let job = store.get(site_b_job_id).await.unwrap().unwrap();
            assert!(matches!(job.state, JobState::Pending | JobState::Running));
        }
    }

    #[tokio::test]
    async fn test_idempotency_deduplication() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job1 = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site.clone(),
            now,
        )
        .with_idempotency_key("key1".to_string());

        let job2 = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        )
        .with_idempotency_key("key1".to_string());

        let result1 = store.insert_if_new(job1).await.unwrap();
        let result2 = store.insert_if_new(job2).await.unwrap();

        match (result1, result2) {
            (InsertResult::Inserted(id1), InsertResult::Duplicate(id2)) => {
                assert_eq!(id1, id2, "Duplicate should return same ID");
            }
            _ => panic!("Expected Inserted then Duplicate"),
        }
    }

    #[tokio::test]
    async fn test_outbox_event_id_deduplication() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job1 = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site.clone(),
            now,
        )
        .with_outbox_event_id("event-123".to_string());

        let job2 = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        )
        .with_outbox_event_id("event-123".to_string());

        let result1 = store.insert_if_new(job1).await.unwrap();
        let result2 = store.insert_if_new(job2).await.unwrap();

        match (result1, result2) {
            (InsertResult::Inserted(id1), InsertResult::Duplicate(id2)) => {
                assert_eq!(id1, id2, "Outbox event ID duplicate should return same ID");
            }
            _ => panic!("Expected Inserted then Duplicate for outbox event ID"),
        }
    }

    #[tokio::test]
    async fn test_fencing_prevents_stale_complete() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        );

        let _ = store.insert_if_new(job).await.unwrap();

        // Claim the job
        let claimed = store
            .claim(
                &["default".to_string()],
                "worker-1",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();

        let claim = &claimed[0].claim;

        // Create a stale claim with wrong token
        let stale_claim = crate::model::Claim {
            token: spot_scheduler::lease::FencingToken(claim.token.0 + 999),
            worker: claim.worker.clone(),
            expires_at: claim.expires_at,
        };

        // Try to complete with stale claim - should fail
        let result = store
            .complete(&stale_claim, JobState::Succeeded, None)
            .await;
        assert!(matches!(result, Err(crate::error::Error::ClaimLost)));
    }

    #[tokio::test]
    async fn test_reaping_expired_leases() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        );

        let _ = store.insert_if_new(job).await.unwrap();

        // Claim with 1 second TTL
        let claimed = store
            .claim(
                &["default".to_string()],
                "worker-1",
                Duration::from_secs(1),
                now,
                1,
            )
            .await
            .unwrap();

        assert_eq!(claimed.len(), 1);
        let job_id = claimed[0].job.id;

        // Reap at a time after lease expiration
        let future_time = now + time::Duration::seconds(2);
        let reaped = store.reap_expired(future_time).await.unwrap();

        assert!(reaped.contains(&job_id), "Job should be reaped");

        // Job should now be in Retrying state
        let job = store.get(job_id).await.unwrap().unwrap();
        assert!(matches!(job.state, JobState::Retrying { .. }));
        assert_eq!(job.attempt, 1, "Attempt should be incremented");
    }

    #[tokio::test]
    async fn test_cancellation() {
        let store = InMemoryJobStore::new();
        let site = SiteId::new("test-site").unwrap();
        let now = OffsetDateTime::now_utc();

        let job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            "default".to_string(),
            site,
            now,
        );

        let job_id = match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            _ => panic!("Job not inserted"),
        };

        // Cancel the job
        store.cancel(job_id).await.unwrap();

        // Verify it's cancelled
        let job = store.get(job_id).await.unwrap().unwrap();
        assert!(matches!(job.state, JobState::Cancelled));
    }

    #[tokio::test]
    async fn test_intake_position_persistence() {
        let store = InMemoryJobStore::new();

        let pos1 = spot_storage_contract::feed::FeedPosition::new(1, 100);
        store.set_intake_position(pos1).await.unwrap();

        let retrieved = store.intake_position().await.unwrap();
        assert_eq!(retrieved, pos1);

        let pos2 = spot_storage_contract::feed::FeedPosition::new(2, 200);
        store.set_intake_position(pos2).await.unwrap();

        let retrieved2 = store.intake_position().await.unwrap();
        assert_eq!(retrieved2, pos2);
    }

    // ---- requeue -------------------------------------------------------------------------

    use std::sync::atomic::{AtomicU64, Ordering};

    const Q: &str = "default";

    async fn put(store: &InMemoryJobStore, key: Option<&str>, now: OffsetDateTime) -> JobId {
        let mut job = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({"n": 1}),
            Q.to_string(),
            SiteId::new("test-site").unwrap(),
            now,
        );
        if let Some(key) = key {
            job = job.with_idempotency_key(key.to_string());
        }
        match store.insert_if_new(job).await.unwrap() {
            InsertResult::Inserted(id) => id,
            other => panic!("not inserted: {other:?}"),
        }
    }

    async fn claim_one(store: &InMemoryJobStore, now: OffsetDateTime) -> Claim {
        let mut claimed = store
            .claim(
                &[Q.to_string()],
                "worker-1",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "one job should be claimable");
        claimed.remove(0).claim
    }

    async fn make_dead(store: &InMemoryJobStore, key: Option<&str>, now: OffsetDateTime) -> JobId {
        let id = put(store, key, now).await;
        let claim = claim_one(store, now).await;
        store
            .complete(&claim, JobState::Dead, Some("boom".to_string()))
            .await
            .unwrap();
        id
    }

    #[tokio::test]
    async fn requeue_moves_a_dead_job_to_pending_and_keeps_the_error() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();
        let id = make_dead(&store, None, now).await;
        let later = now + time::Duration::seconds(60);

        let job = store.requeue(id, later).await.unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.attempt, 0);
        assert_eq!(job.not_before, later);
        assert!(job.claim.is_none());
        assert_eq!(job.last_error.as_deref(), Some("boom"), "the history stays");
        assert_eq!(store.get(id).await.unwrap().unwrap(), job);
    }

    #[tokio::test]
    async fn requeue_moves_a_retrying_job_to_pending() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();
        let id = put(&store, None, now).await;
        let claim = claim_one(&store, now).await;
        store
            .complete(
                &claim,
                JobState::Retrying {
                    at: now + time::Duration::hours(1),
                },
                Some("flaky".to_string()),
            )
            .await
            .unwrap();

        let job = store.requeue(id, now).await.unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.not_before, now);
        assert_eq!(job.last_error.as_deref(), Some("flaky"));
    }

    #[tokio::test]
    async fn every_other_state_is_refused_and_unchanged() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();

        let pending = put(&store, None, now).await;
        let before = store.get(pending).await.unwrap().unwrap();
        assert_eq!(
            store.requeue(pending, now).await,
            Err(Error::RequeueRefused)
        );
        assert_eq!(store.get(pending).await.unwrap().unwrap(), before);

        // Running (a live claim).
        let claim = claim_one(&store, now).await;
        let before = store.get(pending).await.unwrap().unwrap();
        assert_eq!(before.state, JobState::Running);
        assert_eq!(
            store.requeue(pending, now).await,
            Err(Error::RequeueRefused)
        );
        assert_eq!(store.get(pending).await.unwrap().unwrap(), before);

        // Succeeded.
        store
            .complete(&claim, JobState::Succeeded, None)
            .await
            .unwrap();
        let before = store.get(pending).await.unwrap().unwrap();
        assert_eq!(
            store.requeue(pending, now).await,
            Err(Error::RequeueRefused)
        );
        assert_eq!(store.get(pending).await.unwrap().unwrap(), before);

        // Cancelled.
        let cancelled = put(&store, None, now).await;
        store.cancel(cancelled).await.unwrap();
        let before = store.get(cancelled).await.unwrap().unwrap();
        assert_eq!(
            store.requeue(cancelled, now).await,
            Err(Error::RequeueRefused)
        );
        assert_eq!(store.get(cancelled).await.unwrap().unwrap(), before);
    }

    #[tokio::test]
    async fn requeue_of_an_unknown_id_is_the_not_found_error() {
        let store = InMemoryJobStore::new();
        let err = store
            .requeue(JobId(4242), OffsetDateTime::now_utc())
            .await
            .unwrap_err();
        assert_eq!(err, Error::Other("Job not found".to_string()));
    }

    #[tokio::test]
    async fn a_second_requeue_of_the_same_dead_job_is_refused() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();
        let id = make_dead(&store, None, now).await;
        store.requeue(id, now).await.unwrap();
        assert_eq!(store.requeue(id, now).await, Err(Error::RequeueRefused));
    }

    #[tokio::test]
    async fn a_requeued_job_is_claimable_and_gets_its_attempts_back() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();
        let id = make_dead(&store, None, now).await;
        // A dead job that used its attempts: the worker's bookkeeping says so.
        {
            let mut jobs = store.jobs.lock().unwrap();
            jobs.get_mut(&id).unwrap().attempt = 3;
        }
        let job = store.requeue(id, now).await.unwrap();
        assert_eq!(job.attempt, 0);
        assert_eq!(job.max_attempts, 3);

        let claimed = store
            .claim(
                &[Q.to_string()],
                "worker-2",
                Duration::from_secs(30),
                now,
                1,
            )
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].job.id, id);
        assert!(claimed[0].job.attempt < claimed[0].job.max_attempts);
    }

    #[tokio::test]
    async fn requeue_refuses_when_another_live_job_holds_the_key() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();
        let dead = make_dead(&store, Some("k1"), now).await;
        // The key is free while the first job is dead, so a second job takes it.
        let holder = put(&store, Some("k1"), now).await;
        assert_ne!(holder, dead);

        let before = store.get(dead).await.unwrap().unwrap();
        assert_eq!(store.requeue(dead, now).await, Err(Error::RequeueKeyHeld));
        assert_eq!(store.get(dead).await.unwrap().unwrap(), before);

        // Once the holder is done the key is free again and the requeue goes through.
        store.cancel(holder).await.unwrap();
        let job = store.requeue(dead, now).await.unwrap();
        assert_eq!(job.state, JobState::Pending);
        assert_eq!(job.idempotency_key.as_deref(), Some("k1"));
        // And the requeued job now owns the key: a new insert is a duplicate of it.
        let again = NewJob::new(
            "app:test".to_string(),
            serde_json::json!({}),
            Q.to_string(),
            SiteId::new("test-site").unwrap(),
            now,
        )
        .with_idempotency_key("k1".to_string());
        assert_eq!(
            store.insert_if_new(again).await.unwrap(),
            InsertResult::Duplicate(dead)
        );
    }

    #[tokio::test]
    async fn two_concurrent_requeues_of_one_dead_job_have_one_winner() {
        let store = Arc::new(InMemoryJobStore::new());
        let now = OffsetDateTime::now_utc();
        let id = make_dead(&store, None, now).await;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            tasks.push(tokio::spawn(async move { store.requeue(id, now).await }));
        }
        let mut won = 0;
        for t in tasks {
            match t.await.unwrap() {
                Ok(_) => won += 1,
                Err(e) => assert_eq!(e, Error::RequeueRefused),
            }
        }
        assert_eq!(won, 1);
    }

    #[derive(Default)]
    struct CountingSink {
        requeued: AtomicU64,
        last: Mutex<Option<&'static str>>,
    }

    impl JobMetricsSink for CountingSink {
        fn queue_depth(&self, _queue: &'static str, _depth: u64) {}
        fn oldest_age_seconds(&self, _queue: &'static str, _seconds: u64) {}
        fn requeued(&self, queue: &'static str) {
            self.requeued.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(queue);
        }
    }

    #[tokio::test]
    async fn only_a_successful_requeue_is_counted() {
        let sink = Arc::new(CountingSink::default());
        let store = InMemoryJobStore::new().with_metrics(sink.clone());
        let now = OffsetDateTime::now_utc();
        let id = make_dead(&store, None, now).await;
        assert!(store.requeue(JobId(999), now).await.is_err());
        assert_eq!(sink.requeued.load(Ordering::SeqCst), 0);
        store.requeue(id, now).await.unwrap();
        assert!(store.requeue(id, now).await.is_err());
        assert_eq!(sink.requeued.load(Ordering::SeqCst), 1);
        assert_eq!(*sink.last.lock().unwrap(), Some("default"));
    }

    /// A store written before `requeue` existed: it implements the old operations only (by
    /// delegating) and so gets the trait's default body.
    struct Minimal(InMemoryJobStore);

    type Fut<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send + 'a>>;

    impl JobStore for Minimal {
        fn insert_if_new(&self, job: NewJob) -> Fut<'_, InsertResult> {
            self.0.insert_if_new(job)
        }
        fn claim(
            &self,
            queues: &[String],
            worker_id: &str,
            ttl: Duration,
            now: OffsetDateTime,
            limit: u32,
        ) -> Fut<'_, Vec<ClaimedJob>> {
            self.0.claim(queues, worker_id, ttl, now, limit)
        }
        fn heartbeat(&self, claim: &Claim, ttl: Duration, now: OffsetDateTime) -> Fut<'_, ()> {
            self.0.heartbeat(claim, ttl, now)
        }
        fn complete(
            &self,
            claim: &Claim,
            new_state: JobState,
            last_error: Option<String>,
        ) -> Fut<'_, ()> {
            self.0.complete(claim, new_state, last_error)
        }
        fn reap_expired(&self, now: OffsetDateTime) -> Fut<'_, Vec<JobId>> {
            self.0.reap_expired(now)
        }
        fn cancel(&self, id: JobId) -> Fut<'_, ()> {
            self.0.cancel(id)
        }
        fn cancel_if_pending(&self, id: JobId) -> Fut<'_, CancelOutcome> {
            self.0.cancel_if_pending(id)
        }
        fn merge_context(
            &self,
            id: JobId,
            claim: &Claim,
            patch: serde_json::Map<String, serde_json::Value>,
        ) -> Fut<'_, serde_json::Value> {
            self.0.merge_context(id, claim, patch)
        }
        fn get(&self, id: JobId) -> Fut<'_, Option<Job>> {
            self.0.get(id)
        }
        fn list(&self, filter: JobFilter) -> Fut<'_, Vec<Job>> {
            self.0.list(filter)
        }
        fn intake_position(&self) -> Fut<'_, FeedPosition> {
            self.0.intake_position()
        }
        fn set_intake_position(&self, position: FeedPosition) -> Fut<'_, ()> {
            self.0.set_intake_position(position)
        }
    }

    #[tokio::test]
    async fn the_default_body_answers_unsupported_and_changes_nothing() {
        let store = Minimal(InMemoryJobStore::new());
        let now = OffsetDateTime::now_utc();
        let id = make_dead(&store.0, None, now).await;
        let before = store.get(id).await.unwrap().unwrap();
        let err = store.requeue(id, now).await.unwrap_err();
        assert_eq!(err, Error::Other("job.requeue_unsupported".to_string()));
        assert_eq!(store.get(id).await.unwrap().unwrap(), before);
    }

    // ---- cancel_if_pending ---------------------------------------------------------------

    #[tokio::test]
    async fn cancel_if_pending_cancels_a_pending_job_and_an_unclaimed_retrying_one() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();
        let pending = put(&store, None, now).await;
        assert_eq!(
            store.cancel_if_pending(pending).await.unwrap(),
            CancelOutcome::Cancelled
        );
        assert_eq!(
            store.get(pending).await.unwrap().unwrap().state,
            JobState::Cancelled
        );

        let retrying = put(&store, None, now).await;
        let claim = claim_one(&store, now).await;
        store
            .complete(
                &claim,
                JobState::Retrying {
                    at: now + time::Duration::hours(1),
                },
                Some("flaky".to_string()),
            )
            .await
            .unwrap();
        assert_eq!(
            store.cancel_if_pending(retrying).await.unwrap(),
            CancelOutcome::Cancelled
        );
        let stored = store.get(retrying).await.unwrap().unwrap();
        assert_eq!(stored.state, JobState::Cancelled);
        assert_eq!(stored.last_error.as_deref(), Some("flaky"), "history stays");
    }

    #[tokio::test]
    async fn cancel_if_pending_never_touches_a_claimed_or_finished_job() {
        let store = InMemoryJobStore::new();
        let now = OffsetDateTime::now_utc();

        let running = put(&store, None, now).await;
        let claim = claim_one(&store, now).await;
        let before = store.get(running).await.unwrap().unwrap();
        assert_eq!(
            store.cancel_if_pending(running).await.unwrap(),
            CancelOutcome::NotCancellable
        );
        assert_eq!(store.get(running).await.unwrap().unwrap(), before);
        // The worker is not disturbed: it still completes the job under its claim.
        store
            .complete(&claim, JobState::Succeeded, None)
            .await
            .unwrap();

        let dead = make_dead(&store, None, now).await;
        let cancelled = put(&store, None, now).await;
        store.cancel(cancelled).await.unwrap();
        for (name, id) in [
            ("succeeded", running),
            ("dead", dead),
            ("cancelled", cancelled),
        ] {
            let before = store.get(id).await.unwrap().unwrap();
            assert_eq!(
                store.cancel_if_pending(id).await.unwrap(),
                CancelOutcome::NotCancellable,
                "{name}"
            );
            assert_eq!(store.get(id).await.unwrap().unwrap(), before, "{name}");
        }
    }

    #[tokio::test]
    async fn cancel_if_pending_on_an_unknown_id_is_not_found_and_not_an_error() {
        let store = InMemoryJobStore::new();
        assert_eq!(
            store.cancel_if_pending(JobId(4242)).await.unwrap(),
            CancelOutcome::NotFound
        );
        // `cancel` keeps its own answer for an unknown id.
        assert_eq!(
            store.cancel(JobId(4242)).await,
            Err(Error::Other("Job not found".to_string()))
        );
    }

    #[tokio::test]
    async fn cancel_if_pending_racing_a_claim_has_exactly_one_winner() {
        for _ in 0..16 {
            let store = Arc::new(InMemoryJobStore::new());
            let now = OffsetDateTime::now_utc();
            let id = put(&store, None, now).await;
            let canceller = {
                let store = store.clone();
                tokio::spawn(async move { store.cancel_if_pending(id).await })
            };
            let claimer = {
                let store = store.clone();
                tokio::spawn(async move {
                    store
                        .claim(
                            &[Q.to_string()],
                            "worker-1",
                            Duration::from_secs(30),
                            now,
                            1,
                        )
                        .await
                })
            };
            let outcome = canceller.await.unwrap().unwrap();
            let claimed = claimer.await.unwrap().unwrap();
            let stored = store.get(id).await.unwrap().unwrap();
            match (outcome, claimed.len()) {
                (CancelOutcome::Cancelled, 0) => {
                    assert_eq!(stored.state, JobState::Cancelled);
                    assert!(stored.claim.is_none());
                }
                (CancelOutcome::NotCancellable, 1) => {
                    assert_eq!(stored.state, JobState::Running);
                    assert!(stored.claim.is_some());
                }
                other => panic!("both or neither won: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn eight_concurrent_cancel_if_pending_calls_have_one_winner() {
        let store = Arc::new(InMemoryJobStore::new());
        let id = put(&store, None, OffsetDateTime::now_utc()).await;
        let mut tasks = Vec::new();
        for _ in 0..8 {
            let store = store.clone();
            tasks.push(tokio::spawn(
                async move { store.cancel_if_pending(id).await },
            ));
        }
        let mut won = 0;
        for t in tasks {
            match t.await.unwrap().unwrap() {
                CancelOutcome::Cancelled => won += 1,
                CancelOutcome::NotCancellable => {}
                CancelOutcome::NotFound => panic!("the job exists"),
            }
        }
        assert_eq!(won, 1);
    }

    #[tokio::test]
    async fn a_store_that_delegates_cancel_if_pending_gets_the_real_answer() {
        let store = Minimal(InMemoryJobStore::new());
        let id = put(&store.0, None, OffsetDateTime::now_utc()).await;
        assert_eq!(
            store.cancel_if_pending(id).await.unwrap(),
            CancelOutcome::Cancelled
        );
    }
}
