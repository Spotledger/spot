//! The outbox barrier (WP-RTB-01.T3): `cleanup` never prunes an entry the journal shipper has
//! not made durable, whatever its age, and a database without a shipper prunes as before.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::time::Duration;

use documents_support::*;
use spot_postgres::{PgTransaction, cleanup, set_retention};
use spot_storage_contract::{
    ChangeFeed, DocumentStore, FeedPosition, Isolation, NodeRef, Outbox, OutboxEvent, RecordId,
    StorageBackend, Transaction, TxOptions,
};

/// The shipper's state table, as `spot-journal-ship` publishes it (the columns the barrier reads).
const STATE_DDL: &str = "CREATE TABLE IF NOT EXISTS spot_journal_ship_state (\
    source text PRIMARY KEY, generation bigint NOT NULL, seq bigint NOT NULL, \
    updated_at_millis bigint NOT NULL DEFAULT 0)";

fn event(doc: &spot_storage_contract::DocumentData, topic: &str) -> OutboxEvent {
    OutboxEvent {
        id: RecordId::new_v7(),
        topic: n(topic),
        subject: NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        },
        subject_version: None,
        schema_version: 1,
        payload: r#"{"ok":true}"#.into(),
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
    tx.enqueue(&event(&p, topic)).await.unwrap();
    tx.commit().await.unwrap();
}

async fn run_sql(env: &Env, cat: &std::sync::Arc<spot_storage_contract::Catalog>, sql: &str) {
    let mut options = TxOptions::new(n("ship_barrier_test"), Isolation::ReadCommitted);
    options.max_writes = 100;
    let mut tx = env.backend.begin(cat.clone(), options).await.unwrap();
    tx.raw_execute_with(PgTransaction::write(1), sql, &[])
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

async fn old_entries(env: &Env, cat: &std::sync::Arc<spot_storage_contract::Catalog>) {
    set_retention(&env.backend, Duration::from_secs(1))
        .await
        .unwrap();
    for topic in ["one", "two", "three"] {
        commit_event(env, cat, topic).await;
    }
    // Publish them, then let them age past the retention. A fourth, young entry stays the head.
    env.backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1_300)).await;
    commit_event(env, cat, "head").await;
    env.backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
}

#[tokio::test]
async fn without_a_shipper_cleanup_prunes_as_before() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    old_entries(&env, &cat).await;
    let done = cleanup(&env.backend).await.unwrap();
    assert_eq!(done.outbox_events, 3);
    assert_eq!(done.held_back_events, 0);
    assert_eq!(done.held_back_seconds, 0);
}

#[tokio::test]
async fn cleanup_keeps_what_the_shipper_has_not_made_durable() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    old_entries(&env, &cat).await;
    let published = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(published.len(), 4);
    let generation = published[0].position.generation();
    run_sql(&env, &cat, STATE_DDL).await;
    // The shipper is configured and has shipped the first entry only.
    run_sql(
        &env,
        &cat,
        &format!(
            "INSERT INTO spot_journal_ship_state (source, generation, seq) VALUES ('acme', {generation}, {})",
            published[0].position.seq()
        ),
    )
    .await;
    let done = cleanup(&env.backend).await.unwrap();
    assert_eq!(done.outbox_events, 1, "only the shipped entry is pruned");
    assert_eq!(
        done.held_back_events, 2,
        "the two older unshipped entries are held back"
    );
    assert!(done.held_back_seconds >= 1);
    let rest = env
        .backend
        .read_outbox(published[0].position, 10)
        .await
        .unwrap();
    assert_eq!(rest.len(), 3);
    // The shipper catches up: the next pass prunes what is old, and holds back nothing.
    run_sql(
        &env,
        &cat,
        &format!(
            "UPDATE spot_journal_ship_state SET seq = {} WHERE source = 'acme'",
            published[3].position.seq()
        ),
    )
    .await;
    let done = cleanup(&env.backend).await.unwrap();
    assert_eq!(done.outbox_events, 2);
    assert_eq!(done.held_back_events, 0);
}

#[tokio::test]
async fn a_shipper_row_of_an_older_generation_holds_everything_of_the_new_one() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    old_entries(&env, &cat).await;
    run_sql(&env, &cat, STATE_DDL).await;
    // The shipper's row is for generation 0, which is never the feed's: nothing of the current
    // generation is shipped, so nothing may be pruned.
    run_sql(
        &env,
        &cat,
        "INSERT INTO spot_journal_ship_state (source, generation, seq) VALUES ('acme', 0, 99)",
    )
    .await;
    let done = cleanup(&env.backend).await.unwrap();
    assert_eq!(done.outbox_events, 0);
    assert_eq!(done.held_back_events, 3);
}
