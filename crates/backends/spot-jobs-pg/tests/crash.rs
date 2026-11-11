//! The acceptance test of WP-PLT-06: a node dies in the middle of jobs and another node finishes
//! all of them, once as to effect (ADR-068 charter rule 26: recovery paths are exercised).
//!
//! Handlers are idempotent, as the engine requires: the effect row of a job is written once, in
//! the handler's own transaction (`ON CONFLICT DO NOTHING` on the job id). At-least-once
//! execution plus an idempotent effect is "exactly once as to effect".
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use common::*;
use spot_jobs::model::InsertResult;
use spot_jobs::store::JobStore;
use spot_jobs_pg::PgJobStore;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

const JOBS: usize = 50;
const STARTED_BEFORE_KILL: i64 = 20;

async fn insert_jobs(store: &PgJobStore, n: usize) {
    for i in 0..n {
        let r = store.insert_if_new(new_job(i)).await.unwrap();
        assert!(matches!(r, InsertResult::Inserted(_)));
    }
}

async fn started_count(db: &TestDb) -> i64 {
    db.raw_client()
        .await
        .query_one("SELECT count(*) FROM started", &[])
        .await
        .unwrap()
        .get(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_worker_killed_mid_job_loses_nothing_and_doubles_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    create_effect_tables(&db).await;
    let effects_pool = pool(&db, 40, "effects");

    // Node 1 claims 20 jobs, records "started" for each and sleeps inside the handler.
    let pool1 = pool(&db, 40, "node-1");
    let store1 = Arc::new(PgJobStore::new(pool1.clone()));
    store1.prepare().await.unwrap();
    insert_jobs(&store1, JOBS).await;
    // Long enough that a loaded CI runner starting 20 handlers cannot outlast the lease.
    let ttl = Duration::from_secs(10);
    let claimed = store1
        .claim(
            &["default".to_owned()],
            "node-1",
            ttl,
            OffsetDateTime::now_utc(),
            STARTED_BEFORE_KILL as u32,
        )
        .await
        .unwrap();
    assert_eq!(claimed.len(), STARTED_BEFORE_KILL as usize);
    let mut handlers = tokio::task::JoinSet::new();
    for c in claimed {
        let effects_pool = effects_pool.clone();
        handlers
            .spawn(async move { handle(&effects_pool, &c.job, Duration::from_secs(600)).await });
    }
    wait_for("20 handlers to start", Duration::from_secs(20), || async {
        started_count(&db).await == STARTED_BEFORE_KILL
    })
    .await;

    // The node dies: handlers stop mid-sleep, its pool is gone, nothing is completed.
    handlers.abort_all();
    while handlers.join_next().await.is_some() {}
    pool1.close();
    drop(store1);

    // What the dead node left behind: 20 running with leases, 30 never claimed.
    let store2 =
        Arc::new(PgJobStore::new(pool(&db, 20, "node-2")).with_reap_retry_delay(Duration::ZERO));
    let before = all_jobs(&*store2).await;
    assert_eq!(
        before.iter().filter(|j| state_of(j) == "running").count(),
        20
    );
    assert_eq!(
        before.iter().filter(|j| state_of(j) == "pending").count(),
        30
    );
    assert!(
        store2.reap_lapsed().await.unwrap().is_empty(),
        "live leases must not be reaped before they lapse"
    );

    // Node 2 starts; after the lease lapses it reaps and finishes everything.
    let stop = Arc::new(AtomicBool::new(false));
    let tally = Arc::new(Tally::default());
    let worker = spawn_worker(
        store2.clone(),
        effects_pool.clone(),
        "node-2",
        Duration::from_secs(30),
        Duration::from_millis(5),
        10,
        stop.clone(),
        tally.clone(),
    );
    let started = std::time::Instant::now();
    let reaped = std::sync::atomic::AtomicUsize::new(0);
    wait_for("all jobs to succeed", Duration::from_secs(60), || async {
        reaped.fetch_add(store2.reap_lapsed().await.unwrap().len(), Ordering::Relaxed);
        all_jobs(&*store2)
            .await
            .iter()
            .all(|j| state_of(j) == "succeeded")
    })
    .await;
    stop.store(true, Ordering::Relaxed);
    worker.await.unwrap();
    let reaped = reaped.load(Ordering::Relaxed);
    println!(
        "recovered {JOBS} jobs in {:?}; reaped {reaped}; node 2 completed {}",
        started.elapsed(),
        tally.completed.load(Ordering::Relaxed)
    );

    // The worker reaps too, so this loop counts only the reaps it won; what matters is that no
    // more than the dead node's 20 jobs were ever reaped, and that exactly those 20 ran a second
    // attempt (asserted below).
    assert!(
        reaped <= 20,
        "only the dead node's jobs are reaped: {reaped}"
    );
    let jobs = all_jobs(&*store2).await;
    assert_eq!(jobs.len(), JOBS);
    assert!(
        jobs.iter()
            .all(|j| state_of(j) == "succeeded" && j.claim.is_none())
    );
    let effects = effects(&db).await;
    assert_eq!(effects.len(), JOBS, "one effect row per job");
    let mut crashed = 0;
    for job in &jobs {
        let id = i64::try_from(job.id.0).unwrap();
        let (_, effect_attempt) = *effects.iter().find(|(j, _)| *j == id).expect("an effect");
        // The effect is that of the final attempt, and attempts are counted.
        assert_eq!(
            i64::from(effect_attempt),
            i64::from(job.attempt),
            "job {id}"
        );
        if job.attempt == 2 {
            crashed += 1;
        } else {
            assert_eq!(job.attempt, 1, "job {id} ran once");
        }
    }
    assert_eq!(crashed, 20, "the 20 killed jobs ran a second attempt");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn killed_database_connections_mid_complete_lose_nothing_and_double_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    create_effect_tables(&db).await;
    let effects_pool = pool(&db, 40, "effects");
    let victim_pool = pool(&db, 20, "victim-conn");
    // Plenty of attempts: repeated kills must not dead-letter a healthy job in this test.
    let victim = Arc::new(
        PgJobStore::new(victim_pool.clone())
            .with_max_attempts(100)
            .with_reap_retry_delay(Duration::ZERO),
    );
    victim.prepare().await.unwrap();
    insert_jobs(&victim, JOBS).await;

    // The killer terminates the backends of the victim's pool again and again, so claims and
    // completions lose their connection mid-statement.
    let admin = db.admin_client().await;
    let dbname = db.name().to_owned();
    let killing = Arc::new(AtomicBool::new(true));
    let killed = Arc::new(std::sync::atomic::AtomicI64::new(0));
    let killer = {
        let killing = killing.clone();
        let killed = killed.clone();
        tokio::spawn(async move {
            while killing.load(Ordering::Relaxed) {
                let n = admin
                    .query(
                        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
                         WHERE application_name = 'victim-conn' AND datname = $1
                           AND pid <> pg_backend_pid()",
                        &[&dbname],
                    )
                    .await
                    .map(|r| r.len())
                    .unwrap_or(0);
                killed.fetch_add(i64::try_from(n).unwrap(), Ordering::Relaxed);
                sleep_ms(12).await;
            }
        })
    };

    let stop = Arc::new(AtomicBool::new(false));
    let tally = Arc::new(Tally::default());
    let worker = spawn_worker(
        victim.clone(),
        effects_pool.clone(),
        "victim",
        Duration::from_secs(1),
        Duration::from_millis(15),
        10,
        stop.clone(),
        tally.clone(),
    );
    // Let the carnage run until it has demonstrably hit the worker (and for a minimum time).
    let begun = std::time::Instant::now();
    wait_for("injected failures", Duration::from_secs(60), || async {
        begun.elapsed() > Duration::from_secs(2)
            && tally.errors.load(Ordering::Relaxed) >= 3
            && killed.load(Ordering::Relaxed) >= 3
    })
    .await;
    killing.store(false, Ordering::Relaxed);
    killer.await.unwrap();
    let killed = killed.load(Ordering::Relaxed);
    stop.store(true, Ordering::Relaxed);
    worker.await.unwrap();
    victim_pool.close();
    println!(
        "{killed} backends terminated; the victim saw {} errors, completed {}",
        tally.errors.load(Ordering::Relaxed),
        tally.completed.load(Ordering::Relaxed)
    );
    assert!(killed >= 3, "the killer must have hit connections");

    // A healthy node takes over: leases lapse (1 s), it reaps and finishes the rest.
    let rescuer =
        Arc::new(PgJobStore::new(pool(&db, 20, "rescuer")).with_reap_retry_delay(Duration::ZERO));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = spawn_worker(
        rescuer.clone(),
        effects_pool.clone(),
        "rescuer",
        Duration::from_secs(30),
        Duration::from_millis(5),
        10,
        stop.clone(),
        Arc::new(Tally::default()),
    );
    wait_for("all jobs to succeed", Duration::from_secs(60), || async {
        rescuer.reap_lapsed().await.unwrap();
        all_jobs(&*rescuer)
            .await
            .iter()
            .all(|j| state_of(j) == "succeeded")
    })
    .await;
    stop.store(true, Ordering::Relaxed);
    worker.await.unwrap();

    let jobs = all_jobs(&*rescuer).await;
    assert_eq!(jobs.len(), JOBS);
    assert!(
        jobs.iter()
            .all(|j| state_of(j) == "succeeded" && j.claim.is_none()),
        "no job is left running"
    );
    let effects = effects(&db).await;
    assert_eq!(effects.len(), JOBS, "exactly one effect per job");
    for job in &jobs {
        let id = i64::try_from(job.id.0).unwrap();
        let (_, effect_attempt) = *effects.iter().find(|(j, _)| *j == id).expect("an effect");
        assert!(job.attempt >= 1);
        assert!(
            u32::try_from(effect_attempt).unwrap() <= job.attempt,
            "job {id}: effect from attempt {effect_attempt} of {}",
            job.attempt
        );
    }
    let reruns = jobs.iter().filter(|j| j.attempt > 1).count();
    println!("{reruns} of {JOBS} jobs needed more than one attempt");
}
