//! Drop, commit outcome, pool, health and connect behaviour against real PostgreSQL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{MARGIN, commit_killing_proxy, env, env_with, options};
use spot_postgres::{PgBackend, PgConfig};
use spot_storage_contract::{
    ErrorKind, HealthStatus, Isolation, ReadSource, StorageBackend, Transaction,
};

#[tokio::test]
async fn dropped_transaction_leaves_nothing_and_the_pool_never_serves_it_open() {
    let Some(env) = env_with(|c| c.pool_max_size = 1).await else {
        return;
    };
    env.table().await;
    let mut tx = env
        .begin(options("dropped", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (40, 1)", &[])
        .await
        .unwrap();
    drop(tx);
    assert_eq!(env.value(40).await, None, "the insert was rolled back");

    // With one connection in the pool, the next `begin` gets the replacement of the dropped one.
    let mut next = env
        .begin(options("after_drop", Isolation::ReadCommitted))
        .await
        .unwrap();
    let rows = next
        .raw_query("SELECT txid_current_if_assigned()::text", &[], None)
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<_, Option<String>>(0),
        None,
        "no inherited, writing transaction"
    );
    next.rollback().await.unwrap();

    // And no session of this database stays idle in a transaction.
    let admin = env.db.admin_client().await;
    let mut open = 1i64;
    for _ in 0..40 {
        let row = admin
            .query_one(
                "SELECT count(*) FROM pg_stat_activity
                 WHERE datname = $1 AND state LIKE 'idle in transaction%'",
                &[&env.db.name()],
            )
            .await
            .unwrap();
        open = row.get(0);
        if open == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(open, 0);
}

#[tokio::test]
async fn finished_transactions_return_their_connection_to_the_pool() {
    let Some(env) = env_with(|c| c.pool_max_size = 1).await else {
        return;
    };
    env.table().await;
    let admin = env.db.admin_client().await;
    let mut pids = Vec::new();
    for i in 0..3 {
        let mut tx = env
            .begin(options("reuse", Isolation::ReadCommitted))
            .await
            .unwrap();
        let rows = tx
            .raw_query("SELECT pg_backend_pid()", &[], None)
            .await
            .unwrap();
        pids.push(rows[0].get::<_, i32>(0));
        if i % 2 == 0 {
            tx.commit().await.unwrap();
        } else {
            tx.rollback().await.unwrap();
        }
    }
    assert!(
        pids.windows(2).all(|w| w[0] == w[1]),
        "one connection served all: {pids:?}"
    );
    drop(admin);
}

#[tokio::test]
async fn clean_commit_persists_and_deferred_violations_surface_at_commit() {
    let Some(env) = env().await else { return };
    let raw = env.db.raw_client().await;
    raw.batch_execute(
        "CREATE TABLE parent (id int PRIMARY KEY);
         CREATE TABLE child (id int PRIMARY KEY,
            parent_id int REFERENCES parent(id) DEFERRABLE INITIALLY DEFERRED);",
    )
    .await
    .unwrap();
    let mut tx = env
        .begin(options("ok", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO child VALUES (1, 7)", &[])
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO parent VALUES (7)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let n: i64 = raw
        .query_one("SELECT count(*) FROM child", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1);

    let mut bad = env
        .begin(options("bad_link", Isolation::ReadCommitted))
        .await
        .unwrap();
    bad.raw_execute("INSERT INTO child VALUES (2, 999)", &[])
        .await
        .unwrap();
    let err = bad.commit().await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ReferenceViolation);
    let n: i64 = raw
        .query_one("SELECT count(*) FROM child", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(n, 1, "the failed commit rolled back");
}

#[tokio::test]
async fn backend_killed_before_commit_is_unavailable_or_unknown_never_ok() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("killed", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (50, 1)", &[])
        .await
        .unwrap();
    let pid: i32 = tx
        .raw_query("SELECT pg_backend_pid()", &[], None)
        .await
        .unwrap()[0]
        .get(0);
    let admin = env.db.admin_client().await;
    admin
        .execute("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert!(
        matches!(
            err.kind,
            ErrorKind::Unavailable | ErrorKind::CommitOutcomeUnknown
        ),
        "{err}"
    );
    assert_eq!(env.value(50).await, None);
}

#[tokio::test]
async fn connection_already_closed_before_commit_is_unavailable() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("closed", Isolation::ReadCommitted))
        .await
        .unwrap();
    let pid: i32 = tx
        .raw_query("SELECT pg_backend_pid()", &[], None)
        .await
        .unwrap()[0]
        .get(0);
    let admin = env.db.admin_client().await;
    admin
        .execute("SELECT pg_terminate_backend($1)", &[&pid])
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    let err = tx.commit().await.unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::Unavailable,
        "nothing was sent, so nothing can have committed"
    );
}

#[tokio::test]
async fn connection_lost_while_committing_is_commit_outcome_unknown() {
    let Some(env) = env().await else { return };
    env.table().await;
    let port = commit_killing_proxy(&env.db.config()).await;
    let mut config = env.db.config();
    config.host = "127.0.0.1".to_owned();
    config.port = port;
    let through_proxy = PgBackend::connect(config).await.unwrap();
    let mut tx = through_proxy
        .begin(common::catalog(), options("lost", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (60, 1)", &[])
        .await
        .unwrap();
    let err = tx.commit().await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::CommitOutcomeUnknown);
    assert!(!err.kind.is_retryable(), "never retried blindly");
    assert_eq!(
        env.value(60).await,
        None,
        "this proxy never forwarded the commit"
    );
}

#[tokio::test]
async fn exhausted_pool_is_unavailable_after_the_wait_timeout() {
    let Some(env) = env_with(|c| {
        c.pool_max_size = 1;
        c.pool_wait_timeout = Duration::from_millis(300);
    })
    .await
    else {
        return;
    };
    let held = env
        .begin(options("held", Isolation::ReadCommitted))
        .await
        .unwrap();
    let started = Instant::now();
    let err = env
        .begin(options("waiting", Isolation::ReadCommitted))
        .await
        .err()
        .unwrap();
    let waited = started.elapsed();
    assert_eq!(err.kind, ErrorKind::Unavailable);
    assert!(err.kind.is_retryable());
    assert!(
        waited >= Duration::from_millis(250) && waited < Duration::from_millis(300) + MARGIN,
        "{waited:?}"
    );
    drop(held);
    env.begin(options("free_again", Isolation::ReadCommitted))
        .await
        .unwrap();
}

#[tokio::test]
async fn health_and_capabilities() {
    let Some(env) = env().await else { return };
    let report = env.backend.health().await;
    assert_eq!(report.status, HealthStatus::Healthy, "{report:?}");
    let names: Vec<_> = report.checks.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["primary_connection", "pool", "server_version"]);
    assert!(report.checks[0].detail.contains("ms"));
    assert!(
        report.checks[2].detail.contains("18"),
        "{:?}",
        report.checks[2]
    );

    let caps = env.backend.capabilities();
    assert_eq!(caps.backend, "postgres");
    assert_eq!(
        caps.contract_version,
        spot_storage_contract::CONTRACT_VERSION
    );
    assert_eq!(caps.read_sources, [ReadSource::Primary]);
    assert_eq!(caps.max_writes_per_tx, 10_000);
    assert_eq!(caps.feed_retention, Duration::from_secs(7 * 24 * 3600));
}

#[tokio::test]
async fn health_is_down_when_the_connection_is_refused() {
    let Some(env) = env().await else { return };
    let mut config = env.db.config();
    config.port = 1; // nothing listens there
    config.connect_timeout = Duration::from_secs(2);
    let backend = PgBackend::connect_unchecked(config).unwrap();
    let report = backend.health().await;
    assert_eq!(report.status, HealthStatus::Down, "{report:?}");
    assert_eq!(report.checks[0].status, HealthStatus::Down);
    let err = backend
        .begin(common::catalog(), options("down", Isolation::ReadCommitted))
        .await
        .err()
        .unwrap();
    assert_eq!(err.kind, ErrorKind::Unavailable);
}

#[tokio::test]
async fn connect_to_an_unreachable_server_is_unavailable() {
    let Some(env) = env().await else { return };
    let mut config: PgConfig = env.db.config();
    config.port = 1;
    let err = PgBackend::connect(config).await.err().unwrap();
    assert_eq!(err.kind, ErrorKind::Unavailable);
}
