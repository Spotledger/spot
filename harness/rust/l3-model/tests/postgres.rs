//! 60 sequences on PostgreSQL, and the same sequence on both backends.
//!
//! Without `SPOT_TEST_PG_URL` the tests print `SKIPPED`; under `SPOT_REQUIRE_PG=1` a missing
//! server is a failure (`TestDb::create` decides).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{calls_of, run_many, sequences};
use spot_l3_model::Model;
use spot_l3_model::runner::run_sequence;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_fake::FakeBackend;

async fn connect(db: &TestDb) -> PgBackend {
    PgBackend::connect(db.config()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixty_sequences_agree_with_the_model_on_postgres() {
    if TestDb::create().await.is_none() {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    }
    // One scratch database per sequence, kept alive by the closure's own list.
    let keep: Arc<std::sync::Mutex<Vec<TestDb>>> = Arc::default();
    let keep2 = keep.clone();
    run_many(sequences(60), move || {
        let keep = keep2.clone();
        Box::pin(async move {
            let db = TestDb::create().await.expect("scratch database");
            let backend = connect(&db).await;
            keep.lock().unwrap().push(db);
            backend
        })
    })
    .await;
}

/// The same sequence, run on both backends, gives the same report: engine-level parity.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_same_sequence_gives_the_same_result_on_both_backends() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run on PostgreSQL");
        return;
    };
    let calls = calls_of(common::base_seed());
    let fake = run_sequence(FakeBackend::new(), &calls, Model::new()).await;
    let pg = run_sequence(connect(&db).await, &calls, Model::new()).await;
    assert_eq!(fake, pg);
    assert!(fake.is_ok(), "{fake:?}");
}
