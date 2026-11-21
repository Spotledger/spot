//! Conversations across organisations, hosted (WP-BPM-12.T7; ADR-080 section 5, ADR-058).
//!
//! Two sites live in one test process. They are joined by a real tenant-link handshake and talk
//! through the real link-exchange crates over the in-process carrier of the module
//! (`process_conversations::carry`), with the module's own receiver, peer directory, node
//! handlers (run off the runtime thread) and driver. The conversation records are the
//! library's in-memory store behind the module's `OffRuntime` adapter; the engine is the
//! in-memory one, so no database is needed. Time is a `ManualClock` per site: nothing reads a
//! wall clock.
//!
//! A "restart" rebuilds everything volatile (core, engine, handlers, trigger index, driver) from
//! the durable stores of the site, which is exactly what a restart is.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_contract::{ProcessContract, contracts, from_text as contract_from_text};
use spot_bpm_conversations::{
    ContractBook, ConversationStore, LinkSender, MemoryConversationStore, MemoryGateAudit,
    MessageSender, SendRequest, SharedSigner, message_arrived_spec, message_id,
};
use spot_bpm_engine::{
    CaseId, CaseRecord, CaseStore, CaseUpdate, EffectLedger, Engine, HandlerRegistry,
    InMemoryCaseStore, InMemoryTimerPort, NodeKind, RecordingHandler,
};
use spot_bpm_model::{NodeId, from_text};
use spot_bpm_triggers::TriggerIndex;
use spot_cli::services::process_conversations::{
    ConfigPeers, ConversationCore, CoreParts, Driver, InProcessSite, LinkResources, OffRuntime,
    PeerSetting, Settings, carry, handlers, refusals,
};
use spot_core::ManualClock;
use spot_dag_exec::{NodeStatus, RunStatus};
use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_link_exchange::{
    Conversation, InboxStore, Lifetimes, Message, ReceiveStatus, RejectCode, Retention,
};
use spot_tenancy::TenantId;
use spot_tenant_link::{
    CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, ExchangePayload, FieldList,
    FieldName, FieldValue, Grant, Handshake, HandshakePolicy, IdentitySigner, Link, LinkDraft,
    LinkId, MemoryNonceLog, MemoryPeerKeyDirectory, MemoryRegionOracle, Nonce, Offer, Party,
    PeerKey, PolicyStepUp, RevokeReason, SchemaRange, SeedSigner, StepUpProof, Topic, TopicKind,
    Trust,
};

const FAR: u64 = 1_000_000_000_000_000;
const SECOND: u64 = 1_000;
const HOUR: u64 = 3_600_000;
const T0: u64 = 10_000_000;

// --- time and futures ------------------------------------------------------------------------

fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn at(ms: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(i64::try_from(ms / 1_000).unwrap()).unwrap()
}

// --- the processes ---------------------------------------------------------------------------

const REQUESTER: &str = r#"
name = "req_flow"
app = "sales"
version = 1
status = "published"
nodes = [
  { id = "start", kind = "trigger", trigger = "manual" },
  { id = "send_request", kind = "send_message", contract = "request_approve", message = "request" },
  { id = "wait_approval", kind = "wait_message", contract = "request_approve", message = "approve", on_timeout = "timeout_edge", timeout_secs = 3600 },
  { id = "approved", kind = "document", action = "post", doctype = "Sales Order", terminal = true },
  { id = "expired", kind = "document", action = "cancel", doctype = "Sales Order", terminal = true },
]
edges = [
  { from = "start", to = "send_request" },
  { from = "send_request", to = "wait_approval" },
  { from = "wait_approval", to = "approved", condition = 'doc.bpm.wait.wait_approval.result == "received"' },
  { from = "wait_approval", to = "expired", condition = 'doc.bpm.wait.wait_approval.result == "timeout"' },
]
"#;

const APPROVER: &str = r#"
name = "appr_flow"
app = "purchasing"
version = 1
status = "published"
nodes = [
  { id = "start", kind = "trigger", trigger = "manual" },
  { id = "record", kind = "document", action = "create", doctype = "Purchase Request" },
  { id = "reply", kind = "logic", logic = "map", mapping = { "bpm.out.reference" = "doc.received.request.reference" } },
  { id = "send_approval", kind = "send_message", contract = "request_approve", message = "approve" },
  { id = "done", kind = "document", action = "post", doctype = "Purchase Request", terminal = true },
]
edges = [
  { from = "start", to = "record" },
  { from = "record", to = "reply" },
  { from = "reply", to = "send_approval" },
  { from = "send_approval", to = "done" },
]
"#;

fn contract() -> ProcessContract {
    contract_from_text(contracts::REQUEST_APPROVE).unwrap()
}

// --- the handshake: two tenants, one active link ---------------------------------------------

fn topic(name: &str) -> Topic {
    Topic::new(name).unwrap()
}

fn grant(name: &str, fields: &[&str]) -> Grant {
    Grant {
        topic: topic(name),
        kind: TopicKind::Message,
        direction: Direction::Both,
        schema_range: SchemaRange::new(1, 2).unwrap(),
        fields: FieldList::only(fields.iter().map(|f| FieldName::new(*f).unwrap())),
        expires: FAR,
        require_peer_region: None,
        notify: true,
    }
}

fn offer(side: &Party) -> Offer {
    Offer::new(
        side.clone(),
        vec![
            grant(
                "bpm.request",
                &["bpm.message", "reference", "amount_minor", "note", "reason"],
            ),
            grant("bpm.decision", &["bpm.message", "reference", "reason"]),
        ],
    )
    .unwrap()
}

struct HandshakePorts {
    keys: MemoryPeerKeyDirectory,
    log: MemoryNonceLog,
    gate: PolicyStepUp,
}

impl HandshakePorts {
    fn pinned(peer: &Party, key: PeerKey) -> Self {
        Self {
            keys: MemoryPeerKeyDirectory::with(peer.clone(), key).unwrap(),
            log: MemoryNonceLog::new(),
            gate: PolicyStepUp::new(StepUpPolicy::default()),
        }
    }

    fn policy(&self) -> HandshakePolicy<'_> {
        HandshakePolicy {
            keys: &self.keys,
            trust: Trust::PinnedOnly,
            consent: ConsentPolicy::default(),
            gate: &self.gate,
            nonces: &self.log,
        }
    }
}

fn consent(party: &Party, actor: &str, at: u64) -> Consent {
    let gate = PolicyStepUp::new(StepUpPolicy::default());
    Consent::new(
        party.clone(),
        actor,
        at,
        StepUpProof {
            operation: OperationName::new(CONSENT_OPERATION),
            method: AuthMethod::PasswordTotp,
            authenticated_at: i64::try_from(at / 1_000).unwrap() - 10,
        },
        &gate,
    )
    .unwrap()
}

fn tenant(n: u8) -> Party {
    Party::Tenant(TenantId::new_v7(1_700_000_000_000, [n; 10]))
}

/// A complete handshake between tenants `x` and `y` (seeds and ids are their numbers); returns
/// the link as each side holds it.
fn handshake(x: u8, y: u8, link_byte: u8) -> (Link, Link) {
    let (px, py) = (tenant(x), tenant(y));
    let (sx, sy) = (
        SeedSigner::from_seed([x; 32], px.clone()),
        SeedSigner::from_seed([y; 32], py.clone()),
    );
    let (audit_x, audit_y) = (
        spot_tenant_link::MemoryAuditLog::new(),
        spot_tenant_link::MemoryAuditLog::new(),
    );
    let draft = LinkDraft::new(
        LinkId::new_v7(1_700_000_000_000, [link_byte; 10]),
        px.clone(),
        py.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&px),
    )
    .unwrap();
    let nonce = |n: u8| Nonce::from_bytes([n; 16]);
    let ports_x = HandshakePorts::pinned(&py, sy.public_key());
    let ports_y = HandshakePorts::pinned(&px, sx.public_key());
    let (policy_x, policy_y) = (ports_x.policy(), ports_y.policy());
    let (mut proposer, propose) = Handshake::propose(
        &sx,
        &audit_x,
        &policy_x,
        draft,
        consent(&px, "user:x", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&sy, &audit_y, &policy_y, &propose, 1_200).unwrap();
    let accept = peer
        .accept(offer(&py), consent(&py, "user:y", 1_300), 1_300, nonce(2))
        .unwrap();
    proposer.receive(&accept, 1_400).unwrap();
    let key_message = proposer.key_exchange(1_450, nonce(3)).unwrap();
    peer.receive(&key_message, 1_500).unwrap();
    let key_back = peer.key_exchange(1_550, nonce(4)).unwrap();
    proposer.receive(&key_back, 1_600).unwrap();
    let activate = proposer.activate(1_700, nonce(5)).unwrap();
    peer.receive(&activate, 1_800).unwrap();
    (proposer.into_link(), peer.into_link())
}

// --- the sites -------------------------------------------------------------------------------

/// The durable half of one site: everything here survives a restart.
struct Site {
    role: &'static str,
    peer_role: &'static str,
    seed: u8,
    party: Party,
    resources: Arc<InProcessSite>,
    store: Arc<MemoryConversationStore>,
    gate_audit: Arc<MemoryGateAudit>,
    cases: InMemoryCaseStore,
    ledger: Arc<EffectLedger>,
    clock: Arc<ManualClock>,
    process: &'static str,
    known: Mutex<Vec<String>>,
}

fn site(
    role: &'static str,
    peer_role: &'static str,
    seed: u8,
    link: Link,
    process: &'static str,
    region: Option<Arc<MemoryRegionOracle>>,
) -> Site {
    let party = tenant(seed);
    let signer: SharedSigner = Arc::new(SeedSigner::from_seed([seed; 32], party.clone()));
    let mut resources = InProcessSite::new(
        party.clone(),
        link,
        signer,
        Lifetimes::new(Retention::from_days(3).unwrap()),
    );
    if let Some(oracle) = region {
        resources = resources.with_region(oracle);
    }
    Site {
        role,
        peer_role,
        seed,
        party,
        resources: Arc::new(resources),
        store: Arc::new(MemoryConversationStore::new()),
        gate_audit: Arc::new(MemoryGateAudit::new()),
        cases: InMemoryCaseStore::new(),
        ledger: Arc::new(EffectLedger::new()),
        clock: Arc::new(ManualClock::new(at(T0))),
        process,
        known: Mutex::new(Vec::new()),
    }
}

/// Two sites over one link: `a` the requester, `b` the approver.
struct World {
    a: Site,
    b: Site,
}

impl World {
    fn new() -> Self {
        Self::with(None, None)
    }

    fn with(
        region_a: Option<Arc<MemoryRegionOracle>>,
        region_b: Option<Arc<MemoryRegionOracle>>,
    ) -> Self {
        let (la, lb) = handshake(1, 2, 7);
        Self {
            a: site("requester", "approver", 1, la, REQUESTER, region_a),
            b: site("approver", "requester", 2, lb, APPROVER, region_b),
        }
    }
}

/// The volatile half of one site, rebuilt from the durable `Site` on every boot.
struct Booted<'s> {
    site: &'s Site,
    core: Arc<ConversationCore>,
    engine: Arc<Engine<InMemoryCaseStore, InMemoryTimerPort>>,
    driver: Driver<InMemoryCaseStore, InMemoryTimerPort>,
}

impl Site {
    fn link_id(&self) -> LinkId {
        self.resources.id()
    }

    fn boot(&self) -> Booted<'_> {
        self.boot_with(Settings::default())
    }

    fn boot_with(&self, settings: Settings) -> Booted<'_> {
        let signer: SharedSigner =
            Arc::new(SeedSigner::from_seed([self.seed; 32], self.party.clone()));
        let mut book = ContractBook::new();
        book.add(contract(), self.role).unwrap();
        let peers = Arc::new(ConfigPeers::new(
            &[PeerSetting {
                contract: "request_approve".to_owned(),
                role: self.peer_role.to_owned(),
                link: self.link_id().to_string(),
            }],
            self.resources.clone(),
            Arc::clone(&signer),
        ));
        let core = ConversationCore::new(CoreParts {
            site: self.role.to_owned(),
            clock: self.clock.clone(),
            settings,
            book,
            store: Arc::new(OffRuntime(self.store.clone())),
            peers,
            links: self.resources.clone(),
            signer,
            gate_audit: Some(self.gate_audit.clone()),
        });
        let mut registry = HandlerRegistry::logic_default();
        registry.register(RecordingHandler::new(
            NodeKind::Document,
            self.ledger.clone(),
        ));
        for handler in handlers(core.runtime()) {
            registry.register_arc(handler);
        }
        let engine = Arc::new(Engine::new(
            self.cases.clone(),
            InMemoryTimerPort::new(),
            registry,
        ));
        engine.publish(&from_text(self.process).unwrap()).unwrap();
        let mut index = TriggerIndex::new();
        if self.role == "approver" {
            index.add_spec(
                "appr_flow",
                "purchasing",
                1,
                NodeId::new("start").unwrap(),
                message_arrived_spec("request_approve", "request"),
            );
        }
        core.set_index(index);
        let driver = Driver::new(Arc::clone(&core), Arc::clone(&engine));
        Booted {
            site: self,
            core,
            engine,
            driver,
        }
    }
}

impl Booted<'_> {
    fn set_time(&self, now_ms: u64) {
        self.site.clock.set_wall(at(now_ms));
    }

    /// Starts the requester's request with its payload in the variables.
    fn start_request(&self, case: &str, reference: &str, note: &str, now_ms: u64) {
        self.set_time(now_ms);
        let variables = serde_json::json!({
            "bpm": { "out": { "reference": reference, "amount_minor": 12_500, "note": note } }
        });
        let id = CaseId::new(case).unwrap();
        self.site.known.lock().unwrap().push(case.to_owned());
        block_on(async {
            let version = from_text(self.site.process).unwrap();
            self.engine
                .start(&version, id.clone(), case, None, at(now_ms))
                .await
                .unwrap();
            let record = self.engine.load(&id).await.unwrap();
            self.engine
                .store()
                .commit(
                    &id,
                    record.run.version,
                    CaseUpdate::new().with_variables(variables),
                )
                .await
                .unwrap();
        });
    }

    /// Steps every Case the site knows of until none makes progress.
    fn settle(&self, now_ms: u64) {
        block_on(async {
            for _ in 0..16 {
                let mut ids = self
                    .engine
                    .store()
                    .list_runnable(at(now_ms), 100)
                    .await
                    .unwrap();
                for record in self.site.store.records() {
                    if let Some(case) = record.case
                        && let Ok(id) = CaseId::new(&case)
                        && !ids.contains(&id)
                    {
                        ids.push(id);
                    }
                }
                for case in self.site.known.lock().unwrap().iter() {
                    let id = CaseId::new(case).unwrap();
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                let mut progressed = false;
                for id in ids {
                    if self
                        .engine
                        .step(&id, at(now_ms))
                        .await
                        .unwrap()
                        .made_progress()
                    {
                        progressed = true;
                    }
                }
                if !progressed {
                    break;
                }
            }
        });
    }

    /// One driver pass (arrivals, timeouts), then the engine settles; twice, as a host's loop
    /// would over two ticks.
    fn advance(&self, now_ms: u64) {
        self.set_time(now_ms);
        for _ in 0..2 {
            block_on(self.driver.cycle()).unwrap();
            self.settle(now_ms);
        }
    }

    fn case(&self, id: &str) -> CaseRecord {
        block_on(self.engine.load(&CaseId::new(id).unwrap())).unwrap()
    }

    fn status(&self, case: &str, node: &str) -> NodeStatus {
        self.case(case)
            .run
            .node(&spot_dag::NodeId::new(node).unwrap())
            .unwrap()
            .status
    }
}

/// Hands everything `from` has queued to `to` once, in order, over the world's one link.
fn carry_over(from: &Booted<'_>, to: &Booted<'_>, now_ms: u64) -> Vec<Message> {
    from.set_time(now_ms);
    to.set_time(now_ms);
    carry(
        &from.site.resources,
        &to.site.resources,
        &to.core,
        &from.site.link_id(),
        now_ms,
        50,
    )
}

/// Everything the audit trails of a site hold, as text: a body must not be in it.
#[allow(dead_code)]
fn trails(site: &Site) -> String {
    format!(
        "{:?} {:?} {:?}",
        site.resources.audit.entries().unwrap(),
        site.gate_audit.rows(),
        site.resources.quarantine.held()
    )
}

// --- tests -----------------------------------------------------------------------------------

#[test]
fn site_a_sends_site_b_receives_replies_and_site_a_continues() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-SECRET-1", "please approve this", T0);
    a.settle(T0);
    assert_eq!(carry_over(&a, &b, T0 + SECOND).len(), 1);
    b.advance(T0 + 2 * SECOND);
    assert_eq!(carry_over(&b, &a, T0 + 3 * SECOND).len(), 1);
    a.advance(T0 + 4 * SECOND);

    let request = a.case("req1");
    assert_eq!(request.run.status, RunStatus::Completed);
    assert_eq!(a.status("req1", "approved"), NodeStatus::Done);
    assert_eq!(a.status("req1", "expired"), NodeStatus::Skipped);
    assert_eq!(w.a.ledger.total(), 1);
    assert_eq!(
        w.b.ledger.total(),
        2,
        "the approver recorded and posted once each"
    );
    assert!(w.a.ledger.duplicates().is_empty() && w.b.ledger.duplicates().is_empty());
    assert_eq!(b.core.stats().started, 1);
    assert_eq!(a.core.stats().woken, 1);

    // Both sides hold the one conversation, and it ended the same way.
    let records = w.b.store.records();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].role, "approver");
    assert_eq!(records[0].ending.as_ref().unwrap().outcome, "approved");
    assert_eq!(
        w.a.store.records()[0].ending.as_ref().unwrap().outcome,
        "approved"
    );
}

#[test]
fn a_replay_of_the_same_message_id_applies_once() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    let batch = carry_over(&a, &b, T0 + SECOND);
    assert_eq!(batch.len(), 1);
    // The carrier delivers the same message again, twice.
    for n in 2..4 {
        b.set_time(T0 + n * SECOND);
        let outcome = b
            .core
            .receive_message(&w.b.link_id(), &w.a.resources.connection(), &batch[0])
            .unwrap();
        assert_eq!(outcome.status, ReceiveStatus::Duplicate);
    }
    b.advance(T0 + 5 * SECOND);
    carry_over(&b, &a, T0 + 6 * SECOND);
    a.advance(T0 + 7 * SECOND);
    assert_eq!(w.b.store.records().len(), 1, "one conversation");
    assert_eq!(w.b.ledger.total(), 2, "the approver acted once");
    assert_eq!(b.core.stats().started, 1);
    assert_eq!(a.case("req1").run.status, RunStatus::Completed);
}

#[test]
fn revoking_the_link_mid_conversation_refuses_the_next_message() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    carry_over(&a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    // The reply is queued at B; A revokes the link before it arrives.
    let revoked =
        w.a.resources
            .current(&w.a.link_id())
            .unwrap()
            .revoked_by(&w.a.party, RevokeReason::SecurityConcern, T0 + 2 * SECOND)
            .unwrap();
    w.a.resources.replace_link(revoked);
    carry_over(&b, &a, T0 + 3 * SECOND);
    a.advance(T0 + 4 * SECOND);

    assert_ne!(
        a.case("req1").run.status,
        RunStatus::Completed,
        "the reply of a revoked link woke nothing"
    );
    assert_ne!(a.status("req1", "approved"), NodeStatus::Done);
    assert_eq!(a.core.stats().woken, 0);
    let stats = a.core.stats();
    assert!(
        stats.refused + stats.not_taken + stats.errors >= 1,
        "the message was refused, not taken or failed: {stats:?}"
    );
    // A's sender is stopped by the same revocation: it cannot send on the link any more.
    a.start_request("req2", "REF-2", "n", T0 + 5 * SECOND);
    a.settle(T0 + 5 * SECOND);
    assert_eq!(carry_over(&a, &b, T0 + 6 * SECOND).len(), 0);
}

#[test]
fn a_reply_on_a_different_link_wakes_nothing_and_creates_nothing() {
    let w = World::new();
    // A second peer, C, linked to A on its own link.
    let (la2, lc) = handshake(1, 3, 8);
    let l2 = la2.id;
    w.a.resources.add_link(la2);
    let c = site("approver", "requester", 3, lc, APPROVER, None);

    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    carry_over(&a, &b, T0 + SECOND);
    let conversation = w.a.store.records()[0].conversation.clone();

    // C names A's conversation id on its own link, with the header of the awaited reply.
    let signer: SharedSigner = Arc::new(SeedSigner::from_seed([3; 32], c.party.clone()));
    let sender = LinkSender::new(
        c.resources.link(&l2).unwrap(),
        c.resources.outbox.clone(),
        c.resources.audit.clone(),
        signer,
        Lifetimes::new(Retention::from_days(3).unwrap()),
    );
    let mut payload = ExchangePayload::new();
    payload.insert(
        FieldName::new("bpm.message").unwrap(),
        FieldValue::Text("request_approve@1/approve".to_owned()),
    );
    payload.insert(
        FieldName::new("reference").unwrap(),
        FieldValue::Text("REF-1".to_owned()),
    );
    sender
        .send(&SendRequest {
            conversation: &conversation,
            topic: &Topic::new("bpm.decision").unwrap(),
            schema_version: 1,
            id: message_id(T0, &l2, "rogue"),
            payload: &payload,
            now_ms: T0 + 2 * SECOND,
            actor: "system",
        })
        .unwrap();
    a.set_time(T0 + 3 * SECOND);
    let batch = carry(
        &c.resources,
        &w.a.resources,
        &a.core,
        &l2,
        T0 + 3 * SECOND,
        10,
    );
    assert_eq!(batch.len(), 1);
    a.advance(T0 + 4 * SECOND);

    assert_ne!(a.case("req1").run.status, RunStatus::Completed);
    assert_eq!(a.core.stats().woken, 0);
    assert!(
        w.a.store.load(&l2, &conversation).unwrap().is_none(),
        "no conversation was created on the other link"
    );
    assert_eq!(w.a.store.records().len(), 1);
}

#[test]
fn a_grant_that_requires_a_region_refuses_a_peer_the_oracle_places_elsewhere() {
    let region = |name: &str| spot_data_policy::Region::new(name).unwrap();
    for (placed, expected_started) in [(Some("eu"), 1), (Some("us"), 0), (None, 0)] {
        let oracle = Arc::new(MemoryRegionOracle::new());
        if let Some(name) = placed {
            oracle.insert(tenant(1), region(name)).unwrap();
        }
        let w = World::with(None, Some(oracle));
        // B's grants require the peer in the EU.
        let mut link = w.b.resources.current(&w.b.link_id()).unwrap();
        for grant in &mut link.grants {
            grant.require_peer_region = Some(region("eu"));
        }
        w.b.resources.replace_link(link);
        let (a, b) = (w.a.boot(), w.b.boot());
        a.start_request("req1", "REF-1", "n", T0);
        a.settle(T0);
        carry_over(&a, &b, T0 + SECOND);
        b.advance(T0 + 2 * SECOND);
        assert_eq!(
            usize::try_from(b.core.stats().started).unwrap(),
            expected_started,
            "placed in {placed:?}"
        );
        assert_eq!(w.b.store.records().len(), expected_started);
    }
}

#[test]
fn a_timeout_ends_the_wait_and_a_late_reply_is_parked_not_applied() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    carry_over(&a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    // The link is down for two hours: the reply sits at B, the wait limit (one hour) passes.
    a.advance(T0 + 2 * HOUR);
    assert_eq!(a.case("req1").run.status, RunStatus::Completed);
    assert_eq!(
        a.status("req1", "expired"),
        NodeStatus::Done,
        "the timeout edge was taken"
    );
    assert_eq!(a.status("req1", "approved"), NodeStatus::Skipped);
    assert_eq!(a.core.stats().timed_out, 1);
    let ledger_before = w.a.ledger.total();

    // The late reply arrives: it is recorded and held, and applies nothing.
    carry_over(&b, &a, T0 + 2 * HOUR + SECOND);
    a.advance(T0 + 2 * HOUR + 2 * SECOND);
    assert_eq!(w.a.ledger.total(), ledger_before);
    assert_eq!(a.status("req1", "approved"), NodeStatus::Skipped);
    let record = &w.a.store.records()[0];
    assert!(record.ending.as_ref().unwrap().timed_out);
    assert!(record.waits.is_empty());
}

#[test]
fn a_restart_of_either_site_mid_conversation_resumes_from_the_stores() {
    let w = World::new();
    {
        let a = w.a.boot();
        a.start_request("req1", "REF-1", "n", T0);
        a.settle(T0);
    }
    // A restarts; the request travels; B restarts before it runs.
    let (a, b) = (w.a.boot(), w.b.boot());
    carry_over(&a, &b, T0 + SECOND);
    drop(b);
    let b = w.b.boot();
    b.advance(T0 + 2 * SECOND);
    drop(a);
    let a = w.a.boot();
    carry_over(&b, &a, T0 + 3 * SECOND);
    drop(b);
    a.advance(T0 + 4 * SECOND);
    assert_eq!(a.case("req1").run.status, RunStatus::Completed);
    assert_eq!(a.status("req1", "approved"), NodeStatus::Done);
    assert_eq!(w.b.ledger.total(), 2);
    assert_eq!(w.a.ledger.total(), 1);
}

#[test]
fn a_link_over_its_message_rate_is_not_taken_audited_and_taken_next_minute() {
    let w = World::new();
    let settings = Settings {
        max_messages_per_minute: 1,
        ..Settings::default()
    };
    let (a, b) = (w.a.boot(), w.b.boot_with(settings));
    a.start_request("req1", "REF-1", "n", T0);
    a.start_request("req2", "REF-2", "n", T0);
    a.settle(T0);
    let first = T0 + SECOND;
    assert_eq!(carry_over(&a, &b, first).len(), 2);
    let stats = b.core.stats();
    assert_eq!(stats.rate_limited, 1);
    assert!(w.b.gate_audit.codes().contains(&refusals::RATE_LIMITED));
    assert!(b.core.warnings().iter().any(|w| w.contains("rate")));
    b.advance(first + SECOND);
    assert_eq!(
        w.b.store.records().len(),
        1,
        "only one conversation was taken"
    );
    // Later the sender's second message is carried again and taken: it was never acknowledged.
    for later in [
        first + 61 * SECOND,
        first + 10 * 60 * SECOND,
        first + 2 * HOUR,
    ] {
        carry_over(&a, &b, later);
        b.advance(later + SECOND);
        if w.b.store.records().len() == 2 {
            break;
        }
    }
    assert_eq!(w.b.store.records().len(), 2);
}

#[test]
fn a_payload_over_the_size_bound_is_refused_with_an_audit_row_and_starts_nothing() {
    let w = World::new();
    let settings = Settings {
        // Below the 1 KiB a loaded section allows: the shipped contract caps a note at 500
        // characters, so the sender could never produce a message past 1 KiB.
        max_payload_bytes: 256,
        ..Settings::default()
    };
    let (a, b) = (w.a.boot(), w.b.boot_with(settings));
    a.start_request("req1", "REF-1", &"x".repeat(450), T0);
    a.settle(T0);
    carry_over(&a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    assert_eq!(b.core.stats().too_large, 1);
    assert!(w.b.gate_audit.codes().contains(&refusals::TOO_LARGE));
    assert!(w.b.store.records().is_empty());
    assert_eq!(b.core.stats().started, 0);
}

#[test]
fn a_message_on_an_unknown_link_is_not_taken_and_stores_nothing() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    let batch = carry_over(&a, &b, T0 + SECOND);
    let unknown = LinkId::new_v7(1_700_000_000_000, [99u8; 10]);
    let outcome = b
        .core
        .receive_message(&unknown, &w.a.resources.connection(), &batch[0])
        .unwrap();
    assert_eq!(outcome.status, ReceiveStatus::Busy);
    assert!(
        outcome.receipts.is_empty(),
        "no answer confirms or denies anything"
    );
    assert_eq!(b.core.stats().not_taken, 1);
}

#[test]
fn a_failing_conversation_store_fails_the_receive_closed() {
    // A store whose every call fails: the receive is an error, nothing is acknowledged.
    struct Broken;
    type Rec = spot_bpm_conversations::ConversationRecord;
    type Res<T> = spot_bpm_conversations::Result<T>;
    const DOWN: spot_bpm_conversations::ConversationError =
        spot_bpm_conversations::ConversationError::Store;
    impl ConversationStore for Broken {
        fn load(&self, _: &LinkId, _: &Conversation) -> Res<Option<Rec>> {
            Err(DOWN)
        }
        fn create(&self, _: Rec) -> Res<Rec> {
            Err(DOWN)
        }
        fn save(&self, _: Rec) -> Res<Rec> {
            Err(DOWN)
        }
        fn waiting(&self) -> Res<Vec<Rec>> {
            Err(DOWN)
        }
        fn of_case(&self, _: &str) -> Res<Vec<Rec>> {
            Err(DOWN)
        }
        fn open_on(&self, _: &LinkId) -> Res<usize> {
            Err(DOWN)
        }
        fn rejection(
            &self,
            _: &LinkId,
            _: &spot_link_exchange::MessageId,
        ) -> Res<Option<RejectCode>> {
            Err(DOWN)
        }
        fn note_rejection(
            &self,
            _: LinkId,
            _: spot_link_exchange::MessageId,
            _: RejectCode,
        ) -> Res<()> {
            Err(DOWN)
        }
    }

    let w = World::new();
    let a = w.a.boot();
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    let batch = queued(&w.a, T0 + SECOND);
    assert_eq!(batch.len(), 1);
    let signer: SharedSigner = Arc::new(SeedSigner::from_seed([2; 32], w.b.party.clone()));
    let mut book = ContractBook::new();
    book.add(contract(), "approver").unwrap();
    let peers = Arc::new(ConfigPeers::new(
        &[],
        w.b.resources.clone(),
        Arc::clone(&signer),
    ));
    let broken = ConversationCore::new(CoreParts {
        site: "broken".to_owned(),
        clock: w.b.clock.clone(),
        settings: Settings::default(),
        book,
        store: Arc::new(Broken),
        peers,
        links: w.b.resources.clone(),
        signer,
        gate_audit: Some(Arc::new(MemoryGateAudit::new())),
    });
    let result = broken.receive_message(&w.b.link_id(), &w.a.resources.connection(), &batch[0]);
    assert!(
        result.is_err(),
        "a store that cannot answer fails the receive"
    );
    assert_eq!(broken.stats().errors, 1);
    assert!(
        w.b.resources
            .inbox
            .entry(&w.b.link_id(), &batch[0].id)
            .unwrap()
            .is_none(),
        "nothing was taken in"
    );
}

/// The messages `from` has queued, taken without delivering them.
fn queued(from: &Site, now_ms: u64) -> Vec<Message> {
    let ids = TestIds::default();
    let signer = SeedSigner::from_seed([from.seed; 32], from.party.clone());
    let outbox = spot_link_exchange::Outbox::new(
        &*from.resources.outbox,
        &*from.resources.audit,
        &signer,
        &ids,
        Lifetimes::new(Retention::from_days(3).unwrap()),
    );
    let link = from.resources.current(&from.link_id()).unwrap();
    outbox
        .next_batch(&link, now_ms, 10, "system", None)
        .unwrap()
}

#[derive(Default)]
struct TestIds(std::sync::atomic::AtomicU64);

impl spot_link_exchange::IdSource for TestIds {
    fn next_id(&self, now: u64) -> spot_link_exchange::MessageId {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut entropy = [0u8; 10];
        entropy[..8].copy_from_slice(&n.to_be_bytes());
        spot_link_exchange::MessageId::new_v7(now, entropy)
    }
}

#[test]
fn two_sites_in_one_process_never_see_each_others_runs() {
    let w = World::new();
    let (a, b) = (w.a.boot(), w.b.boot());
    a.start_request("req1", "REF-1", "n", T0);
    a.settle(T0);
    carry_over(&a, &b, T0 + SECOND);
    b.advance(T0 + 2 * SECOND);
    // B's Case ids are not A's, and neither engine can load the other's Case by id.
    let b_case = w.b.store.records()[0].case.clone().unwrap();
    assert!(
        block_on(a.engine.store().load(&CaseId::new(&b_case).unwrap()))
            .unwrap()
            .is_none()
    );
    assert!(
        block_on(b.engine.store().load(&CaseId::new("req1").unwrap()))
            .unwrap()
            .is_none()
    );
    // Nor can one site's conversation store list the other's records.
    assert!(w.a.store.of_case(&b_case).unwrap().is_empty());
    assert!(w.b.store.of_case("req1").unwrap().is_empty());
}
