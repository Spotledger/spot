//! Outbox position adoption on PostgreSQL (WP-RTB-01.T6): the head moves forward only, the next
//! published entry is head + 1, and a fork opens a new generation at 1.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use documents_support::*;
use spot_postgres::{adopt_position, open_generation};
use spot_storage_contract::{
    ChangeFeed, DocumentStore as _, ErrorKind, FeedPosition, NodeRef, Outbox, OutboxEvent,
    RecordId, Transaction,
};

async fn commit_event(
    env: &Env,
    cat: &std::sync::Arc<spot_storage_contract::Catalog>,
    topic: &str,
) {
    let p = party(&format!("Acme {topic}"), 0);
    let mut tx = begin(env, cat).await;
    tx.insert(&p).await.unwrap();
    tx.enqueue(&OutboxEvent {
        id: RecordId::new_v7(),
        topic: n(topic),
        subject: NodeRef {
            entity: p.entity.clone(),
            id: p.id,
        },
        subject_version: None,
        schema_version: 1,
        payload: r#"{"ok":true}"#.into(),
    })
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn adopting_an_empty_feed_moves_the_head_and_the_next_entry_is_head_plus_one() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    adopt_position(&env.backend, 1, 41).await.unwrap();
    commit_event(&env, &cat, "after").await;
    let entries = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].position, FeedPosition::new(1, 42));
    // A bookmark at the adopted head is valid and sees exactly the later entry.
    let after = env
        .backend
        .read_outbox(FeedPosition::new(1, 41), 10)
        .await
        .unwrap();
    assert_eq!(after.len(), 1);
}

#[tokio::test]
async fn a_head_past_the_target_is_refused_and_a_feed_never_moves_backwards() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    adopt_position(&env.backend, 1, 10).await.unwrap();
    commit_event(&env, &cat, "a").await;
    commit_event(&env, &cat, "b").await;
    // Publish them (a read publishes), then the head is at 12.
    let entries = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(entries.last().unwrap().position, FeedPosition::new(1, 12));
    let error = adopt_position(&env.backend, 1, 11).await.unwrap_err();
    assert_eq!(error.kind, ErrorKind::InvalidValue);
    assert!(
        error.to_string().contains("feed_adopt.would_regress"),
        "{error}"
    );
    // Adopting the same head again, or a higher one, is fine.
    adopt_position(&env.backend, 1, 12).await.unwrap();
    adopt_position(&env.backend, 1, 20).await.unwrap();
    // An older generation is a regression too.
    assert!(adopt_position(&env.backend, 0, 100).await.is_err());
}

#[tokio::test]
async fn a_fork_opens_a_new_generation_that_starts_at_one() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    adopt_position(&env.backend, 1, 7).await.unwrap();
    commit_event(&env, &cat, "old").await;
    let old = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap()[0]
        .position;
    assert_eq!(old, FeedPosition::new(1, 8));
    open_generation(&env.backend, 2).await.unwrap();
    // The bookmark of the abandoned generation needs a resync.
    assert_eq!(
        env.backend.read_outbox(old, 10).await.unwrap_err().kind,
        ErrorKind::ResyncRequired
    );
    commit_event(&env, &cat, "new").await;
    let fresh = env
        .backend
        .read_outbox(FeedPosition::START, 10)
        .await
        .unwrap();
    assert_eq!(fresh[0].position, FeedPosition::new(2, 1));
    // The generation has to move forward.
    assert!(open_generation(&env.backend, 2).await.is_err());
    assert!(open_generation(&env.backend, 1).await.is_err());
}
