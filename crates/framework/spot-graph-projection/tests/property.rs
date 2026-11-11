//! The property test (WP-GRA-07.T1): random edge-event sequences, with duplicate redelivery and
//! projector restarts, always end with an empty drift list — the projection matches the source
//! graph.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use proptest::prelude::*;
use spot_graph_index::{
    Edge, EdgeEvent, EdgeKey, EdgeOp, GraphIndex, GraphIndexError, GraphKey, GraphSource,
};
use spot_graph_projection::wire::event_to_json;
use spot_graph_projection::{
    EDGE_TOPIC, InMemoryCheckpointStore, InMemorySink, NothingSensitive, ProjectionConfig,
    Projector, verify,
};
use spot_storage_contract::{FeedPosition, Name, NodeRef, OutboxEvent, OutboxRecord, RecordId};

fn key() -> GraphKey {
    GraphKey::new("tenant", "hierarchies")
}

fn id(seed: u8) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[15] = seed;
    RecordId::from_bytes(bytes).unwrap()
}

fn node(entity: &str, seed: u8) -> NodeRef {
    NodeRef {
        entity: Name::new(entity).unwrap(),
        id: id(seed),
    }
}

/// A source over a graph index, built from the canonical events.
struct Oracle(GraphIndex);

impl GraphSource for Oracle {
    fn stream_edges(&self, _key: &GraphKey) -> Result<Vec<Edge>, GraphIndexError> {
        Ok(self.0.edges().collect())
    }

    fn watermark(&self, _key: &GraphKey) -> Result<u64, GraphIndexError> {
        Ok(self.0.watermark())
    }
}

fn record_for(event: &EdgeEvent) -> OutboxRecord {
    OutboxRecord {
        position: FeedPosition::new(1, event.sequence),
        transaction: id(200),
        ordinal: 0,
        event: OutboxEvent {
            id: id(201),
            topic: Name::new(EDGE_TOPIC).unwrap(),
            subject: node("invoice", 1),
            subject_version: None,
            schema_version: 1,
            payload: event_to_json(event).to_string(),
        },
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// A random sequence of add/remove/update events, delivered once, redelivered as duplicates
    /// and interrupted by projector restarts, leaves the sink equal to a graph index built from
    /// the same events.
    #[test]
    fn a_projection_matches_its_source_after_duplicates_and_restarts(
        ops in prop::collection::vec((0u8..3, 0u8..4, 0u8..4), 0..40),
        restart_every in 1usize..9,
    ) {
        let config = ProjectionConfig::new(key()).with_all_fields("invoice.grn", "invoice");
        let sink = Arc::new(InMemorySink::new(key()));
        let checkpoints = Arc::new(InMemoryCheckpointStore::new());
        let sensitivity = Arc::new(NothingSensitive);

        let events: Vec<EdgeEvent> = ops
            .iter()
            .enumerate()
            .map(|(index, (op, from, to))| EdgeEvent {
                sequence: u64::try_from(index).unwrap() + 1,
                graph: key(),
                op: match op {
                    0 => EdgeOp::Add,
                    1 => EdgeOp::Remove,
                    _ => EdgeOp::Update,
                },
                edge: EdgeKey::new(
                    "invoice.grn",
                    node("invoice", *from),
                    node("grn", *to),
                ),
                properties: BTreeMap::new(),
            })
            .collect();

        let mut oracle = GraphIndex::new(key());
        for event in &events {
            oracle.apply(event).unwrap();
        }
        let oracle = Oracle(oracle);

        let records: Vec<OutboxRecord> = events.iter().map(record_for).collect();

        let mut projector = Projector::new(
            sink.clone(),
            checkpoints.clone(),
            config.clone(),
            sensitivity.clone(),
        );
        for (index, record) in records.iter().enumerate() {
            if index % restart_every == 0 {
                // A restart: a fresh projector over the same sink and durable checkpoint.
                projector = Projector::new(
                    sink.clone(),
                    checkpoints.clone(),
                    config.clone(),
                    sensitivity.clone(),
                );
            }
            projector.process(record).unwrap();
            if index % 3 == 0 {
                // A duplicate redelivery: it must change nothing.
                projector.process(record).unwrap();
            }
        }

        let drifts = verify(&oracle, sink.as_ref()).unwrap();
        prop_assert!(drifts.is_empty(), "drift after a random sequence: {drifts:?}");
    }
}
