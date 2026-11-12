//! The enqueue-after-commit path against a real `spot-postgres` outbox: a job exists exactly
//! when the transaction that enqueued it committed, and a restarted intake never doubles one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use spot_core::SiteId;
use spot_events_contract::{Actor, EventMeta, PayloadDocument};
use spot_jobs::model::{Claim, InsertResult, Job, JobId, JobState, NewJob};
use spot_jobs::store::{ClaimedJob, JobFilter, JobStore};
use spot_jobs::{Error, SYS_JOB_ENQUEUED_TOPIC, process_batch};
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_storage_contract::contract_tests;
use spot_storage_contract::feed::FeedPosition;
use spot_storage_contract::{
    Catalog, Isolation, Name, NodeRef, Outbox, OutboxEvent, RecordId, StorageBackend, Transaction,
    TxOptions,
};
use time::OffsetDateTime;

type Fut<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = spot_jobs::Result<T>> + Send + 'a>>;

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

struct Env {
    db: TestDb,
    backend: PgBackend,
    cat: Arc<Catalog>,
    store: Arc<PgJobStore>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    backend.prepare(&cat).await.unwrap();
    let store = Arc::new(PgJobStore::new(common::pool(&db, 8, "intake")));
    store.prepare().await.unwrap();
    Some(Env {
        db,
        backend,
        cat,
        store,
    })
}

impl Env {
    async fn begin(&self) -> PgTransaction {
        let options = TxOptions::new(n("enqueue_test"), Isolation::ReadCommitted);
        self.backend.begin(self.cat.clone(), options).await.unwrap()
    }

    /// An outbox event asking for a job; `key` is its idempotency key, if any.
    fn event(&self, key: Option<&str>) -> OutboxEvent {
        let data = json!({
            "job": "app:work",
            "args": { "invoice": 7 },
            "idempotency_key": key,
            "not_before": null,
            "queue": null,
            "context": null,
        });
        let id = RecordId::new_v7();
        let payload = PayloadDocument {
            meta: EventMeta {
                occurred_at: OffsetDateTime::now_utc(),
                actor: Actor::System {
                    component: n("outbox_intake_test"),
                },
                command_id: None,
                causation_id: None,
                correlation_id: id,
                trace_id: None,
            },
            data,
        };
        OutboxEvent {
            id,
            topic: n(SYS_JOB_ENQUEUED_TOPIC),
            subject: NodeRef {
                entity: n("party"),
                id: RecordId::new_v7(),
            },
            subject_version: None,
            schema_version: 1,
            payload: serde_json::to_string(&payload).unwrap(),
        }
    }

    async fn intake(&self) -> spot_jobs::Result<spot_jobs::IntakeReport> {
        process_batch(
            &*self.store,
            &self.backend,
            &SiteId::new("test-site").unwrap(),
            100,
            OffsetDateTime::now_utc(),
        )
        .await
    }

    async fn jobs(&self) -> Vec<Job> {
        self.store.list(JobFilter::default()).await.unwrap()
    }
}

#[tokio::test]
async fn a_rolled_back_enqueue_creates_no_job_and_a_committed_one_creates_one() {
    let Some(env) = env().await else {
        return;
    };
    // Rolled back: the event never reaches the outbox, so the intake sees nothing.
    let mut tx = env.begin().await;
    tx.enqueue(&env.event(None)).await.unwrap();
    tx.rollback().await.unwrap();
    let report = env.intake().await.unwrap();
    assert_eq!(report.inserted, 0);
    assert!(env.jobs().await.is_empty());

    // Committed: exactly one job, with the event's content.
    let mut tx = env.begin().await;
    tx.enqueue(&env.event(None)).await.unwrap();
    tx.commit().await.unwrap();
    let report = env.intake().await.unwrap();
    assert_eq!(report.inserted, 1);
    assert!(report.rejected.is_empty());
    let jobs = env.jobs().await;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].name, "app:work");
    assert_eq!(jobs[0].args, json!({ "invoice": 7 }));
    assert_eq!(jobs[0].queue, "default");
    assert_eq!(jobs[0].state, JobState::Pending);

    // The position is durable and a second pass has nothing to do.
    assert_eq!(env.store.intake_position().await.unwrap(), report.position);
    assert_ne!(report.position, FeedPosition::START);
    assert_eq!(env.intake().await.unwrap().inserted, 0);
    assert_eq!(env.jobs().await.len(), 1);
}

/// A store whose position save fails once: the intake process "dies" between inserting jobs and
/// saving the position.
struct DiesBeforeSavingPosition {
    inner: Arc<PgJobStore>,
}

impl JobStore for DiesBeforeSavingPosition {
    fn insert_if_new(&self, job: NewJob) -> Fut<'_, InsertResult> {
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
        self.inner.claim(queues, worker_id, ttl, now, limit)
    }
    fn heartbeat(&self, claim: &Claim, ttl: Duration, now: OffsetDateTime) -> Fut<'_, ()> {
        self.inner.heartbeat(claim, ttl, now)
    }
    fn complete(&self, claim: &Claim, s: JobState, e: Option<String>) -> Fut<'_, ()> {
        self.inner.complete(claim, s, e)
    }
    fn reap_expired(&self, now: OffsetDateTime) -> Fut<'_, Vec<JobId>> {
        self.inner.reap_expired(now)
    }
    fn cancel(&self, id: JobId) -> Fut<'_, ()> {
        self.inner.cancel(id)
    }
    fn merge_context(
        &self,
        id: JobId,
        claim: &Claim,
        patch: serde_json::Map<String, serde_json::Value>,
    ) -> Fut<'_, serde_json::Value> {
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
    fn set_intake_position(&self, _position: FeedPosition) -> Fut<'_, ()> {
        Box::pin(async { Err(Error::other("the intake died before saving its position")) })
    }
}

#[tokio::test]
async fn a_restart_between_insert_and_position_save_creates_no_duplicate() {
    let Some(env) = env().await else {
        return;
    };
    // Two events: one without an idempotency key (only the event id protects it), one with.
    let mut tx = env.begin().await;
    tx.enqueue(&env.event(None)).await.unwrap();
    tx.enqueue(&env.event(Some("invoice-7"))).await.unwrap();
    tx.commit().await.unwrap();

    let dying = DiesBeforeSavingPosition {
        inner: env.store.clone(),
    };
    let site = SiteId::new("test-site").unwrap();
    let died = process_batch(&dying, &env.backend, &site, 100, OffsetDateTime::now_utc()).await;
    assert!(died.is_err());
    assert_eq!(env.jobs().await.len(), 2, "the jobs were inserted");
    assert_eq!(
        env.store.intake_position().await.unwrap(),
        FeedPosition::START,
        "the position was not saved"
    );

    // Restart: the same events are read again and create nothing.
    let report = env.intake().await.unwrap();
    assert_eq!(report.inserted, 0, "replayed events create no job");
    assert_eq!(env.jobs().await.len(), 2);
    assert_ne!(
        env.store.intake_position().await.unwrap(),
        FeedPosition::START
    );

    // Even after both jobs finished (their keys are free again), a replay creates nothing.
    for job in env.jobs().await {
        let claimed = env
            .store
            .claim(
                &["default".to_owned()],
                "w",
                Duration::from_secs(30),
                OffsetDateTime::now_utc(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(claimed.len(), 1, "claim for {}", job.id);
        env.store
            .complete(&claimed[0].claim, JobState::Succeeded, None)
            .await
            .unwrap();
    }
    env.store
        .set_intake_position(FeedPosition::START)
        .await
        .unwrap();
    let report = env.intake().await.unwrap();
    assert_eq!(report.inserted, 0);
    assert_eq!(env.jobs().await.len(), 2);
    drop(env.db);
}
