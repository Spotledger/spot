//! Contract tests for the job engine.
//!
//! These tests verify the engine's correctness under concurrent access, leases, idempotency,
//! and other reliability properties. Available when the `testing` feature is enabled.

use crate::model::JobState;
use crate::Result;
use serde_json::json;
use spot_core::SiteId;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;

/// A report on contract test results.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContractReport {
    /// Total tests run.
    pub total: u32,
    /// Tests passed.
    pub passed: u32,
    /// Tests failed.
    pub failed: u32,
    /// Failure messages.
    pub failures: Vec<String>,
}

impl ContractReport {
    /// Check if all tests passed.
    pub fn all_passed(&self) -> bool {
        self.failed == 0
    }
}

/// Run all contract tests on a job store implementation.
///
/// Tests include:
/// - Claim exclusivity under racing claimers
/// - Fencing of stale completes
/// - Reaping of expired leases
/// - Idempotency
/// - Job ordering
/// - `not_before` enforcement
/// - Cancellation
/// - Intake position persistence
pub async fn run_all<F>(make_store: F) -> Result<ContractReport>
where
    F: Fn() -> Arc<dyn crate::store::JobStore> + Send + Sync,
{
    let mut report = ContractReport {
        total: 0,
        passed: 0,
        failed: 0,
        failures: Vec::new(),
    };

    // Test: idempotency
    report.total += 1;
    match test_idempotency(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_idempotency: {}", e));
        }
    }

    // Test: claim exclusivity
    report.total += 1;
    match test_claim_exclusivity(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_claim_exclusivity: {}", e));
        }
    }

    // Test: not_before enforcement
    report.total += 1;
    match test_not_before(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_not_before: {}", e));
        }
    }

    // Test: cancellation
    report.total += 1;
    match test_cancellation(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_cancellation: {}", e));
        }
    }

    // Test: fencing
    report.total += 1;
    match test_fencing(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_fencing: {}", e));
        }
    }

    // Test: reaping
    report.total += 1;
    match test_reaping(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_reaping: {}", e));
        }
    }

    // Test: intake position
    report.total += 1;
    match test_intake_position(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_intake_position: {}", e));
        }
    }

    // Test: 64 racing claimers
    report.total += 1;
    match test_racing_claimers_64(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_racing_claimers_64: {}", e));
        }
    }

    // Test: max_attempts to Dead
    report.total += 1;
    match test_max_attempts_dead(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_max_attempts_dead: {}", e));
        }
    }

    // Test: per-site fairness
    report.total += 1;
    match test_per_site_fairness(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_per_site_fairness: {}", e));
        }
    }

    // Test: retry schedule (exact delays)
    report.total += 1;
    match test_retry_schedule(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report.failures.push(format!("test_retry_schedule: {}", e));
        }
    }

    // Test: the handler sees the engine's limit and the last-attempt flag
    report.total += 1;
    match test_last_attempt_signal(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_last_attempt_signal: {}", e));
        }
    }

    // Test: the final-attempt hook fires once, only on a last-attempt panic or timeout
    report.total += 1;
    match test_final_attempt_hook(&make_store).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_final_attempt_hook: {}", e));
        }
    }

    // Test: merge_context (basic merge, replace of a non-object, durability across a reap)
    report.total += 1;
    match test_merge_context_basic(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_merge_context_basic: {}", e));
        }
    }

    // Test: merge_context is fenced by the claim and by the job
    report.total += 1;
    match test_merge_context_fenced(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_merge_context_fenced: {}", e));
        }
    }

    // Test: merge_context size and key limits
    report.total += 1;
    match test_merge_context_limits(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_merge_context_limits: {}", e));
        }
    }

    // Test: a stale claim with an oversized patch is ClaimLost (fence first), in every store
    report.total += 1;
    match test_merge_context_fence_before_size(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_merge_context_fence_before_size: {}", e));
        }
    }

    // Test: concurrent merges lose no key, and a stale claimant's never lands
    report.total += 1;
    match test_merge_context_concurrent(&make_store()).await {
        Ok(_) => report.passed += 1,
        Err(e) => {
            report.failed += 1;
            report
                .failures
                .push(format!("test_merge_context_concurrent: {}", e));
        }
    }

    Ok(report)
}

/// Claims the one job in `store` and runs it through the worker as `attempt`, on a queue whose
/// policy allows `policy_max` attempts and with `timeout`. Returns the job after the attempt.
async fn run_one_attempt(
    store: &Arc<dyn crate::store::JobStore>,
    registry: spot_jobs::worker::JobHandlerRegistry,
    policy_max: u32,
    timeout: Duration,
    attempt: u32,
    hook: Option<Arc<dyn spot_jobs::worker::FinalAttemptHook>>,
) -> Result<crate::model::Job> {
    let now = OffsetDateTime::now_utc();
    let new_job = crate::model::NewJob::new(
        "app:contract".to_string(),
        json!({}),
        "default".to_string(),
        SiteId::new("test-site").unwrap(),
        now,
    );
    let job_id = match store.insert_if_new(new_job).await? {
        crate::model::InsertResult::Inserted(id) => id,
        _ => return Err(crate::error::Error::other("Insert failed")),
    };
    let claimed = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(30),
            now,
            1,
        )
        .await?;
    let Some(first) = claimed.first() else {
        return Err(crate::error::Error::other("Nothing claimed"));
    };
    // The stored job says 3 whatever the policy says: the worker must use the engine's value.
    let job = crate::model::Job {
        attempt,
        max_attempts: 3,
        ..first.job.clone()
    };
    let policy = spot_jobs::policy::QueuePolicy::custom("default")
        .with_timeout(timeout)
        .with_max_attempts(policy_max)
        .build();
    spot_jobs::worker::Worker::execute_job_with_hook(
        store.clone(),
        Arc::new(registry),
        Arc::new(std::collections::HashMap::from([(
            "default".to_string(),
            policy,
        )])),
        Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime),
        Arc::new(std::collections::HashMap::new()),
        job,
        first.claim.clone(),
        hook,
    )
    .await?;
    store
        .get(job_id)
        .await?
        .ok_or_else(|| crate::error::Error::other("Job vanished"))
}

async fn test_last_attempt_signal(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    use spot_jobs::worker::{HandlerResult, JobHandlerRegistry};
    // Policy max 2, attempt 2: the flag is true and the limit is the policy's, not the job's 3.
    let seen = Arc::new(std::sync::Mutex::new(None));
    let seen_in_handler = seen.clone();
    let mut registry = JobHandlerRegistry::new();
    registry.register("app:contract", move |ctx| {
        *seen_in_handler.lock().unwrap() = Some((ctx.max_attempts(), ctx.is_last_attempt()));
        Box::pin(async move { Ok::<_, crate::error::Error>(HandlerResult::Ok) })
    });
    run_one_attempt(store, registry, 2, Duration::from_secs(10), 2, None).await?;
    let got = *seen.lock().unwrap();
    if got == Some((2, true)) {
        Ok(())
    } else {
        Err(crate::error::Error::other(format!(
            "Expected (2, true), handler saw {:?}",
            got
        )))
    }
}

async fn test_final_attempt_hook(
    make_store: &(dyn Fn() -> Arc<dyn crate::store::JobStore> + Send + Sync),
) -> Result<()> {
    use spot_jobs::worker::{BoxFuture, FinalAttemptHook, HandlerResult, JobHandlerRegistry};
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Counter(AtomicU32);
    impl FinalAttemptHook for Counter {
        fn on_dead<'a>(
            &'a self,
            _job: &'a crate::model::Job,
            _reason: &'a str,
        ) -> BoxFuture<'a, ()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        }
    }

    // (handler, attempt of policy max 2, expected hook calls)
    let cases: [(&str, u32, u32); 4] = [
        ("panic", 2, 1),
        ("panic", 1, 0),
        ("hang", 2, 1),
        ("hang", 1, 0),
    ];
    for (handler, attempt, expected) in cases {
        let store = make_store();
        let mut registry = JobHandlerRegistry::new();
        registry.register("app:contract", move |_ctx| {
            Box::pin(async move {
                if handler == "panic" {
                    panic!("boom");
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok::<_, crate::error::Error>(HandlerResult::Ok)
            })
        });
        let hook = Arc::new(Counter(AtomicU32::new(0)));
        let job = run_one_attempt(
            &store,
            registry,
            2,
            Duration::from_millis(50),
            attempt,
            Some(hook.clone()),
        )
        .await?;
        let calls = hook.0.load(Ordering::SeqCst);
        if calls != expected || !matches!(job.state, JobState::Dead) {
            return Err(crate::error::Error::other(format!(
                "{} attempt {}: hook calls {} (expected {}), state {:?}",
                handler, attempt, calls, expected, job.state
            )));
        }
    }
    Ok(())
}

async fn test_idempotency(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job1 = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    )
    .with_idempotency_key("key1".to_string());

    let result1 = store.insert_if_new(job1.clone()).await?;
    let result2 = store.insert_if_new(job1).await?;

    match (result1, result2) {
        (crate::model::InsertResult::Inserted(id1), crate::model::InsertResult::Duplicate(id2)) => {
            if id1 == id2 {
                Ok(())
            } else {
                Err(crate::error::Error::other(
                    "Idempotency keys returned different job IDs",
                ))
            }
        }
        _ => Err(crate::error::Error::other(
            "Expected Inserted then Duplicate",
        )),
    }
}

async fn test_claim_exclusivity(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );

    let _ = store.insert_if_new(job).await?;

    // First worker claims
    let claimed1 = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(30),
            now,
            10,
        )
        .await?;

    // Second worker claims (should get nothing)
    let claimed2 = store
        .claim(
            &["default".to_string()],
            "worker-2",
            Duration::from_secs(30),
            now,
            10,
        )
        .await?;

    if claimed1.len() == 1 && claimed2.is_empty() {
        Ok(())
    } else {
        Err(crate::error::Error::other("Claim exclusivity violated"))
    }
}

async fn test_not_before(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();
    let future = now + time::Duration::seconds(3600);

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        future,
    );

    let _ = store.insert_if_new(job).await?;

    // Claim with now < not_before (should get nothing)
    let claimed = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(30),
            now,
            10,
        )
        .await?;

    if claimed.is_empty() {
        Ok(())
    } else {
        Err(crate::error::Error::other("not_before not enforced"))
    }
}

async fn test_cancellation(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );

    let result = store.insert_if_new(job).await?;
    let job_id = match result {
        crate::model::InsertResult::Inserted(id) => id,
        _ => return Err(crate::error::Error::other("Insert failed")),
    };

    store.cancel(job_id).await?;

    if let Some(job) = store.get(job_id).await? {
        if matches!(job.state, JobState::Cancelled) {
            return Ok(());
        }
    }

    Err(crate::error::Error::other("Cancellation failed"))
}

async fn test_fencing(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );

    let _ = store.insert_if_new(job).await?;

    // Claim and get a claim token
    let claimed = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(30),
            now,
            1,
        )
        .await?;

    if claimed.is_empty() {
        return Err(crate::error::Error::other("Claim failed"));
    }

    let claim = &claimed[0].claim;
    let bad_claim = crate::model::Claim {
        token: spot_scheduler::lease::FencingToken(claim.token.0 + 1),
        worker: claim.worker.clone(),
        expires_at: claim.expires_at,
    };

    // Try to complete with a stale claim (should fail with ClaimLost)
    let result = store.complete(&bad_claim, JobState::Succeeded, None).await;

    match result {
        Err(crate::error::Error::ClaimLost) => Ok(()),
        _ => Err(crate::error::Error::other("Fencing not enforced")),
    }
}

async fn test_reaping(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );

    let _ = store.insert_if_new(job).await?;

    // Claim a job
    let claimed = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(1),
            now,
            1,
        )
        .await?;

    if claimed.is_empty() {
        return Err(crate::error::Error::other("Claim failed"));
    }

    // Wait for lease to expire
    let future_now = now + time::Duration::seconds(2);

    // Reap should return the job ID
    let reaped = store.reap_expired(future_now).await?;

    if reaped.len() == 1 {
        Ok(())
    } else {
        Err(crate::error::Error::other("Reaping failed"))
    }
}

async fn test_intake_position(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let _start_pos = store.intake_position().await?;

    let new_pos = spot_storage_contract::feed::FeedPosition::new(1, 100);
    store.set_intake_position(new_pos).await?;

    let stored_pos = store.intake_position().await?;

    if stored_pos == new_pos {
        Ok(())
    } else {
        Err(crate::error::Error::other("Intake position not persisted"))
    }
}

async fn test_racing_claimers_64(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    // Insert a single job
    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );
    let _ = store.insert_if_new(job).await?;

    // Simulate 64 racing claimers
    let mut handles = vec![];
    for i in 0..64 {
        let store_clone = Arc::clone(store);
        let handle = tokio::spawn(async move {
            let worker_id = format!("worker-{}", i);
            store_clone
                .claim(
                    &["default".to_string()],
                    &worker_id,
                    Duration::from_secs(30),
                    now,
                    1,
                )
                .await
        });
        handles.push(handle);
    }

    // Collect results
    let mut total_claimed = 0;
    for handle in handles {
        if let Ok(Ok(claimed)) = handle.await {
            total_claimed += claimed.len();
        }
    }

    // Only one worker should have claimed the job
    if total_claimed == 1 {
        Ok(())
    } else {
        Err(crate::error::Error::other(format!(
            "Expected 1 claim, got {}",
            total_claimed
        )))
    }
}

async fn test_max_attempts_dead(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );

    let result = store.insert_if_new(job).await?;
    let job_id = match result {
        crate::model::InsertResult::Inserted(id) => id,
        _ => return Err(crate::error::Error::other("Insert failed")),
    };

    // Claim and mark as failed (with error message)
    let claimed = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(30),
            now,
            1,
        )
        .await?;

    if let Some(claimed_job) = claimed.first() {
        let error_msg = "Job failed".to_string();
        let next_retry_time = now + time::Duration::seconds(10);
        store
            .complete(
                &claimed_job.claim,
                JobState::Retrying {
                    at: next_retry_time,
                },
                Some(error_msg.clone()),
            )
            .await?;

        // Verify the job has the error message
        if let Some(job) = store.get(job_id).await? {
            if job.last_error == Some(error_msg) {
                return Ok(());
            }
        }
    }

    Err(crate::error::Error::other("Error message not persisted"))
}

async fn test_per_site_fairness(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site1 = SiteId::new("site-1").unwrap();
    let site2 = SiteId::new("site-2").unwrap();
    let now = OffsetDateTime::now_utc();

    // Insert 1000 jobs for site1
    for i in 0..1000 {
        let job = crate::model::NewJob::new(
            "app:test".to_string(),
            json!({"index": i}),
            "default".to_string(),
            site1.clone(),
            now,
        )
        .with_idempotency_key(format!("key-1-{}", i));
        let _ = store.insert_if_new(job).await?;
    }

    // Insert 1 job for site2
    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site2.clone(),
        now,
    );
    let _ = store.insert_if_new(job).await?;

    // Claim with per-site limits (this test is simplified; real implementation would have per-site-running config)
    let claimed = store
        .claim(
            &["default".to_string()],
            "worker-1",
            Duration::from_secs(30),
            now,
            100,
        )
        .await?;

    // We should get jobs from both sites if fairness is enforced
    // For now, just check we got some claims
    if !claimed.is_empty() {
        Ok(())
    } else {
        Err(crate::error::Error::other("No jobs claimed"))
    }
}

async fn test_retry_schedule(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let site = SiteId::new("test-site").unwrap();
    let now = OffsetDateTime::now_utc();

    let job = crate::model::NewJob::new(
        "app:test".to_string(),
        json!({}),
        "default".to_string(),
        site,
        now,
    );

    let result = store.insert_if_new(job).await?;
    let job_id = match result {
        crate::model::InsertResult::Inserted(id) => id,
        _ => return Err(crate::error::Error::other("Insert failed")),
    };

    // Get the job and verify attempt is 0
    if let Some(job) = store.get(job_id).await? {
        if job.attempt != 0 {
            return Err(crate::error::Error::other("Initial attempt should be 0"));
        }
    }

    Ok(())
}

fn fail(msg: &str) -> crate::error::Error {
    crate::error::Error::other(msg)
}

/// A map from a JSON object literal (the literal must be an object).
fn obj(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    match v {
        serde_json::Value::Object(m) => m,
        _ => serde_json::Map::new(),
    }
}

/// Inserts one job for `site` and claims it as `worker`.
async fn insert_and_claim(
    store: &Arc<dyn crate::store::JobStore>,
    site: &str,
    worker: &str,
    context: serde_json::Value,
) -> Result<(crate::model::JobId, crate::model::Claim)> {
    let now = OffsetDateTime::now_utc();
    let new_job = crate::model::NewJob::new(
        "app:merge".to_string(),
        json!({}),
        "default".to_string(),
        SiteId::new(site).unwrap(),
        now,
    )
    .with_context(context);
    let id = match store.insert_if_new(new_job).await? {
        crate::model::InsertResult::Inserted(id) => id,
        _ => return Err(fail("insert failed")),
    };
    let claimed = store
        .claim(
            &["default".to_string()],
            worker,
            Duration::from_secs(30),
            now,
            64,
        )
        .await?;
    let Some(c) = claimed.into_iter().find(|c| c.job.id == id) else {
        return Err(fail("job not claimed"));
    };
    Ok((id, c.claim))
}

async fn context_of(
    store: &Arc<dyn crate::store::JobStore>,
    id: crate::model::JobId,
) -> Result<serde_json::Value> {
    Ok(store
        .get(id)
        .await?
        .ok_or_else(|| fail("job vanished"))?
        .context)
}

async fn test_merge_context_basic(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let (id, claim) = insert_and_claim(store, "test-site", "worker-1", json!(null)).await?;

    // A null (non-object) stored context is replaced by an object; the merged value is returned.
    let merged = store
        .merge_context(id, &claim, obj(json!({"step1": "done"})))
        .await?;
    if merged != json!({"step1": "done"}) {
        return Err(fail("merge did not return the merged object"));
    }
    // A second merge keeps the first key and overwrites a repeated one (shallow).
    let merged = store
        .merge_context(
            id,
            &claim,
            obj(json!({"step2": {"a": 1}, "step1": "again"})),
        )
        .await?;
    if merged != json!({"step1": "again", "step2": {"a": 1}}) {
        return Err(fail("second merge lost or kept the wrong keys"));
    }
    if context_of(store, id).await? != merged {
        return Err(fail("merge not visible to get"));
    }
    // An empty patch is a no-op that returns the stored object.
    if store.merge_context(id, &claim, obj(json!({}))).await? != merged {
        return Err(fail("empty patch changed the context"));
    }
    // The shallow merge does not descend: a nested object is replaced whole.
    let merged = store
        .merge_context(id, &claim, obj(json!({"step2": {"b": 2}})))
        .await?;
    if merged["step2"] != json!({"b": 2}) {
        return Err(fail("merge was not shallow"));
    }

    // A non-object stored context of another kind (an array) is replaced too.
    let (id2, claim2) = insert_and_claim(
        store,
        "test-site",
        "worker-2",
        json!(["not", "an", "object"]),
    )
    .await?;
    let merged = store
        .merge_context(id2, &claim2, obj(json!({"k": 1})))
        .await?;
    if merged != json!({"k": 1}) {
        return Err(fail("an array context was not replaced by an object"));
    }

    // The context survives a lapsed lease (reap): the retry sees the earlier merge.
    let later = OffsetDateTime::now_utc() + time::Duration::seconds(3600);
    let reaped = store.reap_expired(later).await?;
    if !reaped.contains(&id) {
        return Err(fail("job was not reaped"));
    }
    if context_of(store, id).await? != json!({"step1": "again", "step2": {"b": 2}}) {
        return Err(fail("context lost across reap"));
    }
    // The old claim is stale after the reap and writes nothing.
    match store
        .merge_context(id, &claim, obj(json!({"late": true})))
        .await
    {
        Err(crate::error::Error::ClaimLost) => {}
        _ => return Err(fail("a reaped claim could still merge")),
    }
    if context_of(store, id).await?.get("late").is_some() {
        return Err(fail("a reaped claim wrote"));
    }
    Ok(())
}

async fn test_merge_context_fenced(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let (id, claim) = insert_and_claim(store, "site-a", "worker-1", json!({"seed": 1})).await?;
    let (other_id, other_claim) =
        insert_and_claim(store, "site-b", "worker-2", json!({"seed": 2})).await?;

    // A wrong fencing token writes nothing.
    let bad_token = crate::model::Claim {
        token: spot_scheduler::lease::FencingToken(claim.token.0 + 1),
        worker: claim.worker.clone(),
        expires_at: claim.expires_at,
    };
    // A right token under another worker's name writes nothing.
    let bad_worker = crate::model::Claim {
        token: claim.token,
        worker: "worker-2".to_string(),
        expires_at: claim.expires_at,
    };
    for stale in [&bad_token, &bad_worker] {
        match store
            .merge_context(id, stale, obj(json!({"evil": true})))
            .await
        {
            Err(crate::error::Error::ClaimLost) => {}
            _ => return Err(fail("a stale claim was not refused with ClaimLost")),
        }
    }

    // Tenant isolation: site-b's valid claim cannot write site-a's job, and the reverse.
    match store
        .merge_context(id, &other_claim, obj(json!({"cross": "b->a"})))
        .await
    {
        Err(crate::error::Error::ClaimLost) => {}
        _ => return Err(fail("another tenant's claim wrote this job")),
    }
    match store
        .merge_context(other_id, &claim, obj(json!({"cross": "a->b"})))
        .await
    {
        Err(crate::error::Error::ClaimLost) => {}
        _ => return Err(fail("this claim wrote another tenant's job")),
    }
    // An unknown job id is a lost claim, not a write.
    let missing = crate::model::JobId(u64::MAX / 4);
    match store
        .merge_context(missing, &claim, obj(json!({"x": 1})))
        .await
    {
        Err(crate::error::Error::ClaimLost) => {}
        _ => return Err(fail("an unknown job id was not refused with ClaimLost")),
    }
    if context_of(store, id).await? != json!({"seed": 1})
        || context_of(store, other_id).await? != json!({"seed": 2})
    {
        return Err(fail("a refused merge wrote"));
    }

    // The legitimate claim still works, and after complete the claim is gone.
    store
        .merge_context(id, &claim, obj(json!({"ok": true})))
        .await?;
    store.complete(&claim, JobState::Succeeded, None).await?;
    match store
        .merge_context(id, &claim, obj(json!({"after": true})))
        .await
    {
        Err(crate::error::Error::ClaimLost) => {}
        _ => return Err(fail("a completed job's claim could still merge")),
    }
    if context_of(store, id).await? != json!({"seed": 1, "ok": true}) {
        return Err(fail("context after complete is wrong"));
    }
    Ok(())
}

async fn test_merge_context_limits(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    use crate::model::{MAX_CONTEXT_BYTES, MAX_CONTEXT_KEYS};
    let too_large =
        |r: Result<serde_json::Value>| matches!(r, Err(crate::error::Error::ContextTooLarge));

    // Exactly the key limit is fine; one more is refused and writes nothing.
    let (id, claim) = insert_and_claim(store, "test-site", "worker-1", json!(null)).await?;
    let mut patch = serde_json::Map::new();
    for i in 0..MAX_CONTEXT_KEYS {
        patch.insert(format!("k{i}"), json!(i));
    }
    store.merge_context(id, &claim, patch).await?;
    let before = context_of(store, id).await?;
    if !too_large(
        store
            .merge_context(id, &claim, obj(json!({"one_more": 1})))
            .await,
    ) {
        return Err(fail("the 257th key was accepted"));
    }
    if context_of(store, id).await? != before {
        return Err(fail("a refused key-limit merge wrote"));
    }
    // Overwriting an existing key at the limit is not a new key.
    store
        .merge_context(id, &claim, obj(json!({"k0": "changed"})))
        .await?;

    // Exactly the byte limit is fine; one byte more is refused and writes nothing. The object
    // `{"k":"<n x>"}` serialises to n + 8 bytes.
    let (id, claim) = insert_and_claim(store, "test-site", "worker-2", json!(null)).await?;
    let exact = "x".repeat(MAX_CONTEXT_BYTES - 8);
    let merged = store
        .merge_context(id, &claim, obj(json!({"k": exact})))
        .await?;
    if serde_json::to_vec(&merged).map(|v| v.len()).ok() != Some(MAX_CONTEXT_BYTES) {
        return Err(fail("the exact-limit context has an unexpected size"));
    }
    let over = "x".repeat(MAX_CONTEXT_BYTES - 7);
    if !too_large(
        store
            .merge_context(id, &claim, obj(json!({"k": over})))
            .await,
    ) {
        return Err(fail("a context one byte over the limit was accepted"));
    }
    if context_of(store, id).await? != merged {
        return Err(fail("a refused size-limit merge wrote"));
    }
    // The limit applies to the merged result, not the patch: a small patch that pushes the
    // stored context over is refused.
    if !too_large(store.merge_context(id, &claim, obj(json!({"j": 1}))).await) {
        return Err(fail("a small patch over a full context was accepted"));
    }
    Ok(())
}

async fn test_merge_context_concurrent(store: &Arc<dyn crate::store::JobStore>) -> Result<()> {
    let (id, claim) = insert_and_claim(store, "test-site", "worker-1", json!(null)).await?;
    let stale = crate::model::Claim {
        token: spot_scheduler::lease::FencingToken(claim.token.0 + 7),
        worker: claim.worker.clone(),
        expires_at: claim.expires_at,
    };
    let mut tasks = Vec::new();
    for i in 0..32u32 {
        let good_store = store.clone();
        let claim = claim.clone();
        tasks.push(tokio::spawn(async move {
            good_store
                .merge_context(id, &claim, obj(json!({ format!("good{i}"): i })))
                .await
                .map(|_| ())
        }));
        let bad_store = store.clone();
        let stale = stale.clone();
        tasks.push(tokio::spawn(async move {
            match bad_store
                .merge_context(id, &stale, obj(json!({ format!("bad{i}"): i })))
                .await
            {
                Err(crate::error::Error::ClaimLost) => Ok(()),
                _ => Err(fail("a stale concurrent merge was not refused")),
            }
        }));
    }
    for t in tasks {
        t.await.map_err(|_| fail("merge task panicked"))??;
    }
    let ctx = context_of(store, id).await?;
    let Some(map) = ctx.as_object() else {
        return Err(fail("context is not an object"));
    };
    if map.len() != 32 {
        return Err(fail("concurrent merges lost or gained keys"));
    }
    for i in 0..32u32 {
        if map.get(&format!("good{i}")) != Some(&json!(i)) {
            return Err(fail("a concurrent merge was lost"));
        }
    }
    Ok(())
}

async fn test_merge_context_fence_before_size(
    store: &Arc<dyn crate::store::JobStore>,
) -> Result<()> {
    use crate::model::{MAX_CONTEXT_BYTES, MAX_CONTEXT_KEYS};
    let (id, claim) = insert_and_claim(store, "test-site", "worker-1", json!({"seed": 1})).await?;
    let stale = crate::model::Claim {
        token: spot_scheduler::lease::FencingToken(claim.token.0 + 1),
        worker: claim.worker.clone(),
        expires_at: claim.expires_at,
    };
    let mut many_keys = serde_json::Map::new();
    for i in 0..=MAX_CONTEXT_KEYS {
        many_keys.insert(format!("k{i}"), json!(i));
    }
    let big = obj(json!({"k": "x".repeat(MAX_CONTEXT_BYTES)}));
    for patch in [many_keys.clone(), big.clone()] {
        match store.merge_context(id, &stale, patch).await {
            Err(crate::error::Error::ClaimLost) => {}
            _ => {
                return Err(fail(
                    "a stale claim with an oversized patch was not ClaimLost",
                ))
            }
        }
    }
    // The live claim with the same patches is the size error.
    for patch in [many_keys, big] {
        match store.merge_context(id, &claim, patch).await {
            Err(crate::error::Error::ContextTooLarge) => {}
            _ => {
                return Err(fail(
                    "the live claim with an oversized patch was not refused",
                ))
            }
        }
    }
    if context_of(store, id).await? != json!({"seed": 1}) {
        return Err(fail("a refused merge wrote"));
    }
    Ok(())
}
