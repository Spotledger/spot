//! The appender as a fenced job: one drain run through the job engine chains the lane.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use common::*;
use spot_app_api::prelude::OffsetDateTime;
use spot_audit_chain::{AuditStore, CheckpointSigner, InMemoryStore, LocalSigner};
use spot_core::SiteId;
use spot_doc_audit::{
    AppenderStore, DocOperation, InMemoryAppenderStore, LaneAppender, LaneConfig, OutboxFeed,
    append_job_name, enqueue_audit, job_lane, lane_site, register_appender,
};
use spot_jobs::model::InsertResult;
use spot_jobs::worker::{HandlerResult, JobHandlerRegistry};
use spot_jobs::{InMemoryJobStore, JobState, JobStore, NewJob, QueuePolicy, Worker};
use spot_storage_contract::{
    Catalog, Isolation, StorageBackend, Transaction, TxOptions, contract_tests,
};
use spot_storage_fake::FakeBackend;
use tokio::sync::Semaphore;

const SITE: &str = "test.spotledger.app";

async fn backend() -> (FakeBackend, Arc<Catalog>) {
    let b = FakeBackend::new();
    let cat = Arc::new(contract_tests::catalog().unwrap());
    b.prepare(&cat).await.unwrap();
    (b, cat)
}

fn signer() -> Arc<dyn CheckpointSigner> {
    Arc::new(LocalSigner::from_seed([11u8; 32]))
}

/// A drain run through the job engine chains the lane and completes the job.
#[tokio::test]
async fn the_appender_runs_as_a_fenced_job() {
    let (b, cat) = backend().await;
    let mut tx = b
        .begin(
            cat.clone(),
            TxOptions::new(
                spot_app_api::prelude::Name::new("docaudit_job").unwrap(),
                Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap();
    let event = event(&id_of(1), 1, DocOperation::Create, "note");
    enqueue_audit(&mut tx, &event).await.unwrap();
    tx.commit().await.unwrap();

    let config = LaneConfig::new(1, 1);
    let store = Arc::new(InMemoryStore::new());
    let state: Arc<dyn AppenderStore> = Arc::new(InMemoryAppenderStore::new());
    let appender = Arc::new(LaneAppender::new(
        Arc::clone(&store),
        SITE,
        0,
        config,
        signer(),
        Arc::clone(&state),
    ));

    let feed: Arc<dyn OutboxFeed> = Arc::new(b);
    let mut registry = JobHandlerRegistry::new();
    register_appender(&mut registry, Arc::clone(&appender), feed, 100);

    let job_store = Arc::new(InMemoryJobStore::new());
    let now = OffsetDateTime::now_utc();
    let job = NewJob::new(
        append_job_name(0),
        serde_json::json!({ "lane": 0 }),
        "default".to_string(),
        SiteId::new(SITE).unwrap(),
        now,
    );
    let InsertResult::Inserted(job_id) = job_store.insert_if_new(job).await.unwrap() else {
        panic!("the job was not inserted");
    };
    let claimed = job_store
        .claim(
            &["default".to_string()],
            "w1",
            Duration::from_secs(30),
            now,
            1,
        )
        .await
        .unwrap()
        .remove(0);
    assert_eq!(job_lane(&claimed.job), Some(0));

    let policies = Arc::new(HashMap::from([(
        "default".to_string(),
        QueuePolicy::default(),
    )]));
    let clock: Arc<dyn Fn() -> OffsetDateTime + Send + Sync> = Arc::new(OffsetDateTime::now_utc);
    let semaphores = Arc::new(HashMap::<String, Arc<Semaphore>>::new());
    let store_trait: Arc<dyn JobStore> = job_store.clone();
    Worker::execute_job(
        store_trait,
        Arc::new(registry),
        policies,
        clock,
        semaphores,
        claimed.job.clone(),
        claimed.claim.clone(),
    )
    .await
    .unwrap();

    let head = store.head(&lane_site(SITE, 0)).await.unwrap().unwrap();
    assert_eq!(head.seq, 1, "the job drained the lane once");
    assert_eq!(
        job_store.get(job_id).await.unwrap().unwrap().state,
        JobState::Succeeded,
        "the handler completed the job"
    );
    // The handler's result type is the engine's, not a private one.
    let _: HandlerResult = HandlerResult::Ok;
}
