//! WP-AI-07.T13: the `JobAdmin` port over an in-memory job store: list by state, requeue, cancel
//! of an unclaimed job, reaping, the site boundary and the stop gate.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_cli::services::ports::{JobAdmin, PortError, StopGate, StoreJobAdmin};
use spot_core::SiteId;
use spot_jobs::model::{Claim, InsertResult, JobId, JobState, NewJob};
use spot_jobs::{CancelOutcome, InMemoryJobStore, JobStore};
use time::OffsetDateTime;

const T0: i64 = 1_795_262_400;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

struct Rig {
    store: Arc<InMemoryJobStore>,
    gate: Arc<StopGate>,
    admin: StoreJobAdmin,
}

fn rig() -> Rig {
    let store = Arc::new(InMemoryJobStore::new());
    let gate = StopGate::new();
    let admin = StoreJobAdmin::new(
        site("site-1"),
        Arc::clone(&store) as Arc<dyn JobStore>,
        Arc::clone(&gate),
    );
    Rig { store, gate, admin }
}

async fn enqueue(store: &InMemoryJobStore, site_name: &str, queue: &str) -> JobId {
    let job = NewJob::new(
        "app:test".to_owned(),
        serde_json::json!({"secret": "argument"}),
        queue.to_owned(),
        site(site_name),
        at(T0),
    );
    match store.insert_if_new(job).await.unwrap() {
        InsertResult::Inserted(id) => id,
        InsertResult::Duplicate(_) => panic!("duplicate"),
    }
}

/// Claims the one claimable job of `queue` with `ttl` seconds at `T0`.
async fn claim(store: &InMemoryJobStore, queue: &str, ttl: u64) -> Claim {
    let claimed = store
        .claim(
            &[queue.to_owned()],
            "worker-1",
            Duration::from_secs(ttl),
            at(T0),
            1,
        )
        .await
        .unwrap();
    claimed[0].claim.clone()
}

/// A dead job of site-1 on its own queue.
async fn dead(store: &InMemoryJobStore, queue: &str) -> JobId {
    let id = enqueue(store, "site-1", queue).await;
    let c = claim(store, queue, 30).await;
    store
        .complete(&c, JobState::Dead, Some("boom".to_owned()))
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn list_filters_by_state_and_clamps_the_limit() {
    let r = rig();
    let dead_id = dead(&r.store, "q-dead").await;
    let pending_id = enqueue(&r.store, "site-1", "q-pending").await;
    let running_id = enqueue(&r.store, "site-1", "q-running").await;
    claim(&r.store, "q-running", 3_600).await;

    let only_dead = r.admin.list(Some(JobState::Dead), 50).await.unwrap();
    assert_eq!(only_dead.len(), 1);
    assert_eq!(only_dead[0].id, dead_id);
    assert_eq!(only_dead[0].state, JobState::Dead);

    let running = r.admin.list(Some(JobState::Running), 50).await.unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].id, running_id);
    assert_eq!(
        running[0].lease_expires_at,
        Some(at(T0 + 3_600)),
        "the lease is shown"
    );

    let pending = r.admin.list(Some(JobState::Pending), 50).await.unwrap();
    assert_eq!(
        pending.iter().map(|j| j.id).collect::<Vec<_>>(),
        vec![pending_id]
    );

    let all = r.admin.list(None, 50).await.unwrap();
    assert_eq!(all.len(), 3);
    // A limit of 0 is raised to 1; a limit over the maximum is cut to it.
    assert_eq!(r.admin.list(None, 0).await.unwrap().len(), 1);
    assert_eq!(r.admin.list(None, u32::MAX).await.unwrap().len(), 3);
}

#[tokio::test]
async fn get_returns_a_summary_and_none_for_a_missing_job() {
    let r = rig();
    let id = dead(&r.store, "default").await;
    let job = r.admin.get(id).await.unwrap().unwrap();
    assert_eq!(job.id, id);
    assert_eq!(job.name, "app:test");
    assert_eq!(job.queue, "default");
    assert_eq!(job.state, JobState::Dead);
    assert_eq!(job.lease_expires_at, None);
    assert!(r.admin.get(JobId(9_999)).await.unwrap().is_none());
}

#[tokio::test]
async fn requeue_moves_a_dead_job_to_pending_and_a_worker_claims_it() {
    let r = rig();
    let id = dead(&r.store, "default").await;
    let job = r.admin.requeue(id, at(T0 + 10)).await.unwrap();
    assert_eq!(job.id, id);
    assert_eq!(job.state, JobState::Pending);
    assert_eq!(job.attempt, 0);

    let claimed = r
        .store
        .claim(
            &["default".to_owned()],
            "worker-2",
            Duration::from_secs(30),
            at(T0 + 10),
            1,
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(claimed[0].job.id, id);
}

#[tokio::test]
async fn requeue_is_refused_for_a_running_job_and_changes_nothing() {
    let r = rig();
    let id = enqueue(&r.store, "site-1", "default").await;
    claim(&r.store, "default", 3_600).await;
    let before = r.store.get(id).await.unwrap().unwrap();
    let error = r.admin.requeue(id, at(T0 + 1)).await.unwrap_err();
    assert!(matches!(error, PortError::Invalid(_)), "{error:?}");
    assert_eq!(r.store.get(id).await.unwrap().unwrap(), before);
}

#[tokio::test]
async fn a_second_requeue_of_the_same_job_is_refused() {
    let r = rig();
    let id = dead(&r.store, "default").await;
    r.admin.requeue(id, at(T0)).await.unwrap();
    assert!(matches!(
        r.admin.requeue(id, at(T0)).await,
        Err(PortError::Invalid(_))
    ));
}

#[tokio::test]
async fn a_requeue_blocked_by_a_live_key_holder_is_refused() {
    let r = rig();
    let keyed = |key: &str| {
        NewJob::new(
            "app:test".to_owned(),
            serde_json::json!({}),
            "default".to_owned(),
            site("site-1"),
            at(T0),
        )
        .with_idempotency_key(key.to_owned())
    };
    let InsertResult::Inserted(first) = r.store.insert_if_new(keyed("k")).await.unwrap() else {
        panic!("not inserted");
    };
    let c = claim(&r.store, "default", 30).await;
    r.store.complete(&c, JobState::Dead, None).await.unwrap();
    // The key is free while the first job is dead.
    let InsertResult::Inserted(_) = r.store.insert_if_new(keyed("k")).await.unwrap() else {
        panic!("not inserted");
    };
    let error = r.admin.requeue(first, at(T0)).await.unwrap_err();
    assert!(matches!(error, PortError::Invalid(_)), "{error:?}");
    assert_eq!(
        r.store.get(first).await.unwrap().unwrap().state,
        JobState::Dead
    );
}

#[tokio::test]
async fn cancel_unclaimed_takes_back_a_pending_job_only() {
    let r = rig();
    let id = dead(&r.store, "default").await;
    r.admin.requeue(id, at(T0)).await.unwrap();
    let cancelled = r.admin.cancel_unclaimed(id).await.unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);

    // A running job is refused and stays running.
    let busy = enqueue(&r.store, "site-1", "q-busy").await;
    claim(&r.store, "q-busy", 3_600).await;
    assert!(matches!(
        r.admin.cancel_unclaimed(busy).await,
        Err(PortError::Invalid(_))
    ));
    assert_eq!(
        r.store.get(busy).await.unwrap().unwrap().state,
        JobState::Running
    );
    // A dead job is refused too.
    let corpse = dead(&r.store, "q-corpse").await;
    assert!(matches!(
        r.admin.cancel_unclaimed(corpse).await,
        Err(PortError::Invalid(_))
    ));
}

#[tokio::test]
async fn cancel_unclaimed_also_takes_back_an_unclaimed_retrying_job() {
    let r = rig();
    let id = enqueue(&r.store, "site-1", "q-retry").await;
    let c = claim(&r.store, "q-retry", 30).await;
    r.store
        .complete(
            &c,
            JobState::Retrying { at: at(T0 + 3_600) },
            Some("flaky".to_owned()),
        )
        .await
        .unwrap();
    let cancelled = r.admin.cancel_unclaimed(id).await.unwrap();
    assert_eq!(cancelled.state, JobState::Cancelled);
    assert_eq!(cancelled.lease_expires_at, None);
}

#[tokio::test]
async fn cancel_unclaimed_on_an_unknown_id_is_invalid_and_not_a_store_error() {
    let r = rig();
    assert!(matches!(
        r.admin.cancel_unclaimed(JobId(9_999)).await,
        Err(PortError::Invalid(_))
    ));
}

#[tokio::test]
async fn a_cancel_of_a_claimed_job_leaves_its_worker_able_to_finish_it() {
    let r = rig();
    let id = enqueue(&r.store, "site-1", "q-held").await;
    let held = claim(&r.store, "q-held", 3_600).await;
    let before = r.store.get(id).await.unwrap().unwrap();
    assert!(matches!(
        r.admin.cancel_unclaimed(id).await,
        Err(PortError::Invalid(_))
    ));
    assert_eq!(r.store.get(id).await.unwrap().unwrap(), before);
    r.store
        .complete(&held, JobState::Succeeded, None)
        .await
        .expect("the worker's claim is still valid");
}

/// A store that behaves as the in-memory one, except that right after its `get` answers (the
/// read a get-check-then-cancel port would decide from) a worker claims the job.
struct ClaimsAfterGet {
    inner: InMemoryJobStore,
    armed: AtomicBool,
}

type StoreFut<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<T>> + Send + 'a>>;

impl JobStore for ClaimsAfterGet {
    fn insert_if_new(&self, job: NewJob) -> StoreFut<'_, InsertResult> {
        self.inner.insert_if_new(job)
    }
    fn claim(
        &self,
        queues: &[String],
        worker_id: &str,
        ttl: Duration,
        now: OffsetDateTime,
        limit: u32,
    ) -> StoreFut<'_, Vec<spot_jobs::store::ClaimedJob>> {
        self.inner.claim(queues, worker_id, ttl, now, limit)
    }
    fn heartbeat(&self, claim: &Claim, ttl: Duration, now: OffsetDateTime) -> StoreFut<'_, ()> {
        self.inner.heartbeat(claim, ttl, now)
    }
    fn complete(
        &self,
        claim: &Claim,
        new_state: JobState,
        last_error: Option<String>,
    ) -> StoreFut<'_, ()> {
        self.inner.complete(claim, new_state, last_error)
    }
    fn reap_expired(&self, now: OffsetDateTime) -> StoreFut<'_, Vec<JobId>> {
        self.inner.reap_expired(now)
    }
    fn cancel(&self, id: JobId) -> StoreFut<'_, ()> {
        self.inner.cancel(id)
    }
    fn cancel_if_pending(&self, id: JobId) -> StoreFut<'_, CancelOutcome> {
        self.inner.cancel_if_pending(id)
    }
    fn requeue(&self, id: JobId, now: OffsetDateTime) -> StoreFut<'_, spot_jobs::Job> {
        self.inner.requeue(id, now)
    }
    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> StoreFut<'_, serde_json::Value> {
        self.inner.merge_context(id, claim, patch)
    }
    fn get(&self, id: JobId) -> StoreFut<'_, Option<spot_jobs::Job>> {
        Box::pin(async move {
            let job = self.inner.get(id).await?;
            if self.armed.swap(false, Ordering::SeqCst) {
                // The worker wins the gap after the port has looked.
                self.inner
                    .claim(
                        &["default".to_owned()],
                        "worker-in-the-gap",
                        Duration::from_secs(3_600),
                        at(T0),
                        1,
                    )
                    .await?;
            }
            Ok(job)
        })
    }
    fn list(&self, filter: spot_jobs::store::JobFilter) -> StoreFut<'_, Vec<spot_jobs::Job>> {
        self.inner.list(filter)
    }
    fn intake_position(&self) -> StoreFut<'_, spot_storage_contract::FeedPosition> {
        self.inner.intake_position()
    }
    fn set_intake_position(
        &self,
        position: spot_storage_contract::FeedPosition,
    ) -> StoreFut<'_, ()> {
        self.inner.set_intake_position(position)
    }
}

#[tokio::test]
async fn a_worker_that_claims_after_the_port_looked_is_never_cancelled() {
    // The undo of a retry used to read the job, see it pending and then call `cancel`, which
    // cancelled a job a worker had claimed in between. The decision is now the store's own
    // atomic step, so the claim that lands after the port's read wins and the job keeps running.
    let store = Arc::new(ClaimsAfterGet {
        inner: InMemoryJobStore::new(),
        armed: AtomicBool::new(false),
    });
    let id = enqueue(&store.inner, "site-1", "default").await;
    let admin = StoreJobAdmin::new(
        site("site-1"),
        Arc::clone(&store) as Arc<dyn JobStore>,
        StopGate::new(),
    );
    store.armed.store(true, Ordering::SeqCst);
    let error = admin.cancel_unclaimed(id).await.unwrap_err();
    assert!(matches!(error, PortError::Invalid(_)), "{error:?}");
    let stored = store.inner.get(id).await.unwrap().unwrap();
    assert_eq!(
        stored.state,
        JobState::Running,
        "the claimed job keeps running"
    );
    assert!(stored.claim.is_some(), "and keeps its claim");
}

#[tokio::test]
async fn reap_expired_releases_lapsed_leases_of_this_site_only() {
    let r = rig();
    let ours = enqueue(&r.store, "site-1", "q-ours").await;
    claim(&r.store, "q-ours", 30).await;
    let theirs = enqueue(&r.store, "site-2", "q-theirs").await;
    claim(&r.store, "q-theirs", 30).await;

    // Not yet lapsed.
    assert!(r.admin.reap_expired(at(T0 + 10)).await.unwrap().is_empty());
    // The store releases both, the port reports only this site's.
    let reaped = r.admin.reap_expired(at(T0 + 100)).await.unwrap();
    assert_eq!(reaped, vec![ours]);
    assert!(matches!(
        r.store.get(ours).await.unwrap().unwrap().state,
        JobState::Retrying { .. }
    ));
    assert!(matches!(
        r.store.get(theirs).await.unwrap().unwrap().state,
        JobState::Retrying { .. }
    ));
}

#[tokio::test]
async fn another_sites_job_is_invisible_and_cannot_be_touched() {
    let r = rig();
    let foreign = enqueue(&r.store, "site-2", "q-foreign").await;
    let c = claim(&r.store, "q-foreign", 30).await;
    r.store.complete(&c, JobState::Dead, None).await.unwrap();

    assert!(r.admin.get(foreign).await.unwrap().is_none());
    assert!(r.admin.list(None, 50).await.unwrap().is_empty());
    assert!(matches!(
        r.admin.requeue(foreign, at(T0)).await,
        Err(PortError::Invalid(_))
    ));
    assert!(matches!(
        r.admin.cancel_unclaimed(foreign).await,
        Err(PortError::Invalid(_))
    ));
    assert_eq!(
        r.store.get(foreign).await.unwrap().unwrap().state,
        JobState::Dead,
        "nothing changed"
    );
}

#[tokio::test]
async fn after_the_stop_signal_every_call_is_stopped() {
    let r = rig();
    let id = dead(&r.store, "default").await;
    r.gate.close();
    assert_eq!(
        r.admin.list(None, 10).await.unwrap_err(),
        PortError::Stopped
    );
    assert_eq!(r.admin.get(id).await.unwrap_err(), PortError::Stopped);
    assert_eq!(
        r.admin.requeue(id, at(T0)).await.unwrap_err(),
        PortError::Stopped
    );
    assert_eq!(
        r.admin.cancel_unclaimed(id).await.unwrap_err(),
        PortError::Stopped
    );
    assert_eq!(
        r.admin.reap_expired(at(T0)).await.unwrap_err(),
        PortError::Stopped
    );
    assert_eq!(
        r.store.get(id).await.unwrap().unwrap().state,
        JobState::Dead,
        "a stopped port changes nothing"
    );
}

#[tokio::test]
async fn a_store_without_requeue_is_unavailable_not_a_crash() {
    // A store that never heard of requeue answers the trait's default; the port says so without
    // quoting anything.
    struct Old(InMemoryJobStore);
    type Fut<'a, T> =
        std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<T>> + Send + 'a>>;
    impl JobStore for Old {
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
        ) -> Fut<'_, Vec<spot_jobs::store::ClaimedJob>> {
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
        fn cancel_if_pending(&self, id: JobId) -> Fut<'_, spot_jobs::CancelOutcome> {
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
        fn get(&self, id: JobId) -> Fut<'_, Option<spot_jobs::Job>> {
            self.0.get(id)
        }
        fn list(&self, filter: spot_jobs::store::JobFilter) -> Fut<'_, Vec<spot_jobs::Job>> {
            self.0.list(filter)
        }
        fn intake_position(&self) -> Fut<'_, spot_storage_contract::FeedPosition> {
            self.0.intake_position()
        }
        fn set_intake_position(
            &self,
            position: spot_storage_contract::FeedPosition,
        ) -> Fut<'_, ()> {
            self.0.set_intake_position(position)
        }
    }
    let old = Arc::new(Old(InMemoryJobStore::new()));
    let id = dead(&old.0, "default").await;
    let admin = StoreJobAdmin::new(
        site("site-1"),
        Arc::clone(&old) as Arc<dyn JobStore>,
        StopGate::new(),
    );
    let error = admin.requeue(id, at(T0)).await.unwrap_err();
    assert!(matches!(error, PortError::Unavailable(_)), "{error:?}");
    assert_eq!(old.0.get(id).await.unwrap().unwrap().state, JobState::Dead);
}
