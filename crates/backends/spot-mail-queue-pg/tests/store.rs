//! What the store promises beyond the contract: a restart keeps the receipt, two writers of one
//! message make one row, sites do not see each other, the receipt comes back as it went in, and
//! retention removes exactly what it is told to.
//!
//! Every test here needs a server and skips cleanly (printing `SKIPPED:`) without one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_mail_queue::SentLog;
use spot_postgres::testing::TestDb;
use time::OffsetDateTime;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_keeps_the_receipt() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let before = common::log(&db, 2, "sent_log_before_restart").await;
    let site = common::site("head.office");
    let receipt = common::receipt(
        "order-1@head.office",
        &["buyer@customer.test", "clerk@customer.test"],
        &[("gone@customer.test", 550)],
    );
    assert!(
        before
            .record_sent(&site, "order-1@head.office", &receipt)
            .await
            .unwrap()
    );

    // A new pool is all a restart leaves behind: nothing in this process is reused.
    let after = common::log(&db, 2, "sent_log_after_restart").await;
    assert!(
        after.is_sent(&site, "order-1@head.office").await.unwrap(),
        "a restart must not forget a receipt"
    );
    let stored = after
        .recorded(&site, "order-1@head.office")
        .await
        .unwrap()
        .expect("the receipt must still be there");
    assert_eq!(stored.message_id, receipt.message_id);
    assert_eq!(
        stored.accepted, receipt.accepted,
        "accepted recipients keep their order"
    );
    assert_eq!(stored.rejected.len(), 1);
    assert_eq!(stored.rejected[0].0, receipt.rejected[0].0);
    assert_eq!(
        stored.rejected[0].1.code, 550,
        "the reply code is stored; the server's reply text is not"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_writers_of_one_message_store_one_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 8, "sent_log_race").await;
    let site = common::site("head.office");
    let receipt = common::sent("raced@head.office");

    let mut writers = Vec::new();
    for _ in 0..6 {
        let log = log.clone();
        let site = site.clone();
        let receipt = receipt.clone();
        writers.push(tokio::spawn(async move {
            log.record_sent(&site, "raced@head.office", &receipt)
                .await
                .unwrap()
        }));
    }
    let mut winners = 0_usize;
    for writer in writers {
        if writer.await.unwrap() {
            winners += 1;
        }
    }
    assert_eq!(
        winners, 1,
        "exactly one writer may claim the receipt, six did otherwise"
    );
    assert_eq!(common::rows(&db).await, 1, "one key, one row");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sites_never_collide() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 4, "sent_log_sites").await;
    let head = common::site("head.office");
    let branch = common::site("branch.office");
    let message_id = "shared-id@mail.test";
    let head_receipt = common::receipt(message_id, &["a@customer.test"], &[]);
    let branch_receipt = common::receipt(message_id, &["b@customer.test"], &[]);

    assert!(
        log.record_sent(&head, message_id, &head_receipt)
            .await
            .unwrap()
    );
    assert!(
        log.record_sent(&branch, message_id, &branch_receipt)
            .await
            .unwrap(),
        "the other site's record of the same id is its own"
    );
    assert_eq!(common::rows(&db).await, 2);
    let head_stored = log.recorded(&head, message_id).await.unwrap().unwrap();
    let branch_stored = log.recorded(&branch, message_id).await.unwrap().unwrap();
    assert_eq!(head_stored.accepted, head_receipt.accepted);
    assert_eq!(branch_stored.accepted, branch_receipt.accepted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_record_leaves_the_first_receipt_alone() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 2, "sent_log_first_wins").await;
    let site = common::site("head.office");
    let message_id = "once@head.office";
    let first = common::receipt(message_id, &["first@customer.test"], &[]);
    let second = common::receipt(
        message_id,
        &["second@customer.test"],
        &[("x@customer.test", 451)],
    );

    assert!(log.record_sent(&site, message_id, &first).await.unwrap());
    assert!(
        !log.record_sent(&site, message_id, &second).await.unwrap(),
        "a repeat must answer false"
    );
    let stored = log.recorded(&site, message_id).await.unwrap().unwrap();
    assert_eq!(stored.accepted, first.accepted);
    assert!(stored.rejected.is_empty());
    assert_eq!(common::rows(&db).await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_removes_only_receipts_before_the_cutoff_and_of_one_site() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 4, "sent_log_purge").await;
    let head = common::site("head.office");
    let branch = common::site("branch.office");
    for message_id in ["a@head.office", "b@head.office"] {
        assert!(
            log.record_sent(&head, message_id, &common::sent(message_id))
                .await
                .unwrap()
        );
    }
    assert!(
        log.record_sent(&branch, "c@branch.office", &common::sent("c@branch.office"))
            .await
            .unwrap()
    );

    // A cutoff in the past leaves every receipt alone.
    let past = OffsetDateTime::now_utc() - time::Duration::days(1);
    assert_eq!(log.purge_older_than(&head, past).await.unwrap(), 0);

    // A cutoff after the rows removes this site's two and nothing else.
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    assert_eq!(log.purge_older_than(&head, later).await.unwrap(), 2);
    assert!(!log.is_sent(&head, "a@head.office").await.unwrap());
    assert!(log.is_sent(&branch, "c@branch.office").await.unwrap());
    assert_eq!(
        common::rows(&db).await,
        1,
        "the other site's receipt is untouched"
    );

    // Nothing is left for the same cutoff: a purge is idempotent.
    assert_eq!(log.purge_older_than(&head, later).await.unwrap(), 0);

    // And the point of the horizon: a purged id can be recorded (and so sent) again.
    assert!(
        log.record_sent(&head, "a@head.office", &common::sent("a@head.office"))
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn purge_walks_its_batches_to_the_end() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let log = common::log(&db, 4, "sent_log_purge_batches").await;
    let site = common::site("head.office");
    let later = OffsetDateTime::now_utc() + time::Duration::hours(1);
    // More rows than one statement may delete, so the purge has to loop; written directly because
    // the point of the test is the loop, not the insert.
    let client = db.raw_client().await;
    client
        .execute(
            "INSERT INTO spot_mail_sent_log (site, message_id, accepted, rejected, rejected_code) \
             SELECT $1, 'bulk-' || g, '{}'::text[], '{}'::text[], '{}'::integer[] \
             FROM generate_series(1, 2500) AS g",
            &[&site.as_str()],
        )
        .await
        .unwrap();
    drop(client);
    assert_eq!(common::rows(&db).await, 2_500);

    assert_eq!(log.purge_older_than(&site, later).await.unwrap(), 2_500);
    assert_eq!(common::rows(&db).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepare_is_idempotent_and_safe_from_two_nodes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let first = common::log(&db, 2, "sent_log_prepare_a").await;
    first.prepare().await.unwrap();
    let second = common::log(&db, 2, "sent_log_prepare_b").await;
    let (a, b) = tokio::join!(first.prepare(), second.prepare());
    a.unwrap();
    b.unwrap();

    // The table is usable after the repeated prepares, and its default stamps a row.
    let site = common::site("head.office");
    assert!(
        first
            .record_sent(&site, "after-prepare@head.office", &common::sent("x"))
            .await
            .unwrap()
    );
    let client = db.raw_client().await;
    let row = client
        .query_one(
            "SELECT recorded_at > 0 FROM spot_mail_sent_log WHERE message_id = $1",
            &[&"after-prepare@head.office"],
        )
        .await
        .unwrap();
    assert!(
        row.get::<_, bool>(0),
        "recorded_at must be stamped with the database clock"
    );
}
