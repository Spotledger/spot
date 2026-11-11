//! The supported raw-SQL seam on [`PgTransaction`] (WP-DAG-02.T3): bound parameters, the
//! transaction's own connection, write counting, error mapping and poisoning.
//!
//! A sibling `*-pg` crate owns tables of its own and writes them in the caller's transaction, so
//! that its rows and the document engine's rows commit or roll back together. These tests stand
//! in for such a crate: a scratch table created through the test helper, plus the contract
//! catalog's parties and invoices through the document store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use documents_support::common;
use documents_support::{catalog, n, party};
use spot_postgres::PgTransaction;
use spot_storage_contract::{
    DocumentStore, ErrorKind, Isolation, RowLock, StorageBackend, StorageResult, Transaction,
    TxOptions,
};

/// A scratch table of the kind a sibling crate owns, created through the test helper's client.
const CREATE_SCRATCH: &str = "CREATE TABLE scratch (id int PRIMARY KEY, note text NOT NULL)";

/// Options for these tests, with the tweak applied.
fn options(tweak: impl FnOnce(&mut TxOptions)) -> TxOptions {
    let mut options = TxOptions::new(n("raw_sql_test"), Isolation::ReadCommitted);
    tweak(&mut options);
    options
}

/// Opens a transaction on `env` with `options`.
async fn begin(
    env: &common::Env,
    cat: &std::sync::Arc<spot_storage_contract::Catalog>,
    options: TxOptions,
) -> PgTransaction {
    env.backend.begin(cat.clone(), options).await.unwrap()
}

/// Whether the scratch table holds `id`.
async fn scratch_has(env: &common::Env, id: i32) -> bool {
    env.db
        .raw_client()
        .await
        .query_opt("SELECT 1 FROM scratch WHERE id = $1", &[&id])
        .await
        .unwrap()
        .is_some()
}

/// Whether the party document exists, read in its own transaction.
async fn party_exists(
    env: &common::Env,
    cat: &std::sync::Arc<spot_storage_contract::Catalog>,
    id: spot_storage_contract::RecordId,
) -> bool {
    let mut tx = begin(env, cat, options(|_| {})).await;
    let found = tx
        .load(&n("party"), id, RowLock::None)
        .await
        .unwrap()
        .is_some();
    tx.rollback().await.unwrap();
    found
}

/// Inserts `id` into the scratch table through the seam, as a sibling crate would.
async fn write_scratch(tx: &mut PgTransaction, id: i32) -> StorageResult<u64> {
    tx.raw_execute(
        "INSERT INTO scratch (id, note) VALUES ($1, $2)",
        &[&id, &"raw"],
    )
    .await
}

#[tokio::test]
async fn a_document_and_a_raw_write_commit_together_on_one_connection() {
    // One pooled connection: a second checkout inside the transaction could never succeed, so
    // this also proves the seam uses the transaction's own connection.
    let Some(env) = common::env_with(|c| c.pool_max_size = 1).await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let p = party("Acme", 0);
    let mut tx = begin(&env, &cat, options(|_| {})).await;
    // The tenant is the database, as ADR-34 says; a sibling crate stamps its rows with it.
    assert_eq!(tx.tenant(), env.db.name());
    // No second checkout: with a pool of one, this transaction is the only session.
    let connections: i64 = tx
        .raw_query(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE datname = current_database() AND backend_type = 'client backend'",
            &[],
            None,
        )
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(connections, 1, "the transaction holds the only connection");

    tx.insert(&p).await.unwrap();
    let first_pid: i32 = tx
        .raw_query("SELECT pg_backend_pid()", &[], None)
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(write_scratch(&mut tx, 1).await.unwrap(), 1);
    let second_pid: i32 = tx
        .raw_query("SELECT pg_backend_pid()", &[], None)
        .await
        .unwrap()[0]
        .get(0);
    assert_eq!(first_pid, second_pid, "every statement uses one connection");
    tx.commit().await.unwrap();

    assert!(party_exists(&env, &cat, p.id).await);
    assert!(scratch_has(&env, 1).await, "the raw write committed too");
}

#[tokio::test]
async fn a_document_and_a_failing_raw_write_roll_back_together() {
    let Some(env) = common::env_with(|c| c.pool_max_size = 1).await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let p = party("Acme", 0);
    let mut tx = begin(&env, &cat, options(|_| {})).await;
    tx.insert(&p).await.unwrap();
    // The row already exists, so the second write fails after the document insert.
    write_scratch(&mut tx, 7).await.unwrap();
    let err = write_scratch(&mut tx, 7).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::UniqueViolation);
    // A recoverable failure aborts the transaction: commit sends no COMMIT and reports the kind.
    assert_eq!(
        tx.commit().await.unwrap_err().kind,
        ErrorKind::UniqueViolation
    );

    assert!(
        !party_exists(&env, &cat, p.id).await,
        "the document is gone"
    );
    assert!(!scratch_has(&env, 7).await, "the raw write is gone");
}

#[tokio::test]
async fn a_read_only_transaction_refuses_a_write_without_sending_sql() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|o| *o = o.clone().read_only())).await;
    // Reading through the seam is allowed, and a statement declared as a read is allowed too.
    tx.raw_query("SELECT count(*) FROM scratch", &[], None)
        .await
        .unwrap();
    let sent = tx.statements_sent();
    let err = write_scratch(&mut tx, 1).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::ReadOnlyTransaction);
    assert_eq!(tx.statements_sent(), sent, "the refused write sent no SQL");
    // The refusal poisons the transaction, exactly as the area modules do after this kind.
    assert_eq!(
        tx.raw_query("SELECT 1", &[], None).await.unwrap_err().kind,
        ErrorKind::ReadOnlyTransaction
    );
    assert_eq!(tx.statements_sent(), sent);
}

#[tokio::test]
async fn raw_writes_count_against_max_writes_and_a_savepoint_gives_them_back() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|o| o.max_writes = 3)).await;
    // One document write plus one raw write fill two of the three.
    let p = party("Acme", 0);
    tx.insert(&p).await.unwrap();
    write_scratch(&mut tx, 1).await.unwrap();

    // A savepoint, a write that fails on the server, and a rollback: the failed write is uncounted.
    let sp = tx.savepoint().await.unwrap();
    let err = write_scratch(&mut tx, 1).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::UniqueViolation);
    tx.rollback_to(sp).await.unwrap();

    // Three writes are still allowed (1 document + 1 raw), so the count came back.
    tx.raw_execute_with(
        PgTransaction::write(1),
        "UPDATE scratch SET note = $1 WHERE id = $2",
        &[&"changed", &1],
    )
    .await
    .unwrap();

    // The next write is one over the limit, refused before any SQL is sent.
    let sent = tx.statements_sent();
    let err = write_scratch(&mut tx, 2).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::TransactionTooLarge);
    assert_eq!(tx.statements_sent(), sent, "the refused write sent no SQL");
    // Over the limit poisons the transaction for good.
    assert_eq!(
        tx.raw_query("SELECT 1", &[], None).await.unwrap_err().kind,
        ErrorKind::TransactionTooLarge
    );
    assert_eq!(tx.statements_sent(), sent);
    tx.rollback().await.unwrap();

    assert!(!party_exists(&env, &cat, p.id).await);
    assert!(!scratch_has(&env, 1).await);
}

#[tokio::test]
async fn a_read_declared_through_the_seam_counts_nothing() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|o| o.max_writes = 1)).await;
    for _ in 0..3 {
        tx.raw_execute_with(PgTransaction::READ, "SELECT count(*) FROM scratch", &[])
            .await
            .unwrap();
    }
    write_scratch(&mut tx, 1).await.unwrap();
    assert_eq!(
        write_scratch(&mut tx, 2).await.unwrap_err().kind,
        ErrorKind::TransactionTooLarge
    );
}

#[tokio::test]
async fn raw_query_opt_returns_none_one_row_and_refuses_many() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|_| {})).await;
    write_scratch(&mut tx, 1).await.unwrap();
    write_scratch(&mut tx, 2).await.unwrap();
    assert!(
        tx.raw_query_opt("SELECT note FROM scratch WHERE id = $1", &[&1], None)
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        tx.raw_query_opt("SELECT note FROM scratch WHERE id = $1", &[&9], None)
            .await
            .unwrap()
            .is_none()
    );
    let err = tx
        .raw_query_opt("SELECT note FROM scratch", &[], None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::InvalidQuery);
    // Nothing failed on the server, so the transaction is still usable.
    tx.raw_query("SELECT 1", &[], None).await.unwrap();
    tx.commit().await.unwrap();
    assert!(scratch_has(&env, 1).await);
    assert!(scratch_has(&env, 2).await);
}

#[tokio::test]
async fn an_unrecoverable_error_poisons_the_seam_and_a_savepoint_cannot_revive_it() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|_| {})).await;
    let sp = tx.savepoint().await.unwrap();
    let err = tx
        .raw_query("SELECT * FROM no_such_table", &[], None)
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Internal);
    let sent = tx.statements_sent();
    // Even rolling back to the savepoint cannot revive a poisoned transaction.
    assert_eq!(
        tx.rollback_to(sp).await.unwrap_err().kind,
        ErrorKind::Internal
    );
    assert_eq!(tx.statements_sent(), sent, "the rollback sent no SQL");
    assert_eq!(
        tx.raw_query("SELECT 1", &[], None).await.unwrap_err().kind,
        ErrorKind::Internal
    );
    assert_eq!(
        write_scratch(&mut tx, 1).await.unwrap_err().kind,
        ErrorKind::Internal
    );
    assert_eq!(tx.statements_sent(), sent, "no SQL after poisoning");
    assert_eq!(tx.commit().await.unwrap_err().kind, ErrorKind::Internal);
}

#[tokio::test]
async fn a_recoverable_error_inside_a_savepoint_leaves_the_seam_usable() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|_| {})).await;
    write_scratch(&mut tx, 1).await.unwrap();
    let sp = tx.savepoint().await.unwrap();
    let err = write_scratch(&mut tx, 1).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::UniqueViolation);
    assert_eq!(
        err.constraint.as_ref().map(|c| c.as_str()),
        Some("scratch_pkey")
    );
    // Until the rollback the aborted transaction refuses everything, sending nothing.
    let sent = tx.statements_sent();
    assert_eq!(
        tx.raw_query("SELECT 1", &[], None).await.unwrap_err().kind,
        ErrorKind::UniqueViolation
    );
    assert_eq!(tx.statements_sent(), sent);
    tx.rollback_to(sp).await.unwrap();
    write_scratch(&mut tx, 2).await.unwrap();
    tx.commit().await.unwrap();

    assert!(scratch_has(&env, 1).await);
    assert!(scratch_has(&env, 2).await);
}

#[tokio::test]
async fn values_are_bound_not_interpolated() {
    let Some(env) = common::env().await else {
        return;
    };
    let cat = catalog();
    env.backend.prepare(&cat).await.unwrap();
    env.db
        .raw_client()
        .await
        .batch_execute(CREATE_SCRATCH)
        .await
        .unwrap();

    let mut tx = begin(&env, &cat, options(|_| {})).await;
    // A value that would end the statement if it were interpolated is stored as data.
    let note = "'); DROP TABLE scratch; --";
    tx.raw_execute(
        "INSERT INTO scratch (id, note) VALUES ($1, $2)",
        &[&1, &note],
    )
    .await
    .unwrap();
    let rows = tx
        .raw_query("SELECT note FROM scratch WHERE id = $1", &[&1], None)
        .await
        .unwrap();
    assert_eq!(rows[0].get::<_, String>(0), note);
    tx.commit().await.unwrap();
    assert!(scratch_has(&env, 1).await);
}
