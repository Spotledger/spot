//! `merge_context` on PostgreSQL: the claim fence, concurrent writers across connections, and
//! the data surviving a store that is dropped and rebuilt (a crash and restart) and a reap.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::Duration;

use common::*;
use serde_json::{Map, Value, json};
use spot_jobs::model::InsertResult;
use spot_jobs::store::JobStore;
use spot_jobs::{Claim, Error, JobId, MAX_CONTEXT_BYTES};
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

fn q() -> Vec<String> {
    vec!["default".to_owned()]
}

async fn claimed(store: &PgJobStore, worker: &str, n: usize) -> (JobId, Claim) {
    let id = match store.insert_if_new(new_job(n)).await.unwrap() {
        InsertResult::Inserted(id) => id,
        other => panic!("expected an insert, got {other:?}"),
    };
    let got = store
        .claim(
            &q(),
            worker,
            Duration::from_secs(30),
            time::OffsetDateTime::now_utc(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].job.id, id);
    (id, got[0].claim.clone())
}

async fn fresh() -> Option<(TestDb, PgJobStore)> {
    let db = TestDb::create().await?;
    let store = PgJobStore::new(pool(&db, 16, "context")).with_reap_retry_delay(Duration::ZERO);
    store.prepare().await.unwrap();
    Some((db, store))
}

#[tokio::test]
async fn a_merge_survives_a_dropped_store_and_a_new_pool() {
    let Some((db, store)) = fresh().await else {
        return;
    };
    let (id, claim) = claimed(&store, "w1", 0).await;
    store
        .merge_context(id, &claim, obj(json!({"step1": "done"})))
        .await
        .unwrap();
    // The process "crashes": every connection goes away. A new store on a new pool sees it.
    drop(store);
    let reborn = PgJobStore::new(pool(&db, 4, "context-reborn"));
    let job = reborn.get(id).await.unwrap().unwrap();
    assert_eq!(job.context, json!({"step1": "done"}));
}

#[tokio::test]
async fn the_retry_after_a_lost_lease_sees_the_earlier_merge_and_the_old_claim_is_fenced() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    let (id, claim) = claimed(&store, "w1", 0).await;
    store
        .merge_context(id, &claim, obj(json!({"step1": "done"})))
        .await
        .unwrap();
    // The lease lapses (database clock) and another worker takes the job over.
    let reaped = store
        .reap_expired(time::OffsetDateTime::now_utc() + time::Duration::hours(2))
        .await
        .unwrap();
    assert_eq!(reaped, vec![id]);
    let again = store
        .claim(
            &q(),
            "w2",
            Duration::from_secs(30),
            time::OffsetDateTime::now_utc(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].job.context, json!({"step1": "done"}));
    let new_claim = again[0].claim.clone();

    // The first worker is fenced out, even though it still holds a well-formed claim.
    let r = store
        .merge_context(id, &claim, obj(json!({"zombie": true})))
        .await;
    assert_eq!(r, Err(Error::ClaimLost));
    // The new holder continues from the earlier progress.
    let merged = store
        .merge_context(id, &new_claim, obj(json!({"step2": "done"})))
        .await
        .unwrap();
    assert_eq!(merged, json!({"step1": "done", "step2": "done"}));
    assert_eq!(
        store.get(id).await.unwrap().unwrap().context,
        json!({"step1": "done", "step2": "done"})
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn merges_from_many_connections_lose_no_key_and_a_stale_claimant_never_lands() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    let (id, claim) = claimed(&store, "w1", 0).await;
    let stale = Claim {
        token: spot_scheduler::lease::FencingToken(claim.token.0 + 1_000_003),
        worker: claim.worker.clone(),
        expires_at: claim.expires_at,
    };
    let mut tasks = Vec::new();
    for i in 0..48u32 {
        let (s, c) = (store.clone(), claim.clone());
        tasks.push(tokio::spawn(async move {
            s.merge_context(id, &c, obj(json!({ format!("good{i}"): i })))
                .await
                .map(|_| ())
        }));
        let (s, c) = (store.clone(), stale.clone());
        tasks.push(tokio::spawn(async move {
            s.merge_context(id, &c, obj(json!({ format!("bad{i}"): i })))
                .await
                .map(|_| ())
        }));
    }
    let mut ok = 0;
    let mut lost = 0;
    for t in tasks {
        match t.await.unwrap() {
            Ok(()) => ok += 1,
            Err(Error::ClaimLost) => lost += 1,
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }
    assert_eq!((ok, lost), (48, 48));
    let ctx = store.get(id).await.unwrap().unwrap().context;
    let map = ctx.as_object().unwrap();
    assert_eq!(map.len(), 48);
    assert!((0..48).all(|i| map.get(&format!("good{i}")) == Some(&json!(i))));
    assert!(map.keys().all(|k| !k.starts_with("bad")));
}

#[tokio::test]
async fn a_token_that_does_not_fit_the_database_is_a_lost_claim() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    let (id, claim) = claimed(&store, "w1", 0).await;
    let huge = Claim {
        token: spot_scheduler::lease::FencingToken(u64::MAX),
        worker: claim.worker.clone(),
        expires_at: claim.expires_at,
    };
    assert_eq!(
        store.merge_context(id, &huge, obj(json!({"x": 1}))).await,
        Err(Error::ClaimLost)
    );
}

#[tokio::test]
async fn a_refused_merge_writes_nothing_and_its_error_carries_no_content() {
    let Some((_db, store)) = fresh().await else {
        return;
    };
    let (id, claim) = claimed(&store, "w1", 0).await;
    store
        .merge_context(id, &claim, obj(json!({"keep": 1})))
        .await
        .unwrap();
    let secret = "s3cret-value-must-not-leak".repeat(MAX_CONTEXT_BYTES / 10);
    let err = store
        .merge_context(id, &claim, obj(json!({"big": secret})))
        .await
        .unwrap_err();
    assert_eq!(err, Error::ContextTooLarge);
    assert_eq!(err.to_string(), "job.context_too_large");
    assert_eq!(
        store.get(id).await.unwrap().unwrap().context,
        json!({"keep": 1})
    );
}
