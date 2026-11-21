//! Recovery and upkeep of the feed on PostgreSQL: generations, retention, the insert-only
//! outbox, the envelopes and `prepare` being repeatable (ADR-053 "Outbox and change feed").
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::time::Duration;

use documents_support::*;
use spot_events_contract::SourceId;
use spot_postgres::{cleanup, envelopes, retention, set_retention, start_new_generation};
use spot_storage_contract::{
    ChangeFeed, DocumentData, DocumentStore, ErrorKind, FeedPosition, Isolation, NodeRef, Outbox,
    OutboxEvent, RecordId, StorageBackend, Transaction, TxOptions,
};

fn event(doc: &DocumentData, topic: &str, payload: &str) -> OutboxEvent {
    OutboxEvent {
        id: RecordId::new_v7(),
        topic: n(topic),
        subject: NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        },
        subject_version: None,
        schema_version: 1,
        payload: payload.into(),
    }
}

async fn commit_event(
    env: &Env,
    cat: &std::sync::Arc<spot_storage_contract::Catalog>,
    topic: &str,
) {
    let p = party(&format!("Acme {topic}"), 0);
    let mut tx = begin(env, cat).await;
    tx.insert(&p).await.unwrap();
    tx.enqueue(&event(&p, topic, r#"{"ok":true}"#))
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn a_new_generation_makes_old_bookmarks_fail_with_resync_required() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    commit_event(&env, &cat, "first").await;
    let old_out = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap()[0]
        .position;
    let old_chg = env
        .backend
        .read_changes(FeedPosition::START, 10)
        .await
        .unwrap()[0]
        .position;
    let generation = start_new_generation(&env.backend).await.unwrap();
    assert_eq!(generation, old_out.generation() + 1);
    for r in [
        env.backend.read_outbox(old_out, 10).await.map(|_| ()),
        env.backend.read_changes(old_chg, 10).await.map(|_| ()),
    ] {
        assert_eq!(r.unwrap_err().kind, ErrorKind::ResyncRequired);
    }
    // START is always valid: it reads the new generation from its beginning.
    assert!(
        env.backend
            .read_outbox(FeedPosition::START, 10)
            .await
            .unwrap()
            .is_empty()
    );
    commit_event(&env, &cat, "second").await;
    let fresh = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].position, FeedPosition::new(generation, 1));
    assert_eq!(fresh[0].event.topic, n("second"));
    // A bookmark taken in the new generation continues to work.
    assert!(
        env.backend
            .read_outbox(fresh[0].position, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn cleanup_prunes_old_entries_and_stale_bookmarks_resync() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    assert_eq!(
        retention(&env.backend).await.unwrap(),
        Duration::from_secs(7 * 24 * 3600)
    );
    assert_eq!(
        set_retention(&env.backend, Duration::ZERO)
            .await
            .unwrap_err()
            .kind,
        ErrorKind::InvalidValue
    );
    set_retention(&env.backend, Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(
        retention(&env.backend).await.unwrap(),
        Duration::from_secs(1)
    );
    commit_event(&env, &cat, "old_one").await;
    commit_event(&env, &cat, "old_two").await;
    let before = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(before.len(), 2);
    env.backend
        .read_changes(FeedPosition::START, 10)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    commit_event(&env, &cat, "young").await;
    env.backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    env.backend
        .read_changes(FeedPosition::START, 10)
        .await
        .unwrap();
    let removed = cleanup(&env.backend).await.unwrap();
    assert_eq!(removed.outbox_events, 2);
    assert_eq!(removed.changes, 2);
    // What remains reads in full; START is always valid.
    let rest = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(rest.len(), 1);
    assert_eq!(rest[0].event.topic, n("young"));
    // A bookmark before the pruned range cannot be continued from; the floor itself can.
    let stale = FeedPosition::new(before[0].position.generation(), 1);
    assert_eq!(
        env.backend.read_outbox(stale, 10).await.unwrap_err().kind,
        ErrorKind::ResyncRequired
    );
    assert!(
        env.backend
            .read_outbox(before[1].position, 10)
            .await
            .is_ok()
    );
    // The head is never pruned, so a reader at the head still reads on.
    assert_eq!(cleanup(&env.backend).await.unwrap().outbox_events, 0);
}

#[tokio::test]
async fn the_outbox_is_insert_only_for_business_code() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    commit_event(&env, &cat, "kept").await;
    let client = env.db.raw_client().await;
    for statement in [
        "UPDATE spot_outbox SET topic = 'changed'",
        "DELETE FROM spot_outbox",
        "TRUNCATE spot_outbox",
    ] {
        let e = client.batch_execute(statement).await.unwrap_err();
        let message = e.as_db_error().map(|d| d.message().to_owned());
        assert!(
            message.is_some_and(|m| m.contains("insert-only")),
            "{statement} was not refused: {e:?}"
        );
    }
    let out = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(out[0].event.topic, n("kept"));
}

#[tokio::test]
async fn prepare_twice_leaves_one_capture_trigger_per_document_table() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    env.backend.prepare(&cat).await.unwrap();
    let client = env.db.raw_client().await;
    let rows = client
        .query(
            "SELECT c.relname FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             WHERE t.tgname = 'spot_capture' ORDER BY 1",
            &[],
        )
        .await
        .unwrap();
    let tables: Vec<String> = rows.iter().map(|r| r.get(0)).collect();
    assert_eq!(tables, vec!["invoice".to_owned(), "party".to_owned()]);
    let generation: String = client
        .query_one(
            "SELECT value FROM spot_meta WHERE key = 'feed_generation'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(generation, "1", "a second prepare keeps the generation");
}

#[tokio::test]
async fn enqueue_counts_against_max_writes_and_duplicates_are_refused() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let mut options = TxOptions::new(n("small"), Isolation::ReadCommitted);
    options.max_writes = 2;
    let mut tx = env.backend.begin(cat.clone(), options).await.unwrap();
    let first = event(&p, "a", "{}");
    tx.enqueue(&first).await.unwrap();
    assert_eq!(
        tx.enqueue(&first).await.unwrap_err().kind,
        ErrorKind::AlreadyExists
    );
    // The refused duplicate left the transaction usable and counted as one write at most.
    tx.rollback().await.unwrap();
    let mut options = TxOptions::new(n("small"), Isolation::ReadCommitted);
    options.max_writes = 1;
    let mut tx = env.backend.begin(cat, options).await.unwrap();
    tx.enqueue(&event(&p, "a", "{}")).await.unwrap();
    assert_eq!(
        tx.enqueue(&event(&p, "b", "{}")).await.unwrap_err().kind,
        ErrorKind::TransactionTooLarge
    );
}

#[tokio::test]
async fn capture_and_publication_work_under_serializable_isolation() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let options = TxOptions::new(n("serial"), Isolation::Serializable);
    let mut tx = env.backend.begin(cat.clone(), options).await.unwrap();
    tx.insert(&p).await.unwrap();
    tx.enqueue(&event(&p, "a", "{}")).await.unwrap();
    tx.enqueue(&event(&p, "b", "{}")).await.unwrap();
    tx.commit().await.unwrap();
    let out = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].transaction, out[1].transaction);
    assert_eq!((out[0].ordinal, out[1].ordinal), (0, 1));
}

#[tokio::test]
async fn envelopes_carry_the_position_and_reject_a_bad_payload() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let good = format!(
        r#"{{"meta":{{"occurred_at":"2026-10-08T10:00:00Z","actor":{{"kind":"system","component":"scheduler"}},"correlation_id":"{}"}},"data":{{"x":1}}}}"#,
        RecordId::new_v7()
    );
    let mut tx = begin(&env, &cat).await;
    tx.insert(&p).await.unwrap();
    tx.enqueue(&event(&p, "party_created", &good))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    let source = SourceId::new("erp.example.com").unwrap();
    let out = envelopes(&source, &records).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].feed_position(), records[0].position);
    assert_eq!(out[0].event_id, records[0].event.id);
    assert_eq!(out[0].transaction, records[0].transaction);
    assert_eq!(out[0].data["x"], 1);

    let mut tx = begin(&env, &cat).await;
    tx.enqueue(&event(&p, "party_created", r#"{"ok":true}"#))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let records = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert!(envelopes(&source, &records).is_err());
}
