//! Consumer filter expressions over the envelope's `meta` and `subject` (the shared expression
//! language, ADR-28): a matrix of twenty cases.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_event_stream::{ConsumerSpec, PayloadMode, Start, TopicSelector};
use spot_permissions::Principal;
use support::{doc_record, fixture_full, rid};

/// The filter cases: the expression and whether the fixture event matches it.
///
/// The fixture event is `doc_created` on entity `task`, actor `system:test`, with no trace id,
/// no command id and a correlation id equal to the subject id.
const CASES: &[(&str, bool)] = &[
    ("doc.subject.entity == 'task'", true),
    ("doc.subject.entity == 'invoice'", false),
    ("doc.meta.actor.kind == 'system'", true),
    ("doc.meta.actor.component == 'test'", true),
    ("doc.meta.actor.kind == 'user'", false),
    ("doc.meta.trace_id == null", true),
    ("doc.meta.trace_id != null", false),
    ("doc.meta.command_id == null", true),
    ("doc.subject.entity != 'invoice'", true),
    ("doc.subject.entity in ['task', 'invoice']", true),
    ("doc.subject.entity in ['x', 'y']", false),
    (
        "doc.meta.actor.kind == 'system' and doc.subject.entity == 'task'",
        true,
    ),
    (
        "doc.meta.actor.kind == 'user' or doc.subject.entity == 'task'",
        true,
    ),
    ("not (doc.subject.entity == 'invoice')", true),
    ("not (doc.subject.entity == 'task')", false),
    ("doc.subject.id != null", true),
    ("doc.meta.actor.component == 'other'", false),
    ("doc.meta.correlation_id != null", true),
    (
        "doc.meta.actor.kind == 'system' and not (doc.subject.entity == 'invoice')",
        true,
    ),
    (
        "doc.subject.entity == 'task' and doc.meta.trace_id == null",
        true,
    ),
];

#[tokio::test]
async fn filter_matrix() {
    assert_eq!(CASES.len(), 20, "the matrix must hold twenty cases");
    for (index, (filter, expected)) in CASES.iter().enumerate() {
        let f = fixture_full(
            Arc::new(spot_event_stream::AllVisible),
            Arc::new(spot_event_stream::AllowAllStreamRights),
            spot_event_stream::BatchPolicy::default(),
            spot_event_stream::ParkPolicy::default(),
        )
        .await;
        f.feed.push(doc_record(0, 1, rid(1)));
        let name = format!("case_{index}");
        let spec = ConsumerSpec {
            name: name.clone(),
            owner: "owner".to_owned(),
            topics: vec![TopicSelector::Prefix("doc".to_owned())],
            filter: Some((*filter).to_owned()),
            partitions: 1,
            payload_mode: PayloadMode::IdsOnly,
            start: Start::FromBeginning,
        };
        f.engine.register(spec, &Principal::system()).await.unwrap();
        let lease = f.engine.acquire(&name).await.unwrap();
        let batch = f.engine.pull(&lease, 0, None, false).await.unwrap();
        assert_eq!(
            !batch.events.is_empty(),
            *expected,
            "case {index}: `{filter}` expected {expected}, got {} event(s)",
            batch.events.len()
        );
    }
}

#[tokio::test]
async fn an_invalid_filter_is_refused_at_registration() {
    let f = fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(spot_event_stream::AllowAllStreamRights),
        spot_event_stream::BatchPolicy::default(),
        spot_event_stream::ParkPolicy::default(),
    )
    .await;
    let spec = ConsumerSpec {
        name: "bad_filter".to_owned(),
        owner: "owner".to_owned(),
        topics: vec![TopicSelector::Prefix("doc".to_owned())],
        filter: Some("doc.subject.entity ==".to_owned()),
        partitions: 1,
        payload_mode: PayloadMode::IdsOnly,
        start: Start::FromBeginning,
    };
    let error = f
        .engine
        .register(spec, &Principal::system())
        .await
        .unwrap_err();
    assert_eq!(error.code(), spot_event_stream::error::CODE_SPEC_INVALID);
}
