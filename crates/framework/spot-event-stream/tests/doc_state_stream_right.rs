//! Reading the `doc_state` topic needs the explicit `doc_` stream right, proved at the stream
//! (registration and delivery), not only in the engine's topic classification (WP-JRN-02.T5,
//! ADR-061 section 7).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_event_stream::error::{CODE_CONSUMER_NOT_FOUND, CODE_SPEC_INVALID};
use spot_event_stream::{
    AllVisible, ConsumerSpec, DenyStreamRights, PayloadMode, Start, StreamClass, StreamRights,
    TopicSelector,
};
use spot_permissions::Principal;
use support::{Fixture, fixture_with, record, rid};

/// A rights source that grants exactly the named reserved classes.
struct Grants(&'static [&'static str]);

impl StreamRights for Grants {
    fn may_stream_class(&self, _owner: &str, class: &str) -> bool {
        self.0.contains(&class)
    }
}

fn spec(name: &str, topic: &str) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: "owner".to_owned(),
        topics: vec![TopicSelector::parse(topic).unwrap()],
        filter: None,
        partitions: 1,
        classes: Some(BTreeSet::from([StreamClass::State])),
        entities: None,
        payload_mode: PayloadMode::IdsOnly,
        start: Start::FromBeginning,
    }
}

async fn seeded(rights: Arc<dyn StreamRights>) -> Fixture {
    let f = fixture_with(Arc::new(AllVisible), rights).await;
    f.feed.push(record(
        0,
        1,
        "doc_state",
        "task",
        rid(1),
        1,
        serde_json::json!({"entity_schema": 1, "op": "insert", "image": {"status": "Open"}}),
    ));
    f
}

/// Nothing was registered, so nothing can be pulled: the consumer does not exist.
async fn assert_no_consumer(f: &Fixture, name: &str) {
    let error = f.engine.acquire(name).await.unwrap_err();
    assert_eq!(error.code(), CODE_CONSUMER_NOT_FOUND);
    assert!(
        f.engine
            .list(&Principal::system())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn without_the_doc_right_the_doc_state_consumer_is_refused_and_nothing_is_delivered() {
    let f = seeded(Arc::new(DenyStreamRights)).await;
    let error = f
        .engine
        .register(spec("journal", "doc_state"), &Principal::system())
        .await
        .unwrap_err();
    assert_eq!(error.code(), CODE_SPEC_INVALID);
    assert_no_consumer(&f, "journal").await;
}

#[tokio::test]
async fn with_the_doc_right_the_consumer_reads_the_doc_state_event() {
    let f = seeded(Arc::new(Grants(&["doc_"]))).await;
    f.engine
        .register(spec("journal", "doc_state"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("journal").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].topic.as_str(), "doc_state");
}

#[tokio::test]
async fn a_grant_of_another_class_does_not_allow_doc_state() {
    for granted in [
        &["sys_"][..],
        &["identity_"][..],
        &["sys_", "identity_"][..],
    ] {
        let granted: &'static [&'static str] = Box::leak(granted.to_vec().into_boxed_slice());
        let f = seeded(Arc::new(Grants(granted))).await;
        let error = f
            .engine
            .register(spec("journal", "doc_state"), &Principal::system())
            .await
            .unwrap_err();
        assert_eq!(error.code(), CODE_SPEC_INVALID, "{granted:?}");
        assert_no_consumer(&f, "journal").await;
    }
}

#[tokio::test]
async fn a_prefix_that_would_include_doc_state_is_refused_without_the_right() {
    for selector in ["doc_.*", "doc.*", "d.*", "doc_state.*"] {
        for rights in [
            Arc::new(DenyStreamRights) as Arc<dyn StreamRights>,
            Arc::new(Grants(&["sys_"])),
        ] {
            let f = seeded(rights).await;
            let error = f
                .engine
                .register(spec("journal", selector), &Principal::system())
                .await
                .unwrap_err();
            // Refused at registration: not registered with the topic silently left out.
            assert_eq!(error.code(), CODE_SPEC_INVALID, "{selector}");
            assert_no_consumer(&f, "journal").await;
        }
    }
}

#[tokio::test]
async fn a_prefix_selector_with_the_right_reads_doc_state() {
    let f = seeded(Arc::new(Grants(&["doc_"]))).await;
    f.engine
        .register(spec("journal", "doc_.*"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("journal").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 1);
}

/// The class selector is gated by the same right: a consumer on a non-reserved topic prefix
/// that names the `state` class without the right is refused too, and without the `state` class
/// the default selection never carries a `doc_state` frame.
#[tokio::test]
async fn the_state_class_needs_the_same_right_and_the_default_never_carries_it() {
    let f = seeded(Arc::new(DenyStreamRights)).await;
    let mut by_class = spec("by_class", "task.*");
    by_class.classes = Some(BTreeSet::from([StreamClass::State]));
    let error = f
        .engine
        .register(by_class, &Principal::system())
        .await
        .unwrap_err();
    assert_eq!(error.code(), CODE_SPEC_INVALID);

    let f = seeded(Arc::new(Grants(&["doc_"]))).await;
    let mut default_classes = spec("default_classes", "doc_state");
    default_classes.classes = None;
    f.engine
        .register(default_classes, &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("default_classes").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert!(
        batch.events.is_empty(),
        "the default classes carry no state event"
    );
}
