//! Shared setup: scratch databases, pools and a small worker loop for the crash tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use serde_json::json;
use spot_core::SiteId;
use spot_jobs::model::NewJob;
use spot_jobs::store::JobStore;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;
use tokio_postgres::NoTls;

/// A pool of `size` connections whose server-side `application_name` is `app`.
pub fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

pub fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

pub fn new_job(n: usize) -> NewJob {
    NewJob::new(
        "app:work".to_owned(),
        json!({ "n": n }),
        "default".to_owned(),
        site("test-site"),
        OffsetDateTime::now_utc() - time::Duration::seconds(1),
    )
}

/// The jobs of a store in id order.
///
/// A pooled connection can be opened while the test database is still settling (the crash tests
/// kill backends; a loaded runner is slow to hand out a new one), so the read is retried a few
/// times before it counts as a failure.
pub async fn all_jobs(store: &dyn JobStore) -> Vec<spot_jobs::Job> {
    let mut last = None;
    for _ in 0..8 {
        match store.list(spot_jobs::store::JobFilter::default()).await {
            Ok(jobs) => return jobs,
            Err(error) => {
                last = Some(error);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
        }
    }
    panic!("the job store could not be read after 8 attempts: {last:?}")
}

pub async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

pub type Store = Arc<dyn JobStore>;

// ---- The crash-test worker ----------------------------------------------------------------

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use spot_jobs::Job;
use spot_jobs::model::JobState;
use spot_jobs_pg::PgJobStore;
use tokio::task::JoinSet;

/// Creates the `started` and `effect` tables the test handlers write.
pub async fn create_effect_tables(db: &TestDb) {
    db.raw_client()
        .await
        .batch_execute(
            "CREATE TABLE started (job_id bigint NOT NULL, attempt integer NOT NULL,
                                   PRIMARY KEY (job_id, attempt));
             CREATE TABLE effect (job_id bigint PRIMARY KEY, attempt integer NOT NULL);",
        )
        .await
        .unwrap();
}

/// What a worker did, for the tests to report.
#[derive(Default)]
pub struct Tally {
    pub claimed: AtomicU64,
    pub completed: AtomicU64,
    pub errors: AtomicU64,
}

/// The test handler: record "started", wait `work`, then write the job's effect in its own
/// transaction, once per job (the handler is idempotent, as the engine requires), and return.
pub async fn handle(effects: &Pool, job: &Job, work: Duration) -> Result<(), String> {
    let id = i64::try_from(job.id.0).unwrap();
    let attempt = i32::try_from(job.attempt).unwrap();
    let conn = effects.get().await.map_err(|e| e.to_string())?;
    conn.execute(
        "INSERT INTO started (job_id, attempt) VALUES ($1, $2) ON CONFLICT DO NOTHING",
        &[&id, &attempt],
    )
    .await
    .map_err(|e| e.to_string())?;
    drop(conn);
    tokio::time::sleep(work).await;
    let mut conn = effects.get().await.map_err(|e| e.to_string())?;
    let tx = conn.transaction().await.map_err(|e| e.to_string())?;
    tx.execute(
        "INSERT INTO effect (job_id, attempt) VALUES ($1, $2) ON CONFLICT (job_id) DO NOTHING",
        &[&id, &attempt],
    )
    .await
    .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}

/// A worker node: claims up to `limit` jobs at a time, runs each in its own task and completes
/// it, and keeps going until `stop` is set. Errors are counted, never retried by the worker: a
/// lost completion is what the lease and the reaper exist for.
#[allow(clippy::too_many_arguments)]
pub fn spawn_worker(
    store: Arc<PgJobStore>,
    effects: Pool,
    id: &'static str,
    ttl: Duration,
    work: Duration,
    limit: u32,
    stop: Arc<AtomicBool>,
    tally: Arc<Tally>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        while !stop.load(Ordering::Relaxed) {
            match store
                .claim(
                    &["default".to_owned()],
                    id,
                    ttl,
                    OffsetDateTime::now_utc(),
                    limit,
                )
                .await
            {
                Ok(claimed) => {
                    let n = claimed.len();
                    tally.claimed.fetch_add(n as u64, Ordering::Relaxed);
                    for c in claimed {
                        let (store, effects, tally) =
                            (store.clone(), effects.clone(), tally.clone());
                        tasks.spawn(async move {
                            if handle(&effects, &c.job, work).await.is_err() {
                                tally.errors.fetch_add(1, Ordering::Relaxed);
                                return;
                            }
                            match store.complete(&c.claim, JobState::Succeeded, None).await {
                                Ok(()) => tally.completed.fetch_add(1, Ordering::Relaxed),
                                Err(_) => tally.errors.fetch_add(1, Ordering::Relaxed),
                            };
                        });
                    }
                    if n == 0 {
                        sleep_ms(25).await;
                    }
                }
                Err(_) => {
                    tally.errors.fetch_add(1, Ordering::Relaxed);
                    sleep_ms(10).await;
                }
            }
            while tasks.try_join_next().is_some() {}
        }
        while tasks.join_next().await.is_some() {}
    })
}

/// Waits until `check` holds, or panics after `limit`.
pub async fn wait_for<F, Fut>(what: &str, limit: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while !check().await {
        assert!(start.elapsed() < limit, "timed out waiting for {what}");
        sleep_ms(50).await;
    }
}

/// `(job id, attempt)` of every effect row.
pub async fn effects(db: &TestDb) -> Vec<(i64, i32)> {
    db.raw_client()
        .await
        .query("SELECT job_id, attempt FROM effect ORDER BY job_id", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect()
}

pub fn state_of(job: &Job) -> &'static str {
    match job.state {
        JobState::Pending => "pending",
        JobState::Running => "running",
        JobState::Succeeded => "succeeded",
        JobState::Retrying { .. } => "retrying",
        JobState::Dead => "dead",
        JobState::Cancelled => "cancelled",
    }
}
