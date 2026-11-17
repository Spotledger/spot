//! The durable bounce log through the worker and the hook (WP-PLT-11.T11): a mail job killed on
//! its last attempt writes one `spot_mail_bounce_log` row per recipient, a second pass of the hook
//! for the same job writes none, and a restart (a new pool) still sees them.
//!
//! Needs a PostgreSQL server: skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`, like
//! the other database tests. The job store, sent log and transport are the in-memory ones; only
//! the bounce sink is PostgreSQL's. As in `mail_bounce.rs`, "the last attempt" is a queue policy
//! with `max_attempts = 0`.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use deadpool_postgres::{PoolConfig, Runtime};
use spot_cli::config;
use spot_cli::services::jobs::{FinalAttemptHooks, build_worker};
use spot_cli::services::mail::{Mail, Settings};
use spot_core::{ManualClock, SiteId};
use spot_files::memory::MemoryFileStore;
use spot_jobs::model::JobState;
use spot_jobs::worker::JobHandlerRegistry;
use spot_jobs::{InMemoryJobStore, JobStore, QueuePolicy};
use spot_mail_queue::{
    BounceClass, InMemorySentLog, QueuedMailbox, QueuedMessage, SentLog, enqueue_mail,
};
use spot_mail_queue_pg::PgBounceSink;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;
use tokio::sync::watch;
use tokio_postgres::NoTls;

const MESSAGE_ID: &str = "msg-bounce-pg-1@example.test";

fn site() -> SiteId {
    SiteId::new("acme").unwrap()
}

fn now() -> OffsetDateTime {
    time::macros::datetime!(2026-11-11 08:00 UTC)
}

fn message() -> QueuedMessage {
    let mut m = QueuedMessage::new(
        QueuedMailbox::address("noreply@localhost.invalid"),
        "Invoice",
    );
    m.to = vec![QueuedMailbox::address("buyer@customer.test")];
    m.cc = vec![QueuedMailbox::address("boss@customer.test")];
    m.text = Some("Body".to_owned());
    m
}

fn pool(db: &TestDb, app: &str) -> deadpool_postgres::Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(4));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

async fn sink(db: &TestDb, app: &str) -> Arc<PgBounceSink> {
    let sink = PgBounceSink::new(pool(db, app));
    sink.prepare().await.unwrap();
    Arc::new(sink)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_killed_mail_job_bounces_each_recipient_once_in_postgres_even_when_the_hook_runs_twice() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let bounces = sink(&db, "bounce_hook").await;
    let sent_log = Arc::new(InMemorySentLog::new());
    let mail = Mail::new(Settings {
        site: site(),
        mail: config::Mail::default(),
        files: Arc::new(MemoryFileStore::new()),
        sent_log: Some(Arc::clone(&sent_log) as Arc<dyn SentLog>),
        bounces: Some(Arc::clone(&bounces) as Arc<dyn spot_mail_queue::BounceSink>),
        clock: Arc::new(ManualClock::new(now())),
    });
    let hooks = FinalAttemptHooks::new();
    hooks.register(mail.final_attempt_hook());
    // The same hook object a second time stands for a second pass over the same dead job.
    hooks.register(mail.final_attempt_hook());
    let mut registry = JobHandlerRegistry::new();
    registry.register(spot_mail_queue::MAIL_JOB_NAME, move |_ctx| {
        Box::pin(async move { panic!("the handler panics") })
    });
    let policy = QueuePolicy {
        timeout: Duration::from_millis(150),
        max_attempts: 0,
        ..QueuePolicy::default()
    };
    let store = Arc::new(InMemoryJobStore::new());
    let worker = build_worker(
        Arc::clone(&store) as Arc<dyn JobStore>,
        Arc::new(registry),
        HashMap::from([("default".to_owned(), policy)]),
        Arc::new(spot_core::SystemClock::new()),
        hooks,
    );
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(async move {
        let _ = worker
            .run("w1", vec!["default".to_owned()], rx, Duration::from_secs(1))
            .await;
    });
    let job = enqueue_mail(site(), &message(), "default", MESSAGE_ID, now()).unwrap();
    let id = match store.insert_if_new(job).await.unwrap() {
        spot_jobs::model::InsertResult::Inserted(id)
        | spot_jobs::model::InsertResult::Duplicate(id) => id,
    };
    let mut state = JobState::Pending;
    for _ in 0..200 {
        state = store.get(id).await.unwrap().unwrap().state;
        if !matches!(state, JobState::Pending | JobState::Running) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(state, JobState::Dead);
    let _ = stop.send(true);
    let _ = task.await;

    // A restart is a new pool over the same database.
    let after = sink(&db, "bounce_hook_after").await;
    let rows = after.records_for(&site(), id.0).await.unwrap();
    let recipients: Vec<&str> = rows.iter().map(|r| r.recipient.as_str()).collect();
    assert_eq!(
        recipients,
        ["boss@customer.test", "buyer@customer.test"],
        "one row per recipient even though two hooks ran for the job"
    );
    assert!(
        rows.iter()
            .all(|r| r.class == BounceClass::RetriesExhausted)
    );
    assert!(
        mail.bounces().is_empty(),
        "the durable sink replaces the in-memory one"
    );
}

#[test]
fn the_bounce_log_setting_is_validated() {
    let mut cfg = config::Mail::default();
    assert_eq!(cfg.bounce_log, "memory");
    assert!(!cfg.bounce_log_in_postgres("postgres"));
    cfg.bounce_log = "postgres".to_owned();
    assert!(cfg.bounce_log_in_postgres("postgres"));
}
