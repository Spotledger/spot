//! The contract suites against PostgreSQL, plus what only a shared database has: restart
//! persistence, two nodes, site isolation, retention and the migrator's acceptance of the
//! tables. Every test needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_coordinator::FencingToken;
use spot_mail::MailboxCursor;
use spot_mail_intake::contract::{cursor_store_contract, seen_store_contract};
use spot_mail_intake::{CursorStore, SeenStore, StoreError};
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};
use time::OffsetDateTime;

fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(seconds).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_contract_suites_pass_on_postgres() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (cursors, seen) = common::stores(&db, 4, "intake_contract", "acme.test").await;
    cursor_store_contract(&cursors, "contract").await;
    seen_store_contract(&seen, "contract").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_survives_a_restart_and_prepare_is_repeatable() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (cursors, seen) = common::stores(&db, 2, "intake_first", "acme.test").await;
    let cursor = MailboxCursor {
        uid_validity: u32::MAX,
        last_uid: 4_000_000_000,
    };
    cursors
        .store("support", cursor, FencingToken(3))
        .await
        .unwrap();
    assert!(seen.first_time("support", "aa", at(10)).await.unwrap());

    // A new pool, as after a restart or on another node; prepare runs again.
    let (cursors, seen) = common::stores(&db, 2, "intake_second", "acme.test").await;
    assert_eq!(cursors.load("support").await.unwrap(), Some(cursor));
    assert!(seen.contains("support", "aa").await.unwrap());
    assert!(!seen.first_time("support", "aa", at(20)).await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_the_stale_token_loses_and_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (node_a, _) = common::stores(&db, 2, "intake_a", "acme.test").await;
    let (node_b, _) = common::stores(&db, 2, "intake_b", "acme.test").await;
    let old = MailboxCursor {
        uid_validity: 1,
        last_uid: 5,
    };
    let new = MailboxCursor {
        uid_validity: 1,
        last_uid: 9,
    };
    node_a.store("support", old, FencingToken(1)).await.unwrap();
    node_b.store("support", new, FencingToken(2)).await.unwrap();
    assert_eq!(
        node_a.store("support", old, FencingToken(1)).await,
        Err(StoreError::StaleFence)
    );
    assert_eq!(node_a.load("support").await.unwrap(), Some(new));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_racing_first_records_have_one_winner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (_, seen_a) = common::stores(&db, 4, "intake_race_a", "acme.test").await;
    let (_, seen_b) = common::stores(&db, 4, "intake_race_b", "acme.test").await;
    let (a, b) = tokio::join!(
        seen_a.first_time("support", "ff", at(1)),
        seen_b.first_time("support", "ff", at(2)),
    );
    assert_eq!(
        [a.unwrap(), b.unwrap()].iter().filter(|won| **won).count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sites_do_not_share_cursors_or_seen_sets_and_purge_is_per_site() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let (cursors_one, seen_one) = common::stores(&db, 2, "intake_one", "one.test").await;
    let (cursors_two, seen_two) = common::stores(&db, 2, "intake_two", "two.test").await;
    let cursor = MailboxCursor {
        uid_validity: 1,
        last_uid: 7,
    };
    cursors_one
        .store("support", cursor, FencingToken(1))
        .await
        .unwrap();
    assert_eq!(cursors_two.load("support").await.unwrap(), None);
    assert!(seen_one.first_time("support", "aa", at(100)).await.unwrap());
    assert!(seen_two.first_time("support", "aa", at(100)).await.unwrap());

    assert_eq!(seen_one.purge_seen_older_than(at(200)).await.unwrap(), 1);
    assert!(!seen_one.contains("support", "aa").await.unwrap());
    assert!(seen_two.contains("support", "aa").await.unwrap());
    assert_eq!(seen_one.purge_seen_older_than(at(200)).await.unwrap(), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_migrator_accepts_the_tables() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let _ = common::stores(&db, 2, "intake_migrate", "acme.test").await;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    assert!(plan(&live, &wanted).refused.is_empty());
}
