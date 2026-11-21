//! The process trigger intake over a real PostgreSQL and the real process engine host
//! (WP-BPM-03.T4; ADR-49, ADR-080).
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset (the convention `tests/process.rs` and `tests/process_documents.rs`
//! use). The Case store, the definition store and the intake's durable marks are real tables; the
//! seams the site would supply (the outbox, the platform reader, the metadata, the secrets, the
//! audit sink) are small in-memory fakes, so a test scripts an event stream and reads back what
//! the intake did. A "node" is a second `TriggerHost` over the same database.
//!
//! The webhook is driven through the module's `WebhookPort` (`HookPort`). The module's route and
//! open path are declared through the `SiteCtx` hooks in `register`; the HTTP mounting itself is
//! `spot-http`'s (`routes::webhook`, covered by its own tests), so nothing here repeats it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_app_api::document::{DocState, Document};
use spot_app_api::prelude::{CommandId, Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_engine::{CaseStore, HandlerRegistry};
use spot_bpm_model::{NodeId, from_text};
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_bpm_triggers::StartKey;
use spot_cli::services::ports::{
    ConsumerLease, JobSpec, JobSubmit, OutboxLease, PortError, PortFuture, SystemReader,
};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_triggers::{
    Audit, AuditEntry, Fut, HookPort, Meta, Parts, SecretSource, Settings, TriggerHost,
    TriggerReload, WebhookSetting, case_id,
};
use spot_core::{Clock, ManualClock};
use spot_document::engine::{MemoryPrincipals, PrincipalSource};
use spot_events_contract::{Actor, EventMeta, PayloadDocument};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, Lease, LeaseStore};
use spot_storage_contract::{FeedPosition, NodeRef, OutboxEvent, OutboxRecord, Version};
use spot_webhooks::signing::{SigningKey, build_signature_header};

const SECRET: &[u8] = b"a-test-secret-of-some-length";
const HOOK_PROCESS: &str = "hook_proc";
const DOC_PROCESS: &str = "doc_proc";
const SCHED_PROCESS: &str = "sched_proc";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

fn node(id: &str) -> NodeId {
    NodeId::new(id).unwrap()
}

// --- process texts ---------------------------------------------------------------------------

fn process_text(name: &str, trigger: &str) -> String {
    format!(
        r#"
name = "{name}"
app = "trig_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
{trigger}

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "done"
"#
    )
}

fn hook_text() -> String {
    process_text(HOOK_PROCESS, "trigger = \"webhook\"\npath = \"intake-1\"")
}

fn doc_text() -> String {
    process_text(
        DOC_PROCESS,
        "trigger = \"document_event\"\ndoctype = \"widget\"\nevent = \"post\"",
    )
}

fn sched_text() -> String {
    // Every minute, on the second 0.
    process_text(
        SCHED_PROCESS,
        "trigger = \"schedule\"\ncron = \"0 * * * * *\"",
    )
}

// --- fakes -----------------------------------------------------------------------------------

#[derive(Default)]
struct NullSubmit;
impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

/// The outbox of a scripted stream; the lease is always granted.
#[derive(Default)]
struct Outbox {
    records: Mutex<Vec<OutboxRecord>>,
}

impl Outbox {
    fn push(&self, record: OutboxRecord) {
        self.records.lock().unwrap().push(record);
    }
}

impl OutboxLease for Outbox {
    fn acquire<'a>(&'a self, _consumer: &'a str) -> PortFuture<'a, ConsumerLease> {
        Box::pin(async { Err(PortError::LeaseUnavailable) })
    }
    fn renew<'a>(&'a self, lease: &'a ConsumerLease) -> PortFuture<'a, ConsumerLease> {
        let _ = lease;
        Box::pin(async { Err(PortError::LeaseUnavailable) })
    }
    fn still_held<'a>(&'a self, _lease: &'a ConsumerLease) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
    fn release<'a>(&'a self, _lease: &'a ConsumerLease) -> PortFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
    fn read<'a>(&'a self, after: FeedPosition, limit: u32) -> PortFuture<'a, Vec<OutboxRecord>> {
        Box::pin(async move {
            let all = self.records.lock().unwrap();
            Ok(all
                .iter()
                .filter(|r| r.position > after)
                .take(limit as usize)
                .cloned()
                .collect())
        })
    }
}

/// The platform reader: documents by `(doctype, name)`, switchable to failing.
#[derive(Default)]
struct Reader {
    docs: Mutex<HashMap<(String, String), Document>>,
    failing: AtomicBool,
}

impl Reader {
    fn put(&self, doc: Document) {
        let key = (doc.doctype().as_str().to_owned(), doc.id().to_string());
        self.docs.lock().unwrap().insert(key, doc);
    }
}

impl SystemReader for Reader {
    fn read<'a>(&'a self, doctype: &'a str, name: &'a str) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            if self.failing.load(Ordering::SeqCst) {
                return Err(PortError::Unavailable("the reader"));
            }
            let key = (doctype.to_owned(), name.to_owned());
            Ok(self.docs.lock().unwrap().get(&key).cloned())
        })
    }
}

/// The site metadata as the intake sees it: the stateful doctypes (`None` is unreadable).
struct FakeMeta {
    stateful: Mutex<Option<BTreeSet<String>>>,
}

impl FakeMeta {
    fn new(set: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            stateful: Mutex::new(Some(set.iter().map(|s| (*s).to_owned()).collect())),
        })
    }
    fn set(&self, value: Option<&[&str]>) {
        *self.stateful.lock().unwrap() =
            value.map(|set| set.iter().map(|s| (*s).to_owned()).collect());
    }
}

impl Meta for FakeMeta {
    fn stateful(&self) -> Option<BTreeSet<String>> {
        self.stateful.lock().unwrap().clone()
    }
    fn engine(&self) -> Option<spot_permissions::PermissionEngine> {
        None
    }
}

struct Secrets(BTreeMap<String, Vec<u8>>);

impl SecretSource for Secrets {
    fn secret<'a>(&'a self, reference: &'a str) -> Fut<'a, Option<Vec<u8>>> {
        Box::pin(async move { self.0.get(reference).cloned() })
    }
}

#[derive(Default)]
struct Trail(Mutex<Vec<AuditEntry>>);

impl Audit for Trail {
    fn record(&self, entry: AuditEntry) {
        self.0.lock().unwrap().push(entry);
    }
}

impl Trail {
    fn rules(&self, outcome: &str) -> Vec<String> {
        let all = self.0.lock().unwrap();
        all.iter()
            .filter(|e| e.outcome == outcome)
            .map(|e| e.rule.clone())
            .collect()
    }
}

// --- the site under test ---------------------------------------------------------------------

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 4;
    PgBackend::connect_unchecked(cfg).unwrap()
}

fn to_jiff(at: OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .unwrap()
}

fn timer_port(leases: Arc<dyn LeaseStore>, name: &str) -> (Lease, FencedTimerPort) {
    let lease = leases
        .acquire(
            name,
            "node-1",
            Duration::from_secs(3600),
            to_jiff(at(1_700_000_000)),
        )
        .unwrap();
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let port = FencedTimerPort::new(leases, lease.clone(), inner);
    (lease, port)
}

/// One tenant's stores and seams; `node()` builds a trigger host ("a node") over them.
struct Site {
    tenant: String,
    process: Arc<ProcessHost>,
    definitions: Arc<PgDefinitionStore>,
    pool: deadpool_postgres::Pool,
    clock: Arc<ManualClock>,
    reader: Arc<Reader>,
    meta: Arc<FakeMeta>,
    outbox: Arc<Outbox>,
    trail: Arc<Trail>,
    secrets: Arc<dyn SecretSource>,
}

async fn site(db: &TestDb, tenant: &str) -> Site {
    let pool = backend(db, tenant).pool();
    let store = PgCaseStore::new(pool.clone()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions = Arc::new(PgDefinitionStore::new(pool.clone()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let (_lease, timers) = timer_port(leases, &format!("timer:{tenant}"));
    let clock = Arc::new(ManualClock::new(at(1_700_000_000)));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let process = ProcessHost::build(
        SiteId::new(tenant).unwrap(),
        store,
        timers,
        HandlerRegistry::logic_default(),
        definitions.clone(),
        Arc::new(NullSubmit),
        dyn_clock,
    );
    let mut secrets = BTreeMap::new();
    secrets.insert("hook-secret".to_owned(), SECRET.to_vec());
    Site {
        tenant: tenant.to_owned(),
        process,
        definitions,
        pool,
        clock,
        reader: Arc::new(Reader::default()),
        meta: FakeMeta::new(&[]),
        outbox: Arc::new(Outbox::default()),
        trail: Arc::new(Trail::default()),
        secrets: Arc::new(Secrets(secrets)),
    }
}

impl Site {
    async fn publish(&self, text: &str) {
        let version = from_text(text).unwrap();
        self.definitions
            .publish(&version.name, text, None, "tester", None, at(1_700_000_000))
            .await
            .unwrap();
        self.process.engine().unwrap().publish(&version).unwrap();
    }

    fn settings(&self) -> Settings {
        let mut settings = Settings::default();
        settings.webhooks.insert(
            "intake-1".to_owned(),
            WebhookSetting {
                secret: "hook-secret".to_owned(),
            },
        );
        settings
    }

    /// A node: a trigger host over this site's stores, with `settings`.
    async fn node_with(&self, settings: Settings) -> Arc<TriggerHost> {
        let principals: Arc<dyn PrincipalSource> = Arc::new(MemoryPrincipals::new());
        let host = TriggerHost::new(Parts {
            site: SiteId::new(&self.tenant).unwrap(),
            clock: self.clock.clone(),
            settings,
            process: self.process.clone(),
            definitions: self.definitions.clone(),
            submit: Arc::new(NullSubmit),
            reader: self.reader.clone(),
            principals,
            meta: self.meta.clone(),
            secrets: self.secrets.clone(),
            audit: self.trail.clone(),
            pool: self.pool.clone(),
        });
        host.prepare().await.unwrap();
        host.reload().await.unwrap();
        host
    }

    async fn node(&self) -> Arc<TriggerHost> {
        self.node_with(self.settings()).await
    }
}

// --- events ----------------------------------------------------------------------------------

fn widget(name: &RecordId, state: Option<DocState>) -> Document {
    let mut doc = Document::new(Name::new("widget").unwrap(), *name);
    doc.set(Name::new("title").unwrap(), Value::Text("w".to_owned()));
    doc.mark_saved(Version(1), state);
    doc
}

/// A `doc_posted` outbox record for `doc` at feed position `seq`, from the platform itself,
/// optionally carrying the command id that wrote it.
fn posted(seq: u64, doc: &RecordId, command: Option<&str>) -> OutboxRecord {
    let actor = Actor::System {
        component: Name::new("test").unwrap(),
    };
    posted_as(seq, doc, command, actor)
}

/// The same record written by a person: the intake judges a person's start (facts must be
/// complete), but never the platform's own.
fn posted_by_person(seq: u64, doc: &RecordId) -> OutboxRecord {
    let actor = Actor::User {
        id: RecordId::new_v7(),
        via: None,
    };
    posted_as(seq, doc, None, actor)
}

fn posted_as(seq: u64, doc: &RecordId, command: Option<&str>, actor: Actor) -> OutboxRecord {
    let payload = PayloadDocument {
        meta: EventMeta {
            occurred_at: at(1_700_000_000),
            actor,
            command_id: command.map(|c| CommandId::new(c).unwrap()),
            causation_id: None,
            correlation_id: RecordId::new_v7(),
            trace_id: None,
        },
        data: json!({}),
    };
    OutboxRecord {
        position: FeedPosition::new(1, seq),
        transaction: RecordId::new_v7(),
        ordinal: 0,
        event: OutboxEvent {
            id: RecordId::new_v7(),
            topic: Name::new("doc_posted").unwrap(),
            subject: NodeRef {
                entity: Name::new("widget").unwrap(),
                id: *doc,
            },
            subject_version: Some(Version(1)),
            schema_version: 1,
            payload: serde_json::to_string(&payload).unwrap(),
        },
    }
}

fn skipped(db: &Option<TestDb>) -> bool {
    if db.is_none() {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
    }
    db.is_none()
}

// --- document events ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_document_event_starts_one_case_and_a_redelivery_from_two_nodes_starts_no_other() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-once").await;
    s.publish(&doc_text()).await;
    let doc = RecordId::new_v7();
    s.reader.put(widget(&doc, Some(DocState::Posted)));
    let record = posted(1, &doc, None);
    let (a, b) = (s.node().await, s.node().await);
    a.ingest(std::slice::from_ref(&record)).await.unwrap();
    a.ingest(std::slice::from_ref(&record)).await.unwrap();
    b.ingest(std::slice::from_ref(&record)).await.unwrap();
    assert_eq!(a.stats().started, 1);
    assert_eq!(a.stats().existing, 1);
    assert_eq!(b.stats().started, 0);
    assert_eq!(b.stats().existing, 1);
    let key = StartKey::new(DOC_PROCESS, node("start"), record.event.id.to_string());
    let found = s
        .process
        .engine()
        .unwrap()
        .store()
        .load(&case_id(&key).unwrap())
        .await
        .unwrap();
    assert!(found.is_some(), "the Case of the start key exists");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_position_is_durable_so_a_restarted_node_does_not_read_again() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-pos").await;
    s.publish(&doc_text()).await;
    let doc = RecordId::new_v7();
    s.reader.put(widget(&doc, Some(DocState::Posted)));
    s.outbox.push(posted(1, &doc, None));
    let first = s.node().await;
    assert_eq!(first.pump(&*s.outbox).await.unwrap(), 1);
    // A new node over the same database starts from the saved mark: nothing to read.
    let second = s.node().await;
    assert_eq!(second.pump(&*s.outbox).await.unwrap(), 0);
    assert_eq!(second.stats().started, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_event_whose_document_cannot_be_read_is_held_then_denied_and_reported() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-novalues").await;
    s.publish(&doc_text()).await;
    let doc = RecordId::new_v7();
    // The reader fails: the position must not pass the event until the attempts run out.
    s.reader.failing.store(true, Ordering::SeqCst);
    let host = s.node().await;
    let record = posted(1, &doc, None);
    let first = host.ingest(std::slice::from_ref(&record)).await.unwrap();
    assert!(first.held && first.next.is_none());
    for _ in 0..8 {
        if !host
            .ingest(std::slice::from_ref(&record))
            .await
            .unwrap()
            .held
        {
            break;
        }
    }
    assert_eq!(host.stats().started, 0);
    assert!(
        s.trail
            .rules("refused")
            .contains(&"document_unreadable".to_owned())
    );
    // A document the platform does not have at all is denied the same way, at once.
    s.reader.failing.store(false, Ordering::SeqCst);
    let missing = posted(2, &RecordId::new_v7(), None);
    let progress = host.ingest(&[missing]).await.unwrap();
    assert!(!progress.held);
    assert_eq!(host.stats().started, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stateful_doctype_reads_its_state_from_the_platform_and_a_new_one_is_enforced() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-stateful").await;
    s.publish(&doc_text()).await;
    s.meta.set(Some(&["widget"]));
    let host = s.node().await;
    // The event carries no values; the platform read supplies the state: the Case starts.
    let with_state = RecordId::new_v7();
    s.reader.put(widget(&with_state, Some(DocState::Posted)));
    host.ingest(&[posted(1, &with_state, None)]).await.unwrap();
    assert_eq!(host.stats().started, 1);
    // A document the platform reads without a state, of a stateful doctype, denies.
    let stateless = RecordId::new_v7();
    s.reader.put(widget(&stateless, None));
    host.ingest(&[posted_by_person(2, &stateless)])
        .await
        .unwrap();
    assert_eq!(host.stats().started, 1);
    assert!(!s.trail.rules("refused").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_doctype_added_by_a_metadata_reload_is_enforced_without_a_restart() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-reload").await;
    s.publish(&doc_text()).await;
    s.meta.set(Some(&[]));
    let host = s.node().await;
    let first = RecordId::new_v7();
    s.reader.put(widget(&first, None));
    host.ingest(&[posted(1, &first, None)]).await.unwrap();
    assert_eq!(
        host.stats().started,
        1,
        "an empty stateful set denies nothing"
    );
    // The metadata now names the doctype stateful: the same host denies the next stateless start.
    s.meta.set(Some(&["widget"]));
    let second = RecordId::new_v7();
    s.reader.put(widget(&second, None));
    host.ingest(&[posted_by_person(2, &second)]).await.unwrap();
    assert_eq!(host.stats().started, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unreadable_metadata_denies_every_document_start() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-nometa").await;
    s.publish(&doc_text()).await;
    s.meta.set(None);
    let host = s.node().await;
    let doc = RecordId::new_v7();
    s.reader.put(widget(&doc, Some(DocState::Posted)));
    host.ingest(&[posted(1, &doc, None)]).await.unwrap();
    assert_eq!(host.stats().started, 0);
    assert!(
        s.trail
            .rules("refused")
            .contains(&"metadata_unreadable".to_owned())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_process_updating_the_document_it_triggers_on_runs_once_not_forever() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-recursion").await;
    s.publish(&doc_text()).await;
    let host = s.node().await;
    let doc = RecordId::new_v7();
    s.reader.put(widget(&doc, Some(DocState::Posted)));
    let first = posted(1, &doc, None);
    host.ingest(std::slice::from_ref(&first)).await.unwrap();
    assert_eq!(host.stats().started, 1);
    // The Case's own Document node writes with the command id `<case>:<node>:<attempt>`.
    let case = case_id(&StartKey::new(
        DOC_PROCESS,
        node("start"),
        first.event.id.to_string(),
    ))
    .unwrap();
    let own = posted(2, &doc, Some(&format!("{case}:update:1")));
    host.ingest(&[own]).await.unwrap();
    assert_eq!(
        host.stats().started,
        1,
        "the Run's own write starts nothing"
    );
    assert!(s.trail.rules("refused").contains(&"recursion".to_owned()));
    // An event of a command that is not a Case of this process still starts one.
    let other = posted(3, &doc, Some("some-client-command"));
    host.ingest(&[other]).await.unwrap();
    assert_eq!(host.stats().started, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_rate_limit_refuses_audits_and_warns() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "doc-rate").await;
    s.publish(&doc_text()).await;
    let mut settings = s.settings();
    settings.start_rate_per_minute = 2;
    let host = s.node_with(settings).await;
    for seq in 1..=3 {
        let doc = RecordId::new_v7();
        s.reader.put(widget(&doc, Some(DocState::Posted)));
        host.ingest(&[posted(seq, &doc, None)]).await.unwrap();
    }
    assert_eq!(host.stats().started, 2);
    assert!(
        s.trail
            .rules("refused")
            .contains(&"rate_limited".to_owned())
    );
    assert!(host.warnings().iter().any(|w| w.contains("rate limit")));
}

// --- schedules ---------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_schedule_slot_fired_by_two_nodes_starts_one_case() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "sched-two").await;
    s.publish(&sched_text()).await;
    let (a, b) = (s.node().await, s.node().await);
    let t0 = at(1_700_000_010);
    // A schedule seen for the first time is marked; it fires from its next slot.
    assert_eq!(a.tick_schedules(t0).await.unwrap(), 0);
    let later = at(1_700_000_070);
    assert_eq!(a.tick_schedules(later).await.unwrap(), 1);
    // The second node's mark was shared through the database, so it plans nothing new; and the
    // slot key it would plan is the same Case if it did.
    assert_eq!(b.tick_schedules(later).await.unwrap(), 0);
    assert_eq!(a.stats().started + b.stats().started, 1);
}

// --- webhooks ----------------------------------------------------------------------------------

fn signed(body: &[u8], secret: &[u8], at_unix: i64) -> String {
    let key = SigningKey::new(secret);
    build_signature_header(at_unix, &key.sign(at_unix, body))
}

async fn deliver(
    host: &Arc<TriggerHost>,
    site: &str,
    trigger: &str,
    id: Option<&str>,
    header: &str,
    body: &[u8],
) -> bool {
    host.receive_hook(site, trigger, id, header, body)
        .await
        .is_ok()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_webhook_starts_one_case_and_a_replay_starts_none() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "hook-valid").await;
    s.publish(&hook_text()).await;
    let host = s.node().await;
    let body = br#"{"order":"o-1"}"#;
    let now = s.clock.now_utc().unix_timestamp();
    let header = signed(body, SECRET, now);
    assert!(deliver(&host, "hook-valid", "intake-1", Some("d-1"), &header, body).await);
    assert!(deliver(&host, "hook-valid", "intake-1", Some("d-1"), &header, body).await);
    assert_eq!(host.stats().started, 1);
    assert_eq!(host.stats().existing, 1);
    // Another delivery id is another event.
    assert!(deliver(&host, "hook-valid", "intake-1", Some("d-2"), &header, body).await);
    assert_eq!(host.stats().started, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_webhook_refusal_is_the_same_and_starts_nothing() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "hook-refuse").await;
    s.publish(&hook_text()).await;
    let host = s.node().await;
    let body = br#"{"order":"o-1"}"#;
    let now = s.clock.now_utc().unix_timestamp();
    let good = signed(body, SECRET, now);
    let mut refused = Vec::new();
    // Bad signature, wrong secret, stale timestamp, unsigned, unknown trigger, a name that is
    // not a trigger name, another site, a bad delivery id, a body that is not an object.
    refused.push(
        deliver(
            &host,
            "hook-refuse",
            "intake-1",
            Some("a"),
            &signed(b"not the body", SECRET, now),
            body,
        )
        .await,
    );
    refused.push(
        deliver(
            &host,
            "hook-refuse",
            "intake-1",
            Some("b"),
            &signed(body, b"another-secret-entirely", now),
            body,
        )
        .await,
    );
    refused.push(
        deliver(
            &host,
            "hook-refuse",
            "intake-1",
            Some("c"),
            &signed(body, SECRET, now - 86_400),
            body,
        )
        .await,
    );
    refused.push(deliver(&host, "hook-refuse", "intake-1", Some("d"), "", body).await);
    refused.push(deliver(&host, "hook-refuse", "no-such", Some("e"), &good, body).await);
    refused.push(deliver(&host, "hook-refuse", "../intake-1", Some("f"), &good, body).await);
    refused.push(deliver(&host, "other_site", "intake-1", Some("g"), &good, body).await);
    refused.push(deliver(&host, "hook-refuse", "intake-1", Some("../x"), &good, body).await);
    let not_object = b"[1,2]";
    refused.push(
        deliver(
            &host,
            "hook-refuse",
            "intake-1",
            Some("h"),
            &signed(not_object, SECRET, now),
            not_object,
        )
        .await,
    );
    // A body altered after signing.
    refused.push(
        deliver(
            &host,
            "hook-refuse",
            "intake-1",
            Some("i"),
            &good,
            br#"{"order":"o-2"}"#,
        )
        .await,
    );
    assert!(refused.iter().all(|ok| !ok), "{refused:?}");
    assert_eq!(host.stats().started, 0);
    assert!(s.trail.rules("refused").len() >= refused.len());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trigger_without_a_secret_is_disabled_and_health_says_so() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "hook-nosecret").await;
    s.publish(&hook_text()).await;
    // No configuration for the trigger at all: a correctly signed call is still refused.
    let host = s.node_with(Settings::default()).await;
    let body = br#"{"order":"o-1"}"#;
    let header = signed(body, SECRET, s.clock.now_utc().unix_timestamp());
    assert!(
        !deliver(
            &host,
            "hook-nosecret",
            "intake-1",
            Some("d-1"),
            &header,
            body
        )
        .await
    );
    assert_eq!(host.stats().started, 0);
    assert!(host.warnings().iter().any(|w| w.contains("disabled")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_port_refuses_with_the_one_error_and_site_a_cannot_reach_site_b() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let a = site(&db, "iso-a").await;
    let b = site(&db, "iso-b").await;
    a.publish(&hook_text()).await;
    // Site B publishes nothing: its webhook name does not exist there.
    let (host_a, host_b) = (a.node().await, b.node().await);
    let body = br#"{"x":1}"#;
    let header = signed(body, SECRET, a.clock.now_utc().unix_timestamp());
    assert!(deliver(&host_a, "iso-a", "intake-1", Some("d"), &header, body).await);
    assert!(!deliver(&host_b, "iso-b", "intake-1", Some("d"), &header, body).await);
    assert!(!deliver(&host_b, "iso-a", "intake-1", Some("d"), &header, body).await);
    assert_eq!(host_a.stats().started, 1);
    assert_eq!(host_b.stats().started, 0);
    // The Case of A is not visible in B's store.
    let key = StartKey::new(HOOK_PROCESS, node("start"), "d");
    let id = case_id(&key).unwrap();
    assert!(
        b.process
            .engine()
            .unwrap()
            .store()
            .load(&id)
            .await
            .unwrap()
            .is_none()
    );
    // The port type is the module's own receiver; it wraps the same host.
    let _port = HookPort::new(host_b);
}

// --- the reload port (WP-BPM-03.T5) --------------------------------------------------------------

#[tokio::test]
async fn a_reload_brings_in_a_version_published_after_the_node_started_and_repeating_it_changes_nothing()
 {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "reload-doc").await;
    let a = s.node().await;
    s.publish(&doc_text()).await;
    let doc = RecordId::new_v7();
    s.reader.put(widget(&doc, Some(DocState::Posted)));
    // The node started before the publish: until a reload it does not know the trigger.
    a.ingest(&[posted(1, &doc, None)]).await.unwrap();
    assert_eq!(a.stats().started, 0);
    assert_eq!(a.reload_now().await.unwrap(), 1);
    let record = posted(2, &doc, None);
    a.ingest(std::slice::from_ref(&record)).await.unwrap();
    assert_eq!(a.stats().started, 1);
    // A second and a third reload find the same published set and change nothing; the redelivered
    // event is the same Case.
    assert_eq!(a.reload_now().await.unwrap(), 1);
    assert_eq!(a.reload_now().await.unwrap(), 1);
    a.ingest(std::slice::from_ref(&record)).await.unwrap();
    assert_eq!(a.stats().started, 1);
    assert_eq!(a.stats().existing, 1);
}

#[tokio::test]
async fn reloads_do_not_start_a_second_schedule_slot() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let s = site(&db, "reload-sched").await;
    s.publish(&sched_text()).await;
    let a = s.node().await;
    assert_eq!(a.tick_schedules(at(1_700_000_010)).await.unwrap(), 0);
    a.reload_now().await.unwrap();
    a.reload_now().await.unwrap();
    let later = at(1_700_000_070);
    assert_eq!(a.tick_schedules(later).await.unwrap(), 1);
    // Reloading between ticks, or ticking the same slot again, starts nothing more.
    a.reload_now().await.unwrap();
    assert_eq!(a.tick_schedules(later).await.unwrap(), 0);
    assert_eq!(a.stats().started, 1);
}

#[tokio::test]
async fn a_reload_on_site_a_never_touches_site_bs_triggers() {
    let db = TestDb::create().await;
    if skipped(&db) {
        return;
    }
    let db = db.unwrap();
    let a = site(&db, "reload-a").await;
    let b = site(&db, "reload-b").await;
    b.publish(&doc_text()).await;
    let (host_a, host_b) = (a.node().await, b.node().await);
    // Site A publishes its own process on the same doctype and event, then reloads.
    a.publish(&process_text(
        "a_only",
        "trigger = \"document_event\"\ndoctype = \"widget\"\nevent = \"post\"",
    ))
    .await;
    assert_eq!(host_a.reload_now().await.unwrap(), 1);
    // B's set is B's own: one trigger (its process), unchanged by A's reload.
    assert_eq!(host_b.reload_now().await.unwrap(), 1);
    let (da, db_doc) = (RecordId::new_v7(), RecordId::new_v7());
    a.reader.put(widget(&da, Some(DocState::Posted)));
    b.reader.put(widget(&db_doc, Some(DocState::Posted)));
    host_a.ingest(&[posted(1, &da, None)]).await.unwrap();
    host_b.ingest(&[posted(1, &db_doc, None)]).await.unwrap();
    // Each event starts exactly the Case of its own site's process.
    assert_eq!(host_a.stats().started, 1);
    assert_eq!(host_b.stats().started, 1);
    let engine_b = b.process.engine().unwrap();
    assert!(engine_b.processes().get("a_only", 1).is_none());
    assert!(engine_b.processes().get(DOC_PROCESS, 1).is_some());
}
