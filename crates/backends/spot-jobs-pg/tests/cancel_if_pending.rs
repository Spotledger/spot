//! `JobStore::cancel_if_pending` on PostgreSQL: what the shared contract suite
//! (`tests/contract.rs`, which also runs the race) does not reach: the columns the statement
//! writes, that `cancel` keeps its old behaviour next to it, and that a worker which holds the
//! job can still complete it after a refused cancel.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use spot_jobs::model::{InsertResult, JobState};
use spot_jobs::store::JobStore;
use spot_jobs::{CancelOutcome, JobId};
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

async fn fresh() -> Option<(TestDb, PgJobStore)> {
    let db = TestDb::create().await?;
    let store = PgJobStore::new(pool(&db, 4, "cancel_if_pending"));
    store.prepare().await.unwrap();
    Some((db, store))
}

fn q() -> Vec<String> {
    vec!["default".to_owned()]
}

async fn insert(store: &PgJobStore) -> JobId {
    match store.insert_if_new(new_job(0)).await.unwrap() {
        InsertResult::Inserted(id) => id,
        other => panic!("expected an insert, got {other:?}"),
    }
}

#[tokio::test]
async fn a_cancelled_pending_job_is_finished_and_holds_no_claim() {
    let Some((db, store)) = fresh().await else {
        return;
    };
    let id = insert(&store).await;
    assert_eq!(
        store.cancel_if_pending(id).await.unwrap(),
        CancelOutcome::Cancelled
    );
    let raw = i64::try_from(id.0).unwrap();
    let row = db
        .raw_client()
        .await
        .query_one(
            "SELECT state, finished_at IS NOT NULL, claim_token IS NULL, claim_worker IS NULL, \
                    lease_expires_at IS NULL FROM spot_job WHERE id = $1",
            &[&raw],
        )
        .await
        .unwrap();
    let state: String = row.get(0);
    assert_eq!(state, "cancelled");
    assert!(
        row.get::<_, bool>(1),
        "a cancelled job is finished (retention)"
    );
    assert!(row.get::<_, bool>(2) && row.get::<_, bool>(3) && row.get::<_, bool>(4));
    // It is not claimable any more.
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
    assert!(got.is_empty());
}

#[tokio::test]
async fn a_running_job_is_left_alone_and_its_worker_still_completes_it() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    let id = insert(&store).await;
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
    let before = store.get(id).await.unwrap().unwrap();
    assert_eq!(before.state, JobState::Running);

    assert_eq!(
        store.cancel_if_pending(id).await.unwrap(),
        CancelOutcome::NotCancellable
    );
    assert_eq!(store.get(id).await.unwrap().unwrap(), before);

    store
        .complete(&got[0].claim, JobState::Succeeded, None)
        .await
        .expect("the worker's claim is still good");
    assert_eq!(
        store.get(id).await.unwrap().unwrap().state,
        JobState::Succeeded
    );
}

#[tokio::test]
async fn cancel_is_unchanged_and_still_stops_a_running_job() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    let id = insert(&store).await;
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
    // The old operation keeps its documented behaviour (it is what an operator's hard stop uses);
    // only `cancel_if_pending` refuses a job a worker holds.
    store.cancel(id).await.unwrap();
    assert_eq!(
        store.get(id).await.unwrap().unwrap().state,
        JobState::Cancelled
    );
    assert_eq!(
        store.cancel_if_pending(id).await.unwrap(),
        CancelOutcome::NotCancellable
    );
}

#[tokio::test]
async fn an_unknown_id_is_not_found_and_not_an_error() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    assert_eq!(
        store.cancel_if_pending(JobId(987_654)).await.unwrap(),
        CancelOutcome::NotFound
    );
}
