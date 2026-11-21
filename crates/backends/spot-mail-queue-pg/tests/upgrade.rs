//! The sent log's table carries the `spot_` prefix `spot_pg_migrate` requires, a database that
//! still holds the old name is upgraded without losing a receipt, and `prepare` is safe to run
//! again and from several nodes at once.
//!
//! Every test here needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mail_queue::SentLog;
use spot_pg_migrate::{LiveSchema, plan};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation};

/// The table as the crate created it before the prefix: the same shape, the old names.
const OLD_DDL: &str = r"
CREATE TABLE mail_sent_log (
    site          text    NOT NULL,
    message_id    text    NOT NULL,
    recorded_at   bigint  NOT NULL
        DEFAULT (EXTRACT(EPOCH FROM now()) * 1000000)::bigint,
    accepted      text[]  NOT NULL,
    rejected      text[]  NOT NULL,
    rejected_code integer[] NOT NULL,
    PRIMARY KEY (site, message_id)
);
ALTER TABLE mail_sent_log ADD CONSTRAINT mail_sent_log_rejections_paired
    CHECK (cardinality(rejected) = cardinality(rejected_code)) NOT VALID;
CREATE INDEX mail_sent_log_recorded_idx ON mail_sent_log (site, recorded_at);
INSERT INTO mail_sent_log (site, message_id, accepted, rejected, rejected_code)
    VALUES ('head.office', 'old-1@head.office', '{buyer@customer.test}', '{}', '{}'),
           ('head.office', 'old-2@head.office', '{a@customer.test}',
            '{gone@customer.test}', '{550}');
";

async fn table_names(db: &TestDb) -> Vec<String> {
    let client = db.raw_client().await;
    client
        .query(
            "SELECT table_name::text FROM information_schema.tables \
             WHERE table_schema = current_schema() AND table_type = 'BASE TABLE' \
             ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

async fn object_names(db: &TestDb) -> Vec<String> {
    let client = db.raw_client().await;
    client
        .query(
            "SELECT relname::text FROM pg_class \
             WHERE relnamespace = to_regnamespace(current_schema())::oid \
               AND relkind IN ('r', 'i') \
             UNION SELECT conname::text FROM pg_constraint \
             WHERE connamespace = to_regnamespace(current_schema())::oid ORDER BY 1",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_table_the_crate_creates_is_spot_prefixed_and_passes_the_migrator() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let _log = common::log(&db, 2, "sent_log_prefix").await;

    let tables = table_names(&db).await;
    assert!(
        tables.iter().any(|t| t == "spot_mail_sent_log"),
        "{tables:?}"
    );
    for table in &tables {
        assert!(table.starts_with("spot_"), "{table} lacks the spot_ prefix");
    }
    for name in object_names(&db).await {
        assert!(name.starts_with("spot_"), "{name} lacks the spot_ prefix");
    }

    // The migrator refuses every table that is not `spot_` and not in the catalogue.
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    let plan = plan(&live, &wanted);
    assert!(
        plan.refused.is_empty(),
        "the migrator refuses the sent log's table: {:?}",
        plan.refused
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_named_table_is_renamed_with_every_row_kept() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();

    let log = common::log(&db, 2, "sent_log_upgrade").await;
    let site = common::site("head.office");
    assert!(log.is_sent(&site, "old-1@head.office").await.unwrap());
    let two = log
        .recorded(&site, "old-2@head.office")
        .await
        .unwrap()
        .expect("the receipt written before the upgrade is still readable");
    assert_eq!(two.rejected[0].1.code, 550);
    assert_eq!(common::rows(&db).await, 2, "no row is lost");

    // The old message is still not sent twice, and the old name is gone with all its objects.
    assert!(
        !log.record_sent(
            &site,
            "old-1@head.office",
            &common::sent("old-1@head.office")
        )
        .await
        .unwrap()
    );
    let objects = object_names(&db).await;
    for name in &objects {
        assert!(name.starts_with("spot_"), "{name} kept the old name");
    }
    assert_eq!(
        table_names(&db).await,
        vec!["spot_mail_sent_log".to_owned()]
    );
    assert!(
        objects
            .iter()
            .any(|n| n == "spot_mail_sent_log_recorded_idx")
    );
    assert!(
        objects
            .iter()
            .any(|n| n == "spot_mail_sent_log_rejections_paired")
    );
    assert_eq!(
        objects
            .iter()
            .filter(|n| n.contains("recorded_idx"))
            .count(),
        1
    );

    // The upgraded table passes the migrator.
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let live = LiveSchema::read(&backend).await.unwrap();
    let wanted = Catalog::new(1, Collation::new("en").unwrap(), vec![]).unwrap();
    assert!(plan(&live, &wanted).refused.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn preparing_again_changes_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let log = common::log(&db, 2, "sent_log_again").await;
    let before = object_names(&db).await;
    for _ in 0..3 {
        log.prepare().await.unwrap();
    }
    assert_eq!(object_names(&db).await, before);
    assert_eq!(common::rows(&db).await, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nodes_preparing_at_once_upgrade_once_and_lose_nothing() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    db.raw_client().await.batch_execute(OLD_DDL).await.unwrap();
    let mut tasks = Vec::new();
    for node in 0..6 {
        let log = spot_mail_queue_pg::PgSentLog::new(common::pool(
            &db,
            1,
            &format!("sent_log_node_{node}"),
        ));
        tasks.push(tokio::spawn(async move { log.prepare().await }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    assert_eq!(common::rows(&db).await, 2);
    assert_eq!(
        table_names(&db).await,
        vec!["spot_mail_sent_log".to_owned()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn both_tables_present_is_refused_and_neither_is_touched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let client = db.raw_client().await;
    client.batch_execute(OLD_DDL).await.unwrap();
    client
        .batch_execute(
            "CREATE TABLE spot_mail_sent_log (LIKE mail_sent_log INCLUDING ALL); \
             INSERT INTO spot_mail_sent_log (site, message_id, accepted, rejected, rejected_code) \
             VALUES ('head.office', 'new-1@head.office', '{}', '{}', '{}')",
        )
        .await
        .unwrap();

    let log = spot_mail_queue_pg::PgSentLog::new(common::pool(&db, 1, "sent_log_both"));
    let error = log
        .prepare()
        .await
        .expect_err("two tables must not be merged by guess");
    let text = error.to_string();
    assert!(
        text.contains("`mail_sent_log`") && text.contains("`spot_mail_sent_log`"),
        "{text}"
    );
    for forbidden in ["customer.test", "head.office"] {
        assert!(!text.contains(forbidden), "{text}");
    }
    let count = |table: &'static str| {
        let client = &client;
        async move {
            client
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await
                .unwrap()
                .get::<_, i64>(0)
        }
    };
    assert_eq!(count("mail_sent_log").await, 2);
    assert_eq!(count("spot_mail_sent_log").await, 1);
}
