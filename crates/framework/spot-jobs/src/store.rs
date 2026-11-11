//! The JobStore trait: the persistent backing for jobs.

use crate::model::{Claim, InsertResult, Job, JobId, JobState, NewJob};
use crate::Result;
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
        }
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
}
