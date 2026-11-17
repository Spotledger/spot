//! Consumer selectors (WP-JRN-09.T1): classes and entities resolved from the events registry,
//! enforced before the permission filter, with the reserved-class stream right for `state` and
//! `audit` re-read on every pull.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use spot_event_stream::error::{CODE_CLASS_UNKNOWN, CODE_ENTITIES_NEED_STATE, CODE_SPEC_INVALID};
use spot_event_stream::{
    ConsumerSpec, DenyStreamRights, PayloadMode, Start, StreamClass, StreamRights, TopicSelector,
    Visibility,
};
use spot_events_contract::envelope::Position;
use spot_permissions::Principal;
use spot_storage_contract::Name;
use support::{fixture_full, fixture_with, record, rid};

fn spec(name: &str) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: "owner".to_owned(),
        topics: vec![
            TopicSelector::Prefix("task".to_owned()),
            TopicSelector::Prefix("doc_".to_owned()),
            TopicSelector::Prefix("sys_".to_owned()),
            TopicSelector::Prefix("identity_".to_owned()),
            TopicSelector::Prefix("shop".to_owned()),
        ],
        filter: None,
        partitions: 1,
        classes: None,
        entities: None,
        payload_mode: PayloadMode::IdsOnly,
        start: Start::FromBeginning,
    }
}

fn classes(of: &[StreamClass]) -> Option<BTreeSet<StreamClass>> {
    Some(of.iter().copied().collect())
}

fn entities(of: &[&str]) -> Option<BTreeSet<Name>> {
    Some(of.iter().map(|e| Name::new(e).unwrap()).collect())
}

/// A domain event (an app topic with no registry entry).
fn domain(seq: u64, entity: &str) -> spot_storage_contract::OutboxRecord {
    record(
        0,
        seq,
        "task_created",
        entity,
        rid(seq),
        1,
        serde_json::json!({"n": seq}),
    )
}

/// A `state` event: the registered `doc_state` topic.
fn state(seq: u64, entity: &str) -> spot_storage_contract::OutboxRecord {
    record(
        0,
        seq,
        "doc_state",
        entity,
        rid(seq),
        1,
        serde_json::json!({"entity_schema": 1, "op": "insert", "image": {"status": "Open"}}),
    )
}

/// An `audit` event: the registered `sys_journal_admin` topic.
fn audit(seq: u64) -> spot_storage_contract::OutboxRecord {
    record(
        0,
        seq,
        "sys_journal_admin",
        "journal",
        rid(seq),
        1,
        serde_json::json!({}),
    )
}

fn topics_of(batch: &spot_event_stream::Batch) -> Vec<String> {
    batch
        .events
        .iter()
        .map(|e| e.topic.as_str().to_owned())
        .collect()
}

// Spec: selectors, defaults and parse-time refusals.

#[test]
fn a_definition_from_before_the_selectors_reads_as_the_default() {
    let text = r#"{"name":"c","owner":"o","topics":["sales.*"],"partitions":1,
        "payload_mode":"ids_only","start":"from_beginning"}"#;
    let parsed: ConsumerSpec = serde_json::from_str(text).unwrap();
    assert_eq!(parsed.classes, None);
    assert_eq!(parsed.entities, None);
    assert_eq!(
        parsed.effective_classes(),
        [StreamClass::Domain, StreamClass::Audit]
            .into_iter()
            .collect()
    );
    // Unset selectors are not written back, so a definition round-trips byte for byte.
    let again = serde_json::to_value(&parsed).unwrap();
    assert!(again.get("classes").is_none() && again.get("entities").is_none());
}

#[test]
fn selectors_round_trip() {
    let mut s = spec("c");
    s.classes = classes(&[StreamClass::State, StreamClass::Domain]);
    s.entities = entities(&["task", "invoice"]);
    let text = serde_json::to_string(&s).unwrap();
    assert!(text.contains(r#""classes":["state","domain"]"#), "{text}");
    assert!(text.contains(r#""entities":["invoice","task"]"#), "{text}");
    let back: ConsumerSpec = serde_json::from_str(&text).unwrap();
    assert_eq!(back, s);
}

#[test]
fn telemetry_and_other_class_names_are_refused_at_parse_time() {
    for class in ["telemetry", "State", "", "all"] {
        let text = format!(
            r#"{{"name":"c","owner":"o","topics":["sales.*"],"partitions":1,
            "payload_mode":"ids_only","start":"from_beginning","classes":["{class}"]}}"#
        );
        assert!(
            serde_json::from_str::<ConsumerSpec>(&text).is_err(),
            "{class}"
        );
        assert_eq!(
            StreamClass::parse(class).unwrap_err().code(),
            CODE_CLASS_UNKNOWN,
            "{class}"
        );
    }
}

#[test]
fn entities_need_the_state_class() {
    let mut s = spec("c");
    s.entities = entities(&["task"]);
    assert_eq!(s.validate().unwrap_err().code(), CODE_ENTITIES_NEED_STATE);
    s.classes = classes(&[StreamClass::Domain]);
    assert_eq!(s.validate().unwrap_err().code(), CODE_ENTITIES_NEED_STATE);
    s.classes = classes(&[StreamClass::State]);
    assert!(s.validate().is_ok());
    s.entities = Some(BTreeSet::new());
    assert_eq!(s.validate().unwrap_err().code(), CODE_SPEC_INVALID);
    let mut empty = spec("c");
    empty.classes = Some(BTreeSet::new());
    assert_eq!(empty.validate().unwrap_err().code(), CODE_SPEC_INVALID);
}

// Resolution against the registry.

#[tokio::test]
async fn the_default_delivers_domain_and_audit_and_not_state() {
    let f = support::fixture().await;
    f.feed.push(domain(1, "task"));
    f.feed.push(state(2, "task"));
    f.feed.push(audit(3));
    f.engine
        .register(spec("c"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(topics_of(&batch), vec!["task_created", "sys_journal_admin"]);
    // A class the consumer did not ask for is not a skip: nothing counted.
    assert_eq!((batch.skipped, batch.unclassified), (0, 0));
}

#[tokio::test]
async fn state_is_delivered_only_when_asked_for() {
    let f = support::fixture().await;
    f.feed.push(domain(1, "task"));
    f.feed.push(state(2, "task"));
    f.feed.push(audit(3));
    let mut s = spec("c");
    s.classes = classes(&[StreamClass::State]);
    f.engine.register(s, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(topics_of(&batch), vec!["doc_state"]);
}

#[tokio::test]
async fn an_unregistered_platform_topic_is_dropped_and_counted() {
    let f = support::fixture().await;
    f.feed.push(record(
        0,
        1,
        "sys_x",
        "journal",
        rid(1),
        1,
        serde_json::json!({}),
    ));
    f.feed.push(record(
        0,
        2,
        "doc_made_up",
        "task",
        rid(2),
        1,
        serde_json::json!({}),
    ));
    f.feed.push(record(
        0,
        3,
        "identity_x",
        "user",
        rid(3),
        1,
        serde_json::json!({}),
    ));
    // An app topic with no entry is `domain`.
    f.feed.push(record(
        0,
        4,
        "shop_order",
        "order",
        rid(4),
        1,
        serde_json::json!({}),
    ));
    let mut s = spec("c");
    s.classes = classes(&[StreamClass::State, StreamClass::Domain, StreamClass::Audit]);
    f.engine.register(s, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(topics_of(&batch), vec!["shop_order"]);
    assert_eq!(batch.unclassified, 3);
    assert_eq!(batch.skipped, 0);
}

#[tokio::test]
async fn entities_filter_state_events_only() {
    let f = support::fixture().await;
    f.feed.push(state(1, "task"));
    f.feed.push(state(2, "invoice"));
    f.feed.push(domain(3, "invoice"));
    f.feed.push(state(4, "task"));
    let mut s = spec("c");
    s.classes = classes(&[StreamClass::State, StreamClass::Domain]);
    s.entities = entities(&["task"]);
    f.engine.register(s, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    let got: Vec<(String, String)> = batch
        .events
        .iter()
        .map(|e| (e.topic.as_str().to_owned(), e.subject.entity.clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("doc_state".to_owned(), "task".to_owned()),
            ("task_created".to_owned(), "invoice".to_owned()),
            ("doc_state".to_owned(), "task".to_owned()),
        ],
        "the invoice state event is out, the invoice domain event is not touched"
    );
    assert_eq!(batch.skipped, 0);
}

// Rights.

#[tokio::test]
async fn naming_state_or_audit_needs_the_stream_right() {
    let f = fixture_with(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(DenyStreamRights),
    )
    .await;
    for class in [StreamClass::State, StreamClass::Audit] {
        let mut s = spec("c");
        s.topics = vec![TopicSelector::Prefix("task".to_owned())];
        s.classes = classes(&[class]);
        let error = f
            .engine
            .register(s, &Principal::system())
            .await
            .unwrap_err();
        assert_eq!(error.code(), CODE_SPEC_INVALID, "{class:?}");
    }
    // Domain needs none, and the default (which names nothing) registers.
    let mut domain_only = spec("d");
    domain_only.topics = vec![TopicSelector::Prefix("task".to_owned())];
    domain_only.classes = classes(&[StreamClass::Domain]);
    f.engine
        .register(domain_only, &Principal::system())
        .await
        .unwrap();
    let mut plain = spec("p");
    plain.topics = vec![TopicSelector::Prefix("task".to_owned())];
    f.engine
        .register(plain, &Principal::system())
        .await
        .unwrap();
}

/// A rights source that can be revoked while the stream runs.
struct Revocable(AtomicBool);

impl StreamRights for Revocable {
    fn may_stream_class(&self, _owner: &str, _class: &str) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

#[tokio::test]
async fn a_right_revoked_mid_stream_stops_the_reserved_classes_at_the_next_pull() {
    let rights = Arc::new(Revocable(AtomicBool::new(true)));
    let f = fixture_with(Arc::new(spot_event_stream::AllVisible), rights.clone()).await;
    f.feed.push(domain(1, "task"));
    f.feed.push(state(2, "task"));
    f.feed.push(audit(3));
    let mut s = spec("c");
    s.classes = classes(&[StreamClass::State, StreamClass::Domain, StreamClass::Audit]);
    f.engine.register(s, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let first = f.engine.pull(&lease, 0, Some(1), false).await.unwrap();
    assert_eq!(topics_of(&first), vec!["task_created"]);
    // The right is revoked: the stored definition still says state and audit.
    rights.0.store(false, Ordering::SeqCst);
    f.feed.push(domain(4, "task"));
    let second = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(
        topics_of(&second),
        vec!["task_created"],
        "state and audit stopped"
    );
    // Granted again, new reserved events flow (the skipped ones are not replayed by a pull).
    rights.0.store(true, Ordering::SeqCst);
    f.feed.push(state(5, "task"));
    let third = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(topics_of(&third), vec!["doc_state"]);
}

#[tokio::test]
async fn the_default_audit_class_also_needs_the_right_at_pull_time() {
    let rights = Arc::new(Revocable(AtomicBool::new(true)));
    let f = fixture_with(Arc::new(spot_event_stream::AllVisible), rights.clone()).await;
    f.feed.push(domain(1, "task"));
    f.feed.push(audit(2));
    f.engine
        .register(spec("c"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    rights.0.store(false, Ordering::SeqCst);
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(
        topics_of(&batch),
        vec!["task_created"],
        "no right: no audit"
    );
}

// Position and ordering with selectors.

#[tokio::test]
async fn a_filtered_event_advances_the_cursor_and_an_ack_never_redelivers_it() {
    let f = support::fixture().await;
    f.feed.push(domain(1, "task"));
    f.feed.push(state(2, "task"));
    f.feed.push(domain(3, "task"));
    f.feed.push(state(4, "task"));
    f.engine
        .register(spec("c"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(topics_of(&batch), vec!["task_created", "task_created"]);
    let next = batch.next_position.unwrap();
    assert_eq!(next.seq, 3, "the position is the last delivered event's");
    f.engine.ack(&lease, 0, next).await.unwrap();
    // Nothing is left; the trailing filtered event is not delivered either.
    let again = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert!(again.events.is_empty() && again.next_position.is_none());
    // A restart from the start re-reads everything, and still never delivers a filtered class.
    f.engine
        .restart(
            &lease,
            0,
            Position {
                generation: 0,
                seq: 0,
            },
        )
        .await
        .unwrap();
    let replay = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(topics_of(&replay), vec!["task_created", "task_created"]);
    assert!(
        replay
            .events
            .iter()
            .all(|e| e.topic.as_str() != "doc_state"),
        "a filtered event was delivered"
    );
}

#[tokio::test]
async fn the_selector_runs_before_the_permission_filter() {
    // A visibility that counts every question it is asked: events the class selector drops
    // must never reach it.
    struct Counting(std::sync::atomic::AtomicUsize);
    impl Visibility for Counting {
        fn may_read(
            &self,
            _owner: &str,
            _subject: &spot_event_stream::Subject,
        ) -> Result<bool, spot_event_stream::StreamError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
        fn project(
            &self,
            _owner: &str,
            _subject: &spot_event_stream::Subject,
            _data: &mut serde_json::Map<String, serde_json::Value>,
        ) -> Result<(), spot_event_stream::StreamError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    let counting = Arc::new(Counting(std::sync::atomic::AtomicUsize::new(0)));
    let f = fixture_full(
        counting.clone(),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        spot_event_stream::BatchPolicy::default(),
        spot_event_stream::ParkPolicy::default(),
    )
    .await;
    f.feed.push(state(1, "task"));
    f.feed.push(state(2, "task"));
    f.feed.push(record(
        0,
        3,
        "sys_x",
        "journal",
        rid(3),
        1,
        serde_json::json!({}),
    ));
    f.feed.push(domain(4, "task"));
    f.engine
        .register(spec("c"), &Principal::system())
        .await
        .unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(batch.events.len(), 1);
    assert_eq!(
        counting.0.load(Ordering::SeqCst),
        1,
        "only the selected event was asked about"
    );
}

/// A visibility whose field guard refuses: the image cannot be filtered.
struct RefusingGuard;

impl Visibility for RefusingGuard {
    fn may_read(
        &self,
        _owner: &str,
        _subject: &spot_event_stream::Subject,
    ) -> Result<bool, spot_event_stream::StreamError> {
        Ok(true)
    }
    fn project(
        &self,
        _owner: &str,
        _subject: &spot_event_stream::Subject,
        data: &mut serde_json::Map<String, serde_json::Value>,
    ) -> Result<(), spot_event_stream::StreamError> {
        data.clear();
        Err(spot_event_stream::StreamError::Permission {
            code: "fieldguard.unknown_doctype",
            message: "the guard does not know this entity".to_owned(),
        })
    }
}

#[tokio::test]
async fn an_image_the_guard_cannot_filter_is_dropped_not_passed_through() {
    let f = fixture_with(
        Arc::new(RefusingGuard),
        Arc::new(spot_event_stream::AllowAllStreamRights),
    )
    .await;
    f.feed.push(record(
        0,
        1,
        "doc_state",
        "task",
        rid(1),
        1,
        serde_json::json!({
            "entity_schema": 1,
            "op": "update",
            "image": {"status": "Open", "iban": "GB82WEST12345698765432"},
            "extra": "GB82WEST12345698765432"
        }),
    ));
    let mut s = spec("c");
    s.classes = classes(&[StreamClass::State]);
    s.payload_mode = PayloadMode::Fields;
    f.engine.register(s, &Principal::system()).await.unwrap();
    let lease = f.engine.acquire("c").await.unwrap();
    let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
    assert_eq!(
        batch.events.len(),
        1,
        "the event is delivered, without its image"
    );
    let data = batch.events[0].data.as_object().unwrap();
    assert!(!data.contains_key("image"));
    assert!(
        !data.contains_key("extra"),
        "keys the state payload does not define are dropped"
    );
    assert_eq!(data.get("op").unwrap(), "update");
    let text = serde_json::to_string(&batch.events[0]).unwrap();
    assert!(!text.contains("GB82"), "a value leaked: {text}");
}
