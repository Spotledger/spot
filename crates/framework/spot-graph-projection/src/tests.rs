//! Unit tests for the projection: the wire form, the sinks, the config, the decoder, the
//! projector, the verifier and the rebuild.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Map, Value as Json};
use spot_graph_index::{Edge, EdgeEvent, EdgeKey, EdgeOp, GraphIndex, GraphKey, GraphSource};
use spot_storage_contract::{
    FeedPosition, Name, NodeRef, OutboxEvent, OutboxRecord, RecordId, Value,
};

use crate::checkpoint::{CheckpointStore, InMemoryCheckpointStore};
use crate::config::{FieldSensitivity, MetaSensitivity, NothingSensitive, ProjectionConfig};
use crate::decode::{
    EDGE_TOPIC, EdgeTopic, RETIRED_EDGE_TOPIC, decode_record, decode_record_with_topic,
};
use crate::error::ProjectionError;
use crate::projector::{Projected, Projector};
use crate::sink::{EdgeBatch, FileSink, GraphSink, InMemorySink, SinkReader, replay};
use crate::verify::verify;
use crate::wire::{event_to_json, graph_to_json, node_to_json, value_from_json, value_to_json};

fn key() -> GraphKey {
    GraphKey::new("tenant", "hierarchies")
}

fn id(seed: u8) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[6] = 0x70; // UUIDv7 version nibble
    bytes[8] = 0x80; // RFC 4122 variant
    bytes[15] = seed;
    RecordId::from_bytes(bytes).unwrap()
}

fn node(entity: &str, seed: u8) -> NodeRef {
    NodeRef {
        entity: Name::new(entity).unwrap(),
        id: id(seed),
    }
}

fn edge_event(sequence: u64, op: EdgeOp, from: NodeRef, to: NodeRef) -> EdgeEvent {
    EdgeEvent {
        sequence,
        graph: key(),
        op,
        edge: EdgeKey::new("invoice.grn", from, to),
        properties: BTreeMap::new(),
    }
}

fn record_with(
    topic: &str,
    schema_version: u32,
    payload: String,
    position: FeedPosition,
) -> OutboxRecord {
    OutboxRecord {
        position,
        transaction: id(200),
        ordinal: 0,
        event: OutboxEvent {
            id: id(201),
            topic: Name::new(topic).unwrap(),
            subject: node("invoice", 1),
            subject_version: None,
            schema_version,
            payload,
        },
    }
}

fn edge_payload(op: &str, from: NodeRef, to: NodeRef) -> String {
    let mut object = Map::new();
    object.insert("graph".to_owned(), graph_to_json(&key()));
    object.insert("op".to_owned(), Json::String(op.to_owned()));
    object.insert(
        "edge_type".to_owned(),
        Json::String("invoice.grn".to_owned()),
    );
    object.insert("from".to_owned(), node_to_json(&from));
    object.insert("to".to_owned(), node_to_json(&to));
    Json::Object(object).to_string()
}

fn edge_record(sequence: u64, op: &str, from: NodeRef, to: NodeRef) -> OutboxRecord {
    record_with(
        EDGE_TOPIC,
        1,
        edge_payload(op, from, to),
        FeedPosition::new(1, sequence),
    )
}

fn config() -> ProjectionConfig {
    ProjectionConfig::new(key()).with_all_fields("invoice.grn", "invoice")
}

fn harness() -> (Projector, Arc<InMemorySink>, Arc<InMemoryCheckpointStore>) {
    let sink = Arc::new(InMemorySink::new(key()));
    let checkpoints = Arc::new(InMemoryCheckpointStore::new());
    let projector = Projector::new(
        sink.clone(),
        checkpoints.clone(),
        config(),
        Arc::new(NothingSensitive),
    );
    (projector, sink, checkpoints)
}

fn temp_path(name: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "spot-graph-projection-{}-{name}-{n}.jsonl",
        std::process::id()
    ))
}

/// A source over a graph index, for verify and rebuild.
struct Oracle(GraphIndex);

impl GraphSource for Oracle {
    fn stream_edges(
        &self,
        _key: &GraphKey,
    ) -> Result<Vec<Edge>, spot_graph_index::GraphIndexError> {
        Ok(self.0.edges().collect())
    }

    fn watermark(&self, _key: &GraphKey) -> Result<u64, spot_graph_index::GraphIndexError> {
        Ok(self.0.watermark())
    }
}

// --- wire -----------------------------------------------------------------

#[test]
fn the_wire_form_round_trips_every_value() {
    let values = vec![
        Value::Null,
        Value::Bool(true),
        Value::Int(-7),
        Value::Decimal(rust_decimal::Decimal::new(12345, 2)),
        Value::Text("hi".to_owned()),
        Value::Date(time::Date::from_calendar_date(2026, time::Month::October, 17).unwrap()),
        Value::Time(time::Time::from_hms_milli(13, 45, 0, 123).unwrap()),
        Value::DateTime(time::OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()),
        Value::Id(id(9)),
        Value::Json("{\"a\":1}".to_owned()),
        Value::Bytes(vec![0, 1, 255]),
    ];
    for value in values {
        let json = value_to_json(&value);
        let back = value_from_json(&json).unwrap();
        assert_eq!(back, value, "round trip of {value:?}");
    }
}

// --- decode ---------------------------------------------------------------

#[test]
fn a_non_edge_topic_is_not_an_edge_event() {
    let record = record_with("doc_created", 1, "{}".to_owned(), FeedPosition::new(1, 1));
    assert_eq!(decode_record(&record).unwrap(), None);
}

#[test]
fn an_unsupported_schema_version_is_a_decode_error() {
    let record = record_with(EDGE_TOPIC, 2, "{}".to_owned(), FeedPosition::new(1, 1));
    let error = decode_record(&record).unwrap_err();
    assert_eq!(error.code(), crate::error::DECODE);
}

#[test]
fn a_malformed_payload_is_a_decode_error() {
    let record = record_with(
        EDGE_TOPIC,
        1,
        "not json".to_owned(),
        FeedPosition::new(1, 1),
    );
    assert_eq!(
        decode_record(&record).unwrap_err().code(),
        crate::error::DECODE
    );
}

#[test]
fn a_valid_edge_record_decodes_to_an_event_with_the_feed_sequence() {
    let record = edge_record(4, "add", node("invoice", 1), node("grn", 2));
    let event = decode_record(&record).unwrap().unwrap();
    assert_eq!(event.sequence, 4);
    assert_eq!(event.graph, key());
    assert_eq!(event.op, EdgeOp::Add);
    assert_eq!(event.edge.edge_type, "invoice.grn");
    assert_eq!(event.edge.from, node("invoice", 1));
    assert_eq!(event.edge.to, node("grn", 2));
}

// --- sink -----------------------------------------------------------------

#[test]
fn the_in_memory_sink_ignores_duplicates_and_reports_a_gap() {
    let sink = InMemorySink::new(key());
    let first = edge_event(1, EdgeOp::Add, node("invoice", 1), node("grn", 2));
    let outcome = sink
        .apply(&EdgeBatch::events(key(), vec![first.clone()]))
        .unwrap();
    assert_eq!(outcome.changed, 1);
    assert_eq!(sink.checkpoint(), 1);

    let again = sink.apply(&EdgeBatch::events(key(), vec![first])).unwrap();
    assert_eq!(again.ignored, 1);

    let hole = edge_event(3, EdgeOp::Add, node("invoice", 2), node("grn", 3));
    let error = sink
        .apply(&EdgeBatch::events(key(), vec![hole]))
        .unwrap_err();
    assert_eq!(error.code(), crate::error::GAP);
}

#[test]
fn the_in_memory_sink_refuses_another_graph() {
    let sink = InMemorySink::new(key());
    let mut event = edge_event(1, EdgeOp::Add, node("invoice", 1), node("grn", 2));
    event.graph = GraphKey::new("tenant", "other");
    let error = sink
        .apply(&EdgeBatch::events(
            GraphKey::new("tenant", "other"),
            vec![event],
        ))
        .unwrap_err();
    assert_eq!(error.code(), crate::error::WRONG_GRAPH);
}

#[test]
fn the_file_sink_log_replays_to_the_same_graph() {
    let path = temp_path("replay");
    let _ = std::fs::remove_file(&path);
    let sink = FileSink::new(&path, key()).unwrap();
    sink.apply(&EdgeBatch::events(
        key(),
        vec![
            edge_event(1, EdgeOp::Add, node("invoice", 1), node("grn", 2)),
            edge_event(2, EdgeOp::Add, node("invoice", 1), node("grn", 3)),
        ],
    ))
    .unwrap();
    sink.apply(&EdgeBatch::events(
        key(),
        vec![edge_event(
            3,
            EdgeOp::Remove,
            node("invoice", 1),
            node("grn", 3),
        )],
    ))
    .unwrap();

    let replayed = replay(&path, key()).unwrap();
    assert_eq!(replayed.watermark(), sink.watermark());
    let live: Vec<Edge> = sink.edges();
    let back: Vec<Edge> = replayed.edges().collect();
    assert_eq!(back, live);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_file_sink_reopened_resumes_from_its_log() {
    let path = temp_path("reopen");
    let _ = std::fs::remove_file(&path);
    {
        let sink = FileSink::new(&path, key()).unwrap();
        sink.apply(&EdgeBatch::events(
            key(),
            vec![edge_event(
                1,
                EdgeOp::Add,
                node("invoice", 1),
                node("grn", 2),
            )],
        ))
        .unwrap();
    }
    let reopened = FileSink::new(&path, key()).unwrap();
    assert_eq!(reopened.checkpoint(), 1);
    assert_eq!(reopened.edges().len(), 1);
    let _ = std::fs::remove_file(&path);
}

// --- checkpoint -----------------------------------------------------------

#[test]
fn the_checkpoint_store_saves_loads_and_clears() {
    let store = InMemoryCheckpointStore::new();
    assert_eq!(store.load(&key()).unwrap(), None);
    store.save(&key(), FeedPosition::new(1, 7)).unwrap();
    assert_eq!(store.load(&key()).unwrap(), Some(FeedPosition::new(1, 7)));
    store.clear(&key()).unwrap();
    assert_eq!(store.load(&key()).unwrap(), None);
}

// --- config and the permission model --------------------------------------

fn metadata() -> spot_meta::Registry {
    use spot_meta::{Definition, FieldSpec, FieldType, ProtectionGroupSpec};
    let mut definition = Definition::new("invoice");
    let mut salary = FieldSpec::new("salary", FieldType::Decimal);
    salary.scale = Some(2);
    salary.protection_group = Some("financial".to_owned());
    let mut note = FieldSpec::new("note", FieldType::Text);
    note.protection_group = Some("financial".to_owned());
    note.declassified = Some("reviewed for export".to_owned());
    definition.fields = vec![FieldSpec::new("status", FieldType::Text), salary, note];
    definition.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];
    spot_meta::Registry::new(vec![definition]).unwrap()
}

#[test]
fn the_config_drops_unlisted_fields() {
    let config = ProjectionConfig::new(key()).with_edge("invoice.grn", "invoice", ["status"]);
    let mut properties = BTreeMap::new();
    properties.insert("status".to_owned(), Value::Text("Open".to_owned()));
    properties.insert("note".to_owned(), Value::Text("x".to_owned()));
    let kept = config.project_properties("invoice.grn", &properties, &NothingSensitive);
    assert_eq!(kept.len(), 1);
    assert!(kept.contains_key("status"));
}

#[test]
fn the_config_drops_sensitive_fields_even_when_listed() {
    let meta = metadata().effective();
    let sensitivity = MetaSensitivity::from_meta(&meta);
    assert!(sensitivity.is_sensitive("invoice", "salary"));
    assert!(!sensitivity.is_sensitive("invoice", "note"), "declassified");
    assert!(!sensitivity.is_sensitive("invoice", "status"));

    // The config lists both fields, so only the sensitive one is dropped.
    let config = ProjectionConfig::new(key()).with_edge(
        "invoice.grn",
        "invoice",
        ["status", "salary", "note"],
    );
    let mut properties = BTreeMap::new();
    properties.insert("status".to_owned(), Value::Text("Open".to_owned()));
    properties.insert(
        "salary".to_owned(),
        Value::Decimal(rust_decimal::Decimal::new(500000, 2)),
    );
    properties.insert("note".to_owned(), Value::Text("x".to_owned()));
    let kept = config.project_properties("invoice.grn", &properties, &sensitivity);
    assert!(kept.contains_key("status"));
    assert!(kept.contains_key("note"));
    assert!(
        !kept.contains_key("salary"),
        "a sensitive field is excluded by construction"
    );
}

#[test]
fn an_unconfigured_edge_type_carries_no_properties() {
    let config = config();
    let mut properties = BTreeMap::new();
    properties.insert("status".to_owned(), Value::Text("Open".to_owned()));
    let kept = config.project_properties("other.edge", &properties, &NothingSensitive);
    assert!(kept.is_empty());
}

// --- projector ------------------------------------------------------------

#[test]
fn the_projector_applies_a_record_and_advances_the_checkpoint() {
    let (projector, sink, checkpoints) = harness();
    let record = edge_record(1, "add", node("invoice", 1), node("grn", 2));
    let report = projector.process(&record).unwrap();
    assert!(matches!(report.projected, Projected::Applied(_)));
    assert_eq!(sink.checkpoint(), 1);
    assert_eq!(
        checkpoints.load(&key()).unwrap(),
        Some(FeedPosition::new(1, 1))
    );

    let again = projector.process(&record).unwrap();
    assert_eq!(again.projected, Projected::Skipped);
}

#[test]
fn the_projector_passes_over_other_topics_and_advances() {
    let (projector, _sink, checkpoints) = harness();
    let record = record_with("doc_created", 1, "{}".to_owned(), FeedPosition::new(1, 1));
    let report = projector.process(&record).unwrap();
    assert_eq!(report.projected, Projected::NotEdge);
    assert_eq!(
        checkpoints.load(&key()).unwrap(),
        Some(FeedPosition::new(1, 1))
    );
}

#[test]
fn the_projector_rejects_a_malformed_edge_record_and_advances() {
    let (projector, sink, checkpoints) = harness();
    let record = record_with(
        EDGE_TOPIC,
        1,
        "not json".to_owned(),
        FeedPosition::new(1, 1),
    );
    let report = projector.process(&record).unwrap();
    match report.projected {
        Projected::Rejected { code, .. } => assert_eq!(code, crate::error::DECODE),
        other => panic!("expected a rejection, got {other:?}"),
    }
    assert_eq!(sink.checkpoint(), 0);
    assert_eq!(
        checkpoints.load(&key()).unwrap(),
        Some(FeedPosition::new(1, 1))
    );
}

#[test]
fn a_restart_resumes_from_the_durable_checkpoint() {
    let sink = Arc::new(InMemorySink::new(key()));
    let checkpoints = Arc::new(InMemoryCheckpointStore::new());
    let first = Projector::new(
        sink.clone(),
        checkpoints.clone(),
        config(),
        Arc::new(NothingSensitive),
    );
    first
        .process(&edge_record(1, "add", node("invoice", 1), node("grn", 2)))
        .unwrap();
    first
        .process(&edge_record(2, "add", node("invoice", 1), node("grn", 3)))
        .unwrap();

    // A new projector over the same sink and checkpoint store continues where the last stopped.
    let second = Projector::new(
        sink.clone(),
        checkpoints.clone(),
        config(),
        Arc::new(NothingSensitive),
    );
    let report = second
        .process(&edge_record(
            3,
            "remove",
            node("invoice", 1),
            node("grn", 3),
        ))
        .unwrap();
    assert!(matches!(report.projected, Projected::Applied(_)));
    assert_eq!(sink.edges().len(), 1);
}

#[test]
fn a_new_outbox_generation_is_a_resync() {
    let (projector, _sink, _checkpoints) = harness();
    projector
        .process(&edge_record(1, "add", node("invoice", 1), node("grn", 2)))
        .unwrap();
    let later = record_with(
        EDGE_TOPIC,
        1,
        edge_payload("add", node("invoice", 1), node("grn", 3)),
        FeedPosition::new(2, 1),
    );
    let error = projector.process(&later).unwrap_err();
    assert_eq!(error.code(), crate::error::RESYNC);
}

#[test]
fn a_batch_stops_at_an_infrastructure_failure() {
    let (projector, _sink, _checkpoints) = harness();
    // A gap in the middle of the batch: the second record skips a sequence, so the sink reports
    // it and the batch stops.
    let records = vec![
        edge_record(1, "add", node("invoice", 1), node("grn", 2)),
        edge_record(3, "add", node("invoice", 1), node("grn", 3)),
    ];
    let error = projector.process_batch(&records).unwrap_err();
    assert_eq!(error.code(), crate::error::GAP);
}

// --- verify and rebuild ---------------------------------------------------

#[test]
fn verify_is_empty_when_the_sink_matches_the_source() {
    let (projector, sink, _checkpoints) = harness();
    let event = edge_event(1, EdgeOp::Add, node("invoice", 1), node("grn", 2));
    let mut oracle = GraphIndex::new(key());
    oracle.apply(&event).unwrap();
    let oracle = Oracle(oracle);
    projector
        .process(&edge_record(1, "add", node("invoice", 1), node("grn", 2)))
        .unwrap();
    assert_eq!(verify(&oracle, sink.as_ref()).unwrap(), vec![]);
}

#[test]
fn verify_reports_a_missing_edge_and_a_watermark_drift() {
    let mut oracle = GraphIndex::new(key());
    oracle
        .apply(&edge_event(
            1,
            EdgeOp::Add,
            node("invoice", 1),
            node("grn", 2),
        ))
        .unwrap();
    let oracle = Oracle(oracle);
    let empty = InMemorySink::new(key());
    let drifts = verify(&oracle, &empty).unwrap();
    assert!(
        drifts.iter().any(|d| d.kind() == "missing_edge"),
        "{drifts:?}"
    );
    assert!(
        drifts.iter().any(|d| d.kind() == "watermark_mismatch"),
        "{drifts:?}"
    );
}

#[test]
fn rebuild_streams_a_snapshot_and_clears_the_checkpoint() {
    let (projector, sink, checkpoints) = harness();
    let mut oracle = GraphIndex::new(key());
    for (sequence, target) in [(1u64, 2u8), (2, 3), (3, 4)] {
        oracle
            .apply(&edge_event(
                sequence,
                EdgeOp::Add,
                node("invoice", 1),
                node("grn", target),
            ))
            .unwrap();
    }
    let oracle = Oracle(oracle);

    let report = projector.rebuild(&oracle).unwrap();
    assert_eq!(report.edges, 3);
    assert_eq!(report.watermark, 3);
    assert_eq!(sink.watermark(), 3);
    assert_eq!(checkpoints.load(&key()).unwrap(), None);
    assert_eq!(verify(&oracle, sink.as_ref()).unwrap(), vec![]);
}

#[test]
fn a_projection_never_sees_a_sensitive_field_end_to_end() {
    let meta = metadata().effective();
    let config = ProjectionConfig::new(key()).with_all_fields("invoice.grn", "invoice");
    let sink = Arc::new(InMemorySink::new(key()));
    let projector = Projector::new(
        sink.clone(),
        Arc::new(InMemoryCheckpointStore::new()),
        config,
        Arc::new(MetaSensitivity::from_meta(&meta)),
    );

    let mut payload =
        serde_json::from_str::<Json>(&edge_payload("add", node("invoice", 1), node("grn", 2)))
            .unwrap();
    let mut properties = Map::new();
    properties.insert(
        "status".to_owned(),
        value_to_json(&Value::Text("Open".to_owned())),
    );
    properties.insert(
        "salary".to_owned(),
        value_to_json(&Value::Decimal(rust_decimal::Decimal::new(500000, 2))),
    );
    payload["properties"] = Json::Object(properties);
    let record = record_with(EDGE_TOPIC, 1, payload.to_string(), FeedPosition::new(1, 1));

    projector.process(&record).unwrap();
    let edge = sink.edges().into_iter().next().unwrap();
    assert!(edge.properties.contains_key("status"));
    assert!(
        !edge.properties.contains_key("salary"),
        "a sensitive field never reaches the sink"
    );
}

#[test]
fn the_wire_event_form_is_a_valid_outbox_payload() {
    // The decoder and the wire form agree: an encoded event decodes back to itself.
    let event = edge_event(2, EdgeOp::Update, node("invoice", 1), node("grn", 2));
    let record = record_with(
        EDGE_TOPIC,
        1,
        event_to_json(&event).to_string(),
        FeedPosition::new(1, 2),
    );
    assert_eq!(decode_record(&record).unwrap().unwrap(), event);
}

#[test]
fn an_error_code_is_catalogued_and_stable() {
    let error = ProjectionError::sink("boom");
    assert_eq!(error.code(), "graph_projection.sink");
    assert!(error.to_string().contains("boom"));
}

fn edge_record_on(
    topic: &str,
    sequence: u64,
    op: &str,
    from: NodeRef,
    to: NodeRef,
) -> OutboxRecord {
    record_with(
        topic,
        1,
        edge_payload(op, from, to),
        FeedPosition::new(1, sequence),
    )
}

#[test]
fn the_edge_topic_is_the_registered_name_and_the_alias_is_the_retired_one() {
    assert_eq!(EDGE_TOPIC, "sys_graph_edge");
    assert_eq!(RETIRED_EDGE_TOPIC, "doc_edge");
    assert!(spot_events_contract::registry::lookup(EDGE_TOPIC).is_some_and(|e| !e.retired));
    assert!(spot_events_contract::registry::lookup(RETIRED_EDGE_TOPIC).is_some_and(|e| e.retired));
}

#[test]
fn both_names_decode_to_the_same_event_and_say_which_was_read() {
    let new = edge_record_on(EDGE_TOPIC, 3, "add", node("invoice", 1), node("grn", 2));
    let old = edge_record_on(
        RETIRED_EDGE_TOPIC,
        3,
        "add",
        node("invoice", 1),
        node("grn", 2),
    );
    let (new_event, new_topic) = decode_record_with_topic(&new).unwrap().unwrap();
    let (old_event, old_topic) = decode_record_with_topic(&old).unwrap().unwrap();
    assert_eq!(new_event, old_event);
    assert_eq!(new_topic, EdgeTopic::Registered);
    assert_eq!(old_topic, EdgeTopic::RetiredAlias);
}

#[test]
fn an_unknown_edge_like_topic_is_still_ignored() {
    for topic in [
        "doc_edges",
        "sys_graph_edge_v2",
        "graph_edge",
        "doc_created",
    ] {
        let record = edge_record_on(topic, 1, "add", node("invoice", 1), node("grn", 2));
        assert_eq!(decode_record(&record).unwrap(), None, "{topic}");
        let (projector, sink, _) = harness();
        let report = projector.process(&record).unwrap();
        assert_eq!(report.projected, Projected::NotEdge);
        assert_eq!(report.edge_topic, None);
        assert!(sink.edges().is_empty());
    }
}

#[test]
fn the_alias_has_the_same_decode_errors() {
    let record = record_with(
        RETIRED_EDGE_TOPIC,
        2,
        "{}".to_owned(),
        FeedPosition::new(1, 1),
    );
    let error = decode_record(&record).unwrap_err();
    assert_eq!(error.code(), crate::error::DECODE);
    assert!(error.to_string().contains("doc_edge"));
}

#[test]
fn both_names_project_to_the_same_state() {
    let ops = [
        ("add", 1u8, 2u8),
        ("add", 3, 4),
        ("update", 1, 2),
        ("remove", 3, 4),
    ];
    let run = |topic: &str| {
        let (projector, sink, _) = harness();
        for (index, (op, from, to)) in ops.iter().enumerate() {
            let record = edge_record_on(
                topic,
                u64::try_from(index).unwrap() + 1,
                op,
                node("invoice", *from),
                node("grn", *to),
            );
            projector.process(&record).unwrap();
        }
        (sink.edges(), projector.retired_alias_reads())
    };
    let (registered, registered_reads) = run(EDGE_TOPIC);
    let (alias, alias_reads) = run(RETIRED_EDGE_TOPIC);
    assert_eq!(registered, alias);
    assert_eq!(registered.len(), 1);
    assert_eq!(registered_reads, 0);
    assert_eq!(alias_reads, 4);
}

#[test]
fn a_replay_of_a_journal_with_both_names_is_idempotent() {
    let records = vec![
        edge_record_on(
            RETIRED_EDGE_TOPIC,
            1,
            "add",
            node("invoice", 1),
            node("grn", 2),
        ),
        edge_record_on(EDGE_TOPIC, 2, "add", node("invoice", 3), node("grn", 4)),
        edge_record_on(
            RETIRED_EDGE_TOPIC,
            3,
            "update",
            node("invoice", 1),
            node("grn", 2),
        ),
        edge_record_on(EDGE_TOPIC, 4, "remove", node("invoice", 3), node("grn", 4)),
    ];
    let (projector, sink, _) = harness();
    let reports = projector.process_batch(&records).unwrap();
    assert_eq!(reports[0].edge_topic, Some(EdgeTopic::RetiredAlias));
    assert_eq!(reports[1].edge_topic, Some(EdgeTopic::Registered));
    let first = sink.edges();
    assert_eq!(projector.retired_alias_reads(), 2);

    // The whole journal again: every record is at or below the checkpoint.
    let again = projector.process_batch(&records).unwrap();
    assert!(again.iter().all(|r| r.projected == Projected::Skipped));
    assert_eq!(sink.edges(), first);
    assert_eq!(projector.retired_alias_reads(), 2);

    // A fresh projector with no checkpoint over the same sink: the sink ignores applied sequences.
    let fresh = Projector::new(
        sink.clone(),
        Arc::new(InMemoryCheckpointStore::new()),
        config(),
        Arc::new(NothingSensitive),
    );
    fresh.process_batch(&records).unwrap();
    assert_eq!(sink.edges(), first);
}
