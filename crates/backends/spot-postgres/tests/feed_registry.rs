//! The outbox refuses an unregistered platform topic and a retired one before any SQL, and
//! warns once per process for an unregistered app topic (WP-JRN-01.T3). Real PostgreSQL.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod documents_support;

use std::sync::{Arc, Mutex};

use documents_support::*;
use spot_storage_contract::{
    ChangeFeed, ErrorKind, FeedPosition, NodeRef, Outbox, OutboxEvent, RecordId, Transaction,
};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::Registry;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};

const CODE: &str = "feed_registry.unregistered_topic";

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

/// Collects the `topic` field of every warn event.
#[derive(Clone, Default)]
struct Warns(Arc<Mutex<Vec<String>>>);

struct TopicOf(String);
impl Visit for TopicOf {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "topic" {
            self.0 = value.to_owned();
        }
    }
    fn record_debug(&mut self, _: &Field, _: &dyn std::fmt::Debug) {}
}

impl<S: Subscriber> Layer<S> for Warns {
    fn on_event(&self, e: &Event<'_>, _: Context<'_, S>) {
        if *e.metadata().level() == tracing::Level::WARN {
            let mut v = TopicOf(String::new());
            e.record(&mut v);
            self.0.lock().unwrap().push(v.0);
        }
    }
}

async fn outbox_len(env: &Env) -> usize {
    env.backend
        .read_outbox(FeedPosition::START, 100)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn registered_topics_are_accepted_and_unregistered_or_retired_ones_refused() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let p = party("Acme", 0);
    let mut tx = begin(&env, &cat).await;
    tx.enqueue(&event(&p, "doc_state")).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(outbox_len(&env).await, 1);

    for topic in ["doc_nonsense", "sys_nonsense", "doc_edge"] {
        let mut tx = begin(&env, &cat).await;
        let err = tx.enqueue(&event(&p, topic)).await.unwrap_err();
        assert_eq!(err.kind, ErrorKind::InvalidValue, "{topic}");
        assert!(err.explanation.contains(CODE), "{topic}");
        assert!(err.explanation.contains(topic), "{topic}");
        // `InvalidValue` is a recoverable kind (`StorageTx::fail_storage`): the transaction is
        // not poisoned, but the refused event was never written, so committing it adds nothing.
        tx.commit().await.unwrap();
        assert_eq!(outbox_len(&env).await, 1, "{topic}: nothing was written");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_app_topic_is_accepted_and_warned_about_once() {
    let Some((env, cat)) = prepared().await else {
        return;
    };
    let warns = Warns::default();
    let _guard = tracing::subscriber::set_default(Registry::default().with(warns.clone()));
    let p = party("Acme", 0);
    for _ in 0..3 {
        let mut tx = begin(&env, &cat).await;
        tx.enqueue(&event(&p, "registry_test_app_topic"))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    assert_eq!(outbox_len(&env).await, 3);
    let logged = warns.0.lock().unwrap().clone();
    assert_eq!(
        logged
            .iter()
            .filter(|t| *t == "registry_test_app_topic")
            .count(),
        1
    );
}
