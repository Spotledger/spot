//! WP-AI-07.T6: the jobs part of the world over an in-memory job store. The part's ops block on the
//! runtime handle, so every op runs on a blocking thread, as the remedy host calls them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_cli::services::world_access::jobs::{StoreJobsPart, from_parts, life_of};
use spot_cli::services::world_access::parts::JobsPart;
use spot_core::SiteId;
use spot_jobs::model::{Claim, InsertResult, Job, JobId, JobState, NewJob};
use spot_jobs::store::{ClaimedJob, JobFilter};
use spot_jobs::{InMemoryJobStore, JobStore};
use spot_remedies::{JobLife, JobOps, JobRef};
use spot_storage_contract::FeedPosition;
use time::OffsetDateTime;
use tokio::runtime::Handle;

const T0: i64 = 1_795_262_400;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

type Fut<'a, T> = Pin<Box<dyn Future<Output = spot_jobs::Result<T>> + Send + 'a>>;

/// An in-memory store that counts every call that writes.
struct Counting {
    inner: InMemoryJobStore,
    writes: AtomicUsize,
}

impl Counting {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryJobStore::new(),
            writes: AtomicUsize::new(0),
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
        self.inner.get(id)
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retry_and_a_real_restore_are_not_supported_by_the_port_and_write_nothing() {
    let s = seeded().await;
    let writes = s.store.writes();
    let (dead, stale) = (jref(s.dead), jref(s.stale_running));
    let (retry, restore_other, restore_same) = live(&s.part, move |ops| {
        (
            ops.retry(&dead, T0),
            ops.restore(&dead, JobLife::Queued),
            // A restore to the state the job already has is a no-op, not a refusal.
            ops.restore(&stale, JobLife::Running),
        )
    })
    .await;
    assert_eq!(retry.unwrap_err().detail, "not supported by the jobs port");
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_part_with_no_job_store_is_none() {
    let store: Arc<dyn JobStore> = Counting::new();
    assert!(from_parts(None, "site-1", Some(Handle::current())).is_none());
    assert!(from_parts(Some(Arc::clone(&store)), "site-1", None).is_none());
    assert!(from_parts(Some(store), "site-1", Some(Handle::current())).is_some());
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
