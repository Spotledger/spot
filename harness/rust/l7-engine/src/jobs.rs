//! The job-engine workload (WP-TST-07.T2): idempotent jobs enqueued from document events,
//! processed by workers that fail at random and are killed mid-handler, and the invariants that
//! prove no job is lost and no idempotency key takes effect twice.
//!
//! It uses the real job engine (`spot-jobs`) over its in-memory store; the PostgreSQL store
//! joins in a later task. A "worker kill" is modelled by claiming a job and then abandoning the
//! claim without completing: the lease expires, `reap_expired` returns the job to `Retrying`,
//! and another worker takes it.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use serde_json::json;
use spot_core::SiteId;
use spot_jobs::model::InsertResult;
use spot_jobs::store::JobFilter;
use spot_jobs::{InMemoryJobStore, JobId, JobState, JobStore, NewJob};
use spot_l7_invariants::history::History;
use spot_l7_invariants::invariant::{
    BoxFuture, CheckContext, Invariant, InvariantRegistry, Report, RunMeta, StoreAccess, Violation,
};
use spot_l7_invariants::oracle::Oracle;
use spot_l7_invariants::schedule::Rng;
use spot_storage_contract::{
    Catalog, Change, OutboxRecord, Query, Rows, StorageError, StorageResult,
};
use time::Duration as TimeDuration;
use time::OffsetDateTime;

/// Id of [`ExactlyOneEffect`].
pub const JOBS_EXACTLY_ONE_EFFECT: &str = "jobs-exactly_one_effect_per_idempotency_key";
/// Id of [`NoLostJob`].
pub const JOBS_NO_LOST_JOB: &str = "jobs-no_lost_job";
/// Id of [`RetryBounds`].
pub const JOBS_RETRY_BOUNDS: &str = "jobs-retry_bounds";

/// How a job run is configured.
#[derive(Clone, Debug)]
pub struct JobConfig {
    /// The seed.
    pub seed: u64,
    /// Jobs to enqueue.
    pub jobs: u64,
    /// One in this many handler runs fails and retries.
    pub fail_one_in: u64,
    /// One in this many handler runs is killed (the claim is abandoned).
    pub kill_one_in: u64,
    /// The profile name.
    pub profile: String,
}

impl Default for JobConfig {
    fn default() -> Self {
        JobConfig {
            seed: 7_700_007,
            jobs: 200,
            fail_one_in: 3,
            kill_one_in: 5,
            profile: "smoke".to_owned(),
        }
    }
}

/// The final state of one job.
#[derive(Clone, Debug)]
pub struct JobOutcome {
    /// The idempotency key.
    pub key: String,
    /// The final state.
    pub state: JobState,
    /// The attempts it took.
    pub attempts: u32,
    /// The maximum attempts allowed.
    pub max_attempts: u32,
}

/// The exact expected state of a job run.
#[derive(Clone, Debug, Default)]
pub struct JobRun {
    /// Jobs the store inserted (unique keys).
    pub inserted: Vec<JobId>,
    /// Enqueues the store deduplicated.
    pub duplicates: u64,
    /// The final state of every job.
    pub outcomes: Vec<JobOutcome>,
    /// Effects applied per idempotency key: exactly one per succeeded job.
    pub effects: BTreeMap<String, u64>,
}

impl JobRun {
    /// The keys with an effect.
    pub fn effect_keys(&self) -> Vec<&String> {
        self.effects.keys().collect()
    }
}

struct Shared {
    effects: Mutex<BTreeMap<String, u64>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs the job workload and returns its oracle.
pub async fn run_jobs(config: JobConfig) -> JobRun {
    let site = SiteId::new("l7-engine").unwrap_or_else(|e| panic!("a site: {e:?}"));
    let store = Arc::new(InMemoryJobStore::new());
    let shared = Arc::new(Shared {
        effects: Mutex::new(BTreeMap::new()),
    });
    let mut rng = Rng::new(config.seed);

    // Enqueue every job twice: the second enqueue of a key is a duplicate while the first is
    // active, exactly as an at-least-once document event stream would deliver it.
    let mut inserted = Vec::new();
    let mut duplicates = 0u64;
    let mut keys: BTreeMap<JobId, String> = BTreeMap::new();
    for i in 0..config.jobs {
        let key = format!("job-{i}");
        for _ in 0..2 {
            let job = NewJob::new(
                "l7:work".to_owned(),
                json!({"i": i}),
                "default".to_owned(),
                site.clone(),
                OffsetDateTime::now_utc(),
            )
            .with_idempotency_key(key.clone());
            match store
                .insert_if_new(job)
                .await
                .unwrap_or_else(|e| panic!("insert: {e:?}"))
            {
                InsertResult::Inserted(id) => {
                    inserted.push(id);
                    keys.insert(id, key.clone());
                }
                InsertResult::Duplicate(_) => duplicates += 1,
            }
        }
    }

    // The worker loop: claim, process, reap; it ends when nothing is claimable.
    let ttl = Duration::from_secs(30);
    let queues = vec!["default".to_owned()];
    let mut rounds = 0u32;
    loop {
        rounds += 1;
        if rounds > 100_000 {
            break;
        }
        let now = OffsetDateTime::now_utc();
        // A claim that was abandoned is reaped and returns to `Retrying`.
        let _ = store
            .reap_expired(now + ttl + TimeDuration::seconds(1))
            .await
            .unwrap_or_else(|e| panic!("reap: {e:?}"));
        let claimed = store
            .claim(&queues, "l7-worker", ttl, now, 32)
            .await
            .unwrap_or_else(|e| panic!("claim: {e:?}"));
        if claimed.is_empty() {
            break;
        }
        for c in claimed {
            let next = c.job.attempt + 1;
            let max = c.job.max_attempts;
            let key = c
                .job
                .idempotency_key
                .clone()
                .unwrap_or_else(|| c.job.id.to_string());
            if next > max {
                // The retry budget is spent: dead-letter it.
                let _ = store
                    .complete(
                        &c.claim,
                        JobState::Dead,
                        Some("retry budget spent".to_owned()),
                    )
                    .await;
                continue;
            }
            let killed = rng.chance(1, config.kill_one_in.max(1));
            let fails = !killed && rng.chance(1, config.fail_one_in.max(1));
            if killed || fails {
                // The attempt is abandoned: a killed worker, or a handler that failed. The lease
                // expires and `reap_expired` returns the job to `Retrying`, bumping its attempt.
                continue;
            }
            // The effect is applied only on the path that completes successfully.
            *lock(&shared.effects).entry(key).or_insert(0) += 1;
            let _ = store.complete(&c.claim, JobState::Succeeded, None).await;
        }
    }

    // Read the final state of every job.
    let jobs = store
        .list(JobFilter::default())
        .await
        .unwrap_or_else(|e| panic!("list: {e:?}"));
    let mut outcomes = Vec::new();
    for job in &jobs {
        outcomes.push(JobOutcome {
            key: job
                .idempotency_key
                .clone()
                .unwrap_or_else(|| job.id.to_string()),
            state: job.state.clone(),
            attempts: job.attempt,
            max_attempts: job.max_attempts,
        });
    }
    JobRun {
        inserted,
        duplicates,
        outcomes,
        effects: std::mem::take(&mut *lock(&shared.effects)),
    }
}

// ---------------------------------------------------------------------------------------------
// Invariants
// ---------------------------------------------------------------------------------------------

macro_rules! job_invariant {
    ($ty:ty, $id:expr, $desc:expr) => {
        impl Invariant for $ty {
            fn id(&self) -> &str {
                $id
            }
            fn describe(&self) -> &str {
                $desc
            }
            fn check<'a>(&'a self, _ctx: &'a CheckContext) -> BoxFuture<'a, Vec<Violation>> {
                let violations = self.check_run();
                Box::pin(async move { violations })
            }
        }
    };
}

/// `jobs-exactly_one_effect_per_idempotency_key`.
pub struct ExactlyOneEffect {
    run: Arc<JobRun>,
}

impl ExactlyOneEffect {
    /// The invariant over a job run.
    pub fn new(run: Arc<JobRun>) -> ExactlyOneEffect {
        ExactlyOneEffect { run }
    }

    fn check_run(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        for (key, count) in &self.run.effects {
            if *count != 1 {
                out.push(Violation::new(
                    JOBS_EXACTLY_ONE_EFFECT,
                    "job",
                    key,
                    format!("effects expected=1 actual={count}"),
                ));
            }
        }
        for outcome in &self.run.outcomes {
            if matches!(outcome.state, JobState::Succeeded)
                && !self.run.effects.contains_key(&outcome.key)
            {
                out.push(Violation::new(
                    JOBS_EXACTLY_ONE_EFFECT,
                    "job",
                    &outcome.key,
                    "succeeded_without_effect",
                ));
            }
        }
        out
    }
}

job_invariant!(
    ExactlyOneEffect,
    JOBS_EXACTLY_ONE_EFFECT,
    "Every idempotency key took effect exactly once: a key never applied twice (no double \
     processing across retries, reaps or a killed worker), and every job that succeeded has an \
     effect."
);

/// `jobs-no_lost_job`.
pub struct NoLostJob {
    run: Arc<JobRun>,
}

impl NoLostJob {
    /// The invariant over a job run.
    pub fn new(run: Arc<JobRun>) -> NoLostJob {
        NoLostJob { run }
    }

    fn check_run(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        if self.run.outcomes.len() < self.run.inserted.len() {
            out.push(Violation::new(
                JOBS_NO_LOST_JOB,
                "job",
                "-",
                format!(
                    "lost_jobs expected={} actual={}",
                    self.run.inserted.len(),
                    self.run.outcomes.len()
                ),
            ));
        }
        for outcome in &self.run.outcomes {
            match &outcome.state {
                JobState::Succeeded | JobState::Dead => {}
                other => out.push(Violation::new(
                    JOBS_NO_LOST_JOB,
                    "job",
                    &outcome.key,
                    format!("not_terminal state={other}"),
                )),
            }
        }
        out
    }
}

job_invariant!(
    NoLostJob,
    JOBS_NO_LOST_JOB,
    "Every enqueued job reached a terminal state (succeeded or dead-lettered): none was lost to \
     a failed handler, a killed worker or a lease that was never renewed."
);

/// `jobs-retry_bounds`.
pub struct RetryBounds {
    run: Arc<JobRun>,
}

impl RetryBounds {
    /// The invariant over a job run.
    pub fn new(run: Arc<JobRun>) -> RetryBounds {
        RetryBounds { run }
    }

    fn check_run(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        for outcome in &self.run.outcomes {
            if outcome.attempts > outcome.max_attempts {
                out.push(Violation::new(
                    JOBS_RETRY_BOUNDS,
                    "job",
                    &outcome.key,
                    format!(
                        "attempts_over_bound attempts={} max={}",
                        outcome.attempts, outcome.max_attempts
                    ),
                ));
            }
            if matches!(outcome.state, JobState::Dead) && outcome.attempts != outcome.max_attempts {
                out.push(Violation::new(
                    JOBS_RETRY_BOUNDS,
                    "job",
                    &outcome.key,
                    format!(
                        "dead_wrong_bound attempts={} max={}",
                        outcome.attempts, outcome.max_attempts
                    ),
                ));
            }
        }
        out
    }
}

job_invariant!(
    RetryBounds,
    JOBS_RETRY_BOUNDS,
    "No job ran more attempts than its maximum, and a dead-lettered job reached exactly its \
     maximum: retries are bounded and a job is only given up on when the bound is spent."
);

/// The job invariants on a registry.
pub fn job_registry(run: &Arc<JobRun>) -> InvariantRegistry {
    let mut registry = InvariantRegistry::new();
    let _ = registry.register(Box::new(ExactlyOneEffect::new(run.clone())));
    let _ = registry.register(Box::new(NoLostJob::new(run.clone())));
    let _ = registry.register(Box::new(RetryBounds::new(run.clone())));
    registry
}

/// Checks the job invariants (they read the run's oracle, not the store).
pub async fn check_jobs(run: &Arc<JobRun>, catalog: Arc<Catalog>, meta: RunMeta) -> Report {
    let registry = job_registry(run);
    let ctx = CheckContext::new(
        Arc::new(NoStore),
        catalog,
        Arc::new(History::new(meta.seed, meta.workers, &meta.profile)),
        Arc::new(Oracle::new()),
    );
    registry.check_all(&ctx, meta).await
}

/// A store the job invariants never read: any access is an internal error.
struct NoStore;

fn no_store() -> StorageError {
    StorageError::new(
        spot_storage_contract::ErrorKind::Internal,
        "the job invariants do not read the store",
    )
}

impl StoreAccess for NoStore {
    fn query<'a>(&'a self, _query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async { Err(no_store()) })
    }
    fn outbox(&self) -> BoxFuture<'_, StorageResult<Vec<OutboxRecord>>> {
        Box::pin(async { Err(no_store()) })
    }
    fn changes(&self) -> BoxFuture<'_, StorageResult<Vec<Change>>> {
        Box::pin(async { Err(no_store()) })
    }
    fn next_number<'a>(&'a self, _series: &'a str) -> BoxFuture<'a, StorageResult<u64>> {
        Box::pin(async { Err(no_store()) })
    }
}
