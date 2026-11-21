//! WP-AI-10.T3: the retrieval service on a site: the outbox-fed indexer, the index policy and the
//! permission-aware `retrieve` tool. The proof is the acceptance of WP-AI-10: a restricted user's
//! agent never surfaces an unreadable document, including right after a permission change. The
//! site's own ports are the real ones; the embedding port and the store are doubles, the field
//! guard is real. One test runs the tool over PostgreSQL when `SPOT_TEST_PG_URL` is set.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::document::Document;
use spot_app_api::prelude::{Name, RecordId};
use spot_cli::services::SiteCtx;
use spot_cli::services::ports::{
    DocumentLoader, OutboxLease, PortError, PortFuture, PortParts, SystemReader, provide,
};
use spot_cli::services::retrieval::Settings;
use spot_cli::services::retrieval::access::{LoaderAccess, PolicyError, PolicySource};
use spot_cli::services::retrieval::ports::{
    EmbedFailure, EmbedPort, EmbedPurpose, EmbedRequest, FilePositions, Fut, GuardSource,
};
use spot_cli::services::retrieval::pump::{Pump, PumpDeps, Shared};
use spot_cli::services::retrieval::tool::{Limits, RetrieveTool, ToolDeps};
use spot_coordinator::{InMemoryPresence, InProcessCoordinator, NodeId};
use spot_core::{Clock, ManualClock, SiteId};
use spot_field_guard::{
    AuditRecord, BoundedQueue, DocProtection, FieldAccessRecord, FieldGuard, StaticProtection,
};
use spot_jobs::{InMemoryJobStore, JobStore};
use spot_mcp::principal::McpPrincipal;
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, ProtectionGroupSpec, Registry};
use spot_permissions::{
    AssignmentContext, FieldFacts, FieldGrant, FieldRight, FieldTarget, GroupName, Model,
    PermissionEngine, ProtectionGroup, Right, Rights, Role, RoleGrant, Snapshot,
};
use spot_retrieval::testing::{FakeEmbedding, FakeFeed, change};
use spot_retrieval::{
    EmbeddingProvider, IndexPolicy, MemoryVectorStore, PositionStore, VectorStore,
};
use spot_storage_contract::{ChangeOp, Value, Version};
use time::macros::datetime;
use tokio::runtime::Handle;

const DIM: usize = 8;
const SECRET: &str = "SALARY-SECRET-VALUE";

/// Documents of the site and the rights of its users: the platform reads them all; a user reads
/// only a document listed for them. Stands in for the document engine behind `LiveLoader`.
#[derive(Default)]
struct Docs {
    docs: Mutex<HashMap<(String, RecordId), Document>>,
    readable: Mutex<BTreeSet<(RecordId, RecordId)>>,
    reader_down: AtomicBool,
}

impl Docs {
    fn put(&self, doctype: &str, id: RecordId, title: &str, body: &str, secret: &str) {
        let mut doc = Document::new(Name::new(doctype).unwrap(), id);
        doc.set(Name::new("title").unwrap(), Value::Text(title.to_owned()));
        doc.set(Name::new("body").unwrap(), Value::Text(body.to_owned()));
        doc.set(
            Name::new("secret_note").unwrap(),
            Value::Text(secret.to_owned()),
        );
        self.docs
            .lock()
            .unwrap()
            .insert((doctype.to_owned(), id), doc);
    }

    fn remove(&self, doctype: &str, id: RecordId) {
        self.docs.lock().unwrap().remove(&(doctype.to_owned(), id));
    }

    fn grant(&self, user: RecordId, doc: RecordId) {
        self.readable.lock().unwrap().insert((user, doc));
    }

    fn revoke(&self, user: RecordId, doc: RecordId) {
        self.readable.lock().unwrap().remove(&(user, doc));
    }
}

impl DocumentLoader for Docs {
    fn load<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
    ) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            let doc = self
                .docs
                .lock()
                .unwrap()
                .get(&(doctype.to_owned(), id))
                .cloned();
            match &inv.actor {
                Actor::System { .. } => {
                    if self.reader_down.load(Ordering::SeqCst) {
                        return Err(PortError::Unavailable("the document engine"));
                    }
                    Ok(doc)
                }
                Actor::User { id: user, .. } => {
                    let allowed = self.readable.lock().unwrap().contains(&(*user, id));
                    Ok(doc.filter(|_| allowed))
                }
                Actor::Consumer { .. } => Ok(None),
            }
        })
    }
}

/// The LLM service's embedding port, as a double that records what it was asked to embed.
struct Embedder {
    requests: Mutex<Vec<EmbedRequest>>,
    fail: AtomicBool,
}

impl Embedder {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
            fail: AtomicBool::new(false),
        })
    }

    fn texts_sent(&self, purpose: EmbedPurpose) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.purpose == purpose)
            .flat_map(|request| request.texts.clone())
            .collect()
    }
}

impl EmbedPort for Embedder {
    fn embed<'a>(
        &'a self,
        request: &'a EmbedRequest,
    ) -> Fut<'a, Result<Vec<Vec<f32>>, EmbedFailure>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request.clone());
            if self.fail.load(Ordering::SeqCst) {
                return Err(EmbedFailure {
                    code: "test_embedder_failure",
                });
            }
            FakeEmbedding::with_dim(DIM)
                .embed(&request.texts)
                .map_err(|_| EmbedFailure {
                    code: "test_embedder_failure",
                })
        })
    }
}

fn pay() -> GroupName {
    GroupName("pay".to_owned())
}

fn meta() -> Arc<EffectiveMeta> {
    let mut secret = FieldSpec::new("secret_note", FieldType::Text);
    secret.protection_group = Some("pay".into());
    let mut note = Definition::new("note");
    note.fields = vec![
        FieldSpec::new("title", FieldType::Text),
        FieldSpec::new("body", FieldType::Text),
        secret,
    ];
    note.protection_groups = vec![ProtectionGroupSpec::new("pay", "Pay")];
    Registry::new(vec![note]).unwrap().effective()
}

fn secret_facts() -> FieldFacts {
    let mut facts = FieldFacts::plain("secret_note");
    facts.groups = vec![pay()];
    facts
}

fn snapshot() -> Snapshot {
    let grant = |role: &str| RoleGrant {
        doctype: "note".to_owned(),
        role: Role(role.to_owned()),
        rights: Rights::new([Right::Read, Right::Report]),
        if_owner: false,
    };
    let model = Model {
        grants: vec![grant("reader"), grant("payer")],
        field_grants: vec![FieldGrant {
            doctype: "note".to_owned(),
            role: Role("payer".to_owned()),
            target: FieldTarget::Group(pay()),
            access: BTreeSet::from([FieldRight::Read]),
            deny: false,
            condition: None,
        }],
        groups: vec![ProtectionGroup {
            name: pay(),
            trace: false,
        }],
        ..Model::default()
    };
    Snapshot::build(&meta(), &model)
        .unwrap()
        .with_field_facts("note", vec![secret_facts()])
        .unwrap()
}

fn protection() -> Arc<StaticProtection> {
    Arc::new(StaticProtection::new(vec![DocProtection {
        name: "note".to_owned(),
        fields: vec![secret_facts()],
        tables: BTreeMap::default(),
        trace_groups: Vec::new(),
    }]))
}

struct Guards(Arc<FieldGuard>);

impl GuardSource for Guards {
    fn guard(&self) -> Option<Arc<FieldGuard>> {
        Some(Arc::clone(&self.0))
    }
}

struct NoGuards;

impl GuardSource for NoGuards {
    fn guard(&self) -> Option<Arc<FieldGuard>> {
        None
    }
}

/// The index policy loaded against the snapshot, as the live source does.
struct Policy {
    text: String,
    snapshot: Snapshot,
}

impl PolicySource for Policy {
    fn policy(&self) -> Result<IndexPolicy, PolicyError> {
        IndexPolicy::load(&self.text, &self.snapshot).map_err(PolicyError::Invalid)
    }
}

const POLICY_DEFAULT: &str = "[doctypes.note]\nfields = [\"title\", \"body\"]\n";
const POLICY_WITH_SECRET: &str = "[doctypes.note]\nfields = [\"title\", \"body\", \"secret_note\"]\nrestricted_ok = [\"secret_note\"]\n";

fn unique_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("spot_retrieval_svc_{name}_{}", RecordId::new_v7()))
}

struct World {
    site: SiteId,
    clock: Arc<ManualClock>,
    docs: Arc<Docs>,
    embedder: Arc<Embedder>,
    store: Arc<MemoryVectorStore>,
    cursor: Arc<FilePositions>,
    shared: Arc<Shared>,
    lease: Arc<dyn OutboxLease>,
    reader: Arc<dyn SystemReader>,
    guard: Arc<FieldGuard>,
    _ctx: SiteCtx<'static>,
}

impl World {
    async fn new() -> World {
        let clock = Arc::new(ManualClock::new(datetime!(2026-11-20 10:00:00 UTC)));
        let docs = Arc::new(Docs::default());
        let site = SiteId::new("acme").unwrap();
        let mut ctx = SiteCtx::new(
            "acme",
            Duration::from_secs(1),
            clock.clone(),
            BTreeMap::new(),
            true,
        );
        let coordinator = InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("node-a").unwrap(),
            clock.clone(),
        )
        .await
        .unwrap();
        ctx.as_module("jobs", 600);
        provide(
            &mut ctx,
            PortParts {
                site: site.clone(),
                store: Arc::new(InMemoryJobStore::new()) as Arc<dyn JobStore>,
                queues: vec!["default".to_owned()],
                clock: clock.clone(),
                coordinator,
                feed: FakeFeed::new(Vec::new()),
                node: "node-a".to_owned(),
                ttl: Duration::from_secs(30),
                loader: docs.clone() as Arc<dyn DocumentLoader>,
            },
        )
        .unwrap();
        ctx.as_module("retrieval", 1270);
        let lease = ctx.port::<dyn OutboxLease>().unwrap();
        let reader = ctx.port::<dyn SystemReader>().unwrap();
        let guard = Arc::new(FieldGuard::new(
            Arc::new(PermissionEngine::new(Arc::new(snapshot()))),
            protection() as Arc<dyn spot_field_guard::ProtectionSource>,
            Arc::new(BoundedQueue::<FieldAccessRecord>::new(10_000)),
            Arc::new(BoundedQueue::<AuditRecord>::new(10_000)),
            clock.clone() as Arc<dyn Clock>,
        ));
        World {
            site,
            clock,
            docs,
            embedder: Embedder::new(),
            store: Arc::new(MemoryVectorStore::new()),
            cursor: Arc::new(FilePositions::new(
                unique_dir("cursor").join("retrieval.cursor"),
            )),
            shared: Shared::new(),
            lease,
            reader,
            guard,
            _ctx: ctx,
        }
    }

    fn settings(&self, page: u32) -> Settings {
        Settings {
            enabled: true,
            tenants: vec!["acme".to_owned()],
            provider: "test".to_owned(),
            dimensions: DIM,
            page,
            interval_ms: 100,
            ..Settings::default()
        }
    }

    fn pump(&self, feed: FakeFeed, policy: &str, protected: bool, page: u32) -> Pump<FakeFeed> {
        let mut settings = self.settings(page);
        settings.index_protected = protected;
        Pump::new(PumpDeps {
            site: self.site.clone(),
            feed,
            lease: Arc::clone(&self.lease),
            reader: Arc::clone(&self.reader),
            embed: self.embedder.clone() as Arc<dyn EmbedPort>,
            store: self.store.clone() as Arc<dyn VectorStore>,
            cursor: Arc::clone(&self.cursor),
            protection: protection() as Arc<dyn spot_field_guard::ProtectionSource>,
            policy: Arc::new(Policy {
                text: policy.to_owned(),
                snapshot: snapshot(),
            }),
            clock: self.clock.clone() as Arc<dyn Clock>,
            settings,
            shared: Arc::clone(&self.shared),
        })
    }

    fn tool(&self) -> RetrieveTool {
        self.tool_with(Arc::new(Guards(Arc::clone(&self.guard))), true)
    }

    fn tool_with(&self, guards: Arc<dyn GuardSource>, ready: bool) -> RetrieveTool {
        self.tool_over(
            self.store.clone() as Arc<dyn VectorStore>,
            "acme",
            guards,
            ready,
        )
    }

    fn tool_over(
        &self,
        store: Arc<dyn VectorStore>,
        tenant: &str,
        guards: Arc<dyn GuardSource>,
        ready: bool,
    ) -> RetrieveTool {
        RetrieveTool::new(ToolDeps {
            site: SiteId::new(tenant).unwrap(),
            store,
            embed: self.embedder.clone() as Arc<dyn EmbedPort>,
            access: Arc::new(LoaderAccess::new(
                self.docs.clone() as Arc<dyn DocumentLoader>,
                self.site.clone(),
                self.clock.clone() as Arc<dyn Clock>,
            )),
            guards,
            ready: Arc::new(AtomicBool::new(ready)),
            limits: limits(),
        })
    }

    /// Indexes `changes` in one pass with a fresh pump over the shared cursor.
    async fn index(
        &self,
        changes: Vec<spot_storage_contract::Change>,
        policy: &str,
        protected: bool,
    ) {
        let pump = self.pump(FakeFeed::new(changes), policy, protected, 100);
        let mut indexer = pump.indexer(Handle::current()).unwrap();
        pump.pass(&mut indexer).await.unwrap();
    }
}

fn user(id: RecordId, role: &str) -> McpPrincipal {
    McpPrincipal::new(
        &format!("user-{id}"),
        Actor::User { id, via: None },
        vec![AssignmentContext::unscoped("a1", &[role])],
    )
}

fn hit_ids(answer: &Json) -> Vec<String> {
    answer["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["id"].as_str().unwrap().to_owned())
        .collect()
}

fn snippets(answer: &Json) -> String {
    answer["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["snippet"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Two documents about the same topic: the second is for the board only.
struct Corpus {
    open: RecordId,
    board: RecordId,
    alice: RecordId,
    bob: RecordId,
}

fn corpus(world: &World) -> Corpus {
    let c = Corpus {
        open: RecordId::new_v7(),
        board: RecordId::new_v7(),
        alice: RecordId::new_v7(),
        bob: RecordId::new_v7(),
    };
    world.docs.put(
        "note",
        c.open,
        "Budget",
        "the budget forecast for the quarter",
        SECRET,
    );
    world.docs.put(
        "note",
        c.board,
        "Board",
        "the budget forecast of the board only",
        SECRET,
    );
    // Alice reads the open note only; Bob reads both.
    world.docs.grant(c.alice, c.open);
    world.docs.grant(c.bob, c.open);
    world.docs.grant(c.bob, c.board);
    c
}

fn inserts(c: &Corpus) -> Vec<spot_storage_contract::Change> {
    vec![
        change(1, "note", c.open, ChangeOp::Insert, Version(1)).unwrap(),
        change(2, "note", c.board, ChangeOp::Insert, Version(1)).unwrap(),
    ]
}

fn limits() -> Limits {
    Limits {
        provider: "test".to_owned(),
        dimensions: DIM,
        max_query_chars: 200,
        max_hits: 10,
        snippet_chars: 120,
    }
}

const QUERY: &str = "budget forecast";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restricted_users_agent_never_surfaces_an_unreadable_document() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let tool = world.tool();

    let alice = tool
        .call(&user(c.alice, "reader"), &json!({"query": QUERY, "k": 5}))
        .await
        .unwrap();
    let ids = hit_ids(&alice);
    assert!(
        ids.contains(&c.open.to_string()),
        "the readable note is found"
    );
    assert!(
        !ids.contains(&c.board.to_string()),
        "the board note is unreadable for Alice and must not surface"
    );
    assert!(!snippets(&alice).contains("board only"));

    let bob = tool
        .call(&user(c.bob, "reader"), &json!({"query": QUERY, "k": 5}))
        .await
        .unwrap();
    assert!(
        hit_ids(&bob).contains(&c.board.to_string()),
        "the same query finds the board note for a user who may read it"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_revoked_permission_applies_to_the_very_next_retrieval() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let tool = world.tool();
    let bob = user(c.bob, "reader");
    let args = json!({"query": QUERY, "k": 5});

    assert!(hit_ids(&tool.call(&bob, &args).await.unwrap()).contains(&c.board.to_string()));
    let indexed_before = world.store.count();
    world.docs.revoke(c.bob, c.board);
    let after = tool.call(&bob, &args).await.unwrap();
    assert!(
        !hit_ids(&after).contains(&c.board.to_string()),
        "revoked and retrieved at once: no hit, no re-index in between"
    );
    assert_eq!(
        world.store.count(),
        indexed_before,
        "nothing was re-indexed"
    );
    // And a grant shows at once too.
    world.docs.grant(c.bob, c.board);
    assert!(hit_ids(&tool.call(&bob, &args).await.unwrap()).contains(&c.board.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_empty_answer_and_a_hidden_answer_are_the_same_answer() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let tool = world.tool();
    // Carol may read nothing: what she gets for a query that matches only unreadable documents is
    // exactly what a query that matches nothing gets.
    let carol = user(RecordId::new_v7(), "reader");
    let hidden = tool
        .call(&carol, &json!({"query": QUERY, "k": 5}))
        .await
        .unwrap();
    let nothing = tool
        .call(&carol, &json!({"query": "zebra quartz", "k": 5}))
        .await
        .unwrap();
    assert_eq!(hidden, nothing);
    assert_eq!(hidden["hits"], json!([]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_protected_field_never_reaches_an_embedding_request_by_default() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let sent = world.embedder.texts_sent(EmbedPurpose::Index).join("\n");
    assert!(
        sent.contains("budget forecast"),
        "the indexer did embed text"
    );
    assert!(
        !sent.contains(SECRET),
        "the protected value reached the embedding request"
    );
    // Even a policy that lists the field is refused its protected field unless the tenant allows it.
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_WITH_SECRET, false).await;
    assert!(
        !world
            .embedder
            .texts_sent(EmbedPurpose::Index)
            .join("\n")
            .contains(SECRET),
        "restricted_ok alone does not index a protected field"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_field_the_caller_may_not_read_never_appears_in_a_snippet() {
    let world = World::new().await;
    let c = corpus(&world);
    // The tenant opts the protected field in; the chunk exists, tagged with its field.
    world.index(inserts(&c), POLICY_WITH_SECRET, true).await;
    assert!(
        world
            .embedder
            .texts_sent(EmbedPurpose::Index)
            .join("\n")
            .contains(SECRET),
        "the opted-in field was indexed, so the proof is not vacuous"
    );
    let tool = world.tool();
    // Alice reads the document but holds no right to the pay group: the secret chunk is dropped.
    let answer = tool
        .call(
            &user(c.alice, "reader"),
            &json!({"query": "salary secret value", "k": 10}),
        )
        .await
        .unwrap();
    assert!(
        !snippets(&answer).contains(SECRET),
        "a field the caller may not read reached a snippet: {answer}"
    );
    // She still finds the readable fields of the same document.
    let plain = tool
        .call(&user(c.alice, "reader"), &json!({"query": QUERY, "k": 10}))
        .await
        .unwrap();
    assert!(hit_ids(&plain).contains(&c.open.to_string()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_query_text_is_the_only_thing_a_query_embedding_carries() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let alice = user(c.alice, "reader");
    world
        .tool()
        .call(&alice, &json!({"query": QUERY, "k": 3}))
        .await
        .unwrap();
    let requests = world.embedder.requests.lock().unwrap().clone();
    let query_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.purpose == EmbedPurpose::Query)
        .collect();
    assert_eq!(query_requests.len(), 1);
    assert_eq!(query_requests[0].texts, vec![QUERY.to_owned()]);
    assert_eq!(
        query_requests[0].principal.as_deref(),
        Some(alice.audit_actor().as_str()),
        "the query is embedded on behalf of the caller, who the send record names"
    );
    assert!(
        requests
            .iter()
            .filter(|request| request.purpose == EmbedPurpose::Index)
            .all(|request| request.principal.is_none()),
        "indexing has no calling user"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deleted_documents_disappear_from_retrieval() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let tool = world.tool();
    let bob = user(c.bob, "reader");
    assert!(
        hit_ids(&tool.call(&bob, &json!({"query": QUERY})).await.unwrap())
            .contains(&c.board.to_string())
    );

    world.docs.remove("note", c.board);
    world
        .index(
            vec![change(3, "note", c.board, ChangeOp::Delete, Version(2)).unwrap()],
            POLICY_DEFAULT,
            false,
        )
        .await;
    assert!(
        !hit_ids(&tool.call(&bob, &json!({"query": QUERY})).await.unwrap())
            .contains(&c.board.to_string()),
        "a deleted document is gone from the index"
    );
    assert!(world.store.chunks_of("note", c.board).is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crashed_indexer_resumes_from_its_cursor_with_no_duplicates() {
    let world = World::new().await;
    let c = corpus(&world);
    let feed = || FakeFeed::new(inserts(&c));
    // The first node reads one change and "crashes".
    let first = world.pump(feed(), POLICY_DEFAULT, false, 1);
    let mut indexer = first.indexer(Handle::current()).unwrap();
    let report = first.pass(&mut indexer).await.unwrap();
    assert_eq!(report.read, 1);
    let after_one = world.store.count();
    assert!(after_one > 0);
    assert!(world.cursor.load().is_some(), "the cursor is durable");

    // A new node takes over from the cursor and finishes; a third replays everything.
    let second = world.pump(feed(), POLICY_DEFAULT, false, 10);
    let mut indexer = second.indexer(Handle::current()).unwrap();
    let report = second.pass(&mut indexer).await.unwrap();
    assert_eq!(
        report.read, 1,
        "it resumed after the first change, not before it"
    );
    let complete = world.store.count();
    world.cursor.reset().unwrap();
    let third = world.pump(feed(), POLICY_DEFAULT, false, 10);
    let mut indexer = third.indexer(Handle::current()).unwrap();
    third.pass(&mut indexer).await.unwrap();
    assert_eq!(world.store.count(), complete, "a replay added chunks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failing_provider_leaves_the_cursor_and_the_next_pass_catches_up() {
    let world = World::new().await;
    let c = corpus(&world);
    world.embedder.fail.store(true, Ordering::SeqCst);
    let pump = world.pump(FakeFeed::new(inserts(&c)), POLICY_DEFAULT, false, 10);
    let mut indexer = pump.indexer(Handle::current()).unwrap();
    let error = pump.pass(&mut indexer).await.unwrap_err();
    assert_eq!(error.code(), "retrieval.embed_failed");
    assert!(
        !error.to_string().contains("budget"),
        "the failure names no document text"
    );
    assert_eq!(world.cursor.load(), None, "the cursor did not move");
    assert_eq!(world.store.count(), 0);

    world.embedder.fail.store(false, Ordering::SeqCst);
    let report = pump.pass(&mut indexer).await.unwrap();
    assert_eq!(report.indexed, 2);
    assert!(world.cursor.load().is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_reader_failure_is_not_taken_for_a_deleted_document() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let indexed = world.store.count();
    // The document engine is down while an update arrives.
    world.docs.reader_down.store(true, Ordering::SeqCst);
    let update = vec![change(3, "note", c.open, ChangeOp::Update, Version(2)).unwrap()];
    let pump = world.pump(FakeFeed::new(update), POLICY_DEFAULT, false, 10);
    let mut indexer = pump.indexer(Handle::current()).unwrap();
    let before = world.cursor.load();
    let error = pump.pass(&mut indexer).await.unwrap_err();
    assert_eq!(error.code(), "retrieval.position_failed");
    assert_eq!(
        world.cursor.load(),
        before,
        "the position stayed so the range is retried"
    );

    world.docs.reader_down.store(false, Ordering::SeqCst);
    pump.pass(&mut indexer).await.unwrap();
    assert_eq!(
        world.store.count(),
        indexed,
        "the document is indexed again, once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_loop_takes_the_lease_indexes_and_stops_cleanly() {
    let world = World::new().await;
    let c = corpus(&world);
    let pump = Arc::new(world.pump(FakeFeed::new(inserts(&c)), POLICY_DEFAULT, false, 10));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let runner = tokio::spawn({
        let pump = Arc::clone(&pump);
        async move { pump.run(stopped).await }
    });
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while world.shared.status().indexed < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the indexer did not run: {:?}",
            world.shared.status()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        world.shared.leader.load(Ordering::SeqCst),
        "it holds the lease"
    );
    stop.send(true).unwrap();
    runner.await.unwrap().unwrap();
    assert!(
        !world.shared.leader.load(Ordering::SeqCst),
        "it let the lease go"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_tool_refuses_what_it_must() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let tool = world.tool();
    let alice = user(c.alice, "reader");

    // Bad arguments.
    for args in [
        json!({}),
        json!({"query": ""}),
        json!({"query": "x", "k": 0}),
        json!({"query": "x", "k": 11}),
        json!({"query": "x", "extra": 1}),
        json!({"query": "x", "doctypes": []}),
        json!({"query": "x".repeat(201)}),
        json!("a string"),
    ] {
        let error = tool.call(&alice, &args).await.unwrap_err();
        assert_eq!(error.kind, "mcp.invalid_arguments", "{args}");
    }

    // The platform is never a caller, and neither is a consumer.
    let system = McpPrincipal {
        principal: spot_permissions::Principal::system(),
        actor: Actor::System {
            component: Name::new("scheduler").unwrap(),
        },
        key: None,
        key_id: None,
        tier: None,
        agent_id: None,
        now_unix: 0,
    };
    let error = tool
        .call(&system, &json!({"query": "x"}))
        .await
        .unwrap_err();
    assert_eq!(error.kind, "mcp.permission_denied");

    // No guard, no answer; not ready, no answer.
    let error = world
        .tool_with(Arc::new(NoGuards), true)
        .call(&alice, &json!({"query": QUERY}))
        .await
        .unwrap_err();
    assert_eq!(error.kind, "mcp.not_supported");
    let error = world
        .tool_with(Arc::new(Guards(Arc::clone(&world.guard))), false)
        .call(&alice, &json!({"query": QUERY}))
        .await
        .unwrap_err();
    assert_eq!(error.kind, "mcp.not_supported");

    // A failing provider names no text.
    world.embedder.fail.store(true, Ordering::SeqCst);
    let error = tool
        .call(&alice, &json!({"query": "TOP-SECRET-QUERY"}))
        .await
        .unwrap_err();
    assert_eq!(error.kind, "mcp.internal");
    assert!(!error.message.contains("TOP-SECRET-QUERY"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_description_matches_the_limits_and_the_answer_is_marked_untrusted() {
    let world = World::new().await;
    let c = corpus(&world);
    world.index(inserts(&c), POLICY_DEFAULT, false).await;
    let tool = world.tool();
    assert_eq!(
        tool.describe().input_schema["properties"]["k"]["maximum"],
        json!(10)
    );
    let answer = tool
        .call(&user(c.alice, "reader"), &json!({"query": QUERY}))
        .await
        .unwrap();
    assert!(answer["provenance"].as_str().unwrap().contains("untrusted"));
    for key in ["doctype", "id", "version", "score", "snippet"] {
        assert!(answer["hits"][0].get(key).is_some(), "a hit carries {key}");
    }
    assert!(answer.get("total").is_none(), "no total count is exposed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn one_tenants_embeddings_are_never_returned_to_another() {
    use spot_postgres::testing::TestDb;
    use spot_retrieval::Chunk;
    use spot_retrieval_pg::{IndexKind, PgVectorStore, PrepareError, pool_for};

    let Some(db) = TestDb::create().await else {
        return;
    };
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap(),
    );
    std::mem::forget(Arc::clone(&runtime));
    let store = |tenant: &str| {
        PgVectorStore::new(
            pool_for(&db.config()).unwrap(),
            SiteId::new(tenant).unwrap(),
            DIM,
            IndexKind::Hnsw,
            Arc::clone(&runtime),
        )
        .unwrap()
    };
    let acme = Arc::new(store("acme"));
    match tokio::task::block_in_place(|| acme.prepare()) {
        Ok(()) => {}
        Err(PrepareError::ExtensionMissing) => {
            println!("SKIPPED: the test server has no pgvector extension");
            return;
        }
        Err(error) => panic!("{error}"),
    }
    let globex = Arc::new(store("globex"));
    let world = World::new().await;
    let c = corpus(&world);
    let vector = FakeEmbedding::with_dim(DIM)
        .embed(&[QUERY.to_owned()])
        .unwrap()
        .remove(0);
    let chunk = |id: RecordId, text: &str| Chunk {
        doctype: "note".to_owned(),
        id,
        version: Version(1),
        chunk_no: 0,
        fields: vec!["body".to_owned()],
        text: text.to_owned(),
        vector: vector.clone(),
    };
    tokio::task::block_in_place(|| {
        acme.upsert(&[chunk(c.open, "ACME-ONLY budget forecast")])
            .unwrap();
        globex
            .upsert(&[chunk(c.board, "GLOBEX budget forecast")])
            .unwrap();
    });
    // Alice is allowed to read both documents, so only the tenant key can keep them apart.
    world.docs.grant(c.alice, c.board);
    let tool = world.tool_over(
        globex as Arc<dyn VectorStore>,
        "globex",
        Arc::new(Guards(Arc::clone(&world.guard))),
        true,
    );
    let answer = tool
        .call(&user(c.alice, "reader"), &json!({"query": QUERY, "k": 10}))
        .await
        .unwrap();
    let text = snippets(&answer);
    assert!(
        text.contains("GLOBEX"),
        "the tenant's own chunk is found: {answer}"
    );
    assert!(
        !text.contains("ACME-ONLY"),
        "another tenant's chunk surfaced: {answer}"
    );
}
