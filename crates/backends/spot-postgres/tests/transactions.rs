//! Isolation, timeouts, read-only access, savepoints and poisoning against real PostgreSQL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::time::{Duration, Instant};

use common::{MARGIN, env, options};
use spot_storage_contract::{ErrorKind, Isolation, Transaction};

#[tokio::test]
async fn repeatable_read_second_writer_gets_serialization_failure() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut a = env
        .begin(options("rr_a", Isolation::RepeatableRead))
        .await
        .unwrap();
    let mut b = env
        .begin(options("rr_b", Isolation::RepeatableRead))
        .await
        .unwrap();
    a.raw_query("SELECT v FROM t WHERE id = 1", &[], None)
        .await
        .unwrap();
    b.raw_query("SELECT v FROM t WHERE id = 1", &[], None)
        .await
        .unwrap();
    a.raw_execute("UPDATE t SET v = v + 1 WHERE id = 1", &[])
        .await
        .unwrap();
    a.commit().await.unwrap();

    let err = b
        .raw_execute("UPDATE t SET v = v + 1 WHERE id = 1", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::SerializationFailure);
    assert!(err.kind.is_retryable());
    assert_eq!(env.value(1).await, Some(11));
}

#[tokio::test]
async fn serializable_write_skew_fails_one_transaction() {
    let Some(env) = env().await else { return };
    let raw = env.db.raw_client().await;
    raw.batch_execute(
        "CREATE TABLE oncall (id int PRIMARY KEY, on_duty bool NOT NULL);
         INSERT INTO oncall VALUES (1, true), (2, true);",
    )
    .await
    .unwrap();
    let mut a = env
        .begin(options("ser_a", Isolation::Serializable))
        .await
        .unwrap();
    let mut b = env
        .begin(options("ser_b", Isolation::Serializable))
        .await
        .unwrap();
    for tx in [&mut a, &mut b] {
        let rows = tx
            .raw_query("SELECT count(*) FROM oncall WHERE on_duty", &[], None)
            .await
            .unwrap();
        assert_eq!(rows[0].get::<_, i64>(0), 2);
    }
    a.raw_execute("UPDATE oncall SET on_duty = false WHERE id = 1", &[])
        .await
        .unwrap();
    b.raw_execute("UPDATE oncall SET on_duty = false WHERE id = 2", &[])
        .await
        .unwrap();
    a.commit().await.unwrap();
    // PostgreSQL reports the conflict when the second transaction commits.
    let err = b.commit().await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::SerializationFailure);
    let left = raw
        .query_one("SELECT count(*) FROM oncall WHERE on_duty", &[])
        .await
        .unwrap();
    assert_eq!(left.get::<_, i64>(0), 1);
}

#[tokio::test]
async fn read_committed_sees_a_concurrent_commit_on_the_next_statement() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("rc", Isolation::ReadCommitted))
        .await
        .unwrap();
    let before = tx
        .raw_query("SELECT v FROM t WHERE id = 1", &[], None)
        .await
        .unwrap();
    assert_eq!(before[0].get::<_, i32>(0), 10);
    env.db
        .raw_client()
        .await
        .batch_execute("UPDATE t SET v = 99 WHERE id = 1")
        .await
        .unwrap();
    let after = tx
        .raw_query("SELECT v FROM t WHERE id = 1", &[], None)
        .await
        .unwrap();
    assert_eq!(after[0].get::<_, i32>(0), 99);
    tx.rollback().await.unwrap();
}

#[tokio::test]
async fn declared_settings_reach_the_server() {
    let Some(env) = env().await else { return };
    let mut o = options("post_sales_invoice", Isolation::RepeatableRead);
    o.lock_timeout = Duration::from_millis(1234);
    o.statement_timeout = Duration::from_secs(20);
    let mut tx = env.begin(o).await.unwrap();
    let rows = tx
        .raw_query(
            "SELECT current_setting('lock_timeout'), current_setting('statement_timeout'),
                    current_setting('idle_in_transaction_session_timeout'),
                    current_setting('application_name'),
                    current_setting('transaction_isolation'),
                    current_setting('transaction_read_only')",
            &[],
            None,
        )
        .await
        .unwrap();
    let get = |i: usize| rows[0].get::<_, String>(i);
    assert_eq!(get(0), "1234ms");
    assert_eq!(get(1), "20s");
    assert_eq!(get(2), "30s", "statement timeout plus the 10 s margin");
    assert_eq!(get(3), "spot:post_sales_invoice");
    assert_eq!(get(4), "repeatable read");
    assert_eq!(get(5), "off");
    assert_eq!(tx.options().label.as_str(), "post_sales_invoice");
}

#[tokio::test]
async fn lock_timeout_fires_within_the_declared_time() {
    let Some(env) = env().await else { return };
    env.table().await;
    let holder = env.db.raw_client().await;
    holder
        .batch_execute("BEGIN; SELECT * FROM t WHERE id = 1 FOR UPDATE;")
        .await
        .unwrap();
    let mut o = options("lock_wait", Isolation::ReadCommitted);
    o.lock_timeout = Duration::from_millis(300);
    let mut tx = env.begin(o).await.unwrap();
    let started = Instant::now();
    let err = tx
        .raw_execute("UPDATE t SET v = 0 WHERE id = 1", &[])
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert_eq!(err.kind, ErrorKind::LockTimeout);
    assert!(
        waited >= Duration::from_millis(250),
        "gave up early: {waited:?}"
    );
    assert!(
        waited < Duration::from_millis(300) + MARGIN,
        "waited too long: {waited:?}"
    );
    holder.batch_execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn nowait_refusal_is_told_apart_from_a_lock_timeout() {
    let Some(env) = env().await else { return };
    env.table().await;
    let holder = env.db.raw_client().await;
    holder
        .batch_execute("BEGIN; SELECT * FROM t WHERE id = 1 FOR UPDATE;")
        .await
        .unwrap();
    let mut tx = env
        .begin(options("nowait", Isolation::ReadCommitted))
        .await
        .unwrap();
    let err = tx
        .raw_query(
            "SELECT * FROM t WHERE id = 1 FOR UPDATE NOWAIT",
            &[],
            Some(spot_storage_contract::Wait::NoWait),
        )
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::LockNotAvailable);
    holder.batch_execute("ROLLBACK").await.unwrap();
}

#[tokio::test]
async fn statement_timeout_and_poisoning_send_no_further_sql() {
    let Some(env) = env().await else { return };
    let mut o = options("slow", Isolation::ReadCommitted);
    o.statement_timeout = Duration::from_millis(200);
    let mut tx = env.begin(o).await.unwrap();
    let started = Instant::now();
    let err = tx
        .raw_query("SELECT pg_sleep(10)", &[], None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::StatementTimeout);
    assert!(started.elapsed() < Duration::from_millis(200) + MARGIN);

    let sent = tx.statements_sent();
    let again = tx.raw_query("SELECT 1", &[], None).await.unwrap_err();
    assert_eq!(
        again.kind,
        ErrorKind::StatementTimeout,
        "same kind, no new error"
    );
    assert_eq!(
        tx.savepoint().await.unwrap_err().kind,
        ErrorKind::StatementTimeout
    );
    assert_eq!(tx.statements_sent(), sent, "nothing was sent to the server");
    assert_eq!(
        tx.commit().await.unwrap_err().kind,
        ErrorKind::StatementTimeout,
        "a poisoned transaction cannot commit"
    );
}

#[tokio::test]
async fn read_only_transactions_refuse_writes() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("ro", Isolation::ReadCommitted).read_only())
        .await
        .unwrap();
    tx.raw_query("SELECT * FROM t", &[], None).await.unwrap();
    let err = tx
        .raw_execute("UPDATE t SET v = 1 WHERE id = 1", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::ReadOnlyTransaction);
}

#[tokio::test]
async fn work_after_a_rolled_back_savepoint_is_gone_and_earlier_work_survives() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("sp", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (10, 1)", &[])
        .await
        .unwrap();
    let sp = tx.savepoint().await.unwrap();
    tx.raw_execute("INSERT INTO t VALUES (11, 1)", &[])
        .await
        .unwrap();
    tx.rollback_to(sp).await.unwrap();
    tx.raw_execute("INSERT INTO t VALUES (12, 1)", &[])
        .await
        .unwrap();
    // The savepoint stays usable: roll back to it a second time.
    tx.rollback_to(sp).await.unwrap();
    tx.raw_execute("INSERT INTO t VALUES (13, 1)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(env.value(10).await, Some(1));
    assert_eq!(env.value(11).await, None);
    assert_eq!(env.value(12).await, None);
    assert_eq!(env.value(13).await, Some(1));
}

#[tokio::test]
async fn released_and_discarded_savepoints_are_refused_without_harming_the_transaction() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("sp_gone", Isolation::ReadCommitted))
        .await
        .unwrap();
    let s1 = tx.savepoint().await.unwrap();
    let s2 = tx.savepoint().await.unwrap();
    let s3 = tx.savepoint().await.unwrap();
    assert!(s1 < s2 && s2 < s3);

    tx.rollback_to(s1).await.unwrap();
    for gone in [s2, s3] {
        let err = tx.rollback_to(gone).await.unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::InvalidQuery,
            "discarded by rolling back to s1"
        );
    }
    let s4 = tx.savepoint().await.unwrap();
    let s5 = tx.savepoint().await.unwrap();
    tx.release(s4).await.unwrap(); // forgets s4 and the later s5
    for gone in [s4, s5] {
        assert_eq!(
            tx.release(gone).await.unwrap_err().kind,
            ErrorKind::InvalidQuery
        );
        assert_eq!(
            tx.rollback_to(gone).await.unwrap_err().kind,
            ErrorKind::InvalidQuery
        );
    }
    // Still usable: a refused savepoint never reached the server.
    tx.raw_query("SELECT 1", &[], None).await.unwrap();
    tx.rollback_to(s1).await.unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn unique_violation_inside_a_rolled_back_savepoint_leaves_the_transaction_usable() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("sp_unique", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (20, 1)", &[])
        .await
        .unwrap();
    let sp = tx.savepoint().await.unwrap();
    let err = tx
        .raw_execute("INSERT INTO t VALUES (1, 5)", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::UniqueViolation);
    assert_eq!(err.constraint.as_ref().map(|c| c.as_str()), Some("t_pkey"));
    assert_eq!(err.entity.as_ref().map(|c| c.as_str()), Some("t"));
    // Until the rollback the aborted transaction refuses everything, sending nothing.
    let sent = tx.statements_sent();
    assert_eq!(
        tx.raw_query("SELECT 1", &[], None).await.unwrap_err().kind,
        ErrorKind::UniqueViolation
    );
    assert_eq!(tx.statements_sent(), sent);
    tx.rollback_to(sp).await.unwrap();
    tx.raw_execute("INSERT INTO t VALUES (21, 1)", &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(env.value(20).await, Some(1));
    assert_eq!(env.value(21).await, Some(1));
    assert_eq!(env.value(1).await, Some(10));
}

#[tokio::test]
async fn recoverable_error_without_a_savepoint_aborts_the_transaction() {
    let Some(env) = env().await else { return };
    env.table().await;
    let mut tx = env
        .begin(options("no_sp", Isolation::ReadCommitted))
        .await
        .unwrap();
    tx.raw_execute("INSERT INTO t VALUES (30, 1)", &[])
        .await
        .unwrap();
    let err = tx
        .raw_execute("INSERT INTO t VALUES (1, 5)", &[])
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::UniqueViolation);
    // `commit` must not be sent: PostgreSQL would answer a COMMIT of an aborted transaction
    // with a silent ROLLBACK.
    assert_eq!(
        tx.commit().await.unwrap_err().kind,
        ErrorKind::UniqueViolation
    );
    assert_eq!(env.value(30).await, None);
}

#[tokio::test]
async fn write_limit_counts_records_and_forgets_rolled_back_ones() {
    let Some(env) = env().await else { return };
    let mut o = options("limit", Isolation::ReadCommitted);
    o.max_writes = 5;
    let mut tx = env.begin(o).await.unwrap();
    tx.raw_count_writes(3).unwrap();
    let sp = tx.savepoint().await.unwrap();
    tx.raw_count_writes(2).unwrap();
    tx.rollback_to(sp).await.unwrap();
    tx.raw_count_writes(2).unwrap(); // 3 + 2 = 5, not 7
    let err = tx.raw_count_writes(1).unwrap_err();
    assert_eq!(err.kind, ErrorKind::TransactionTooLarge);
    assert_eq!(
        tx.raw_query("SELECT 1", &[], None).await.unwrap_err().kind,
        ErrorKind::TransactionTooLarge,
        "the transaction is poisoned"
    );
}

#[tokio::test]
async fn max_writes_above_the_backend_limit_is_refused() {
    let Some(env) = env().await else { return };
    let mut o = options("too_big", Isolation::ReadCommitted);
    o.max_writes = 2_000_000;
    let err = env.begin(o).await.err().unwrap();
    assert_eq!(err.kind, ErrorKind::InvalidValue);
}
