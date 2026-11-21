//! Shared fixtures for the bridge suites (WP-PLT-08.T3): a deterministic fake
//! [`ChangeFeed`] with injectable gaps and read failures, the fake position store, the
//! telemetry recorder, the pump builder, and the permission fixture the authorizer
//! decides through (loaded through the public TOML loader over a real `EffectiveMeta`).
//!
//! Suites declare `mod support;` privately; `dead_code` is allowed at the declaration
//! because a suite uses only the part of the support tree its name implies.

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{HashMap, VecDeque};
use std::future::{Future, ready};
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_core::SiteId;
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::model::{ModelTexts, Principal as PermPrincipal};
use spot_permissions::{DocFacts, PermissionEngine, Snapshot, load};
use spot_realtime::{
    BusConfig, Closed, Event, Principal, RealtimeBus, RoomAuthorizer, Subscription,
};
use spot_storage_contract::{
    Change, ChangeFeed, ChangeOp, ErrorKind, FeedPosition, Name, NodeRef, OutboxEvent,
    OutboxRecord, RecordId, StorageError, Version,
};
use time::OffsetDateTime;

use spot_realtime_bridge::pump::{
    FeedHead, MappingOutcome, PositionStore, PumpConfig, PumpPosition, PumpTelemetry,
};
use spot_realtime_bridge::{EngineSource, FeedPump};

// ---------------------------------------------------------------------------
// Test-only unwrapping that names the failure.

pub trait Must<T> {
    fn must(self) -> T;
}

impl<T, E: std::fmt::Debug> Must<T> for Result<T, E> {
    fn must(self) -> T {
        self.unwrap_or_else(|error| panic!("unexpected error: {error:?}"))
    }
}

// ---------------------------------------------------------------------------
// Names, ids, sites

pub fn site(name: &str) -> SiteId {
    SiteId::new(name).must()
}

/// A deterministic UUIDv7-shaped record id, `seq` visible in the id's counter bytes.
pub fn stored_id(seq: u32) -> RecordId {
    let unix_ms: i64 = 1_790_000_000 * 1000;
    let mut b = [0u8; 16];
    b[0] = (unix_ms >> 40) as u8;
    b[1] = (unix_ms >> 32) as u8;
    b[2] = (unix_ms >> 24) as u8;
    b[3] = (unix_ms >> 16) as u8;
    b[4] = (unix_ms >> 8) as u8;
    b[5] = unix_ms as u8;
    b[6] = 0x70 | (0x0f & ((seq >> 8) as u8));
    b[7] = seq as u8;
    b[8] = 0x80;
    b[15] = seq as u8;
    RecordId::from_bytes(b)
        .ok()
        .unwrap_or_else(RecordId::new_v7)
}

pub fn name(text: &str) -> Name {
    Name::new(text).must()
}

pub fn node(doctype: &str, seq: u32) -> NodeRef {
    NodeRef {
        entity: name(doctype),
        id: stored_id(seq),
    }
}

/// The text of the user ids the identity notifications name (room names `user:<id>`).
pub fn user_text(seq: u32) -> String {
    stored_id(seq).to_string()
}

/// The stored-id **text** a document with feed-local id `seq` maps to (the room-name
/// string in `doc:<doctype>:<text>`); documents and users share the stored-id encoding.
pub fn doc_id_text(seq: u32) -> String {
    stored_id(seq).to_string()
}

// ---------------------------------------------------------------------------
// The fake feed: committed entries behind retention watermarks, injectable failures.

/// Which log of the fake feed an operation addresses.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Log {
    Changes,
    Outbox,
}

/// A fake [`ChangeFeed`] over scripted committed entries, with the gap vocabulary of the
/// contract: a stored position from another generation, or below the retention watermark,
/// is refused with `ResyncRequired`; `START` always reads (the retained range only); a
/// read may fail once with a non-position error. Entries are immutable once pushed.
pub struct FakeFeed {
    changes: Mutex<ChangesState>,
    outbox: Mutex<OutboxState>,
}

struct ChangesState {
    entries: Vec<Change>,
    retained_from: u64,
    generation: u64,
    fail_once: Option<&'static str>,
}

struct OutboxState {
    entries: Vec<OutboxRecord>,
    retained_from: u64,
    generation: u64,
    fail_once: Option<&'static str>,
}

/// Whether position `after` may be continued on a log in `generation` retaining from
/// `retained_from`: the same generation, and the position is at or above the watermark
/// (`evict_through(n)` sweeps positions below `n`, so a reader parked at `n` or later
/// missed nothing) — or `START`, which is always acceptable and reads the retained range.
fn continuable(after: FeedPosition, generation: u64, retained_from: u64) -> bool {
    (after.seq() == 0 && after.generation() == 0)
        || (after.generation() == generation && after.seq() >= retained_from)
}

fn resync_error() -> StorageError {
    StorageError::new(
        ErrorKind::ResyncRequired,
        "the stored position is no longer continuable on this log",
    )
}

fn failed_error(message: &str) -> StorageError {
    StorageError::new(ErrorKind::InvalidValue, message.to_owned())
}

impl ChangesState {
    fn page(&self, after: FeedPosition, limit: u32) -> Vec<Change> {
        let floor = after.seq().max(self.retained_from.saturating_sub(1));
        self.entries
            .iter()
            .filter(|change| change.position.seq() > floor)
            .take(limit as usize)
            .cloned()
            .collect()
    }
}

impl OutboxState {
    fn page(&self, after: FeedPosition, limit: u32) -> Vec<OutboxRecord> {
        let floor = after.seq().max(self.retained_from.saturating_sub(1));
        self.entries
            .iter()
            .filter(|record| record.position.seq() > floor)
            .take(limit as usize)
            .cloned()
            .collect()
    }
}

impl Default for FakeFeed {
    fn default() -> Self {
        Self::new()
    }
}

/// Unwraps a possibly-poisoned lock guard; the fakes hold plain test data a panic
/// elsewhere cannot invalidate.
pub fn guard<'a, T>(mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

impl FakeFeed {
    pub fn new() -> Self {
        Self {
            changes: Mutex::new(ChangesState {
                entries: Vec::new(),
                retained_from: 0,
                generation: 1,
                fail_once: None,
            }),
            outbox: Mutex::new(OutboxState {
                entries: Vec::new(),
                retained_from: 0,
                generation: 1,
                fail_once: None,
            }),
        }
    }

    /// Appends one change-feed entry; its position becomes `(generation, seq)`.
    pub fn push_change(&self, seq: u64, doctype: &str, id: u32, op: ChangeOp, version: u64) {
        let mut st = guard(&self.changes);
        let generation = st.generation;
        st.entries.push(Change {
            position: FeedPosition::new(generation, seq),
            transaction: stored_id(600 + u32::try_from(seq).unwrap_or(u32::MAX)),
            document: node(doctype, id),
            op,
            version: Version(version),
        });
    }

    /// Appends one outbox record; its position becomes `(generation, seq)`.
    pub fn push_record(
        &self,
        seq: u64,
        topic: &str,
        subject_doctype: &str,
        subject_id: u32,
        subject_version: Option<u64>,
        payload: &str,
    ) {
        let mut st = guard(&self.outbox);
        let generation = st.generation;
        st.entries.push(OutboxRecord {
            position: FeedPosition::new(generation, seq),
            transaction: stored_id(700 + u32::try_from(seq).unwrap_or(u32::MAX)),
            ordinal: 0,
            event: OutboxEvent {
                id: stored_id(800 + u32::try_from(seq).unwrap_or(u32::MAX)),
                topic: name(topic),
                subject: node(subject_doctype, subject_id),
                subject_version: subject_version.map(Version),
                schema_version: 1,
                payload: payload.to_owned(),
            },
        });
    }

    /// Makes the log refuse every stored position below `seq`: the entries and positions
    /// below it are gone (retention moved). A read from `START` still returns the
    /// retained range — what a pump after a resync redelivers.
    pub fn evict_through(&self, log: Log, seq: u64) {
        match log {
            Log::Changes => guard(&self.changes).retained_from = seq,
            Log::Outbox => guard(&self.outbox).retained_from = seq,
        }
    }

    /// Starts a new stream generation on the log: later entries are tagged with it and
    /// positions from the old generation are refused. Returns the new generation.
    pub fn bump_generation(&self, log: Log) -> u64 {
        match log {
            Log::Changes => {
                let mut st = guard(&self.changes);
                st.generation += 1;
                st.retained_from = 0;
                st.generation
            }
            Log::Outbox => {
                let mut st = guard(&self.outbox);
                st.generation += 1;
                st.retained_from = 0;
                st.generation
            }
        }
    }

    /// The log's next read fails with a non-position storage error, once.
    pub fn fail_next_read(&self, log: Log, message: &'static str) {
        match log {
            Log::Changes => guard(&self.changes).fail_once = Some(message),
            Log::Outbox => guard(&self.outbox).fail_once = Some(message),
        }
    }
}

impl ChangeFeed for FakeFeed {
    async fn read_outbox(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> Result<Vec<OutboxRecord>, StorageError> {
        let mut st = guard(&self.outbox);
        if let Some(message) = st.fail_once.take() {
            return Err(failed_error(message));
        }
        if !continuable(after, st.generation, st.retained_from) {
            return Err(resync_error());
        }
        Ok(st.page(after, limit))
    }

    async fn read_changes(
        &self,
        after: FeedPosition,
        limit: u32,
    ) -> Result<Vec<Change>, StorageError> {
        let mut st = guard(&self.changes);
        if let Some(message) = st.fail_once.take() {
            return Err(failed_error(message));
        }
        if !continuable(after, st.generation, st.retained_from) {
            return Err(resync_error());
        }
        Ok(st.page(after, limit))
    }
}

impl FeedHead for FakeFeed {
    fn head_changes(&self) -> Pin<Box<dyn Future<Output = Option<FeedPosition>> + Send + '_>> {
        let head = guard(&self.changes).entries.last().map(|c| c.position);
        Box::pin(ready(head))
    }

    fn head_outbox(&self) -> Pin<Box<dyn Future<Output = Option<FeedPosition>> + Send + '_>> {
        let head = guard(&self.outbox).entries.last().map(|r| r.position);
        Box::pin(ready(head))
    }
}

// ---------------------------------------------------------------------------
// The fake position store: one saved `PumpPosition`, injectable load/save failures.

pub struct FakeStore {
    saved: Mutex<Option<PumpPosition>>,
    fail_load: Mutex<bool>,
    fail_save: Mutex<bool>,
    saves: AtomicU32,
}

impl Default for FakeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeStore {
    pub fn new() -> Self {
        Self {
            saved: Mutex::new(None),
            fail_load: Mutex::new(false),
            fail_save: Mutex::new(false),
            saves: AtomicU32::new(0),
        }
    }

    /// The next load fails, once.
    pub fn fail_next_load(&self) {
        *guard(&self.fail_load) = true;
    }

    /// The next save fails, once.
    pub fn fail_next_save(&self) {
        *guard(&self.fail_save) = true;
    }

    /// How many saves succeeded so far.
    pub fn saves(&self) -> u32 {
        self.saves.load(Ordering::SeqCst)
    }

    /// The saved position, if any save succeeded.
    pub fn saved(&self) -> Option<PumpPosition> {
        guard(&self.saved).clone()
    }
}

impl PositionStore for FakeStore {
    fn load(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Option<PumpPosition>, String>> + Send + '_>> {
        let failing = std::mem::take(&mut *guard(&self.fail_load));
        if failing {
            return Box::pin(ready(Err(
                "load failed: the store is unreachable".to_owned()
            )));
        }
        let saved = guard(&self.saved).clone();
        Box::pin(ready(Ok(saved)))
    }

    fn save<'a>(
        &'a self,
        position: &'a PumpPosition,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let failing = std::mem::take(&mut *guard(&self.fail_save));
        if failing {
            return Box::pin(ready(Err("save failed: the store is read-only".to_owned())));
        }
        *guard(&self.saved) = Some(position.clone());
        self.saves.fetch_add(1, Ordering::SeqCst);
        Box::pin(ready(Ok(())))
    }
}

// ---------------------------------------------------------------------------
// The telemetry recorder.

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum TelemetryRecord {
    Run(MappingOutcome),
    Lag(&'static str, u64),
    Resync(&'static str),
}

pub struct FakeTelemetry {
    pub records: Mutex<VecDeque<TelemetryRecord>>,
}

impl Default for FakeTelemetry {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeTelemetry {
    pub fn new() -> Self {
        Self {
            records: Mutex::new(VecDeque::new()),
        }
    }
}

impl PumpTelemetry for FakeTelemetry {
    fn record_run(&self, outcome: &MappingOutcome) {
        guard(&self.records).push_back(TelemetryRecord::Run(*outcome));
    }

    fn record_lag(&self, log: &'static str, entries_behind: u64) {
        guard(&self.records).push_back(TelemetryRecord::Lag(log, entries_behind));
    }

    fn record_resync(&self, log: &'static str) {
        guard(&self.records).push_back(TelemetryRecord::Resync(log));
    }
}

// ---------------------------------------------------------------------------
// The pump builder and the event-capture helpers.

/// A pump over the fakes with the real bus. `with_head` wires lag probing through the
/// same fake feed (its head is the last entry's position).
pub fn pump(
    site_id: &SiteId,
    feed: &Arc<FakeFeed>,
    bus: &RealtimeBus,
    store: &Arc<FakeStore>,
    telemetry: &Arc<FakeTelemetry>,
    page_limit: u32,
    with_head: bool,
) -> FeedPump<FakeFeed, FakeTelemetry> {
    FeedPump::new(
        site_id.clone(),
        Arc::clone(feed),
        bus.clone(),
        Arc::clone(store) as Arc<dyn PositionStore>,
        if with_head {
            Some(Arc::clone(feed) as Arc<dyn FeedHead>)
        } else {
            None
        },
        Arc::clone(telemetry),
        PumpConfig {
            page_limit,
            ..PumpConfig::default()
        },
    )
}

/// A bus with a manual clock (nothing ages during a test) and `capacity`.
pub fn bus_with(capacity: usize, authorizer: Arc<dyn RoomAuthorizer>) -> RealtimeBus {
    RealtimeBus::new(
        BusConfig {
            queue_capacity: capacity,
            ..BusConfig::default()
        },
        Arc::new(spot_core::ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
        authorizer,
    )
}

/// Non-blocking receive: the first already-buffered result, or `None` when nothing is
/// buffered yet.
pub async fn rx_now(sub: &mut Subscription) -> Option<Result<Event, Closed>> {
    tokio::time::timeout(Duration::ZERO, sub.recv()).await.ok()
}

/// Drains every buffered event of a **live** subscription; stops the moment the
/// subscription reports its close (a closed slow consumer keeps answering with the close
/// reason, which would loop forever on a `while rx_now(…).is_some()` drain).
pub async fn drain(sub: &mut Subscription) -> usize {
    let mut count = 0;
    while let Some(result) = rx_now(sub).await {
        if result.is_err() {
            break;
        }
        count += 1;
    }
    count
}

/// Blocking receive with a safety timeout, failing the test instead of hanging CI.
pub async fn next_event(sub: &mut Subscription) -> Event {
    tokio::time::timeout(Duration::from_secs(5), sub.recv())
        .await
        .unwrap_or_else(|_| panic!("recv timed out"))
        .unwrap_or_else(|closed| panic!("subscription closed unexpectedly: {closed}"))
}

/// A wire principal subscribed for captures (the pump tests authorize with `AllowAll`).
pub fn watcher(bus: &RealtimeBus, site_id: &SiteId, name: &str) -> Subscription {
    bus.subscribe(site_id.clone(), Principal::user(name))
}

// ---------------------------------------------------------------------------
// Outbox payload documents: the exact `{meta, data}` shape the events contract pins.

pub fn payload_with_data(data_json: &str) -> String {
    format!(
        r#"{{"meta":{{"occurred_at":"2026-01-01T00:00:00Z","actor":{{"kind":"system","component":"identity"}},"correlation_id":"01926f0a-0000-7000-8000-000000009004"}},"data":{data_json}}}"#
    )
}

pub fn identity_payload(user_id_text: &str) -> String {
    payload_with_data(&format!(r#"{{"user":"{user_id_text}"}}"#))
}

pub fn changed_fields_payload(names: &[&str]) -> String {
    let list: Vec<String> = names.iter().map(|n| format!("\"{n}\"")).collect();
    payload_with_data(&format!(r#"{{"changed_fields":[{}]}}"#, list.join(",")))
}

pub fn rename_payload(old: &str, new: &str) -> String {
    payload_with_data(&format!(r#"{{"old_name":"{old}","new_name":"{new}"}}"#))
}

pub fn empty_data_payload() -> String {
    payload_with_data("{}")
}

// ---------------------------------------------------------------------------
// The permission fixture (authorizer suites): a real EffectiveMeta, the grants loaded
// through the public TOML loader, and the seams the authorizer decides through.

fn fixture_definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    customer.fields = vec![FieldSpec::new("customer_name", FieldType::Text)];

    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut customer_link = FieldSpec::new("customer", FieldType::Link);
    customer_link.required = true;
    customer_link.options = Some("customer".into());
    invoice.fields = vec![FieldSpec::new("number", FieldType::Text), customer_link];

    let mut project = Definition::new("project");
    project.fields = vec![FieldSpec::new("project_name", FieldType::Text)];

    let mut item_master = Definition::new("item_master");
    item_master.fields = vec![FieldSpec::new("item_name", FieldType::Text)];

    vec![customer, invoice, project, item_master]
}

pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(fixture_definitions()).must().effective()
}

/// Snapshot A, the standing world: `Reader` reads every fixture DocType; `Viewer` reads
/// `project`; `Manager` does everything on `project`; `OwnerRole` reads its own
/// `sales_invoice`.
pub const GRANTS_A: &str = r#"
[[entries]]
role = "Reader"
doctype = "sales_invoice"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "customer"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "item_master"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "project"
rights = ["read"]
[[entries]]
role = "Viewer"
doctype = "project"
rights = ["read"]
[[entries]]
role = "Manager"
doctype = "project"
rights = ["read", "write", "create", "delete", "submit", "cancel"]
[[entries]]
role = "OwnerRole"
doctype = "sales_invoice"
rights = ["read"]
if_owner = true
"#;

/// Snapshot B, after the permission swap: only `Viewer`/`project` survives.
pub const GRANTS_B: &str = r#"
[[entries]]
role = "Viewer"
doctype = "project"
rights = ["read"]
"#;

pub const NO_GROUPS: &str = "";
pub const NO_FIELD_GRANTS: &str = "";

pub fn snapshot(grants: &str, shares: &str) -> Snapshot {
    let model = load(&ModelTexts {
        grants: grants.to_owned(),
        field_grants: NO_FIELD_GRANTS.to_owned(),
        groups: NO_GROUPS.to_owned(),
        shares: shares.to_owned(),
    })
    .must();
    Snapshot::build(&meta(), &model).must()
}

/// The canonical document id texts the fixtures share: the posted sales invoice `erin`
/// created and `frank` holds a share on, and a project document.
pub fn doc_a() -> String {
    stored_id(501).to_string()
}

pub fn doc_p1() -> String {
    stored_id(502).to_string()
}

pub fn shares_for_frank() -> String {
    format!(
        r#"
[[entries]]
doc = "{}"
user = "frank"
rights = ["read"]
"#,
        doc_a()
    )
}

/// The six principals' permission worlds under snapshot A: alice reads everywhere, bob
/// only `project`, carol nothing, dave manages `project`, erin owns her invoice, frank
/// nothing (his share lives in the snapshot, not in his assignments).
pub fn principal_worlds() -> HashMap<String, PermPrincipal> {
    HashMap::from([
        (
            "alice".to_owned(),
            PermPrincipal::with_roles("alice", &["Reader"]),
        ),
        (
            "bob".to_owned(),
            PermPrincipal::with_roles("bob", &["Viewer"]),
        ),
        ("carol".to_owned(), PermPrincipal::user("carol", Vec::new())),
        (
            "dave".to_owned(),
            PermPrincipal::with_roles("dave", &["Manager"]),
        ),
        (
            "erin".to_owned(),
            PermPrincipal::with_roles("erin", &["OwnerRole"]),
        ),
        ("frank".to_owned(), PermPrincipal::user("frank", Vec::new())),
    ])
}

/// The committed document facts the `doc:` room decisions ask for: `doc_a` is the posted
/// sales invoice `erin` created; `doc_p1` a project document. Anything else is `None`
/// (the document the room names does not exist, or its facts cannot be loaded).
pub fn fact_sheet() -> HashMap<(String, String), DocFacts> {
    HashMap::from([
        (
            ("sales_invoice".to_owned(), doc_a()),
            DocFacts {
                doctype: "sales_invoice".to_owned(),
                id: Some(doc_a()),
                created_by: Some("erin".to_owned()),
                state: Some("posted".to_owned()),
                ..DocFacts::default()
            },
        ),
        (
            ("project".to_owned(), doc_p1()),
            DocFacts {
                doctype: "project".to_owned(),
                id: Some(doc_p1()),
                ..DocFacts::default()
            },
        ),
    ])
}

/// The engine seam, swap-capable: `swap` installs the engine of a new snapshot — the
/// host's move on every permission snapshot publication.
pub struct LiveEngine(Mutex<PermissionEngine>);

impl LiveEngine {
    pub fn new(engine: PermissionEngine) -> Self {
        Self(Mutex::new(engine))
    }

    pub fn swap(&self, engine: PermissionEngine) {
        *guard(&self.0) = engine;
    }
}

impl EngineSource for LiveEngine {
    fn current(&self) -> PermissionEngine {
        guard(&self.0).clone()
    }
}

/// The principal seam: wire id → permission world; `forget` makes a user unknown (the
/// disabled-account / lookup-failed case).
pub struct MapPrincipals(Mutex<HashMap<String, PermPrincipal>>);

impl MapPrincipals {
    pub fn new(worlds: HashMap<String, PermPrincipal>) -> Self {
        Self(Mutex::new(worlds))
    }

    pub fn forget(&self, id: &str) {
        guard(&self.0).remove(id);
    }

    pub fn restore(&self, id: &str, world: PermPrincipal) {
        guard(&self.0).insert(id.to_owned(), world);
    }
}

impl spot_realtime_bridge::PrincipalSource for MapPrincipals {
    fn principal<'a>(
        &'a self,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<PermPrincipal>> + Send + 'a>> {
        let found = guard(&self.0).get(id).cloned();
        Box::pin(ready(found))
    }
}

/// The facts seam: `(doctype, id)` → `DocFacts`; `None` when the document is unknown.
pub struct MapFacts(Mutex<HashMap<(String, String), DocFacts>>);

impl MapFacts {
    pub fn new(facts: HashMap<(String, String), DocFacts>) -> Self {
        Self(Mutex::new(facts))
    }
}

impl spot_realtime_bridge::DocFactsSource for MapFacts {
    fn facts<'a>(
        &'a self,
        doctype: &'a str,
        id: &'a str,
    ) -> Pin<Box<dyn Future<Output = Option<DocFacts>> + Send + 'a>> {
        let found = guard(&self.0)
            .get(&(doctype.to_owned(), id.to_owned()))
            .cloned();
        Box::pin(ready(found))
    }
}
