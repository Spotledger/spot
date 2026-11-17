//! The durable bounce sink: the port's idempotency rules (the in-memory sink is the reference, no
//! shared contract suite exists), first write wins, 16 concurrent writers of one key make one row,
//! sites do not see each other, a restart keeps the record, and an outage is `Unavailable` with
//! nothing in it.
//!
//! Database tests skip cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use deadpool_postgres::{PoolConfig, Runtime};
use spot_core::SiteId;
use spot_jobs::JobId;
use spot_mail_queue::{BounceClass, BounceRecord, BounceSink, Unavailable};
use spot_mail_queue_pg::PgBounceSink;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;
use tokio_postgres::NoTls;

fn record(site: &str, job: u64, recipient: &str, class: BounceClass, at: i64) -> BounceRecord {
    BounceRecord {
        site: common::site(site),
        job_id: JobId(job),
        message_id: format!("m{job}@{site}"),
        recipient: recipient.to_owned(),
        class,
        code: "mail_queue.recipient_rejected",
        reply_code: Some(550),
        attempts: None,
        at: OffsetDateTime::from_unix_timestamp(at).unwrap(),
    }
}

async fn sink(db: &TestDb, app: &str) -> PgBounceSink {
    let sink = PgBounceSink::new(common::pool(db, 4, app));
    sink.prepare().await.unwrap();
    sink
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_record_wins_and_a_repeat_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let sink = sink(&db, "bounce_first").await;
    let site = common::site("head.office");
    let first = record(
        "head.office",
        7,
        "gone@customer.test",
        BounceClass::Rejected,
        1_800_000_000,
    );
    sink.record(first.clone()).await.unwrap();
    let mut again = first.clone();
    again.at = OffsetDateTime::from_unix_timestamp(1_900_000_000).unwrap();
    again.attempts = Some(9);
    again.code = "mail_queue.something_else";
    sink.record(again).await.unwrap();
    let rows = sink.records_for(&site, 7).await.unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].code, "mail_queue.recipient_rejected");
    assert_eq!(rows[0].attempts, None);
    assert_eq!(rows[0].recorded_at, 1_800_000_000 * 1_000_000);
    assert_eq!(rows[0].reply_code, Some(550));
    // A different class or recipient of the same job is a different bounce.
    sink.record(record(
        "head.office",
        7,
        "gone@customer.test",
        BounceClass::Deferred,
        1_800_000_001,
    ))
    .await
    .unwrap();
    sink.record(record(
        "head.office",
        7,
        "other@customer.test",
        BounceClass::Rejected,
        1_800_000_001,
    ))
    .await
    .unwrap();
    assert_eq!(sink.records_for(&site, 7).await.unwrap().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sixteen_concurrent_writers_of_one_key_make_one_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let sink = Arc::new(sink(&db, "bounce_race").await);
    let mut tasks = Vec::new();
    for n in 0..16_i64 {
        let sink = Arc::clone(&sink);
        tasks.push(tokio::spawn(async move {
            sink.record(record(
                "head.office",
                1,
                "gone@customer.test",
                BounceClass::Rejected,
                1_800_000_000 + n,
            ))
            .await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let rows = sink
        .records_for(&common::site("head.office"), 1)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1, "exactly one row, every call Ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sites_are_isolated() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let sink = sink(&db, "bounce_sites").await;
    sink.record(record(
        "alpha.test",
        3,
        "a@customer.test",
        BounceClass::Rejected,
        1_800_000_000,
    ))
    .await
    .unwrap();
    sink.record(record(
        "beta.test",
        3,
        "a@customer.test",
        BounceClass::Rejected,
        1_800_000_000,
    ))
    .await
    .unwrap();
    for name in ["alpha.test", "beta.test"] {
        let rows = sink.records_for(&common::site(name), 3).await.unwrap();
        assert_eq!(rows.len(), 1, "{name} sees its own row only");
    }
    assert!(
        sink.records_for(&common::site("gamma.test"), 3)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_record_and_retries_exhausted_keeps_its_attempts() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let before = sink(&db, "bounce_before").await;
    let mut exhausted = record(
        "head.office",
        5,
        "slow@customer.test",
        BounceClass::RetriesExhausted,
        1_800_000_000,
    );
    exhausted.reply_code = None;
    exhausted.attempts = Some(6);
    exhausted.code = "mail_queue.timeout";
    before.record(exhausted).await.unwrap();
    // A new pool is all a restart leaves behind; prepare again is harmless.
    let after = sink(&db, "bounce_after").await;
    let rows = after
        .records_for(&common::site("head.office"), 5)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].class, BounceClass::RetriesExhausted);
    assert_eq!(rows[0].attempts, Some(6));
    assert_eq!(rows[0].reply_code, None);
}

#[tokio::test]
async fn an_outage_is_unavailable_with_nothing_in_it() {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".to_owned());
    cfg.port = Some(1);
    cfg.dbname = Some("spot_dead_database".to_owned());
    cfg.user = Some("spot_dead_user".to_owned());
    cfg.pool = Some(PoolConfig::new(1));
    let dead = PgBounceSink::new(cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap());
    let err: Unavailable = dead
        .record(record(
            "head.office",
            1,
            "gone@customer.test",
            BounceClass::Rejected,
            1_800_000_000,
        ))
        .await
        .unwrap_err();
    let text = err.to_string();
    assert_eq!(text, "backing service unavailable");
    for forbidden in ["127.0.0.1", "spot_dead", "gone@customer.test"] {
        assert!(!text.contains(forbidden) && !format!("{dead:?}").contains(forbidden));
    }
    let site: SiteId = common::site("head.office");
    assert!(dead.records_for(&site, 1).await.is_err());
    let prepared = dead.prepare().await.unwrap_err();
    assert_eq!(prepared.code, spot_mail_queue_pg::MIGRATION_FAILED);
}
