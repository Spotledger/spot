//! Shared test support: a fake outbox feed, record builders and an engine factory.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_coordinator::in_process::InProcessCoordinator;
use spot_coordinator::presence::InMemoryPresence;
use spot_coordinator::types::NodeId;
use spot_core::ManualClock;
use spot_event_stream::{
    AllowAllStreamRights, EngineConfig, FeedHead, InMemoryConsumerStore, RecordingAudit, Seams,
    StreamEngine, StreamRights, Visibility,
};
use spot_events_contract::names::SourceId;
use spot_storage_contract::{
    Change, ChangeFeed, ErrorKind, FeedPosition, Name, NodeRef, OutboxEvent, OutboxRecord,
    RecordId, StorageError, StorageResult, Version,
};

/// A deterministic UUIDv7 record id from a seed.
pub fn rid(seed: u64) -> RecordId {
    let mut bytes = [0u8; 16];
    bytes[6] = 0x70 | ((seed >> 8) as u8 & 0x0f);
    bytes[7] = seed as u8;
    bytes[8] = 0x80;
    bytes[9] = (seed >> 16) as u8;
    bytes[15] = seed as u8;
    RecordId::from_bytes(bytes).expect("a v7 record id")
}

/// The state of the fake feed.
#[derive(Default)]
struct FeedState {
    generation: u64,
    records: Vec<OutboxRecord>,
    retained_floor: u64,
    head: u64,
}

/// A fake outbox feed: an ordered, in-memory log with a retention floor and a generation.
#[derive(Default)]
pub struct FakeFeed {
    state: Mutex<FeedState>,
}

impl FakeFeed {
    /// An empty feed at generation 0.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a record; the feed's head is the last position.
    pub fn push(&self, record: OutboxRecord) {
        let mut state = self.state.lock().unwrap();
        state.head = state.head.max(record.position.seq());
        state.records.push(record);
    }

    /// Starts a new stream generation: old positions are lost.
    pub fn set_generation(&self, generation: u64) {
        let mut state = self.state.lock().unwrap();
        state.generation = generation;
        state.records.clear();
        state.retained_floor = 0;
        state.head = 0;
    }

    /// Drops every record before `seq` (retention), so a checkpoint below it resyncs.
    pub fn retain_from(&self, seq: u64) {
        let mut state = self.state.lock().unwrap();
        state.retained_floor = seq;
        state.records.retain(|r| r.position.seq() >= seq);
    }

    /// How many records are retained.
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().records.len()
    }

    /// Whether nothing is retained.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl FeedHead for FakeFeed {
    fn head(&self) -> Option<FeedPosition> {
        let state = self.state.lock().unwrap();
        Some(FeedPosition::new(state.generation, state.head))
    }
}

impl ChangeFeed for FakeFeed {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        let state = self.state.lock().unwrap();
        let is_start = after.generation() == 0 && after.seq() == 0;
        let floor = if is_start {
            0
        } else {
            if after.generation() != state.generation {
                return Err(resync("the position is from another generation"));
            }
            if after.seq() < state.retained_floor {
                return Err(resync("the position is older than the retention"));
            }
            after.seq()
        };
        Ok(state
            .records
            .iter()
            .filter(|r| r.position.generation() == state.generation && r.position.seq() > floor)
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn read_changes(&self, _after: FeedPosition, _limit: u32) -> StorageResult<Vec<Change>> {
        Ok(Vec::new())
    }
}

fn resync(why: &str) -> StorageError {
    StorageError::new(ErrorKind::ResyncRequired, why)
}

/// Builds one outbox record with a valid payload document.
pub fn record(
    generation: u64,
    seq: u64,
    topic: &str,
    entity: &str,
    id: RecordId,
    version: u64,
    data: serde_json::Value,
) -> OutboxRecord {
    let payload = serde_json::json!({
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "test"},
            "correlation_id": id.to_string(),
        },
        "data": data,
    })
    .to_string();
    OutboxRecord {
        position: FeedPosition::new(generation, seq),
        transaction: rid(seq.wrapping_mul(7).wrapping_add(1)),
        ordinal: 0,
        event: OutboxEvent {
            id: rid(seq.wrapping_mul(13).wrapping_add(3)),
            topic: Name::new(topic).expect("a topic name"),
            subject: NodeRef {
                entity: Name::new(entity).expect("an entity name"),
                id,
            },
            subject_version: Some(Version(version)),
            schema_version: 1,
            payload,
        },
    }
}

/// A `doc_created` record for `id` at `(generation, seq)`.
pub fn doc_record(generation: u64, seq: u64, id: RecordId) -> OutboxRecord {
    record(
        generation,
        seq,
        "doc_created",
        "task",
        id,
        seq.max(1),
        serde_json::json!({"name": format!("T-{seq}"), "status": "Open"}),
    )
}

/// The pieces a test keeps hold of.
pub struct Fixture {
    /// The engine.
    pub engine: StreamEngine<FakeFeed>,
    /// The feed.
    pub feed: Arc<FakeFeed>,
    /// The store.
    pub store: Arc<InMemoryConsumerStore>,
    /// The audit sink.
    pub audit: Arc<RecordingAudit>,
    /// The clock.
    pub clock: Arc<ManualClock>,
    /// The coordinator (shared, so a second engine can be built over the same store).
    pub coordinator: Arc<dyn spot_coordinator::Coordinator>,
    /// The source id.
    pub source: SourceId,
}

/// Builds an engine over a fresh fake feed, with `AllVisible` and all stream rights.
pub async fn fixture() -> Fixture {
    fixture_full(
        Arc::new(spot_event_stream::AllVisible),
        Arc::new(AllowAllStreamRights),
        spot_event_stream::BatchPolicy::default(),
        spot_event_stream::ParkPolicy::default(),
    )
    .await
}

/// Builds an engine with a chosen visibility and stream-rights source.
pub async fn fixture_with(
    visibility: Arc<dyn Visibility>,
    rights: Arc<dyn StreamRights>,
) -> Fixture {
    fixture_full(
        visibility,
        rights,
        spot_event_stream::BatchPolicy::default(),
        spot_event_stream::ParkPolicy::default(),
    )
    .await
}

/// Builds an engine with a chosen visibility, stream rights and policy.
pub async fn fixture_full(
    visibility: Arc<dyn Visibility>,
    rights: Arc<dyn StreamRights>,
    policy: spot_event_stream::BatchPolicy,
    park: spot_event_stream::ParkPolicy,
) -> Fixture {
    let feed = Arc::new(FakeFeed::new());
    let store = Arc::new(InMemoryConsumerStore::new());
    let audit = Arc::new(RecordingAudit::new());
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let coordinator: Arc<dyn spot_coordinator::Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("test-node").expect("a node id"),
        clock.clone(),
    )
    .await
    .expect("the coordinator starts");
    let source = SourceId::new("acme.spotledger.app").expect("a source id");
    let seams = Seams {
        feed: feed.clone(),
        head: Some(feed.clone()),
        store: store.clone(),
        coordinator: coordinator.clone(),
        visibility,
        audit: audit.clone(),
        clock: clock.clone(),
        rights,
    };
    let mut config = EngineConfig::new(source.clone());
    config.policy = policy;
    config.park = park;
    let engine = StreamEngine::new(seams, config);
    Fixture {
        engine,
        feed,
        store,
        audit,
        clock,
        coordinator,
        source,
    }
}

/// A second engine over the same feed, store, coordinator and clock: a "restart".
pub fn rebuild(
    fixture: &Fixture,
    policy: spot_event_stream::BatchPolicy,
) -> StreamEngine<FakeFeed> {
    let seams = Seams {
        feed: fixture.feed.clone(),
        head: Some(fixture.feed.clone()),
        store: fixture.store.clone(),
        coordinator: fixture.coordinator.clone(),
        visibility: Arc::new(spot_event_stream::AllVisible),
        audit: fixture.audit.clone(),
        clock: fixture.clock.clone(),
        rights: Arc::new(AllowAllStreamRights),
    };
    let mut config = EngineConfig::new(fixture.source.clone());
    config.policy = policy;
    StreamEngine::new(seams, config)
}
