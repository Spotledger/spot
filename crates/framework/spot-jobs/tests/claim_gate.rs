//! The optional [`ClaimGate`]: the worker asks it before every claim and skips the tick when the
//! host says it no longer holds the right to consume the queues.
//!
//! Every test drives a real [`Worker`] against the in-memory store. Time is paused
//! (`tokio::time::start_paused`), so the worker's 100 ms claim interval advances deterministically
//! and the waits below are test time, not wall time.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(clippy::panic)]

use spot_core::SiteId;
use spot_jobs::model::{Claim, InsertResult, Job, JobId, JobState, NewJob};
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs::worker::{HandlerResult, JobHandlerRegistry};
use spot_jobs::{BoxFuture, ClaimGate, Error, InMemoryJobStore, QueuePolicy, Worker};
use spot_storage_contract::feed::FeedPosition;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::task::JoinHandle;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn site() -> SiteId {
    SiteId::new("test-site").expect("site id")
}

fn clock() -> Arc<dyn Fn() -> OffsetDateTime + Send + Sync> {
    Arc::new(OffsetDateTime::now_utc as fn() -> OffsetDateTime)
}

fn policies() -> HashMap<String, QueuePolicy> {
    let mut policies = HashMap::new();
    policies.insert(
        "default".to_string(),
        QueuePolicy::custom("default").build(),
    );
    policies
}

/// A registry whose `app:work` handler counts its runs and succeeds.
fn counting_registry(runs: Arc<AtomicU32>) -> Arc<JobHandlerRegistry> {
    let mut registry = JobHandlerRegistry::new();
    registry.register("app:work", move |_ctx| {
        let runs = runs.clone();
        Box::pin(async move {
            runs.fetch_add(1, Ordering::SeqCst);
            Ok::<HandlerResult, Error>(HandlerResult::Ok)
        })
    });
    Arc::new(registry)
}

/// Inserts `count` claimable `app:work` jobs on the `default` queue.
async fn seed(store: &dyn JobStore, count: u32) -> Vec<JobId> {
    let mut ids = Vec::new();
    for i in 0..count {
        let job = NewJob::new(
            "app:work".to_string(),
            serde_json::json!({ "i": i }),
            "default".to_string(),
            site(),
            OffsetDateTime::now_utc(),
        );
        match store.insert_if_new(job).await.expect("insert") {
            InsertResult::Inserted(id) => ids.push(id),
            InsertResult::Duplicate(_) => panic!("duplicate insert in a fresh store"),
        }
    }
    ids
}

/// Advances paused test time in steps until `done` holds, for at most 5 s of test time.
async fn wait_until(mut done: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    done()
}

/// A gate whose answer is a switch the test flips, counting how often the worker asked.
struct SwitchGate {
    permit_now: AtomicBool,
    calls: AtomicU32,
}

impl SwitchGate {
    fn new(permit_now: bool) -> Self {
        Self {
            permit_now: AtomicBool::new(permit_now),
            calls: AtomicU32::new(0),
        }
    }

    fn set(&self, permit_now: bool) {
        self.permit_now.store(permit_now, Ordering::SeqCst);
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ClaimGate for SwitchGate {
    fn permit<'a>(&'a self) -> BoxFuture<'a, bool> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let permitted = self.permit_now.load(Ordering::SeqCst);
        Box::pin(async move { permitted })
    }
}

/// A gate with a bug in it: it panics every time the worker asks.
struct PanickingGate {
    calls: AtomicU32,
}

impl PanickingGate {
    fn new() -> Self {
        Self {
            calls: AtomicU32::new(0),
        }
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

impl ClaimGate for PanickingGate {
    fn permit<'a>(&'a self) -> BoxFuture<'a, bool> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("claim gate bug: this gate always panics");
    }
}

/// The in-memory store with its `claim` calls counted, so a test can prove the gate was asked
/// before every claim attempt (1:1) and that a refusal never reached the store.
struct CountingStore {
    inner: InMemoryJobStore,
    claims: AtomicU32,
}

impl CountingStore {
    fn new() -> Self {
        Self {
            inner: InMemoryJobStore::new(),
            claims: AtomicU32::new(0),
        }
    }

    fn claims(&self) -> u32 {
        self.claims.load(Ordering::SeqCst)
    }
}

impl JobStore for CountingStore {
    fn insert_if_new(
        &self,
        job: NewJob,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<InsertResult>> + Send + '_>> {
        self.inner.insert_if_new(job)
    }

    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        now: OffsetDateTime,
        limit: u32,
    ) -> Pin<
        Box<dyn Future<Output = spot_jobs::Result<Vec<spot_jobs::store::ClaimedJob>>> + Send + '_>,
    > {
        self.claims.fetch_add(1, Ordering::SeqCst);
        self.inner.claim(queues, worker_id, ttl, now, limit)
    }

    fn heartbeat(
        &self,
        claim: &Claim,
        ttl: Duration,
        now: OffsetDateTime,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<()>> + Send + '_>> {
        self.inner.heartbeat(claim, ttl, now)
    }

    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<()>> + Send + '_>> {
        self.inner.complete(claim, new_state, last_error)
    }

    fn reap_expired(
        &self,
        now: OffsetDateTime,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<Vec<JobId>>> + Send + '_>> {
        self.inner.reap_expired(now)
    }

    fn cancel(
        &self,
        id: JobId,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<()>> + Send + '_>> {
        self.inner.cancel(id)
    }

    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<serde_json::Value>> + Send + '_>> {
        self.inner.merge_context(id, claim, patch)
    }

    fn get(
        &self,
        id: JobId,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<Option<Job>>> + Send + '_>> {
        self.inner.get(id)
    }

    fn list(
        &self,
        filter: JobFilter,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<Vec<Job>>> + Send + '_>> {
        self.inner.list(filter)
    }

    fn intake_position(
        &self,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<FeedPosition>> + Send + '_>> {
        self.inner.intake_position()
    }

    fn set_intake_position(
        &self,
        position: FeedPosition,
    ) -> Pin<Box<dyn Future<Output = spot_jobs::Result<()>> + Send + '_>> {
        self.inner.set_intake_position(position)
    }
}

/// Starts a worker on the `default` queue and returns its shutdown switch and task handle.
fn start(worker: Arc<Worker>) -> (watch::Sender<bool>, JoinHandle<()>) {
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let _ = worker
            .run(
                "test-worker",
                vec!["default".to_string()],
                rx,
                Duration::from_secs(2),
            )
            .await;
    });
    (tx, handle)
}

/// Sends the shutdown signal and waits (bounded) for the worker loop to end.
async fn stop(tx: watch::Sender<bool>, handle: JoinHandle<()>) {
    let _ = tx.send(true);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the worker loop did not stop on the shutdown signal")
        .expect("the worker task panicked");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// (a) Without a gate the worker claims and runs jobs exactly as before.
#[tokio::test(start_paused = true)]
async fn without_a_gate_jobs_are_claimed_and_run() {
    let store = Arc::new(CountingStore::new());
    let ids = seed(store.as_ref(), 3).await;
    let runs = Arc::new(AtomicU32::new(0));
    let worker = Arc::new(Worker::new(
        store.clone(),
        counting_registry(runs.clone()),
        policies(),
        clock(),
    ));

    let (tx, handle) = start(worker);
    assert!(
        wait_until(|| runs.load(Ordering::SeqCst) == 3).await,
        "all three jobs should run without a gate, ran {}",
        runs.load(Ordering::SeqCst)
    );
    stop(tx, handle).await;

    assert!(
        store.claims() > 0,
        "the store should have been asked for claims"
    );
    for id in ids {
        let job = store.get(id).await.expect("get").expect("job exists");
        assert_eq!(job.state, JobState::Succeeded);
    }
}

/// (b) A gate that answers `false` claims nothing: three pending jobs survive 500 ms of the
/// worker's own 100 ms claim interval untouched.
#[tokio::test(start_paused = true)]
async fn a_refusing_gate_claims_nothing() {
    let store = Arc::new(CountingStore::new());
    let ids = seed(store.as_ref(), 3).await;
    let runs = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(SwitchGate::new(false));
    let worker = Arc::new(
        Worker::new(
            store.clone(),
            counting_registry(runs.clone()),
            policies(),
            clock(),
        )
        .with_claim_gate(gate.clone()),
    );

    let (tx, handle) = start(worker);
    tokio::time::sleep(Duration::from_millis(500)).await;
    stop(tx, handle).await;

    assert_eq!(runs.load(Ordering::SeqCst), 0, "no job should have run");
    assert_eq!(store.claims(), 0, "the store must not be asked to claim");
    assert!(
        gate.calls() >= 3,
        "the gate should be asked once per tick while it refuses, asked {} times",
        gate.calls()
    );
    for id in ids {
        let job = store.get(id).await.expect("get").expect("job exists");
        assert_eq!(job.state, JobState::Pending);
        assert!(job.claim.is_none(), "no claim may be taken while refused");
    }
}

/// (c) When the gate flips to `true` the worker resumes claiming and the jobs run.
#[tokio::test(start_paused = true)]
async fn a_gate_that_permits_again_lets_the_jobs_run() {
    let store = Arc::new(CountingStore::new());
    let ids = seed(store.as_ref(), 3).await;
    let runs = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(SwitchGate::new(false));
    let worker = Arc::new(
        Worker::new(
            store.clone(),
            counting_registry(runs.clone()),
            policies(),
            clock(),
        )
        .with_claim_gate(gate.clone()),
    );

    let (tx, handle) = start(worker);
    // Refused for 300 ms of test time: nothing claimed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(store.claims(), 0, "nothing may be claimed while refused");

    // The host regains the lease; the worker is still running and picks the work back up.
    gate.set(true);
    assert!(
        wait_until(|| runs.load(Ordering::SeqCst) == 3).await,
        "the three jobs should run once the gate permits, ran {}",
        runs.load(Ordering::SeqCst)
    );
    stop(tx, handle).await;

    for id in ids {
        let job = store.get(id).await.expect("get").expect("job exists");
        assert_eq!(job.state, JobState::Succeeded);
    }
}

/// (d) The gate is asked exactly once per claim attempt: while it permits, gate calls and store
/// claim calls are 1:1.
#[tokio::test(start_paused = true)]
async fn the_gate_is_asked_once_per_claim_attempt() {
    let store = Arc::new(CountingStore::new());
    let runs = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(SwitchGate::new(true));
    let worker = Arc::new(
        Worker::new(store.clone(), counting_registry(runs), policies(), clock())
            .with_claim_gate(gate.clone()),
    );

    let (tx, handle) = start(worker);
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop(tx, handle).await;

    assert!(gate.calls() > 0, "the gate should have been asked");
    assert_eq!(
        gate.calls(),
        store.claims(),
        "every claim attempt must be preceded by exactly one gate call"
    );
}

/// (e) A panicking gate counts as a refusal and the worker keeps running (it is a host bug, not a
/// reason to exit the loop).
#[tokio::test(start_paused = true)]
async fn a_panicking_gate_counts_as_false_and_the_worker_keeps_running() {
    let store = Arc::new(CountingStore::new());
    let ids = seed(store.as_ref(), 3).await;
    let runs = Arc::new(AtomicU32::new(0));
    let gate = Arc::new(PanickingGate::new());
    let worker = Arc::new(
        Worker::new(
            store.clone(),
            counting_registry(runs.clone()),
            policies(),
            clock(),
        )
        .with_claim_gate(gate.clone()),
    );

    let (tx, handle) = start(worker);
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(!handle.is_finished(), "the worker must not exit on a panic");
    assert_eq!(runs.load(Ordering::SeqCst), 0, "no job should have run");
    assert_eq!(store.claims(), 0, "the store must not be asked to claim");
    assert!(
        gate.calls() >= 3,
        "the loop should keep asking after the panic, asked {} times",
        gate.calls()
    );
    for id in ids {
        let job = store.get(id).await.expect("get").expect("job exists");
        assert_eq!(job.state, JobState::Pending);
    }

    // The loop is still healthy enough to shut down cleanly.
    stop(tx, handle).await;
}
