//! Channel `Export`, third producer: the data subscription of `spot-event-stream`. A consumer
//! with `payload_mode = fields` receives values, so the crate guards it as `Channel::Export`
//! (not `Journal`) by design. Covered: the JSON frames, the SSE frames, resuming after a right
//! was revoked, and the audit records (no value in them).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use common::*;
use spot_coordinator::in_process::InProcessCoordinator;
use spot_coordinator::presence::InMemoryPresence;
use spot_coordinator::types::NodeId;
use spot_core::ManualClock;
use spot_event_stream::{
    AllowAllStreamRights, Batch, BatchPolicy, ConsumerSpec, EngineConfig, FeedHead,
    InMemoryConsumerStore, MapSubjectFacts, ParkPolicy, PayloadMode, PermissionVisibility,
    PrincipalSource, RecordingAudit, Seams, Start, StreamEngine, SubjectFacts, TopicSelector, sse,
};
use spot_events_contract::names::SourceId;
use spot_field_guard::leakcheck::{Corpus, Output, assert_no_leak, find_leaks};
use spot_permissions::model::Principal;
use spot_storage_contract::{
    Change, ChangeFeed, ErrorKind, FeedPosition, Name as StoreName, NodeRef, OutboxEvent,
    OutboxRecord, RecordId as StoreRecordId, StorageError, StorageResult, Version,
};

const BIG: BatchPolicy = BatchPolicy {
    max_events: 1_000,
    max_bytes: 64 * 1024 * 1024,
    window: 1_000,
    page_limit: 1_000,
    max_wait: std::time::Duration::from_secs(25),
};

/// The labels planted in an event payload (the child-row secrets are not part of it).
const IN_PAYLOAD: [&str; 5] = ["card", "lookalike", "in_json", "salary", "annual"];

fn rid(seed: u64) -> StoreRecordId {
    let mut bytes = [0u8; 16];
    bytes[6] = 0x70 | ((seed >> 8) as u8 & 0x0f);
    bytes[7] = seed as u8;
    bytes[8] = 0x80;
    bytes[9] = (seed >> 16) as u8;
    bytes[15] = seed as u8;
    StoreRecordId::from_bytes(bytes).expect("a v7 record id")
}

/// An ordered, in-memory outbox.
#[derive(Default)]
struct Feed {
    records: Mutex<Vec<OutboxRecord>>,
}

impl Feed {
    fn push(&self, record: OutboxRecord) {
        self.records.lock().unwrap().push(record);
    }
}

impl FeedHead for Feed {
    fn head(&self) -> Option<FeedPosition> {
        let records = self.records.lock().unwrap();
        let seq = records.last().map_or(0, |r| r.position.seq());
        Some(FeedPosition::new(0, seq))
    }
}

impl ChangeFeed for Feed {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> StorageResult<Vec<OutboxRecord>> {
        if after.generation() != 0 {
            return Err(StorageError::new(
                ErrorKind::ResyncRequired,
                "the position is from another generation",
            ));
        }
        Ok(self
            .records
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.position.seq() > after.seq())
            .take(limit as usize)
            .cloned()
            .collect())
    }

    async fn read_changes(&self, _after: FeedPosition, _limit: u32) -> StorageResult<Vec<Change>> {
        Ok(Vec::new())
    }
}

/// The payload of an invoice event: the corpus planted in every protected field.
fn payload(corpus: &Corpus) -> serde_json::Value {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    serde_json::json!({
        "number": PLAIN_NUMBER,
        "notes": PLAIN_NOTE,
        "card_number": value("card"),
        "holder": value("lookalike"),
        "profile_json": value("in_json"),
        "salary": value("salary"),
        "bonus": "1500.50",
        "annual_cost": value("annual"),
    })
}

fn record(seq: u64, id: StoreRecordId, data: serde_json::Value) -> OutboxRecord {
    let body = serde_json::json!({
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "leakcheck"},
            "correlation_id": id.to_string(),
        },
        "data": data,
    })
    .to_string();
    OutboxRecord {
        position: FeedPosition::new(0, seq),
        transaction: rid(seq * 7 + 1),
        ordinal: 0,
        event: OutboxEvent {
            id: rid(seq * 13 + 3),
            topic: StoreName::new("doc_updated").unwrap(),
            subject: NodeRef {
                entity: StoreName::new(DOCTYPE).unwrap(),
                id,
            },
            subject_version: Some(Version(seq)),
            schema_version: 1,
            payload: body,
        },
    }
}

/// Owners to principals, changeable while the engine runs (a right revoked).
#[derive(Default)]
struct Principals {
    map: Mutex<HashMap<String, Principal>>,
}

impl Principals {
    fn set(&self, owner: &str, principal: Principal) {
        self.map.lock().unwrap().insert(owner.to_owned(), principal);
    }
}

impl PrincipalSource for Principals {
    fn principal(&self, owner: &str) -> Principal {
        self.map
            .lock()
            .unwrap()
            .get(owner)
            .cloned()
            .unwrap_or_else(|| Principal::user(owner, Vec::new()))
    }
}

struct Rig {
    engine: StreamEngine<Feed>,
    feed: Arc<Feed>,
    audit: Arc<RecordingAudit>,
    principals: Arc<Principals>,
    ids: Vec<StoreRecordId>,
}

async fn rig(docs: u64) -> Rig {
    let core = core();
    let ids: Vec<StoreRecordId> = (1..=docs).map(rid).collect();
    let mut facts = MapSubjectFacts::new();
    for id in &ids {
        facts = facts.with(DOCTYPE, id, doc_facts());
    }
    let principals = Arc::new(Principals::default());
    for name in PRINCIPALS {
        principals.set(name, principal(name));
    }
    let facts: Arc<dyn SubjectFacts> = Arc::new(facts);
    let source_of_principals: Arc<dyn PrincipalSource> = principals.clone();
    let visibility = Arc::new(PermissionVisibility::new(
        core.engine.clone(),
        core.guard.clone(),
        facts,
        source_of_principals,
    ));
    let feed = Arc::new(Feed::default());
    let audit = Arc::new(RecordingAudit::new());
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    let coordinator: Arc<dyn spot_coordinator::Coordinator> = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("leakcheck-node").unwrap(),
        clock.clone(),
    )
    .await
    .unwrap();
    let source = SourceId::new("acme.spotledger.app").unwrap();
    let seams = Seams {
        feed: feed.clone(),
        head: Some(feed.clone()),
        store: Arc::new(InMemoryConsumerStore::new()),
        coordinator,
        visibility,
        audit: audit.clone(),
        clock,
        rights: Arc::new(AllowAllStreamRights),
    };
    let mut config = EngineConfig::new(source);
    config.policy = BIG;
    config.park = ParkPolicy::default();
    Rig {
        engine: StreamEngine::new(seams, config),
        feed,
        audit,
        principals,
        ids,
    }
}

fn spec(name: &str, owner: &str) -> ConsumerSpec {
    ConsumerSpec {
        name: name.to_owned(),
        owner: owner.to_owned(),
        topics: vec![TopicSelector::Prefix("doc".to_owned())],
        filter: None,
        partitions: 1,
        classes: None,
        entities: None,
        payload_mode: PayloadMode::Fields,
        start: Start::FromBeginning,
    }
}

/// The batch as it leaves the engine: one JSON frame per event and one SSE frame per event.
fn frames(consumer: &str, batch: &Batch) -> Vec<Output> {
    let mut out = Vec::new();
    for event in &batch.events {
        out.push(Output::Json(serde_json::to_string(event).unwrap()));
        out.push(Output::Bytes(sse::frame(consumer, event)));
    }
    out
}

fn audit_text(rig: &Rig) -> Output {
    Output::Text(format!("{:?}", rig.audit.records()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_data_subscription_never_leaks() {
    let corpus = corpus();
    for owner in PRINCIPALS {
        let rig = rig(3).await;
        for (index, id) in rig.ids.iter().enumerate() {
            rig.feed
                .push(record(index as u64 + 1, *id, payload(&corpus)));
        }
        let consumer = format!("sub_{owner}");
        rig.engine
            .register(spec(&consumer, owner), &Principal::system())
            .await
            .unwrap();
        let lease = rig.engine.acquire(&consumer).await.unwrap();

        let mut out: Vec<Output> = Vec::new();
        match rig.engine.pull(&lease, 0, None, false).await {
            Ok(batch) => {
                out.extend(frames(&consumer, &batch));
                out.push(Output::Text(format!("skipped={}", batch.skipped)));
                if matches!(owner, "analyst" | "admin") {
                    // The positive control: a principal who may report the fields gets the
                    // values, in the JSON and in the SSE frames, so clean is not empty.
                    assert_eq!(batch.events.len(), 3, "{owner}: every event arrives");
                    for encoding in [&out[0], &out[1]] {
                        let found: Vec<String> =
                            find_leaks(&corpus.only(&IN_PAYLOAD), std::slice::from_ref(encoding))
                                .into_iter()
                                .map(|l| l.secret)
                                .collect();
                        for label in IN_PAYLOAD {
                            assert!(
                                found.iter().any(|s| s == label),
                                "{owner}: `{label}` should be delivered"
                            );
                        }
                    }
                }
                if let Some(next) = batch.next_position {
                    rig.engine.ack(&lease, 0, next).await.unwrap();
                }
            }
            Err(e) => out.push(Output::Text(e.to_string())),
        }
        out.push(audit_text(&rig));

        let seen = visible_labels(owner, true);
        assert_no_leak(&corpus.without(&seen), &out);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resuming_after_a_right_is_revoked_delivers_nothing_the_owner_may_no_longer_see() {
    let corpus = corpus();
    for revoked_to in [
        Principal::with_roles("analyst", &[]),
        Principal::with_roles("analyst", &["MaskedReader"]),
    ] {
        let rig = rig(4).await;
        rig.feed.push(record(1, rig.ids[0], payload(&corpus)));
        rig.feed.push(record(2, rig.ids[1], payload(&corpus)));
        rig.engine
            .register(spec("resumer", "analyst"), &Principal::system())
            .await
            .unwrap();
        rig.principals.set("analyst", principal("analyst"));
        let lease = rig.engine.acquire("resumer").await.unwrap();

        // Before the revocation the analyst receives the values and acknowledges them.
        let first = rig.engine.pull(&lease, 0, None, false).await.unwrap();
        let first_frames = frames("resumer", &first);
        assert!(
            !find_leaks(&corpus.only(&IN_PAYLOAD), &first_frames).is_empty(),
            "the positive control: the values were delivered before the revocation"
        );
        rig.engine
            .ack(&lease, 0, first.next_position.unwrap())
            .await
            .unwrap();

        // The right is revoked; new events arrive; the consumer reconnects and resumes from its
        // acknowledged position, with the position token of its last frame.
        rig.principals.set("analyst", revoked_to.clone());
        rig.feed.push(record(3, rig.ids[2], payload(&corpus)));
        rig.feed.push(record(4, rig.ids[3], payload(&corpus)));
        rig.engine.release(&lease).await.unwrap();
        let token = match first_frames.get(1) {
            Some(Output::Bytes(bytes)) => String::from_utf8_lossy(bytes)
                .lines()
                .find_map(|l| l.strip_prefix("id: ").map(str::to_owned))
                .unwrap(),
            other => panic!("expected an SSE frame, got {other:?}"),
        };
        sse::parse_last_event_id("resumer", &token).expect("the token still resumes");
        let lease = rig.engine.acquire("resumer").await.unwrap();

        let mut out: Vec<Output> = Vec::new();
        match rig.engine.pull(&lease, 0, None, false).await {
            Ok(batch) => {
                out.extend(frames("resumer", &batch));
                out.push(Output::Text(format!(
                    "skipped={} events={}",
                    batch.skipped,
                    batch.events.len()
                )));
            }
            Err(e) => out.push(Output::Text(e.to_string())),
        }
        out.push(audit_text(&rig));
        // Nothing planted may appear, unmasked or otherwise; the analyst's earlier frames are
        // not part of what the revoked owner receives now.
        assert_no_leak(&corpus, &out);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nested_child_row_inside_an_event_payload_is_guarded() {
    // A payload may carry child rows (`items`). The child DocType's protected `iban` must not
    // reach an owner without the `bank` right. If a producer leaks here, mark this test
    // `#[ignore = "leaks: WP-SEC-04.T11"]` and list it first in the PR description.
    let corpus = corpus();
    let rig = rig(1).await;
    let mut data = payload(&corpus);
    data["items"] = serde_json::json!([{
        "qty": "2.000",
        "iban": corpus.get("iban").unwrap().value(),
        "memo": corpus.get("split_rows").unwrap().planted[0],
    }]);
    rig.feed.push(record(1, rig.ids[0], data));
    rig.engine
        .register(spec("nested", "masked_reader"), &Principal::system())
        .await
        .unwrap();
    let lease = rig.engine.acquire("nested").await.unwrap();
    let mut out = Vec::new();
    match rig.engine.pull(&lease, 0, None, false).await {
        Ok(batch) => out.extend(frames("nested", &batch)),
        Err(e) => out.push(Output::Text(e.to_string())),
    }
    assert_no_leak(&corpus.only(&["iban"]), &out);
}

/// The negative control: a stub that frames the stored event as it is, without the visibility
/// projection, is caught, and the failure names the label and the encoding, never the value.
#[test]
fn an_unprojected_event_frame_fails_the_harness_without_printing_the_value() {
    let corpus = corpus();
    let frame = format!(
        "id: x\nevent: doc_updated\ndata: {}\n\n",
        serde_json::json!({"data": payload(&corpus)})
    );
    let result = std::panic::catch_unwind(|| {
        assert_no_leak(&corpus_for_stub(), &[Output::Bytes(frame.into_bytes())]);
    });
    let payload = result.expect_err("the unprojected frame must fail the harness");
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_owned()))
        .unwrap_or_default();
    assert!(message.contains("card"), "{message}");
    assert!(message.contains("bytes"), "{message}");
    for secret in &corpus.secrets {
        for part in &secret.planted {
            assert!(
                !message.contains(part.as_str()),
                "the failure printed a value"
            );
        }
    }
}

fn corpus_for_stub() -> Corpus {
    corpus()
}
