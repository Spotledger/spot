//! `JobStore::requeue` on PostgreSQL: what the shared contract suite (`tests/contract.rs`) does
//! not reach. The database clock bounds `not_before`, a requeued job leaves the retention
//! window, and a successful requeue is counted. The state rules, the key rule and the races
//! run in the contract suite on both stores.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use spot_jobs::JobId;
use spot_jobs::metrics::JobMetricsSink;
use spot_jobs::model::{InsertResult, JobState};
use spot_jobs::store::JobStore;
use spot_jobs_pg::{PgJobStore, Retention};
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

async fn fresh(sink: Option<Arc<dyn JobMetricsSink>>) -> Option<(TestDb, PgJobStore)> {
    let db = TestDb::create().await?;
    let mut store = PgJobStore::new(pool(&db, 4, "requeue"));
    if let Some(sink) = sink {
        store = store.with_metrics(sink);
    }
    store.prepare().await.unwrap();
    Some((db, store))
}

fn q() -> Vec<String> {
    vec!["default".to_owned()]
}

/// Inserts a job, claims it and completes it as dead.
async fn dead_job(store: &PgJobStore) -> JobId {
    let id = match store.insert_if_new(new_job(0)).await.unwrap() {
        InsertResult::Inserted(id) => id,
        other => panic!("expected an insert, got {other:?}"),
    };
    let got = store
        .claim(
            &q(),
            "w",
            Duration::from_secs(30),
            OffsetDateTime::now_utc(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    store
        .complete(&got[0].claim, JobState::Dead, Some("boom".to_owned()))
        .await
        .unwrap();
    id
}

#[tokio::test]
async fn a_caller_clock_far_ahead_does_not_delay_the_job() {
    let Some((db, store)) = fresh(None).await else {
        return;
    };
    let id = dead_job(&store).await;
    // A node whose clock is a year ahead must not push the job a year out.
    let skewed = OffsetDateTime::now_utc() + time::Duration::days(365);
    let job = store.requeue(id, skewed).await.unwrap();
    let db_now: OffsetDateTime = db
        .raw_client()
        .await
        .query_one("SELECT now()", &[])
        .await
        .unwrap()
        .get(0);
    assert!(
        job.not_before <= db_now,
        "not_before {} must not be after the database clock {db_now}",
        job.not_before
    );
    // And it is claimable at once.
    let got = store
        .claim(&q(), "w2", Duration::from_secs(30), skewed, 1)
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].job.id, id);
    assert_eq!(
        got[0].job.attempt, 1,
        "the full attempts are back: this is the first"
    );
}

#[tokio::test]
async fn a_caller_clock_in_the_past_is_taken_as_given() {
    let Some((_db, store)) = fresh(None).await else {
        return;
    };
    let id = dead_job(&store).await;
    let past = OffsetDateTime::now_utc() - time::Duration::days(1);
    let job = store.requeue(id, past).await.unwrap();
    assert!(
        job.not_before <= past,
        "never later than the caller's clock"
    );
    assert!(job.not_before > past - time::Duration::seconds(1));
}

#[tokio::test]
async fn a_requeued_job_leaves_the_retention_window() {
    let Some((db, store)) = fresh(None).await else {
        return;
    };
    let id = dead_job(&store).await;
    let raw = i64::try_from(id.0).unwrap();
    let c = db.raw_client().await;
    // Dead for longer than the retention period: the next purge would delete it.
    c.batch_execute(&format!(
        "UPDATE spot_job SET finished_at = now() - interval '100 days' WHERE id = {raw}"
    ))
    .await
    .unwrap();
    store.requeue(id, OffsetDateTime::now_utc()).await.unwrap();
    let cleared: bool = c
        .query_one(
            "SELECT finished_at IS NULL FROM spot_job WHERE id = $1",
            &[&raw],
        )
        .await
        .unwrap()
        .get(0);
    assert!(cleared, "a requeued job is not finished");
    let report = store.purge_finished(&Retention::default()).await.unwrap();
    assert_eq!(report.dead, 0);
    assert!(store.get(id).await.unwrap().is_some());
}

#[derive(Default)]
struct Counting {
    count: AtomicU64,
}

impl JobMetricsSink for Counting {
    fn queue_depth(&self, _queue: &'static str, _depth: u64) {}
    fn oldest_age_seconds(&self, _queue: &'static str, _seconds: u64) {}
    fn requeued(&self, _queue: &'static str) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn only_a_successful_requeue_is_counted() {
    let sink = Arc::new(Counting::default());
    let Some((_db, store)) = fresh(Some(sink.clone())).await else {
        return;
    };
    let id = dead_job(&store).await;
    assert!(
        store
            .requeue(JobId(777_777), OffsetDateTime::now_utc())
            .await
            .is_err()
    );
    assert_eq!(sink.count.load(Ordering::SeqCst), 0);
    store.requeue(id, OffsetDateTime::now_utc()).await.unwrap();
    assert!(store.requeue(id, OffsetDateTime::now_utc()).await.is_err());
    assert_eq!(sink.count.load(Ordering::SeqCst), 1);
}
