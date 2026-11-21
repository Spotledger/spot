//! Shared harness: in-memory stores, a manual sleeper, bundle builders and a scripted fake hub
//! that speaks the real wire (`spot_sync_http::wire`), with fault injection.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::Method;
use serde_json::{Value, json};
use spot_core::{Clock, ManualClock};
use spot_events_contract::SourceId;
use spot_sync_closure::{Bundle, BundleKind, BundleRecord, ClosureError, RecordKey, Role};
use spot_sync_http::wire::{self, HelloReply, PushReply};
use spot_sync_http::{
    BoxFuture, BreakerConfig, ClientIdentity, Error as HttpError, HttpRequest, HttpResponse,
    NoJitter, Sleeper, Transport,
};
use spot_sync_node::{
    AppliedRecord, AppliedSet, ApplyRefusal, BundleApply, MemoryParking, NodeCatalog, NodeConfig,
    NodeIntake, ParkingLimits,
};
use spot_sync_node_host::envelope;
use spot_sync_node_host::push::{OutboxEntry, OutboxSource, PushCursor, PushCursorStore};
use spot_sync_node_host::{
    CursorStore, HubLink, Intake, IntakeApplier, LoopParts, LoopSettings, NodeLoop, OutboxSender,
    PushSide, StatusHandle, Worker,
};
use spot_sync_protocol::{Ack, Batch, Hello, Nack, NodeId, ProtocolRange, Record};
use spot_sync_skew::{Incoming, NodeReport, RecordRef, SoftwareVersion};
use time::OffsetDateTime;

pub const TENANT: &str = "acme";
pub const T0: i64 = 1_800_000_000;

pub fn hub_source() -> SourceId {
    SourceId::new("head-office#hub").unwrap()
}

pub fn node_source() -> SourceId {
    SourceId::new("edge-khi#branch_01").unwrap()
}

pub fn node_id() -> NodeId {
    NodeId::new("edge-khi#branch_01").unwrap()
}

// ---------------------------------------------------------------------------------------------
// The receiving side's in-memory parts.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct MemApply {
    pub stored: BTreeMap<(String, RecordKey), u64>,
    pub order: Vec<RecordKey>,
    pub calls: usize,
    pub refuse: bool,
}

/// A shared handle so a test can look at what a worker-owned apply stored.
#[derive(Clone, Default)]
pub struct SharedApply(pub Arc<Mutex<MemApply>>);

impl BundleApply for SharedApply {
    fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.0
            .lock()
            .unwrap()
            .stored
            .get(&(tenant.to_owned(), key.clone()))
            .is_some_and(|v| *v >= min_version)
    }

    fn has_tombstone(&self, _tenant: &str, _key: &RecordKey, _min_version: u64) -> bool {
        false
    }

    fn apply_bundle(&mut self, tenant: &str, bundle: &Bundle) -> Result<AppliedSet, ApplyRefusal> {
        let mut inner = self.0.lock().unwrap();
        inner.calls += 1;
        if inner.refuse {
            return Err(ApplyRefusal {
                reason: "refused".into(),
            });
        }
        let mut records = Vec::new();
        for r in &bundle.records {
            inner
                .stored
                .insert((tenant.to_owned(), r.key.clone()), r.position);
            inner.order.push(r.key.clone());
            records.push(AppliedRecord {
                key: r.key.clone(),
                version: r.position,
            });
        }
        Ok(AppliedSet { records })
    }

    fn delete(
        &mut self,
        _tenant: &str,
        _key: &RecordKey,
        _version: u64,
        _position: u64,
    ) -> Result<(), ApplyRefusal> {
        Ok(())
    }
}

/// References are `image.refs = [[entity, id], ...]`; `image.major` (default 1) feeds triage.
pub struct JsonCatalog;

impl NodeCatalog for JsonCatalog {
    fn references(&self, record: &BundleRecord) -> Result<Vec<RecordKey>, ClosureError> {
        let mut out = Vec::new();
        if let Some(Value::Array(refs)) = record.image.get("refs") {
            for r in refs {
                if let (Some(e), Some(i)) = (
                    r.get(0).and_then(Value::as_str),
                    r.get(1).and_then(Value::as_str),
                ) {
                    out.push(RecordKey::new(e, i));
                }
            }
        }
        Ok(out)
    }

    fn classify(&self, record: &BundleRecord) -> Incoming {
        let major = record
            .image
            .get("major")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .unwrap_or(1);
        Incoming {
            record: RecordRef {
                entity: record.key.entity.clone(),
                id: record.key.id.clone(),
                version: record.position,
            },
            topic: "t".into(),
            major,
            requires_metadata: None,
        }
    }
}

pub fn report() -> NodeReport {
    let mut majors = BTreeMap::new();
    majors.insert("t".to_owned(), BTreeSet::from([1_u32]));
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: majors,
    }
}

/// A cursor store whose value a test can read.
#[derive(Clone, Default)]
pub struct MemCursor(pub Arc<Mutex<u64>>);

impl CursorStore for MemCursor {
    fn load(&mut self) -> spot_sync_node_host::Result<u64> {
        Ok(*self.0.lock().unwrap())
    }

    fn store(&mut self, prefix: u64) -> spot_sync_node_host::Result<()> {
        *self.0.lock().unwrap() = prefix;
        Ok(())
    }
}

pub fn intake(
    apply: SharedApply,
    cursor: MemCursor,
) -> IntakeApplier<SharedApply, MemoryParking, JsonCatalog, MemCursor> {
    let config = NodeConfig {
        hub_source: hub_source(),
        limits: ParkingLimits::default(),
        max_tenants: 4,
    };
    let node = NodeIntake::new(
        config,
        apply,
        MemoryParking::default(),
        JsonCatalog,
        report(),
    )
    .unwrap();
    IntakeApplier::new(node, cursor, TENANT, hub_source())
}

/// A one-record root bundle `entity/id` at `position`, with optional references.
pub fn bundle(entity: &str, id: &str, position: u64, refs: &[(&str, &str)]) -> Bundle {
    let image = json!({ "refs": refs.iter().map(|(e, i)| json!([e, i])).collect::<Vec<_>>() });
    Bundle {
        kind: BundleKind::Root,
        records: vec![BundleRecord {
            role: Role::Root,
            key: RecordKey::new(entity, id),
            position,
            image,
            deleted: false,
        }],
        bytes: 10,
    }
}

/// A bundle whose record needs schema major `major`.
pub fn bundle_major(entity: &str, id: &str, position: u64, major: u64) -> Bundle {
    Bundle {
        kind: BundleKind::Root,
        records: vec![BundleRecord {
            role: Role::Root,
            key: RecordKey::new(entity, id),
            position,
            image: json!({ "major": major }),
            deleted: false,
        }],
        bytes: 10,
    }
}

pub fn served(seq: u64, after: u64, bundle: &Bundle) -> spot_sync_node_host::envelope::Served {
    let record = envelope::encode(seq, TENANT, after, bundle).unwrap();
    envelope::decode(&record).unwrap()
}

// ---------------------------------------------------------------------------------------------
// The push side's in-memory parts.
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct MemOutbox(pub Arc<Mutex<Vec<OutboxEntry>>>);

impl MemOutbox {
    pub fn push(&self, position: u64, name: &str, owned: bool) {
        self.0.lock().unwrap().push(OutboxEntry {
            position,
            name: name.to_owned(),
            version: 1,
            payload: format!("payload-{position}"),
            owned,
        });
    }
}

impl OutboxSource for MemOutbox {
    fn read_after(
        &mut self,
        after: u64,
        max: usize,
    ) -> spot_sync_node_host::Result<Vec<OutboxEntry>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.position > after)
            .take(max)
            .cloned()
            .collect())
    }

    fn queued_after(&mut self, after: u64) -> spot_sync_node_host::Result<u64> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.position > after)
            .count() as u64)
    }
}

#[derive(Clone, Default)]
pub struct MemPushCursor(pub Arc<Mutex<PushCursor>>);

impl PushCursorStore for MemPushCursor {
    fn load(&mut self) -> spot_sync_node_host::Result<PushCursor> {
        Ok(*self.0.lock().unwrap())
    }

    fn store(&mut self, cursor: PushCursor) -> spot_sync_node_host::Result<()> {
        *self.0.lock().unwrap() = cursor;
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Time: a clock and a sleeper that advances it.
// ---------------------------------------------------------------------------------------------

pub fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(T0).unwrap(),
    ))
}

/// A sleeper that moves the manual clock by the slept time and yields, recording each wait.
pub struct ManualSleeper {
    pub clock: Arc<ManualClock>,
    pub waits: Mutex<Vec<Duration>>,
}

impl ManualSleeper {
    pub fn new(clock: Arc<ManualClock>) -> Arc<Self> {
        Arc::new(Self {
            clock,
            waits: Mutex::new(Vec::new()),
        })
    }

    pub fn waits(&self) -> Vec<Duration> {
        self.waits.lock().unwrap().clone()
    }
}

impl Sleeper for ManualSleeper {
    fn sleep(&self, duration: Duration) -> BoxFuture<()> {
        self.waits.lock().unwrap().push(duration);
        self.clock.advance(duration);
        Box::pin(async {
            tokio::task::yield_now().await;
        })
    }
}

// ---------------------------------------------------------------------------------------------
// A scripted hub on the real wire.
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct HubState {
    /// The bundles the hub queued for this node (serve sequence = index + 1).
    pub queue: Vec<Record>,
    /// Every `from_seq` a pull asked for, in order.
    pub pulls: Vec<u64>,
    /// The records the hub holds from the node's pushes, by push sequence.
    pub pushed: BTreeMap<u64, Record>,
    /// How many times each push sequence arrived (a resend shows as more than one).
    pub push_arrivals: BTreeMap<u64, usize>,
    /// Hellos answered.
    pub hellos: usize,
    /// Requests refused while down.
    pub refused: usize,
}

pub struct FakeHub {
    pub state: Mutex<HubState>,
    /// While set, every request fails as unreachable.
    pub down: AtomicBool,
    /// Fail the next `n` pulls (after counting the request).
    pub fail_pulls: AtomicUsize,
    /// Apply the next push, then fail the reply (a cut after the push, before its ack).
    pub cut_push_reply: AtomicUsize,
    /// Fail the next `n` pushes before applying (a cut mid-push).
    pub fail_pushes: AtomicUsize,
    /// A hub that answers the hello with a metadata epoch refusal.
    pub refuse_hello: AtomicBool,
}

impl FakeHub {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(HubState::default()),
            down: AtomicBool::new(false),
            fail_pulls: AtomicUsize::new(0),
            cut_push_reply: AtomicUsize::new(0),
            fail_pushes: AtomicUsize::new(0),
            refuse_hello: AtomicBool::new(false),
        })
    }

    /// Queues a bundle as the next serve sequence; `after` is the sequence below it.
    pub fn queue_bundle(&self, bundle: &Bundle) -> u64 {
        let mut state = self.state.lock().unwrap();
        let seq = state.queue.len() as u64 + 1;
        let record = envelope::encode(seq, TENANT, seq - 1, bundle).unwrap();
        state.queue.push(record);
        seq
    }

    pub fn pushed_through(&self) -> u64 {
        self.state
            .lock()
            .unwrap()
            .pushed
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0)
    }

    fn query_value(query: &str, key: &str) -> Option<u64> {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
            .and_then(|v| v.parse().ok())
    }

    fn handle(&self, request: HttpRequest) -> Result<HttpResponse, HttpError> {
        if self.down.load(Ordering::SeqCst) {
            self.state.lock().unwrap().refused += 1;
            return Err(HttpError::unavailable("the hub is unreachable"));
        }
        let ok = |body: Vec<u8>| HttpResponse {
            status: 200,
            body: body.into(),
        };
        match (request.method.clone(), request.path.as_str()) {
            (Method::POST, "/sync/v1/hello") => {
                if self.refuse_hello.load(Ordering::SeqCst) {
                    return Ok(HttpResponse {
                        status: 409,
                        body: br#"{"error":{"code":"sync_http.metadata_epoch","message":"epochs differ","retryable":false,"request_id":""}}"#
                            .to_vec()
                            .into(),
                    });
                }
                let theirs: Hello = serde_json::from_slice(&request.body).unwrap();
                let hub = Hello::new(
                    NodeId::new("head-office").unwrap(),
                    ProtocolRange::current(),
                    7,
                );
                let protocol = hub.negotiate(&theirs).unwrap();
                self.state.lock().unwrap().hellos += 1;
                Ok(ok(
                    serde_json::to_vec(&HelloReply { protocol, hub }).unwrap()
                ))
            }
            (Method::GET, "/sync/v1/pull") => {
                let from = Self::query_value(&request.query, "from_seq").unwrap();
                let max = Self::query_value(&request.query, "max").unwrap() as usize;
                let mut state = self.state.lock().unwrap();
                state.pulls.push(from);
                if self.fail_pulls.load(Ordering::SeqCst) > 0 {
                    self.fail_pulls.fetch_sub(1, Ordering::SeqCst);
                    return Err(HttpError::unavailable("the connection was cut"));
                }
                let records: Vec<Record> = state
                    .queue
                    .iter()
                    .filter(|r| r.seq >= from)
                    .take(max)
                    .cloned()
                    .collect();
                if records.is_empty() {
                    return Ok(ok(wire::heartbeat_line().to_vec()));
                }
                let batch = Batch::new(hub_source(), records).unwrap();
                Ok(ok(wire::encode_batch(&batch).unwrap().to_vec()))
            }
            (Method::POST, "/sync/v1/push") => {
                if self.fail_pushes.load(Ordering::SeqCst) > 0 {
                    self.fail_pushes.fetch_sub(1, Ordering::SeqCst);
                    return Err(HttpError::unavailable("the connection was cut"));
                }
                let batch = wire::decode_batch(&request.body).unwrap();
                let mut state = self.state.lock().unwrap();
                let through = state.pushed.keys().next_back().copied().unwrap_or(0);
                let reply = if batch.from_seq > through + 1 {
                    PushReply::Nack(Nack::Gap {
                        source: batch.source.clone(),
                        expected: through + 1,
                    })
                } else {
                    for record in &batch.records {
                        *state.push_arrivals.entry(record.seq).or_default() += 1;
                        state
                            .pushed
                            .entry(record.seq)
                            .or_insert_with(|| record.clone());
                    }
                    PushReply::Ack(Ack::new(
                        batch.source.clone(),
                        state.pushed.keys().next_back().copied().unwrap_or(0),
                    ))
                };
                if self.cut_push_reply.load(Ordering::SeqCst) > 0 {
                    self.cut_push_reply.fetch_sub(1, Ordering::SeqCst);
                    return Err(HttpError::unavailable("the reply was cut"));
                }
                Ok(ok(serde_json::to_vec(&reply).unwrap()))
            }
            _ => Ok(HttpResponse {
                status: 404,
                body: Vec::new().into(),
            }),
        }
    }
}

pub struct FakeTransport(pub Arc<FakeHub>);

impl Transport for FakeTransport {
    fn request(&self, request: HttpRequest) -> BoxFuture<spot_sync_http::Result<HttpResponse>> {
        let result = self.0.handle(request);
        Box::pin(async move { result })
    }
}

// ---------------------------------------------------------------------------------------------
// The loop on top of it all.
// ---------------------------------------------------------------------------------------------

pub struct Rig {
    pub hub: Arc<FakeHub>,
    pub clock: Arc<ManualClock>,
    pub sleeper: Arc<ManualSleeper>,
    pub apply: SharedApply,
    pub cursor: MemCursor,
    pub outbox: MemOutbox,
    pub push_cursor: MemPushCursor,
    pub status: StatusHandle,
    pub breaker: BreakerConfig,
}

impl Rig {
    pub fn new() -> Self {
        let clock = clock();
        Self {
            hub: FakeHub::new(),
            sleeper: ManualSleeper::new(Arc::clone(&clock)),
            clock,
            apply: SharedApply::default(),
            cursor: MemCursor::default(),
            outbox: MemOutbox::default(),
            push_cursor: MemPushCursor::default(),
            status: StatusHandle::new("hub-1", spot_sync_node_host::LinkState::Connecting),
            breaker: BreakerConfig::new(1_000, Duration::from_secs(30), 1).unwrap(),
        }
    }

    pub fn settings() -> LoopSettings {
        LoopSettings {
            batch_records: 4,
            offer_records: 8,
            steps_per_round: 8,
            ..LoopSettings::default()
        }
    }

    /// Builds the loop parts. `with_push` wires the node's outbox.
    pub fn parts(&self, settings: LoopSettings, with_push: bool) -> LoopParts {
        let clock_dyn: Arc<dyn Clock> = Arc::clone(&self.clock) as Arc<dyn Clock>;
        let link = HubLink::new(
            Arc::new(FakeTransport(Arc::clone(&self.hub))),
            ClientIdentity::new(node_id()).with_protocol(ProtocolRange::current(), 7),
            Arc::clone(&clock_dyn),
            self.breaker,
            "hub:head-office",
            Duration::from_secs(30),
            settings.batch_records,
        )
        .unwrap();
        let apply = self.apply.clone();
        let cursor = self.cursor.clone();
        let intake: Arc<Worker<Box<dyn Intake>>> = Arc::new(
            Worker::spawn("test-intake", move || {
                Ok(Box::new(intake(apply, cursor)) as Box<dyn Intake>)
            })
            .unwrap(),
        );
        let push = with_push.then(|| {
            let outbox = self.outbox.clone();
            let store = self.push_cursor.clone();
            Arc::new(
                Worker::spawn("test-push", move || {
                    Ok(Box::new(OutboxSender::new(outbox, store, node_source()))
                        as Box<dyn PushSide>)
                })
                .unwrap(),
            )
        });
        LoopParts {
            settings,
            link,
            hub_source: hub_source(),
            intake,
            push,
            clock: clock_dyn,
            jitter: Box::new(NoJitter),
            sleeper: Arc::clone(&self.sleeper) as Arc<dyn Sleeper>,
            status: self.status.clone(),
        }
    }

    /// Runs the loop until `done` is true (polled between yields), then stops it cleanly.
    pub async fn run_until(
        &self,
        parts: LoopParts,
        mut done: impl FnMut() -> bool,
    ) -> spot_sync_node_host::Result<()> {
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let looped = NodeLoop::new(parts).unwrap();
        let handle = tokio::spawn(looped.run(stop_rx));
        for _ in 0..20_000 {
            if done() {
                break;
            }
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let reached = done();
        let _ = stop_tx.send(true);
        let result = handle.await.unwrap();
        assert!(reached, "the loop did not reach the expected state in time");
        result
    }
}
