//! The job worker's claim gate over the coordinator lease: a node claims jobs only
//! while its fencing token is still the current one, and the answer is cached so the claim tick
//! (10 a second) does not make a coordinator round trip each time.
//!
//! The worker is the one the host builds (`build_worker`) with the gate the host attaches; the
//! coordinator is the in-process one on a manual clock, so the gate's cache moves without
//! sleeping; the worker's own 100 ms claim tick runs in real time, so each wait is a short sleep.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use spot_cli::services::jobs::{FinalAttemptHooks, Keeper, LeaseClaimGate, build_worker};
use spot_coordinator::{
    Admit, BoxFuture, Coordinator, FencingToken, InMemoryPresence, InProcessCoordinator, Lease,
    NodeId, RateLimit, Result as CoordResult,
};
use spot_core::{ManualClock, SiteId};
use spot_jobs::model::{InsertResult, NewJob};
use spot_jobs::store::JobStore;
use spot_jobs::worker::{HandlerResult, JobHandlerRegistry};
use spot_jobs::{ClaimGate, Error, InMemoryJobStore, QueuePolicy};
use time::OffsetDateTime;
use tokio::sync::{Notify, watch};

const LEASE: &str = "serve:acme:jobs";
const TTL: Duration = Duration::from_secs(3);

/// Counts `check_fence` calls and delegates everything to the inner coordinator.
struct Counting {
    inner: Arc<dyn Coordinator>,
    fences: AtomicU32,
}

impl Coordinator for Counting {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, CoordResult<Lease>> {
        self.inner.acquire(name, holder, ttl)
    }
    fn renew<'a>(&'a self, lease: &'a Lease, ttl: Duration) -> BoxFuture<'a, CoordResult<Lease>> {
        self.inner.renew(lease, ttl)
    }
    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, CoordResult<()>> {
        self.inner.release(lease)
    }
    fn current<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<Option<Lease>>> {
        self.inner.current(name)
    }
    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: FencingToken,
    ) -> BoxFuture<'a, CoordResult<()>> {
        self.fences.fetch_add(1, Ordering::SeqCst);
        self.inner.check_fence(name, token)
    }
    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.next(name, by)
    }
    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.peek(name)
    }
    fn set_at_least<'a>(&'a self, name: &'a str, value: u64) -> BoxFuture<'a, CoordResult<u64>> {
        self.inner.set_at_least(name, value)
    }
    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, CoordResult<Admit>> {
        self.inner.take(key, limit, cost)
    }
    fn waker(&self) -> Arc<Notify> {
        self.inner.waker()
    }
}

struct Fixture {
    coordinator: Arc<Counting>,
    clock: Arc<ManualClock>,
    keeper: Arc<Keeper>,
    gate: Arc<LeaseClaimGate>,
}

/// A coordinator, a keeper that already holds the lease as `node-1`, and the gate over it.
async fn fixture() -> Fixture {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let inner: Arc<dyn Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        clock.clone(),
    )
    .await
    .unwrap();
    let coordinator = Arc::new(Counting {
        inner,
        fences: AtomicU32::new(0),
    });
    let keeper = Arc::new(Keeper::new(
        coordinator.clone(),
        LEASE.to_owned(),
        "node-1".to_owned(),
        TTL,
    ));
    assert!(keeper.acquire().await, "the first acquire takes the lease");
    let gate = Arc::new(LeaseClaimGate::new(keeper.clone(), clock.clone()));
    Fixture {
        coordinator,
        clock,
        keeper,
        gate,
    }
}

fn site() -> SiteId {
    SiteId::new("acme").unwrap()
}

async fn seed(store: &dyn JobStore, count: u32, tag: &str) {
    for i in 0..count {
        let job = NewJob::new(
            "app:work".to_owned(),
            serde_json::json!({ "tag": tag, "i": i }),
            "default".to_owned(),
            site(),
            OffsetDateTime::now_utc(),
        );
        assert!(matches!(
            store.insert_if_new(job).await.unwrap(),
            InsertResult::Inserted(_)
        ));
    }
}

fn registry(runs: Arc<AtomicU32>) -> Arc<JobHandlerRegistry> {
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

/// Starts the host's worker with the gate; returns the stop switch and the task.
fn start(
    store: Arc<dyn JobStore>,
    runs: Arc<AtomicU32>,
    gate: Arc<LeaseClaimGate>,
) -> (watch::Sender<bool>, tokio::task::JoinHandle<()>) {
    let mut policies = HashMap::new();
    policies.insert("default".to_owned(), QueuePolicy::custom("default").build());
    let worker = build_worker(
        store,
        registry(runs),
        policies,
        Arc::new(spot_core::SystemClock::new()),
        FinalAttemptHooks::new(),
    )
    .with_claim_gate(gate);
    let (tx, rx) = watch::channel(false);
    let handle = tokio::spawn(async move {
        let _ = worker
            .run(
                "node-1:acme",
                vec!["default".to_owned()],
                rx,
                Duration::from_secs(1),
            )
            .await;
    });
    (tx, handle)
}

async fn stop(tx: watch::Sender<bool>, handle: tokio::task::JoinHandle<()>) {
    let _ = tx.send(true);
    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the worker stops")
        .expect("the worker task did not panic");
}

/// Lets a few claim ticks pass: sleeps of at most 100 ms, `ticks` times.
async fn pass_ticks(ticks: u32) {
    for _ in 0..ticks {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for(runs: &AtomicU32, want: u32) -> bool {
    for _ in 0..100 {
        if runs.load(Ordering::SeqCst) >= want {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    runs.load(Ordering::SeqCst) >= want
}

/// (a) The holder claims and runs its jobs.
#[tokio::test]
async fn the_lease_holder_claims_and_runs_jobs() {
    let f = fixture().await;
    let store: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
    seed(store.as_ref(), 3, "a").await;
    let runs = Arc::new(AtomicU32::new(0));
    let (tx, handle) = start(store, runs.clone(), f.gate.clone());
    assert!(
        wait_for(&runs, 3).await,
        "ran {}",
        runs.load(Ordering::SeqCst)
    );
    stop(tx, handle).await;
}

/// (b) Once another node holds the lease, no further job is claimed within one cache period,
/// although pending jobs exist; the first node's gate stays closed afterwards.
#[tokio::test]
async fn a_deposed_node_claims_nothing_more() {
    let f = fixture().await;
    let store: Arc<dyn JobStore> = Arc::new(InMemoryJobStore::new());
    seed(store.as_ref(), 1, "before").await;
    let runs = Arc::new(AtomicU32::new(0));
    let (tx, handle) = start(store.clone(), runs.clone(), f.gate.clone());
    assert!(wait_for(&runs, 1).await);

    // Another node takes the lease over: node-1's token is now stale.
    let mine = f.keeper_lease().await;
    f.coordinator.release(&mine).await.unwrap();
    let other = f
        .coordinator
        .acquire(LEASE, "node-2", TTL)
        .await
        .expect("the other node takes the released lease");
    assert!(other.token > mine.token);

    // One cache period later the gate looks again and closes.
    f.clock.advance(f.gate.period());
    seed(store.as_ref(), 3, "after").await;
    pass_ticks(6).await;
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "a node that lost the lease must not claim another job"
    );
    // The refusal sticks without more coordinator calls and without the lease coming back.
    let fences = f.coordinator.fences.load(Ordering::SeqCst);
    f.clock.advance(Duration::from_secs(1));
    pass_ticks(6).await;
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    assert_eq!(f.coordinator.fences.load(Ordering::SeqCst), fences);
    assert!(!f.keeper.still_held().await);
    stop(tx, handle).await;
}

/// (c) Many claim ticks make at most one `check_fence` per cache period.
#[tokio::test]
async fn the_cache_bounds_coordinator_calls() {
    let f = fixture().await;
    assert_eq!(f.gate.period(), Duration::from_millis(300));
    for _ in 0..50 {
        assert!(f.gate.permit().await);
    }
    assert_eq!(f.coordinator.fences.load(Ordering::SeqCst), 1);
    // Still inside the period: no new call.
    f.clock.advance(Duration::from_millis(299));
    for _ in 0..50 {
        assert!(f.gate.permit().await);
    }
    assert_eq!(f.coordinator.fences.load(Ordering::SeqCst), 1);
    // Past it: exactly one more.
    f.clock.advance(Duration::from_millis(1));
    for _ in 0..50 {
        assert!(f.gate.permit().await);
    }
    assert_eq!(f.coordinator.fences.load(Ordering::SeqCst), 2);
}

/// A released lease closes the gate at once, whatever the cache holds; a renewal refreshes it.
#[tokio::test]
async fn release_closes_the_gate_and_renewal_refreshes_it() {
    let f = fixture().await;
    assert!(f.gate.permit().await);
    assert!(f.keeper.renew().await);
    f.gate.renewed();
    assert!(f.gate.permit().await);
    assert_eq!(f.coordinator.fences.load(Ordering::SeqCst), 1);
    f.keeper.release().await;
    f.gate.cleared();
    assert!(!f.gate.permit().await, "no lease, no claim");
}

/// The cache period is a tenth of the time to live, between 50 ms and 500 ms.
#[test]
fn the_period_is_clamped() {
    use spot_cli::services::jobs::claim_gate_period as p;
    assert_eq!(p(Duration::from_millis(100)), Duration::from_millis(50));
    assert_eq!(p(Duration::from_secs(3)), Duration::from_millis(300));
    assert_eq!(p(Duration::from_secs(60)), Duration::from_millis(500));
}

impl Fixture {
    /// The lease node-1 holds, read back from the coordinator.
    async fn keeper_lease(&self) -> Lease {
        self.coordinator.current(LEASE).await.unwrap().unwrap()
    }
}
