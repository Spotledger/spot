//! 200 concurrent claimers over 5,000 jobs: nothing is claimed twice, everything completes, and
//! the run fits a stated time bound.
//!
//! Needs up to 200 pooled connections. A server whose `max_connections` is lower gets as many as
//! it can spare (the test prints the pool size); CI starts PostgreSQL with `max_connections >= 250`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::*;
use spot_jobs::model::JobState;
use spot_jobs::store::JobStore;
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

const SESSIONS: usize = 200;
const JOBS: i64 = 5_000;
/// The whole run, from the first claim to the last completion. A laptop takes a few seconds;
/// the bound leaves room for a loaded CI runner and still catches a serialised claim path.
const TIME_BOUND: Duration = Duration::from_secs(60);

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_claimers_claim_five_thousand_jobs_exactly_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let admin = db.admin_client().await;
    let max: i64 = admin
        .query_one("SELECT current_setting('max_connections')::bigint", &[])
        .await
        .unwrap()
        .get(0);
    let size = usize::try_from((max - 40).clamp(10, SESSIONS as i64)).unwrap();
    println!("pool of {size} connections for {SESSIONS} claimers (server max_connections {max})");
    let store = Arc::new(PgJobStore::new(pool(&db, size, "load")));
    store.prepare().await.unwrap();
    // Bulk-load the jobs in one statement; enqueueing is not what this test measures.
    db.raw_client()
        .await
        .execute(
            "INSERT INTO spot_job (site, name, args, queue, state, max_attempts, not_before)
             SELECT 'test-site', 'app:work', jsonb_build_object('n', g), 'default', 'pending',
                    3, now() - interval '1 second'
             FROM generate_series(1, $1::bigint) g",
            &[&JOBS],
        )
        .await
        .unwrap();

    let claimed_ids = Arc::new(Mutex::new(Vec::<u64>::new()));
    let started = Instant::now();
    let mut tasks = Vec::new();
    for i in 0..SESSIONS {
        let (store, claimed_ids) = (store.clone(), claimed_ids.clone());
        tasks.push(tokio::spawn(async move {
            let worker = format!("claimer-{i}");
            let mut empty_in_a_row = 0;
            while empty_in_a_row < 3 {
                let got = store
                    .claim(
                        &["default".to_owned()],
                        &worker,
                        Duration::from_secs(60),
                        OffsetDateTime::now_utc(),
                        5,
                    )
                    .await
                    .unwrap();
                if got.is_empty() {
                    empty_in_a_row += 1;
                    sleep_ms(20).await;
                    continue;
                }
                empty_in_a_row = 0;
                for c in got {
                    claimed_ids.lock().unwrap().push(c.job.id.0);
                    store
                        .complete(&c.claim, JobState::Succeeded, None)
                        .await
                        .unwrap();
                }
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let elapsed = started.elapsed();
    println!("{SESSIONS} claimers completed {JOBS} jobs in {elapsed:?} (bound {TIME_BOUND:?})");

    let ids = claimed_ids.lock().unwrap().clone();
    let unique: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(ids.len(), unique.len(), "a job was claimed twice");
    assert_eq!(
        i64::try_from(ids.len()).unwrap(),
        JOBS,
        "every job was claimed"
    );
    let row = db
        .raw_client()
        .await
        .query_one(
            "SELECT count(*) FILTER (WHERE state = 'succeeded'), count(*), max(attempt)
             FROM spot_job",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(row.get::<_, i64>(0), JOBS);
    assert_eq!(row.get::<_, i64>(1), JOBS);
    assert_eq!(row.get::<_, i32>(2), 1, "no job needed a second attempt");
    assert!(
        elapsed < TIME_BOUND,
        "took {elapsed:?}, bound {TIME_BOUND:?}"
    );
}
