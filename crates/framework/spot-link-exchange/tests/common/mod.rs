//! Shared fixtures: two tenants joined by one complete handshake, with a message topic and a
//! record topic, plus an id source and failing doubles.
//!
//! Every instant is an argument; nothing reads a clock.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU64, Ordering};

use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_link_exchange::{
    Conversation, IdSource, Inbox, InputQuarantine, MemoryInbox, MemoryOutbox, MemoryQuarantine,
    MessageId, Outbox, QuarantinedInput, Submit, TransportParty,
};
use spot_tenancy::TenantId;
use spot_tenant_link::{
    AuditSink, CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, ExchangePayload,
    FieldList, FieldName, FieldValue, Grant, Handshake, HandshakePolicy, IdentitySigner, Link,
    LinkDraft, LinkId, MemoryAuditLog, MemoryNonceLog, MemoryPeerKeyDirectory, Nonce, Offer, Party,
    PeerKey, PolicyStepUp, SchemaRange, SeedSigner, StepUpProof, Topic, TopicKind, Trust,
};

/// What a carrier would hand the exchange: the party the connection authenticated.
pub fn connection(party: &Party) -> TransportParty {
    TransportParty::from_verified_connection(party.clone())
}

/// The grants never expire inside a test.
pub const FAR: u64 = 1_000_000_000_000_000;
pub const DAY: u64 = 86_400_000;
pub const HOUR: u64 = 3_600_000;
/// Test time starts here, well after the handshake.
pub const T0: u64 = 10_000_000;

pub fn tenant(n: u8) -> Party {
    Party::Tenant(TenantId::new_v7(1_700_000_000_000, [n; 10]))
}

pub fn message_topic() -> Topic {
    Topic::new("bpm.message").unwrap()
}

pub fn record_topic() -> Topic {
    Topic::new("sales.invoice").unwrap()
}

pub fn conversation(name: &str) -> Conversation {
    Conversation::new(name).unwrap()
}

fn grant(topic: Topic, kind: TopicKind, direction: Direction, fields: &[&str]) -> Grant {
    Grant {
        topic,
        kind,
        direction,
        schema_range: SchemaRange::new(1, 2).unwrap(),
        fields: FieldList::only(fields.iter().map(|f| FieldName::new(*f).unwrap())),
        expires: FAR,
        require_peer_region: None,
        notify: true,
    }
}

fn offer(side: &Party, direction: Direction) -> Offer {
    Offer::new(
        side.clone(),
        vec![
            grant(
                message_topic(),
                TopicKind::Message,
                direction,
                &["message.body", "message.subject"],
            ),
            grant(
                record_topic(),
                TopicKind::Record,
                direction,
                &["customer", "total"],
            ),
        ],
    )
    .unwrap()
}

/// One side's handshake ports: the peer's pinned identity key, a nonce log and the step-up gate.
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

/// Two tenants, A sending and B receiving, with a fully activated link on each side.
pub struct World {
    pub a: Party,
    pub b: Party,
    pub link_a: Link,
    pub link_b: Link,
    pub signer_a: SeedSigner,
    pub signer_b: SeedSigner,
    pub audit_a: MemoryAuditLog,
    pub audit_b: MemoryAuditLog,
    pub ids: Counter,
    pub outbox: MemoryOutbox,
    pub inbox: MemoryInbox,
    pub quarantine: MemoryQuarantine,
}

impl World {
    pub fn new() -> Self {
        Self::with_directions(Direction::Send, Direction::Receive)
    }

    pub fn with_directions(direction_a: Direction, direction_b: Direction) -> Self {
        let a = tenant(1);
        let b = tenant(2);
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
            offer(&a, direction_a),
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
            .accept(
                offer(&b, direction_b),
                consent(&b, "user:b", 1_300),
                1_300,
                nonce(2),
            )
            .unwrap();
        proposer.receive(&accept, 1_400).unwrap();
        let key_message = proposer.key_exchange(1_450, nonce(3)).unwrap();
        peer.receive(&key_message, 1_500).unwrap();
        let key_back = peer.key_exchange(1_550, nonce(4)).unwrap();
        proposer.receive(&key_back, 1_600).unwrap();
        let activate = proposer.activate(1_700, nonce(5)).unwrap();
        peer.receive(&activate, 1_800).unwrap();
        Self {
            a,
            b,
            link_a: proposer.into_link(),
            link_b: peer.into_link(),
            signer_a,
            signer_b,
            audit_a: MemoryAuditLog::new(),
            audit_b: MemoryAuditLog::new(),
            ids: Counter::default(),
            outbox: MemoryOutbox::new(),
            inbox: MemoryInbox::new(),
            quarantine: MemoryQuarantine::new(),
        }
    }

    /// The sender's outbox with the given lifetimes.
    pub fn sender(&self, lifetimes: spot_link_exchange::Lifetimes) -> Outbox<'_> {
        Outbox::new(
            &self.outbox,
            &self.audit_a,
            &self.signer_a,
            &self.ids,
            lifetimes,
        )
    }

    /// The receiver's inbox.
    pub fn receiver(&self) -> Inbox<'_> {
        Inbox::new(&self.inbox, &self.audit_b, &self.quarantine, &self.signer_b)
    }

    pub fn submit<'x>(
        &'x self,
        conversation: &'x Conversation,
        topic: &'x Topic,
        now: u64,
    ) -> Submit<'x> {
        Submit {
            link: &self.link_a,
            conversation,
            topic,
            schema_version: 1,
            now,
            actor: "user:alice",
            oracle: None,
        }
    }
}

pub fn body(text: &str) -> ExchangePayload {
    let mut payload = ExchangePayload::new();
    payload.insert(
        FieldName::new("message.body").unwrap(),
        FieldValue::Text(text.to_owned()),
    );
    payload
}

/// A counter-based id source: ids are unique and time-ordered, with no randomness.
#[derive(Default)]
pub struct Counter(AtomicU64);

impl IdSource for Counter {
    fn next_id(&self, now: u64) -> MessageId {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let mut entropy = [0u8; 10];
        entropy[..8].copy_from_slice(&n.to_be_bytes());
        MessageId::new_v7(now, entropy)
    }
}

/// An audit sink that always fails.
pub struct FailingAudit;

impl AuditSink for FailingAudit {
    fn append(&self, _record: spot_audit_chain::AuditRecord) -> spot_tenant_link::Result<u64> {
        Err(spot_tenant_link::Error::StoreFailed { reason: "down" })
    }
}

/// A quarantine that always fails.
pub struct FailingQuarantine;

impl InputQuarantine for FailingQuarantine {
    fn hold(&self, _input: QuarantinedInput) -> spot_link_exchange::Result<()> {
        Err(spot_link_exchange::Error::QuarantineFailed)
    }
}
