//! The backend-independent store contract suites, run against PostgreSQL. Skips cleanly when no
//! server is configured (`SPOT_TEST_PG_URL`); `SPOT_REQUIRE_PG=1` makes a missing server a
//! failure.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_coordinator::FencingToken;
use spot_core::SiteId;
use spot_mail::MailboxCursor;
use spot_mail_intake::contract::{cursor_store_contract, seen_store_contract};
use spot_mail_intake::{CursorStore, SeenStore, StoreError};
use spot_mail_intake_pg::{PgCursorStore, PgSeenStore, prepare};
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;
use tokio_postgres::NoTls;

fn pool(db: &TestDb, size: usize) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn cursor(validity: u32, last: u32) -> MailboxCursor {
    MailboxCursor {
        uid_validity: validity,
        last_uid: last,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_cursor_contract_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4);
    prepare(&pool).await.unwrap();
    let store = PgCursorStore::new(pool, site("head.office"));
    cursor_store_contract(&store, "contract").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_seen_contract_passes_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4);
    prepare(&pool).await.unwrap();
    let store = PgSeenStore::new(pool, site("head.office"));
    seen_store_contract(&store, "contract").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_concurrent_safe() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 8);
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move { prepare(&pool).await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    prepare(&pool).await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sites_never_share_a_cursor_or_a_seen_set() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4);
    prepare(&pool).await.unwrap();
    let alpha = PgCursorStore::new(pool.clone(), site("alpha.test"));
    let beta = PgCursorStore::new(pool.clone(), site("beta.test"));
    alpha
        .store("support", cursor(1, 5), FencingToken(9))
        .await
        .unwrap();
    assert_eq!(beta.load("support").await, Ok(None));
    // A high fence on one site does not make a low one stale on the other.
    assert_eq!(
        beta.store("support", cursor(1, 1), FencingToken(1)).await,
        Ok(())
    );

    let digest = "ef".repeat(32);
    let now = OffsetDateTime::UNIX_EPOCH;
    let seen_a = PgSeenStore::new(pool.clone(), site("alpha.test"));
    let seen_b = PgSeenStore::new(pool, site("beta.test"));
    assert_eq!(seen_a.first_time("support", &digest, now).await, Ok(true));
    assert_eq!(seen_b.contains("support", &digest).await, Ok(false));
    assert_eq!(seen_b.first_time("support", &digest, now).await, Ok(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_fences_leave_the_newest_writer_in_place() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 8);
    prepare(&pool).await.unwrap();
    let store = PgCursorStore::new(pool, site("head.office"));
    let old = store.clone();
    let new = store.clone();
    let a = tokio::spawn(async move { old.store("race", cursor(1, 3), FencingToken(4)).await });
    let b = tokio::spawn(async move { new.store("race", cursor(1, 9), FencingToken(5)).await });
    let (ra, rb) = (a.await.unwrap(), b.await.unwrap());
    assert_eq!(rb, Ok(()), "the newer fence always wins");
    assert!(ra == Ok(()) || ra == Err(StoreError::StaleFence));
    assert_eq!(store.load("race").await, Ok(Some(cursor(1, 9))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_removes_in_batches_and_only_this_site() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let pool = pool(&db, 4);
    prepare(&pool).await.unwrap();
    let mine = PgSeenStore::new(pool.clone(), site("alpha.test"));
    let other = PgSeenStore::new(pool, site("beta.test"));
    let old = OffsetDateTime::from_unix_timestamp(1_000).unwrap();
    for i in 0..2_050_u32 {
        mine.first_time("support", &format!("{i:064x}"), old)
            .await
            .unwrap();
    }
    other
        .first_time("support", &"a".repeat(64), old)
        .await
        .unwrap();
    let cutoff = OffsetDateTime::from_unix_timestamp(2_000).unwrap();
    assert_eq!(mine.purge_seen_older_than(cutoff).await, Ok(2_050));
    assert_eq!(mine.purge_seen_older_than(cutoff).await, Ok(0));
    assert_eq!(other.contains("support", &"a".repeat(64)).await, Ok(true));
}
