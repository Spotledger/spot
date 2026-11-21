//! WP-AI-07.T6 and T13: the jobs part of the world over an in-memory job store (through the
//! `JobAdmin` port), and the retrieval-index part's durable generation. The part's ops block on the
//! runtime handle, so every op runs on a blocking thread, as the remedy host calls them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture, StopGate, StoreJobAdmin};
use spot_cli::services::world_access::index::{
    FileGeneration, INDEX_NAME, IndexLag, LagFuture, RetrievalIndexPart, generation_path,
};
use spot_cli::services::world_access::jobs::{StoreJobsPart, from_admin, from_parts, life_of};
use spot_cli::services::world_access::parts::{IndexPart, JobsPart};
use spot_core::{Clock, ManualClock, SiteId};
use spot_jobs::model::{Claim, InsertResult, Job, JobId, JobState, NewJob};
use spot_jobs::store::{ClaimedJob, JobFilter};
use spot_jobs::{CancelOutcome, InMemoryJobStore, JobStore};
use spot_remedies::{IndexRef, JobLife, JobOps, JobRef};
use spot_storage_contract::FeedPosition;
use time::OffsetDateTime;
use tokio::runtime::Handle;

const T0: i64 = 1_795_262_400;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

type Fut<'a, T> = Pin<Box<dyn Future<Output = spot_jobs::Result<T>> + Send + 'a>>;

/// An in-memory store that counts every call that writes. When `claim_after_get` is armed, the
/// next `get` answers and then a worker claims the one claimable job of the `default` queue: the
/// gap a read-then-cancel would lose a race in.
struct Counting {
    inner: InMemoryJobStore,
    writes: AtomicUsize,
    claim_after_get: AtomicBool,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryJobStore::new(),
            writes: AtomicUsize::new(0),
            claim_after_get: AtomicBool::new(false),
        })
    }

    fn writes(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    fn wrote(&self) {
        self.writes.fetch_add(1, Ordering::SeqCst);
    }
}

impl JobStore for Counting {
    fn insert_if_new(&self, job: NewJob) -> Fut<'_, InsertResult> {
        self.wrote();
        self.inner.insert_if_new(job)
    }

    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        now: OffsetDateTime,
        limit: u32,
    ) -> Fut<'_, Vec<ClaimedJob>> {
        self.wrote();
        self.inner.claim(queues, worker_id, ttl, now, limit)
    }

    fn heartbeat(&self, claim: &Claim, ttl: Duration, now: OffsetDateTime) -> Fut<'_, ()> {
        self.wrote();
        self.inner.heartbeat(claim, ttl, now)
    }

    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> Fut<'_, ()> {
        self.wrote();
        self.inner.complete(claim, new_state, last_error)
    }

    fn reap_expired(&self, now: OffsetDateTime) -> Fut<'_, Vec<JobId>> {
        self.wrote();
        self.inner.reap_expired(now)
    }

    fn cancel(&self, id: JobId) -> Fut<'_, ()> {
        self.wrote();
        self.inner.cancel(id)
    }

    fn cancel_if_pending(&self, id: JobId) -> Fut<'_, CancelOutcome> {
        self.wrote();
        self.inner.cancel_if_pending(id)
    }

    fn requeue(&self, id: JobId, now: OffsetDateTime) -> Fut<'_, Job> {
        self.wrote();
        self.inner.requeue(id, now)
    }

    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> Fut<'_, serde_json::Value> {
        self.wrote();
        self.inner.merge_context(id, claim, patch)
    }

    fn get(&self, id: JobId) -> Fut<'_, Option<Job>> {
        Box::pin(async move {
            let job = self.inner.get(id).await?;
            if self.claim_after_get.swap(false, Ordering::SeqCst) {
                self.inner
                    .claim(
                        &["default".to_owned()],
                        "worker-in-the-gap",
                        Duration::from_secs(3_600),
                        at(T0 + 1),
                        10,
                    )
                    .await?;
            }
            Ok(job)
        })
    }

    fn list(&self, filter: JobFilter) -> Fut<'_, Vec<Job>> {
        self.inner.list(filter)
    }

    fn intake_position(&self) -> Fut<'_, FeedPosition> {
        self.inner.intake_position()
    }

    fn set_intake_position(&self, position: FeedPosition) -> Fut<'_, ()> {
        self.wrote();
        self.inner.set_intake_position(position)
    }
}

fn site() -> SiteId {
    SiteId::new("site-1").unwrap()
}

async fn enqueue(store: &Counting, site: &SiteId) -> JobId {
    let job = NewJob::new(
        "app:test".to_owned(),
        serde_json::json!({}),
        "default".to_owned(),
        site.clone(),
        at(T0),
    );
    match store.insert_if_new(job).await.unwrap() {
        InsertResult::Inserted(id) => id,
        InsertResult::Duplicate(_) => panic!("duplicate"),
    }
}

/// Claims the one pending job with `ttl` at `T0`.
async fn claim_one(store: &Counting, ttl: u64) -> Claim {
    let claimed = store
        .claim(
            &["default".to_owned()],
            "worker-1",
            Duration::from_secs(ttl),
            at(T0),
            1,
        )
        .await
        .unwrap();
    claimed[0].claim.clone()
}

struct Seeded {
    store: Arc<Counting>,
    part: Arc<StoreJobsPart>,
    dead: JobId,
    stale_running: JobId,
    live_running: JobId,
}

/// One dead job, one running job whose lease lapsed at T0+30 and one whose lease runs to T0+3600.
async fn seeded() -> Seeded {
    let store = Counting::new();
    let site = site();
    let dead = enqueue(&store, &site).await;
    let claim = claim_one(&store, 30).await;
    store
        .complete(&claim, JobState::Dead, Some("boom".to_owned()))
        .await
        .unwrap();
    let stale_running = enqueue(&store, &site).await;
    claim_one(&store, 30).await;
    let live_running = enqueue(&store, &site).await;
    claim_one(&store, 3_600).await;
    let part = Arc::new(StoreJobsPart::new(
        Arc::clone(&store) as Arc<dyn JobStore>,
        site,
        Handle::current(),
    ));
    Seeded {
        store,
        part,
        dead,
        stale_running,
        live_running,
    }
}

fn jref(id: JobId) -> JobRef {
    JobRef::new(&id.to_string())
}

/// Runs `work` over the live ops on a blocking thread.
async fn live<R: Send + 'static>(
    part: &Arc<StoreJobsPart>,
    work: impl FnOnce(&mut dyn JobOps) -> R + Send + 'static,
) -> R {
    let part = Arc::clone(part);
    tokio::task::spawn_blocking(move || {
        let mut ops = part.live();
        work(&mut *ops)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_and_running_list_from_the_store() {
    let s = seeded().await;
    let (dead, running) = (jref(s.dead), [jref(s.stale_running), jref(s.live_running)]);
    let (failed_now, mut running_now) = live(&s.part, |ops| (ops.failed(), ops.running())).await;
    assert_eq!(failed_now, vec![dead]);
    running_now.sort();
    assert_eq!(running_now, running.to_vec());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_view_carries_state_attempts_and_lease_and_other_sites_are_invisible() {
    let s = seeded().await;
    let other = SiteId::new("site-2").unwrap();
    let foreign = enqueue(&s.store, &other).await;
    let (stale, dead, missing, foreign_view, garbage) = {
        let (a, b, c, d) = (
            jref(s.stale_running),
            jref(s.dead),
            JobRef::new("999"),
            jref(foreign),
        );
        live(&s.part, move |ops| {
            (
                ops.job(&a),
                ops.job(&b),
                ops.job(&c),
                ops.job(&d),
                ops.job(&JobRef::new("not-a-number")),
            )
        })
        .await
    };
    let stale = stale.unwrap();
    assert_eq!(stale.life, JobLife::Running);
    assert_eq!(stale.lease_expires_at, Some(T0 + 30));
    assert_eq!(dead.unwrap().life, JobLife::Failed);
    assert!(missing.is_none());
    assert!(foreign_view.is_none(), "another site's job is not ours");
    assert!(garbage.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstick_refuses_an_unexpired_lease_and_changes_nothing() {
    let s = seeded().await;
    let writes = s.store.writes();
    let id = jref(s.live_running);
    let refused = live(&s.part, move |ops| ops.unstick(&id, T0 + 100)).await;
    let error = refused.unwrap_err();
    assert!(error.detail.contains("has not expired"), "{error}");
    assert_eq!(s.store.writes(), writes, "nothing was written");
    let job = s.store.get(s.live_running).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unstick_releases_an_expired_lease_through_the_store() {
    let s = seeded().await;
    let id = jref(s.stale_running);
    let change = live(&s.part, move |ops| ops.unstick(&id, T0 + 100))
        .await
        .unwrap();
    assert_eq!(change.before, JobLife::Running);
    assert_eq!(change.after, JobLife::Queued);
    let job = s.store.get(s.stale_running).await.unwrap().unwrap();
    assert_eq!(life_of(&job.state), JobLife::Queued);
    assert!(matches!(job.state, JobState::Retrying { .. }));
    // The job whose lease had not lapsed was left running.
    let other = s.store.get(s.live_running).await.unwrap().unwrap();
    assert_eq!(other.state, JobState::Running);
    // A job that is not running is refused.
    let dead = jref(s.dead);
    let refused = live(&s.part, move |ops| ops.unstick(&dead, T0 + 100)).await;
    assert!(refused.unwrap_err().detail.contains("not running"));
}

// The T6 test `retry_and_a_real_restore_are_not_supported_by_the_port_and_write_nothing` asserted
// that retry and restore answered "not supported by the jobs port". Since T13 the port requeues,
// so retry and the undo of a retry work (below); only a restore to a state other than failed (the
// undo of unstick) is still "not supported", and a same-state restore is still a no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restore_to_another_state_is_not_supported_and_a_same_state_restore_is_a_no_op() {
    let s = seeded().await;
    let writes = s.store.writes();
    let (dead, stale) = (jref(s.dead), jref(s.stale_running));
    let (restore_other, restore_same) = live(&s.part, move |ops| {
        (
            ops.restore(&dead, JobLife::Queued),
            // A restore to the state the job already has is a no-op, not a refusal.
            ops.restore(&stale, JobLife::Running),
        )
    })
    .await;
    assert_eq!(
        restore_other.unwrap_err().detail,
        "not supported by the jobs port"
    );
    assert!(restore_same.is_ok());
    assert_eq!(s.store.writes(), writes);
    assert_eq!(
        s.store.get(s.dead).await.unwrap().unwrap().state,
        JobState::Dead
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_requeues_a_dead_job_through_the_port() {
    let s = seeded().await;
    let id = jref(s.dead);
    let change = live(&s.part, move |ops| ops.retry(&id, T0 + 5))
        .await
        .unwrap();
    assert_eq!(
        (change.before, change.after),
        (JobLife::Failed, JobLife::Queued)
    );
    let job = s.store.get(s.dead).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.attempt, 0);
    assert_eq!(job.not_before, at(T0 + 5));
    assert_eq!(job.last_error.as_deref(), Some("boom"), "the history stays");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_refuses_a_job_that_is_not_dead_and_writes_nothing() {
    let s = seeded().await;
    let writes = s.store.writes();
    let (running, missing) = (jref(s.live_running), JobRef::new("999"));
    let (refused, none) = live(&s.part, move |ops| {
        (ops.retry(&running, T0), ops.retry(&missing, T0))
    })
    .await;
    assert!(refused.unwrap_err().detail.contains("not failed"));
    assert!(none.unwrap_err().detail.contains("no job"));
    assert_eq!(s.store.writes(), writes, "nothing was written");
    assert_eq!(
        s.store.get(s.live_running).await.unwrap().unwrap().state,
        JobState::Running
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_retry_returns_the_job_to_a_terminal_state_without_running_it() {
    let s = seeded().await;
    let id = jref(s.dead);
    let (retried, undone, again) = {
        let (a, b, c) = (id.clone(), id.clone(), id.clone());
        live(&s.part, move |ops| {
            let retried = ops.retry(&a, T0);
            let undone = ops.restore(&b, JobLife::Failed);
            // Undoing twice finds the job already in the undone state.
            let again = ops.restore(&c, JobLife::Failed);
            (retried, undone, again)
        })
        .await
    };
    retried.unwrap();
    undone.unwrap();
    again.unwrap();
    let job = s.store.get(s.dead).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        JobState::Cancelled,
        "not exact: cancelled, not dead"
    );
    assert_eq!(life_of(&job.state), JobLife::Done);
    assert_eq!(job.last_error.as_deref(), Some("boom"));
    // No worker can claim it.
    let claimed = s
        .store
        .claim(
            &["default".to_owned()],
            "worker-2",
            Duration::from_secs(30),
            at(T0 + 10),
            10,
        )
        .await
        .unwrap();
    assert!(claimed.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn undoing_a_retry_after_a_worker_claimed_the_job_is_refused() {
    let s = seeded().await;
    let id = jref(s.dead);
    let retried_id = id.clone();
    live(&s.part, move |ops| ops.retry(&retried_id, T0))
        .await
        .unwrap();
    // A worker takes the requeued job (the other seeded jobs are already running).
    let claimed = s
        .store
        .claim(
            &["default".to_owned()],
            "worker-2",
            Duration::from_secs(3_600),
            at(T0 + 1),
            10,
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job.id, s.dead);

    let writes = s.store.writes();
    let refused = live(&s.part, move |ops| ops.restore(&id, JobLife::Failed)).await;
    assert_eq!(refused.unwrap_err().detail, "the job already ran again");
    assert_eq!(s.store.writes(), writes, "nothing was written");
    assert_eq!(
        s.store.get(s.dead).await.unwrap().unwrap().state,
        JobState::Running
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_that_claims_between_the_read_and_the_cancel_is_not_cancelled() {
    let s = seeded().await;
    let id = jref(s.dead);
    let retried_id = id.clone();
    live(&s.part, move |ops| ops.retry(&retried_id, T0))
        .await
        .unwrap();
    // The part reads the job (pending, nobody holds it); a worker claims it right after that
    // read; the undo must find the job running and leave it alone.
    s.store.claim_after_get.store(true, Ordering::SeqCst);
    let refused = live(&s.part, move |ops| ops.restore(&id, JobLife::Failed)).await;
    assert_eq!(refused.unwrap_err().detail, "the job already ran again");
    let job = s.store.get(s.dead).await.unwrap().unwrap();
    assert_eq!(
        job.state,
        JobState::Running,
        "the worker's job was not cancelled"
    );
    assert!(job.claim.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_port_makes_the_ops_fail_closed() {
    let store = Counting::new();
    let site = site();
    let dead = enqueue(&store, &site).await;
    let c = claim_one(&store, 30).await;
    store.complete(&c, JobState::Dead, None).await.unwrap();
    let gate = StopGate::new();
    let admin = Arc::new(StoreJobAdmin::new(
        site,
        Arc::clone(&store) as Arc<dyn JobStore>,
        Arc::clone(&gate),
    ));
    let part = Arc::new(StoreJobsPart::over_admin(admin, Handle::current()));
    gate.close();
    let id = jref(dead);
    let (failed, view, retried) = live(&part, move |ops| {
        (ops.failed(), ops.job(&id), ops.retry(&id, T0))
    })
    .await;
    assert!(failed.is_empty());
    assert!(view.is_none());
    assert!(retried.is_err());
    assert_eq!(
        store.get(dead).await.unwrap().unwrap().state,
        JobState::Dead
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rehearsal_retries_and_unsticks_a_snapshot_and_the_real_store_is_unchanged() {
    let s = seeded().await;
    let writes = s.store.writes();
    let part = Arc::clone(&s.part);
    let (dead, stale) = (jref(s.dead), jref(s.stale_running));
    let outcome = tokio::task::spawn_blocking(move || {
        let mut ops = part.rehearsal().unwrap();
        assert_eq!(ops.failed(), vec![dead.clone()]);
        let retried = ops.retry(&dead, T0).unwrap();
        assert_eq!(
            (retried.before, retried.after),
            (JobLife::Failed, JobLife::Queued)
        );
        // The undo of the retry restores the exact state.
        ops.restore(&dead, JobLife::Failed).unwrap();
        assert_eq!(ops.job(&dead).unwrap().life, JobLife::Failed);
        let unstuck = ops.unstick(&stale, T0 + 100).unwrap();
        assert_eq!(unstuck.after, JobLife::Queued);
        // A second rehearsal is a fresh snapshot of the real store.
        let again = part.rehearsal().unwrap();
        again.job(&stale).unwrap().life
    })
    .await
    .unwrap();
    assert_eq!(outcome, JobLife::Running);
    assert_eq!(s.store.writes(), writes, "a rehearsal never writes");
    assert_eq!(
        s.store.get(s.stale_running).await.unwrap().unwrap().state,
        JobState::Running
    );
    assert_eq!(
        s.store.get(s.dead).await.unwrap().unwrap().state,
        JobState::Dead,
        "a rehearsal retry never reaches the store"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_part_with_no_job_store_is_none() {
    let store: Arc<dyn JobStore> = Counting::new();
    assert!(from_parts(None, "site-1", Some(Handle::current())).is_none());
    assert!(from_parts(Some(Arc::clone(&store)), "site-1", None).is_none());
    assert!(from_parts(Some(store), "site-1", Some(Handle::current())).is_some());
    assert!(from_admin(None, Some(Handle::current())).is_none());
}

#[test]
fn the_states_map_to_lives() {
    assert_eq!(life_of(&JobState::Pending), JobLife::Queued);
    assert_eq!(life_of(&JobState::Retrying { at: at(T0) }), JobLife::Queued);
    assert_eq!(life_of(&JobState::Running), JobLife::Running);
    assert_eq!(life_of(&JobState::Dead), JobLife::Failed);
    assert_eq!(life_of(&JobState::Succeeded), JobLife::Done);
    assert_eq!(life_of(&JobState::Cancelled), JobLife::Done);
}

// ---------------------------------------------------------------------------------------------
// the retrieval index: a durable generation (T13)
// ---------------------------------------------------------------------------------------------

/// A lag that always reports the cursor behind the feed head.
struct Behind;

impl IndexLag for Behind {
    fn behind(&self) -> LagFuture<'_> {
        Box::pin(async { Some(true) })
    }
}

/// A `JobSubmit` that coalesces on the idempotency key while the job is "queued".
struct Submit {
    queued: Mutex<HashSet<String>>,
    count: AtomicUsize,
}

impl Submit {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            queued: Mutex::new(HashSet::new()),
            count: AtomicUsize::new(0),
        })
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    /// The backfill finished: the key is free again.
    fn finish(&self) {
        self.queued.lock().unwrap().clear();
    }
}

impl JobSubmit for Submit {
    fn submit<'a>(&'a self, job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async move {
            let fresh = self.queued.lock().unwrap().insert(job.idempotency_key);
            if fresh {
                self.count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(fresh)
        })
    }
}

/// A scratch directory standing for a site's data root.
fn scratch(name: &str) -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "spot_t13_{name}_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ))
}

/// A part whose generation lives in `path`. Dropping it and building another over the same path is
/// a restart.
fn index_part(path: &std::path::Path, submit: &Arc<Submit>) -> Arc<RetrievalIndexPart> {
    let clock = Arc::new(ManualClock::new(at(T0)));
    Arc::new(
        RetrievalIndexPart::new(
            Arc::new(Behind) as Arc<dyn IndexLag>,
            Arc::clone(submit) as Arc<dyn JobSubmit>,
            "default".to_owned(),
            clock as Arc<dyn Clock>,
            Handle::current(),
        )
        .with_generations(Arc::new(FileGeneration::new(path.to_path_buf()))),
    )
}

fn retrieval() -> IndexRef {
    IndexRef::new(INDEX_NAME)
}

async fn blocking<R: Send + 'static>(work: impl FnOnce() -> R + Send + 'static) -> R {
    tokio::task::spawn_blocking(work).await.unwrap()
}

async fn generation_of(part: &Arc<RetrievalIndexPart>) -> u64 {
    let part = Arc::clone(part);
    blocking(move || part.live().index(&retrieval()).unwrap().generation).await
}

#[test]
fn the_generation_file_sits_beside_the_cursor() {
    let path = generation_path(std::path::Path::new("/data"), "site-1");
    assert_eq!(
        path,
        std::path::Path::new("/data/sites/site-1/retrieval.generation")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_generation_survives_dropping_and_rebuilding_the_part() {
    let dir = scratch("survive");
    let path = generation_path(&dir, "site-1");
    let submit = Submit::new();

    let first = index_part(&path, &submit);
    assert_eq!(generation_of(&first).await, 0, "no file: generation 0");
    let part = Arc::clone(&first);
    let change = blocking(move || part.live().rebuild(&retrieval()).unwrap()).await;
    assert_eq!((change.before_generation, change.after_generation), (0, 1));
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "1");
    drop(first);

    // A restart: a new part over the same directory reads 1, not 0.
    let second = index_part(&path, &submit);
    assert_eq!(generation_of(&second).await, 1);
    submit.finish();
    let part = Arc::clone(&second);
    let change = blocking(move || part.live().rebuild(&retrieval()).unwrap()).await;
    assert_eq!(
        (change.before_generation, change.after_generation),
        (1, 2),
        "a rebuild bumps it by exactly one"
    );
    drop(second);
    assert_eq!(generation_of(&index_part(&path, &submit)).await, 2);
    assert_eq!(submit.count(), 2);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_rebuild_leaves_the_generation_where_it_was() {
    let dir = scratch("refused");
    let path = generation_path(&dir, "site-1");
    let submit = Submit::new();
    let part = index_part(&path, &submit);
    let error = {
        let part = Arc::clone(&part);
        blocking(move || {
            let mut ops = part.live();
            ops.rebuild(&retrieval()).unwrap();
            ops.rebuild(&retrieval())
        })
        .await
        .unwrap_err()
    };
    assert!(error.detail.contains("already queued"), "{error}");
    assert_eq!(submit.count(), 1);
    assert_eq!(generation_of(&part).await, 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restore_after_a_newer_rebuild_is_refused_even_across_a_restart() {
    let dir = scratch("restore");
    let path = generation_path(&dir, "site-1");
    let submit = Submit::new();

    let first = index_part(&path, &submit);
    let part = Arc::clone(&first);
    let older = blocking(move || part.live().rebuild(&retrieval()).unwrap()).await;
    drop(first);

    // After a restart another rebuild runs; the first change must not restore any more.
    submit.finish();
    let second = index_part(&path, &submit);
    let part = Arc::clone(&second);
    let newer = blocking(move || part.live().rebuild(&retrieval()).unwrap()).await;
    assert_eq!(newer.after_generation, 2);
    let part = Arc::clone(&second);
    let refused = blocking(move || part.live().restore(&older)).await;
    assert!(refused.unwrap_err().detail.contains("newer rebuild"));
    assert_eq!(generation_of(&second).await, 2, "nothing was restored");

    // The newest change restores, and the file follows.
    let part = Arc::clone(&second);
    blocking(move || part.live().restore(&newer).unwrap()).await;
    assert_eq!(generation_of(&second).await, 1);
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "1");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_generation_file_reads_as_zero() {
    let dir = scratch("garbled");
    let path = generation_path(&dir, "site-1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "not a number").unwrap();
    let submit = Submit::new();
    let part = index_part(&path, &submit);
    assert_eq!(generation_of(&part).await, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generation_that_cannot_be_saved_refuses_the_rebuild_before_enqueueing() {
    let dir = scratch("unwritable");
    std::fs::create_dir_all(&dir).unwrap();
    // The "directory" of the generation file is a regular file, so the write cannot succeed.
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let path = blocker.join("retrieval.generation");
    let submit = Submit::new();
    let part = index_part(&path, &submit);
    let part2 = Arc::clone(&part);
    let error = blocking(move || part2.live().rebuild(&retrieval()))
        .await
        .unwrap_err();
    assert!(error.detail.contains("could not be saved"), "{error}");
    assert_eq!(submit.count(), 0, "no backfill was enqueued");
    assert_eq!(generation_of(&part).await, 0);
    let _ = std::fs::remove_dir_all(&dir);
}
