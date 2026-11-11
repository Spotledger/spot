//! Shared fixtures: two tenants joined by one complete handshake over the contract's topics, each
//! with its own outbox, inbox, quarantine, conversation store, Case store and effect ledger, and a
//! `boot` that rebuilds everything volatile (engine, handlers, timer port, trigger intake) from
//! the durable stores, which is exactly what a restart is.
//!
//! Every instant is an argument; nothing reads a clock.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::future::Future;
use std::sync::{Arc, RwLock};
use std::task::{Context, Poll, Waker};

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_contract::{ProcessContract, contracts, from_text as contract_from_text};
use spot_bpm_conversations::{
    CompensationHook, ContractBook, Conversations, EngineCaseStarter, LinkLimits, LinkSender,
    MemoryConversationStore, MemoryGateAudit, MessageSender, Runtime, StaticPeers, WaitFor,
    message_arrived_spec, trigger,
};
use spot_bpm_engine::{
    CaseId, CaseRecord, CaseStore, CaseUpdate, EffectLedger, Engine, EngineError, HandlerRegistry,
    InMemoryCaseStore, InMemoryTimerPort, NodeKind, RecordingHandler, ScriptedFaults,
};
use spot_bpm_model::{NodeId, from_text};
use spot_bpm_triggers::{AllowAll, TriggerIndex, TriggerIntake, TriggerRegistry};
use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_link_exchange::{
    Conversation, IdSource, Inbox, InputQuarantine, Lifetimes, MemoryInbox, MemoryOutbox,
    MemoryQuarantine, Message, MessageId, Outbox, Retention, TransportParty,
};
use spot_tenancy::TenantId;
use spot_tenant_link::{
    CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, FieldList, FieldName, Grant,
    Handshake, HandshakePolicy, IdentitySigner, Link, LinkDraft, LinkId, MemoryAuditLog,
    MemoryNonceLog, MemoryPeerKeyDirectory, Nonce, Offer, Party, PeerKey, PolicyStepUp,
    SchemaRange, SeedSigner, StepUpProof, Topic, TopicKind, Trust,
};

pub const FAR: u64 = 1_000_000_000_000_000;
pub const SECOND: u64 = 1_000;
pub const HOUR: u64 = 3_600_000;
pub const DAY: u64 = 86_400_000;
/// Test time starts here, well after the handshake.
pub const T0: u64 = 10_000_000;

/// Runs a future to completion on the current thread (every future here is immediately ready).
/// What a carrier would hand the exchange: the party the connection authenticated.
pub fn connection(party: &Party) -> TransportParty {
    TransportParty::from_verified_connection(party.clone())
}

pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

pub fn at(ms: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(i64::try_from(ms / 1_000).unwrap()).unwrap()
}

pub fn contract() -> ProcessContract {
    contract_from_text(contracts::REQUEST_APPROVE).unwrap()
}

/// The requester half: send a request, wait for the approval, record it; take the timeout edge
/// when the answer does not come.
pub const REQUESTER: &str = r#"
name = "req_flow"
app = "sales"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "send_request"
kind = "send_message"
contract = "request_approve"
message = "request"

[[nodes]]
id = "wait_approval"
kind = "wait_message"
contract = "request_approve"
message = "approve"
on_timeout = "timeout_edge"
timeout_secs = 3600

[[nodes]]
id = "approved"
kind = "document"
action = "post"
doctype = "Sales Order"
terminal = true

[[nodes]]
id = "expired"
kind = "document"
action = "cancel"
doctype = "Sales Order"
terminal = true

[[edges]]
from = "start"
to = "send_request"

[[edges]]
from = "send_request"
to = "wait_approval"

[[edges]]
from = "wait_approval"
to = "approved"
condition = 'doc.bpm.wait.wait_approval.result == "received"'

[[edges]]
from = "wait_approval"
to = "expired"
condition = 'doc.bpm.wait.wait_approval.result == "timeout"'
"#;

/// The approver half: a message starts it, it records the request and answers with an approval.
pub const APPROVER: &str = r#"
name = "appr_flow"
app = "purchasing"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "record"
kind = "document"
action = "create"
doctype = "Purchase Request"

[[nodes]]
id = "reply"
kind = "logic"
logic = "map"

[nodes.mapping]
"bpm.out.reference" = "doc.received.request.reference"

[[nodes]]
id = "send_approval"
kind = "send_message"
contract = "request_approve"
message = "approve"

[[nodes]]
id = "done"
kind = "document"
action = "post"
doctype = "Purchase Request"
terminal = true

[[edges]]
from = "start"
to = "record"

[[edges]]
from = "record"
to = "reply"

[[edges]]
from = "reply"
to = "send_approval"

[[edges]]
from = "send_approval"
to = "done"
"#;

/// An approver that, after recording the request, waits for the requester to cancel it.
pub const WATCHER: &str = r#"
name = "appr_flow"
app = "purchasing"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "record"
kind = "document"
action = "create"
doctype = "Purchase Request"

[[nodes]]
id = "wait_cancel"
kind = "wait_message"
contract = "request_approve"
message = "cancel"
on_timeout = "timeout_edge"
timeout_secs = 3600

[[nodes]]
id = "cancelled"
kind = "document"
action = "cancel"
doctype = "Purchase Request"
terminal = true

[[nodes]]
id = "quiet"
kind = "document"
action = "update"
doctype = "Purchase Request"
terminal = true

[[edges]]
from = "start"
to = "record"

[[edges]]
from = "record"
to = "wait_cancel"

[[edges]]
from = "wait_cancel"
to = "cancelled"
condition = 'doc.bpm.wait.wait_cancel.result == "received"'

[[edges]]
from = "wait_cancel"
to = "quiet"
condition = 'doc.bpm.wait.wait_cancel.result == "timeout"'
"#;

/// A requester that sends its request and then, without waiting, cancels it.
pub const CANCELLER: &str = r#"
name = "req_flow"
app = "sales"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "send_request"
kind = "send_message"
contract = "request_approve"
message = "request"

[[nodes]]
id = "send_cancel"
kind = "send_message"
contract = "request_approve"
message = "cancel"
mapping = "cancel_out"

[[nodes]]
id = "done"
kind = "document"
action = "cancel"
doctype = "Sales Order"
terminal = true

[[edges]]
from = "start"
to = "send_request"

[[edges]]
from = "send_request"
to = "send_cancel"

[[edges]]
from = "send_cancel"
to = "done"
"#;

/// An approver with two waits for the requester's cancellation: the first is satisfied by it and
/// the second, which nothing can satisfy any more, is failed to its error branch.
pub const WATCHER2: &str = r#"
name = "appr_flow"
app = "purchasing"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "record"
kind = "document"
action = "create"
doctype = "Purchase Request"

[[nodes]]
id = "split"
kind = "logic"
logic = "parallel"

[[nodes]]
id = "wait_cancel"
kind = "wait_message"
contract = "request_approve"
message = "cancel"
timeout_secs = 3600

[[nodes]]
id = "wait_second"
kind = "wait_message"
contract = "request_approve"
message = "cancel"
timeout_secs = 3600
error = "second_failed"

[[nodes]]
id = "cancelled"
kind = "document"
action = "cancel"
doctype = "Purchase Request"
terminal = true

[[nodes]]
id = "second_done"
kind = "document"
action = "update"
doctype = "Purchase Request"
terminal = true

[[nodes]]
id = "second_failed"
kind = "document"
action = "update"
doctype = "Purchase Request"
terminal = true

[[edges]]
from = "start"
to = "record"

[[edges]]
from = "record"
to = "split"

[[edges]]
from = "split"
to = "wait_cancel"
condition = "1 == 1"

[[edges]]
from = "split"
to = "wait_second"
condition = "2 == 2"

[[edges]]
from = "wait_cancel"
to = "cancelled"

[[edges]]
from = "wait_second"
to = "second_done"
condition = 'doc.bpm.wait.wait_second.result == "received"'
"#;

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

/// The grants a contract's topics need: the contract fields and the reserved message field.
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

struct Ports {
    keys: MemoryPeerKeyDirectory,
    log: MemoryNonceLog,
    gate: PolicyStepUp,
}

impl Ports {
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

/// A counter-based id source for the receiving side's unrelated needs; sends use preset ids.
#[derive(Default)]
pub struct Counter(std::sync::atomic::AtomicU64);

impl IdSource for Counter {
    fn next_id(&self, now: u64) -> MessageId {
        let n = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let mut entropy = [0u8; 10];
        entropy[..8].copy_from_slice(&n.to_be_bytes());
        MessageId::new_v7(now, entropy)
    }
}

/// Which process half a side runs.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Half {
    Requester,
    Canceller,
    Approver,
    Watcher,
    Watcher2,
}

/// The durable half of one tenant. Everything here survives a restart.
pub struct Side {
    pub role: &'static str,
    pub party: Party,
    pub signer: Arc<SeedSigner>,
    pub link: Arc<RwLock<Link>>,
    pub audit: Arc<MemoryAuditLog>,
    pub outbox: Arc<MemoryOutbox>,
    pub inbox: Arc<MemoryInbox>,
    pub quarantine: Arc<MemoryQuarantine>,
    /// Replaces `quarantine` as the port the gate and the inbox hold into, for a case about a
    /// quarantine that behaves unlike the in-memory one (one that cannot count).
    pub quarantine_override: Option<Arc<dyn InputQuarantine>>,
    pub conversations: Arc<MemoryConversationStore>,
    /// The gate's audit trail, so a test can read the refusals back.
    pub gate_audit: Arc<MemoryGateAudit>,
    /// The per-link bounds the gate enforces.
    pub limits: Arc<LinkLimits>,
    pub cases: InMemoryCaseStore,
    pub ledger: Arc<EffectLedger>,
    pub half: Half,
    pub wait_for: WaitFor,
    pub lifetimes: Lifetimes,
    pub route_known: bool,
    pub known: std::sync::Mutex<Vec<String>>,
}

/// Two tenants over one link: `a` the requester, `b` the approver.
pub struct World {
    pub a: Side,
    pub b: Side,
}

fn side(role: &'static str, party: Party, signer: SeedSigner, link: Link, half: Half) -> Side {
    Side {
        role,
        party,
        signer: Arc::new(signer),
        link: Arc::new(RwLock::new(link)),
        audit: Arc::new(MemoryAuditLog::new()),
        outbox: Arc::new(MemoryOutbox::new()),
        inbox: Arc::new(MemoryInbox::new()),
        quarantine: Arc::new(MemoryQuarantine::new()),
        quarantine_override: None,
        conversations: Arc::new(MemoryConversationStore::new()),
        gate_audit: Arc::new(MemoryGateAudit::new()),
        limits: Arc::new(LinkLimits::new()),
        cases: InMemoryCaseStore::new(),
        ledger: Arc::new(EffectLedger::new()),
        half,
        wait_for: WaitFor::Queued,
        lifetimes: Lifetimes::new(Retention::from_days(3).unwrap()),
        route_known: true,
        known: std::sync::Mutex::new(Vec::new()),
    }
}

impl World {
    pub fn new() -> Self {
        Self::with_halves(Half::Approver)
    }

    pub fn with_halves(approver: Half) -> Self {
        let a = Party::Tenant(TenantId::new_v7(1_700_000_000_000, [1; 10]));
        let b = Party::Tenant(TenantId::new_v7(1_700_000_000_000, [2; 10]));
        let signer_a = SeedSigner::from_seed([1u8; 32], a.clone());
        let signer_b = SeedSigner::from_seed([2u8; 32], b.clone());
        let hs_audit_a = MemoryAuditLog::new();
        let hs_audit_b = MemoryAuditLog::new();
        let draft = LinkDraft::new(
            LinkId::new_v7(1_700_000_000_000, [7u8; 10]),
            a.clone(),
            b.clone(),
            Carrier::Direct,
            1_000,
            None,
            offer(&a),
        )
        .unwrap();
        let nonce = |n: u8| Nonce::from_bytes([n; 16]);
        let ports_a = Ports::pinned(&b, signer_b.public_key());
        let ports_b = Ports::pinned(&a, signer_a.public_key());
        let policy_a = ports_a.policy();
        let policy_b = ports_b.policy();
        let (mut proposer, propose) = Handshake::propose(
            &signer_a,
            &hs_audit_a,
            &policy_a,
            draft,
            consent(&a, "user:a", 1_100),
            1_100,
            nonce(1),
        )
        .unwrap();
        let mut peer =
            Handshake::from_proposal(&signer_b, &hs_audit_b, &policy_b, &propose, 1_200).unwrap();
        let accept = peer
            .accept(offer(&b), consent(&b, "user:b", 1_300), 1_300, nonce(2))
            .unwrap();
        proposer.receive(&accept, 1_400).unwrap();
        let key_message = proposer.key_exchange(1_450, nonce(3)).unwrap();
        peer.receive(&key_message, 1_500).unwrap();
        let key_back = peer.key_exchange(1_550, nonce(4)).unwrap();
        proposer.receive(&key_back, 1_600).unwrap();
        let activate = proposer.activate(1_700, nonce(5)).unwrap();
        peer.receive(&activate, 1_800).unwrap();
        Self {
            a: side(
                "requester",
                a.clone(),
                SeedSigner::from_seed([1u8; 32], a),
                proposer.into_link(),
                Half::Requester,
            ),
            b: side(
                "approver",
                b.clone(),
                SeedSigner::from_seed([2u8; 32], b),
                peer.into_link(),
                approver,
            ),
        }
    }
}

/// The volatile half of one tenant: rebuilt from the durable `Side` on every boot.
pub struct Booted<'s> {
    pub side: &'s Side,
    pub runtime: Arc<Runtime>,
    pub engine: Arc<Engine<InMemoryCaseStore, InMemoryTimerPort>>,
    pub driver: Conversations<InMemoryCaseStore, InMemoryTimerPort>,
    pub sender: Arc<LinkSender>,
}

impl Side {
    pub fn link(&self) -> Link {
        self.link.read().unwrap().clone()
    }

    /// Where refused inputs go: the in-memory quarantine the tests read back, unless the case
    /// replaced it with one that behaves differently.
    pub fn quarantine_port(&self) -> &dyn InputQuarantine {
        match &self.quarantine_override {
            Some(port) => port.as_ref(),
            None => self.quarantine.as_ref(),
        }
    }

    pub fn link_id(&self) -> LinkId {
        self.link().id
    }

    pub fn peer(&self) -> Party {
        self.link().peer().unwrap().clone()
    }

    /// Builds the engine, handlers, trigger intake and driver from the durable stores.
    pub fn boot(&self) -> Booted<'_> {
        self.boot_with(None)
    }

    /// As [`Side::boot`], with a compensation hook.
    pub fn boot_with(&self, hook: Option<Arc<dyn CompensationHook>>) -> Booted<'_> {
        self.boot_all(hook, None)
    }

    /// As [`Side::boot`], with the engine crashing where `faults` says.
    pub fn boot_faulty(&self, faults: Arc<ScriptedFaults>) -> Booted<'_> {
        self.boot_all(None, Some(faults))
    }

    fn boot_all(
        &self,
        hook: Option<Arc<dyn CompensationHook>>,
        faults: Option<Arc<ScriptedFaults>>,
    ) -> Booted<'_> {
        let sender = Arc::new(LinkSender::new(
            self.link.clone(),
            self.outbox.clone(),
            self.audit.clone(),
            self.signer.clone(),
            self.lifetimes.clone(),
        ));
        let mut book = ContractBook::new();
        book.add(contract(), self.role).unwrap();
        let peer_role = if self.role == "requester" {
            "approver"
        } else {
            "requester"
        };
        let peers = if self.route_known {
            StaticPeers::new().with_peer(
                "request_approve",
                peer_role,
                self.link_id(),
                Some(sender.clone() as Arc<dyn MessageSender>),
            )
        } else {
            StaticPeers::new()
        };
        let runtime = Arc::new(
            Runtime::new(
                book,
                self.conversations.clone(),
                Arc::new(peers),
                self.gate_audit.clone(),
            )
            .with_limits(self.limits.clone())
            .with_default_wait_for(self.wait_for),
        );
        let mut registry = HandlerRegistry::logic_default();
        registry.register(RecordingHandler::new(
            NodeKind::Document,
            self.ledger.clone(),
        ));
        spot_bpm_conversations::register(&mut registry, &runtime);
        let mut engine = Engine::new(self.cases.clone(), InMemoryTimerPort::new(), registry);
        if let Some(faults) = faults {
            engine = engine.with_faults(faults);
        }
        let engine = Arc::new(engine);
        let text = match self.half {
            Half::Requester => REQUESTER,
            Half::Canceller => CANCELLER,
            Half::Approver => APPROVER,
            Half::Watcher => WATCHER,
            Half::Watcher2 => WATCHER2,
        };
        let version = from_text(text).unwrap();
        engine.publish(&version).unwrap();

        let mut index = TriggerIndex::new();
        if !matches!(self.half, Half::Requester | Half::Canceller) {
            index.add_spec(
                "appr_flow",
                "purchasing",
                1,
                NodeId::new("start").unwrap(),
                message_arrived_spec("request_approve", "request"),
            );
        }
        let mut registry = TriggerRegistry::with_builtin_kinds();
        trigger::register(&mut registry);
        let starter = Arc::new(EngineCaseStarter::new(
            engine.clone(),
            self.conversations.clone(),
        ));
        let intake = Arc::new(TriggerIntake::new(
            Arc::new(index),
            Arc::new(registry),
            starter,
            Arc::new(AllowAll),
        ));
        let mut driver = Conversations::new(runtime.clone(), engine.clone(), intake);
        if let Some(hook) = hook {
            driver = driver.with_compensation(hook);
        }
        Booted {
            side: self,
            runtime,
            engine,
            driver,
            sender,
        }
    }
}

impl Booted<'_> {
    /// Starts the requester's request with its payload in the variables.
    pub fn start_request(&self, case: &str, reference: &str, now_ms: u64) {
        let variables = serde_json::json!({
            "bpm": { "out": {
                "reference": reference,
                "amount_minor": 12_500,
                "note": "please approve"
            } },
            "cancel_out": { "reference": reference }
        });
        self.start_with(case, variables, now_ms);
    }

    /// Starts the requester's request with exactly these variables.
    pub fn start_with(&self, case: &str, variables: serde_json::Value, now_ms: u64) {
        let text = match self.side.half {
            Half::Canceller => CANCELLER,
            _ => REQUESTER,
        };
        self.start_text(case, text, variables, now_ms);
    }

    /// Starts a Case of the given process text with exactly these variables.
    pub fn start_text(&self, case: &str, text: &str, variables: serde_json::Value, now_ms: u64) {
        let id = CaseId::new(case).unwrap();
        self.side.known.lock().unwrap().push(case.to_owned());
        block_on(async {
            let version = from_text(text).unwrap();
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

    /// Steps every runnable Case until none makes progress.
    pub fn settle(&self, now_ms: u64) {
        self.settle_result(now_ms).unwrap();
    }

    /// As [`Booted::settle`], reporting an injected crash instead of panicking.
    pub fn settle_result(&self, now_ms: u64) -> Result<(), EngineError> {
        block_on(async {
            for _ in 0..16 {
                let mut ids = self.engine.store().list_runnable(at(now_ms), 100).await?;
                // A Case a crash left with a node running is not "runnable" to the store; step
                // every Case the side knows of.
                for record in self.side.conversations.records() {
                    if let Some(case) = record.case
                        && let Ok(id) = CaseId::new(&case)
                        && !ids.contains(&id)
                    {
                        ids.push(id);
                    }
                }
                for case in self.side.known.lock().unwrap().iter() {
                    let id = CaseId::new(case).unwrap();
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
                let mut progressed = false;
                for id in ids {
                    if self.engine.step(&id, at(now_ms)).await?.made_progress() {
                        progressed = true;
                    }
                }
                if !progressed {
                    break;
                }
            }
            Ok(())
        })
    }

    /// Reads the notice feed and takes the time-driven turns, then settles the engine.
    pub fn advance(&self, now_ms: u64) {
        self.advance_result(now_ms).unwrap();
    }

    /// As [`Booted::advance`], reporting an injected crash instead of panicking.
    pub fn advance_result(&self, now_ms: u64) -> Result<(), String> {
        let side = self.side;
        for _ in 0..2 {
            block_on(async {
                self.driver
                    .process_arrivals(&*side.inbox, &*side.inbox, at(now_ms))
                    .await?;
                self.driver.tick(at(now_ms)).await
            })
            .map_err(|error| error.code().to_owned())?;
            self.settle_result(now_ms)
                .map_err(|error| error.code().to_owned())?;
        }
        Ok(())
    }

    pub fn case(&self, id: &str) -> CaseRecord {
        block_on(self.engine.load(&CaseId::new(id).unwrap())).unwrap()
    }

    pub fn all_cases(&self) -> Vec<CaseId> {
        block_on(
            self.engine
                .store()
                .list_runnable(at(T0 + 1_000 * DAY), 1_000),
        )
        .unwrap()
    }
}

/// Delivers one message to `to` as `from_peer`, answering the sender with the receipts. Returns
/// the receive status.
pub fn deliver(
    to: &Booted<'_>,
    from: &Side,
    message: &Message,
    now_ms: u64,
) -> spot_link_exchange::ReceiveStatus {
    deliver_with(to, from, message, now_ms, true)
}

/// As [`deliver`], optionally losing the receipts on the way back.
pub fn deliver_with(
    to: &Booted<'_>,
    from: &Side,
    message: &Message,
    now_ms: u64,
    receipts_arrive: bool,
) -> spot_link_exchange::ReceiveStatus {
    through_gate(to, from, message, now_ms, receipts_arrive, true)
}

/// As [`deliver`], but through a gate with no notice feed: it judges the conversation record as
/// the store holds it, without recording anything the inbox already took.
pub fn deliver_stale(
    to: &Booted<'_>,
    from: &Side,
    message: &Message,
    now_ms: u64,
) -> spot_link_exchange::ReceiveStatus {
    through_gate(to, from, message, now_ms, true, false)
}

fn through_gate(
    to: &Booted<'_>,
    from: &Side,
    message: &Message,
    now_ms: u64,
    receipts_arrive: bool,
    with_ledger: bool,
) -> spot_link_exchange::ReceiveStatus {
    let side = to.side;
    let link = side.link();
    let inbox = Inbox::new(
        &*side.inbox,
        &*side.audit,
        side.quarantine_port(),
        &*side.signer,
    );
    let gate = spot_bpm_conversations::ConformantInbox::new(
        &inbox,
        &*side.inbox,
        side.quarantine_port(),
        &*side.signer,
        &to.runtime,
    );
    let gate = if with_ledger {
        gate.with_ledger(&*side.inbox)
    } else {
        gate
    };
    let outcome = gate
        .receive_from(&link, &connection(&from.party), message, now_ms)
        .unwrap();
    let from_link = from.link();
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    if receipts_arrive {
        for receipt in &outcome.receipts {
            let _ = outbox.on_receipt_from(&from_link, &connection(&side.party), receipt, now_ms);
        }
    }
    outcome.status
}

/// Takes the messages `from` has queued without delivering them, so a test can hand them over
/// one at a time (or deliberately leave one unrecorded).
pub fn queued(from: &Side, now_ms: u64, n: usize) -> Vec<Message> {
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    outbox
        .next_batch(&from.link(), now_ms, n, "system", None)
        .unwrap()
}

/// Hands one message to the inbox **without** the contract gate and without recording it in the
/// conversation: the state a crash between the inbox commit and the record write leaves behind.
/// The message gets a delivery notice, exactly as a real commit does.
pub fn deliver_unrecorded(
    to: &Booted<'_>,
    from: &Side,
    message: &Message,
    now_ms: u64,
) -> spot_link_exchange::ReceiveStatus {
    let side = to.side;
    let link = side.link();
    let inbox = Inbox::new(
        &*side.inbox,
        &*side.audit,
        side.quarantine_port(),
        &*side.signer,
    );
    let outcome = inbox
        .receive_from(&link, &connection(&from.party), message, now_ms)
        .unwrap();
    let from_link = from.link();
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    for receipt in &outcome.receipts {
        let _ = outbox.on_receipt_from(&from_link, &connection(&side.party), receipt, now_ms);
    }
    outcome.status
}

/// Hands everything `from` has queued to `to` once, in order, with no loss.
pub fn carry(from: &Side, to: &Booted<'_>, now_ms: u64) -> Vec<Message> {
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    let batch = outbox
        .next_batch(&from.link(), now_ms, 50, "system", None)
        .unwrap();
    for message in &batch {
        deliver(to, from, message, now_ms);
    }
    batch
}

/// Hands over only the first `n` messages `from` has queued.
pub fn carry_n(from: &Side, to: &Booted<'_>, now_ms: u64, n: usize) -> Vec<Message> {
    let ids = Counter::default();
    let outbox = Outbox::new(
        &*from.outbox,
        &*from.audit,
        &*from.signer,
        &ids,
        from.lifetimes.clone(),
    );
    let batch = outbox
        .next_batch(&from.link(), now_ms, n, "system", None)
        .unwrap();
    for message in &batch {
        deliver(to, from, message, now_ms);
    }
    batch
}

pub fn conversation_for_case(side: &Side, case: &str) -> (LinkId, Conversation) {
    let records = side.conversations.of_case(case).unwrap();
    let record = records.first().expect("the case has a conversation");
    (record.link, record.conversation.clone())
}

use spot_bpm_conversations::ConversationStore;

/// Queues a message through the side's own outbox exactly as given, with no contract check: the
/// way a peer that does not follow the contract (or a buggy one) would send it.
pub fn rogue_send(
    side: &Side,
    conversation: &Conversation,
    topic_name: &str,
    schema_version: u32,
    header: Option<&str>,
    fields: &[(&str, spot_tenant_link::FieldValue)],
    now_ms: u64,
) -> MessageId {
    let sender = LinkSender::new(
        side.link.clone(),
        side.outbox.clone(),
        side.audit.clone(),
        side.signer.clone(),
        side.lifetimes.clone(),
    );
    let mut payload = spot_tenant_link::ExchangePayload::new();
    if let Some(header) = header {
        payload.insert(
            FieldName::new("bpm.message").unwrap(),
            spot_tenant_link::FieldValue::Text(header.to_owned()),
        );
    }
    for (name, value) in fields {
        payload.insert(FieldName::new(*name).unwrap(), value.clone());
    }
    let id = spot_bpm_conversations::message_id(now_ms, &side.link_id(), &format!("rogue{now_ms}"));
    sender
        .send(&spot_bpm_conversations::SendRequest {
            conversation,
            topic: &Topic::new(topic_name).unwrap(),
            schema_version,
            id,
            payload: &payload,
            now_ms,
            actor: "system",
        })
        .unwrap();
    id
}

pub fn text(value: &str) -> spot_tenant_link::FieldValue {
    spot_tenant_link::FieldValue::Text(value.to_owned())
}

pub fn int(value: i64) -> spot_tenant_link::FieldValue {
    spot_tenant_link::FieldValue::Int(value)
}
