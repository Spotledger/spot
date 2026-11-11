//! PostgreSQL-specific behaviour the shared contract suite does not reach: the database clock,
//! fencing across reclaims, dead-lettering, the per-site cap, retention and the real `Worker`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use common::*;
use spot_jobs::model::{InsertResult, JobState, NewJob};
use spot_jobs::policy::QueuePolicy;
use spot_jobs::store::{JobFilter, JobStore};
use spot_jobs::worker::{HandlerResult, JobHandlerRegistry, Worker};
use spot_jobs::{Error, JobId};
use spot_jobs_pg::{PgJobStore, Retention};
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

async fn fresh(size: usize) -> Option<(TestDb, PgJobStore)> {
    let db = TestDb::create().await?;
    let store = PgJobStore::new(pool(&db, size, "semantics"));
    store.prepare().await.unwrap();
    Some((db, store))
}

fn q() -> Vec<String> {
    vec!["default".to_owned()]
}

fn inserted(r: InsertResult) -> JobId {
    match r {
        InsertResult::Inserted(id) => id,
        other => panic!("expected an insert, got {other:?}"),
    }
}

#[tokio::test]
async fn the_lease_comes_from_the_database_clock_not_the_callers() {
    let Some((db, store)) = fresh(4).await else {
        return;
    };
    store.insert_if_new(new_job(0)).await.unwrap();
    // A node whose clock is a year ahead.
    let skewed = OffsetDateTime::now_utc() + time::Duration::days(365);
    let got = store
        .claim(&q(), "w", Duration::from_secs(30), skewed, 1)
        .await
        .unwrap();
    let db_now: OffsetDateTime = db
        .raw_client()
        .await
        .query_one("SELECT now()", &[])
        .await
        .unwrap()
        .get(0);
    let expires = got[0].claim.expires_at;
    let ahead = expires - db_now;
    assert!(
        ahead > time::Duration::seconds(20) && ahead < time::Duration::seconds(35),
        "lease should end ~30 s after the database's now, was {ahead}"
    );
    // Heartbeat with the skewed clock does not move it either.
    store
        .heartbeat(&got[0].claim, Duration::from_secs(30), skewed)
        .await
        .unwrap();
    let job = store.get(got[0].job.id).await.unwrap().unwrap();
    assert!(job.claim.unwrap().expires_at - db_now < time::Duration::seconds(40));
    // The database-clock reaper does not touch a live lease.
    assert!(store.reap_lapsed().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_stale_claim_changes_nothing_after_the_job_was_reclaimed() {
    let Some((_db, store)) = fresh(4).await else {
        return;
    };
    let store = store.with_reap_retry_delay(Duration::ZERO);
    let id = inserted(store.insert_if_new(new_job(0)).await.unwrap());
    let first = store
        .claim(
            &q(),
            "w1",
            Duration::from_millis(300),
            OffsetDateTime::now_utc(),
            1,
        )
        .await
        .unwrap();
    sleep_ms(500).await;
    assert_eq!(store.reap_lapsed().await.unwrap(), vec![id]);
    let second = store
        .claim(
            &q(),
            "w2",
            Duration::from_secs(30),
            OffsetDateTime::now_utc(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(second[0].job.attempt, 2);
    assert_ne!(first[0].claim.token, second[0].claim.token);

    // The first worker wakes up: every call with its claim is refused.
    let stale = &first[0].claim;
    assert_eq!(
        store.complete(stale, JobState::Succeeded, None).await,
        Err(Error::ClaimLost)
    );
    assert_eq!(
        store
            .heartbeat(stale, Duration::from_secs(30), OffsetDateTime::now_utc())
            .await,
        Err(Error::ClaimLost)
    );
    let job = store.get(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Running);
    assert_eq!(job.claim.as_ref().unwrap().worker, "w2");
    // The current holder completes; a second completion with the same claim is refused.
    store
        .complete(&second[0].claim, JobState::Succeeded, None)
        .await
        .unwrap();
    assert_eq!(
        store
            .complete(&second[0].claim, JobState::Succeeded, None)
            .await,
        Err(Error::ClaimLost)
    );
}

#[tokio::test]
async fn a_job_that_keeps_losing_its_worker_is_dead_lettered() {
    let Some((_db, store)) = fresh(4).await else {
        return;
    };
    let store = store
        .with_max_attempts(2)
        .with_reap_retry_delay(Duration::ZERO);
    let id = inserted(store.insert_if_new(new_job(0)).await.unwrap());
    for attempt in 1..=2 {
        let got = store
            .claim(
                &q(),
                "w",
                Duration::from_millis(100),
                OffsetDateTime::now_utc(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(got[0].job.attempt, attempt);
        sleep_ms(250).await;
        assert_eq!(store.reap_lapsed().await.unwrap(), vec![id]);
    }
    let job = store.get(id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Dead);
    assert_eq!(job.last_error.as_deref(), Some("job.worker_lost"));
    assert!(
        store
            .claim(
                &q(),
                "w",
                Duration::from_secs(1),
                OffsetDateTime::now_utc(),
                1
            )
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn retrying_waits_for_its_time_and_idempotency_ends_with_the_job() {
    let Some((_db, store)) = fresh(4).await else {
        return;
    };
    let key = |n: NewJob| n.with_idempotency_key("once".to_owned());
    let id = inserted(store.insert_if_new(key(new_job(0))).await.unwrap());
    assert_eq!(
        store.insert_if_new(key(new_job(1))).await.unwrap(),
        InsertResult::Duplicate(id)
    );
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
    // Still a duplicate while running and while retrying.
    assert_eq!(
        store.insert_if_new(key(new_job(1))).await.unwrap(),
        InsertResult::Duplicate(id)
    );
    let at = OffsetDateTime::now_utc() + time::Duration::milliseconds(700);
    store
        .complete(
            &got[0].claim,
            JobState::Retrying { at },
            Some("boom".into()),
        )
        .await
        .unwrap();
    assert_eq!(
        store.insert_if_new(key(new_job(1))).await.unwrap(),
        InsertResult::Duplicate(id)
    );
    let now = OffsetDateTime::now_utc();
    assert!(
        store
            .claim(&q(), "w", Duration::from_secs(30), now, 1)
            .await
            .unwrap()
            .is_empty()
    );
    sleep_ms(900).await;
    let again = store
        .claim(&q(), "w", Duration::from_secs(30), now, 1)
        .await
        .unwrap();
    assert_eq!(again[0].job.attempt, 2);
    assert_eq!(again[0].job.last_error.as_deref(), Some("boom"));
    store
        .complete(&again[0].claim, JobState::Succeeded, None)
        .await
        .unwrap();
    // Finished: the key is free again.
    let new_id = inserted(store.insert_if_new(key(new_job(2))).await.unwrap());
    assert_ne!(new_id, id);
}

#[tokio::test]
async fn invalid_input_and_cancellation() {
    let Some((_db, store)) = fresh(4).await else {
        return;
    };
    let mut bad = new_job(0);
    bad.name = "no-colon".to_owned();
    assert_eq!(store.insert_if_new(bad).await, Err(Error::InvalidName));
    let id = inserted(store.insert_if_new(new_job(1)).await.unwrap());
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
    // Cancelling a running job fences its worker.
    store.cancel(id).await.unwrap();
    assert_eq!(
        store.get(id).await.unwrap().unwrap().state,
        JobState::Cancelled
    );
    assert_eq!(
        store
            .complete(&got[0].claim, JobState::Succeeded, None)
            .await,
        Err(Error::ClaimLost)
    );
    // Cancelling again is fine; an unknown job is an error; a job cannot be completed as pending.
    store.cancel(id).await.unwrap();
    assert!(store.cancel(JobId(999_999)).await.is_err());
    let id2 = inserted(store.insert_if_new(new_job(2)).await.unwrap());
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
    assert_eq!(got[0].job.id, id2);
    assert!(
        store
            .complete(&got[0].claim, JobState::Pending, None)
            .await
            .is_err()
    );
    let listed = store
        .list(JobFilter {
            state: Some(JobState::Cancelled),
            ..JobFilter::default()
        })
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_per_site_cap_is_never_exceeded_under_concurrent_claims() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let store = Arc::new(PgJobStore::new(pool(&db, 32, "cap")).with_max_running_per_site(3));
    store.prepare().await.unwrap();
    for i in 0..30 {
        for s in ["noisy", "quiet"] {
            let mut job = new_job(i);
            job.site = site(s);
            store.insert_if_new(job).await.unwrap();
        }
    }
    let mut tasks = Vec::new();
    for i in 0..24 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            store
                .claim(
                    &q(),
                    &format!("w{i}"),
                    Duration::from_secs(60),
                    OffsetDateTime::now_utc(),
                    4,
                )
                .await
                .unwrap()
        }));
    }
    let mut per_site: HashMap<String, usize> = HashMap::new();
    for t in tasks {
        for c in t.await.unwrap() {
            *per_site.entry(c.job.site.to_string()).or_default() += 1;
        }
    }
    println!("claimed per site: {per_site:?}");
    assert!(
        per_site.values().all(|n| *n <= 3),
        "cap exceeded: {per_site:?}"
    );
    // Both sites reach the cap: 24 claimers asking for 4 each leave nothing unclaimed.
    assert_eq!(per_site.get("noisy"), Some(&3));
    assert_eq!(per_site.get("quiet"), Some(&3));
    // Completing one frees one slot.
    let running = store
        .list(JobFilter {
            state: Some(JobState::Running),
            site: Some(site("noisy")),
            limit: None,
            queue: None,
        })
        .await
        .unwrap();
    let claim = running[0].claim.clone().unwrap();
    store
        .complete(&claim, JobState::Succeeded, None)
        .await
        .unwrap();
    let more = store
        .claim(
            &q(),
            "late",
            Duration::from_secs(60),
            OffsetDateTime::now_utc(),
            10,
        )
        .await
        .unwrap();
    assert_eq!(more.len(), 1);
}

#[tokio::test]
async fn retention_removes_old_finished_jobs_only() {
    let Some((db, store)) = fresh(4).await else {
        return;
    };
    let c = db.raw_client().await;
    // 1,200 old succeeded (more than one batch), old dead, a recent succeeded, live jobs.
    c.batch_execute(
        "INSERT INTO spot_job (site, name, args, queue, state, max_attempts, not_before, finished_at)
         SELECT 'test-site', 'app:work', '{}', 'default', 'succeeded', 3, now(), now() - interval '15 days'
         FROM generate_series(1, 1200);
         INSERT INTO spot_job (site, name, args, queue, state, max_attempts, not_before, finished_at)
         SELECT 'test-site', 'app:work', '{}', 'default', 'dead', 3, now(), now() - interval '80 days'
         FROM generate_series(1, 5);
         INSERT INTO spot_job (site, name, args, queue, state, max_attempts, not_before, finished_at)
         VALUES ('test-site', 'app:work', '{}', 'default', 'dead', 3, now(), now() - interval '91 days'),
                ('test-site', 'app:work', '{}', 'default', 'succeeded', 3, now(), now() - interval '1 day'),
                ('test-site', 'app:work', '{}', 'default', 'cancelled', 3, now(), now() - interval '20 days');
         INSERT INTO spot_job (site, name, args, queue, state, max_attempts, not_before)
         VALUES ('test-site', 'app:work', '{}', 'default', 'pending', 3, now() - interval '200 days');
         INSERT INTO spot_job_outbox_seen (event_id, job_id, seen_at)
         VALUES ('old', 1, now() - interval '100 days'), ('new', 2, now());",
    )
    .await
    .unwrap();
    let report = store.purge_finished(&Retention::default()).await.unwrap();
    assert_eq!(report.succeeded, 1201); // 1,200 succeeded and the cancelled one, over 14 days
    assert_eq!(report.dead, 1); // only the 91-day-old one, over 90 days
    assert_eq!(report.outbox_seen, 1);
    let left: i64 = c
        .query_one("SELECT count(*) FROM spot_job", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(left, 5 + 1 + 1);
    // Idempotent.
    assert_eq!(
        store.purge_finished(&Retention::default()).await.unwrap(),
        Default::default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_worker_runs_jobs_on_postgres() {
    let Some((_db, store)) = fresh(16).await else {
        return;
    };
    let store: Arc<dyn JobStore> = Arc::new(store);
    for i in 0..30 {
        store.insert_if_new(new_job(i)).await.unwrap();
    }
    let ran = Arc::new(AtomicU32::new(0));
    let mut registry = JobHandlerRegistry::new();
    let counter = ran.clone();
    registry.register("app:work", move |ctx| {
        let counter = counter.clone();
        Box::pin(async move {
            assert_eq!(ctx.attempt, 1);
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(HandlerResult::Ok)
        })
    });
    let policies = HashMap::from([("default".to_owned(), QueuePolicy::default())]);
    let worker = Worker::new(
        store.clone(),
        Arc::new(registry),
        policies,
        Arc::new(OffsetDateTime::now_utc),
    );
    let (stop, rx) = tokio::sync::watch::channel(false);
    let run =
        tokio::spawn(async move { worker.run("node", q(), rx, Duration::from_secs(5)).await });
    wait_for("30 jobs to succeed", Duration::from_secs(30), || async {
        store
            .list(JobFilter::default())
            .await
            .unwrap()
            .iter()
            .all(|j| j.state == JobState::Succeeded)
    })
    .await;
    stop.send(true).unwrap();
    run.await.unwrap().unwrap();
    assert_eq!(ran.load(Ordering::SeqCst), 30);
}
