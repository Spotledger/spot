//! Two tenants joined by one complete handshake, each with its own full set of stores, plus
//! helpers to enqueue, to build servers and to present certificates.
//!
//! Real time is used for the clocks (the exchange's grants and lifetimes are far in the future of
//! the handshake instants), so nothing here depends on a manual clock unless a test asks.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_core::{Clock, SystemClock};
use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_link_exchange::{
    Conversation, IdSource, InboxConfig, Lifetimes, MemoryInbox, MemoryOutbox, MemoryQuarantine,
    MessageId, OutboxConfig, Retention, Submit,
};
use spot_link_transport::{CarrierTable, LinkHost, LinkServer, PinnedCertificates};
use spot_sync_http::ClientCertificate;
use spot_tenancy::TenantId;
use spot_tenant_link::{
    CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, ExchangePayload, FieldList,
    FieldName, FieldValue, Grant, Handshake, HandshakePolicy, IdentitySigner, Link, LinkDraft,
    LinkId, LinkStore, MemoryAuditLog, MemoryLinkStore, MemoryNonceLog, MemoryPeerKeyDirectory,
    MemoryRegionOracle, Nonce, Offer, Party, PeerKey, PolicyStepUp, RegionOracle, SchemaRange,
    SeedSigner, StepUpProof, Topic, TopicKind, Trust,
};

pub mod net;

pub const FAR: u64 = 1_000_000_000_000_000;

pub fn tenant(n: u8) -> Party {
    Party::Tenant(TenantId::new_v7(1_700_000_000_000, [n; 10]))
}

pub fn message_topic() -> Topic {
    Topic::new("bpm.message").unwrap()
}

pub fn conversation(name: &str) -> Conversation {
    Conversation::new(name).unwrap()
}

fn grant(direction: Direction) -> Grant {
    Grant {
        topic: message_topic(),
        kind: TopicKind::Message,
        direction,
        schema_range: SchemaRange::new(1, 2).unwrap(),
        fields: FieldList::only(
            ["message.body", "message.subject"]
                .iter()
                .map(|f| FieldName::new(*f).unwrap()),
        ),
        expires: FAR,
        require_peer_region: None,
        notify: true,
    }
}

fn offer(side: &Party) -> Offer {
    Offer::new(side.clone(), vec![grant(Direction::Both)]).unwrap()
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

/// A counter-based id source: unique, time-ordered, no randomness.
pub struct Counter(AtomicU64, u8);

impl Counter {
    pub fn new(tag: u8) -> Self {
        Self(AtomicU64::new(0), tag)
    }
}

impl IdSource for Counter {
    fn next_id(&self, now: u64) -> MessageId {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let mut entropy = [0u8; 10];
        entropy[..8].copy_from_slice(&n.to_be_bytes());
        entropy[9] = self.1;
        MessageId::new_v7(now, entropy)
    }
}

/// One side of the link with all its stores.
pub struct Node {
    pub party: Party,
    pub signer: Arc<SeedSigner>,
    pub link: LinkId,
    pub links: Arc<MemoryLinkStore>,
    pub outbox: Arc<MemoryOutbox>,
    pub inbox: Arc<MemoryInbox>,
    pub audit: Arc<MemoryAuditLog>,
    pub quarantine: Arc<MemoryQuarantine>,
    pub host: LinkHost,
}

impl Node {
    fn build(party: Party, signer: SeedSigner, link: Link, tag: u8, clock: Arc<dyn Clock>) -> Self {
        let signer = Arc::new(signer);
        let links = Arc::new(MemoryLinkStore::new());
        let id = link.id;
        links.insert(link).unwrap();
        let outbox = Arc::new(MemoryOutbox::new());
        let inbox = Arc::new(MemoryInbox::new());
        let audit = Arc::new(MemoryAuditLog::new());
        let quarantine = Arc::new(MemoryQuarantine::new());
        let host = LinkHost {
            links: links.clone(),
            outbox: outbox.clone(),
            inbox: inbox.clone(),
            audit: audit.clone(),
            quarantine: quarantine.clone(),
            signer: signer.clone(),
            ids: Arc::new(Counter::new(tag)),
            lifetimes: Lifetimes::new(Retention::from_days(3).unwrap()),
            inbox_config: InboxConfig::default(),
            outbox_config: OutboxConfig {
                // Tests resend at once: the timing under test is the transport's.
                resend_after_ms: 0,
                unapplied_resend_after_ms: 0,
            },
            clock,
            region: None,
        };
        Self {
            party,
            signer,
            link: id,
            links,
            outbox,
            inbox,
            audit,
            quarantine,
            host,
        }
    }

    /// The link as this side's store holds it.
    pub fn current_link(&self) -> Link {
        self.links.get(&self.link).unwrap()
    }

    /// Enqueues one message on the shared topic.
    pub fn enqueue(&self, conversation: &str, text: &str) -> MessageId {
        let link = self.current_link();
        let conversation = Conversation::new(conversation).unwrap();
        let topic = message_topic();
        let now = self.host.now_ms().unwrap();
        let submit = Submit {
            link: &link,
            conversation: &conversation,
            topic: &topic,
            schema_version: 1,
            now,
            actor: "user:alice",
            oracle: None,
        };
        self.host
            .outbox()
            .enqueue_message(&submit, &body(text))
            .unwrap()
            .id
    }

    /// Makes every grant on this side require the peer in `required`, and places the peer with an
    /// oracle (`placed`, or none at all when `None`, or an oracle that knows no peer when the
    /// region is unknown).
    pub fn require_peer_region(&mut self, required: &str, oracle: Option<MemoryRegionOracle>) {
        let mut link = self.current_link();
        let version = link.version;
        for grant in &mut link.grants {
            grant.require_peer_region = Some(spot_data_policy::Region::new(required).unwrap());
        }
        self.links.update(link, version).unwrap();
        self.host.region = oracle.map(|o| Arc::new(o) as Arc<dyn RegionOracle + Send + Sync>);
    }

    /// Revokes the link on this side.
    pub fn revoke(&self) {
        let link = self.current_link();
        let now = self.host.now_ms().unwrap();
        let peer = link.peer().cloned().unwrap();
        let revoked = link
            .revoked_by(&peer, spot_tenant_link::RevokeReason::SecurityConcern, now)
            .unwrap();
        self.links.update(revoked, link.version).unwrap();
    }

    /// The states of every message in the outbox.
    pub fn states(&self) -> Vec<spot_link_exchange::MessageState> {
        self.outbox.all().unwrap().iter().map(|e| e.state).collect()
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

/// Two parties joined by an active link, each with a complete node.
pub struct World {
    pub a: Node,
    pub b: Node,
}

impl World {
    pub fn new() -> Self {
        Self::with_clocks(Arc::new(SystemClock::new()), Arc::new(SystemClock::new()))
    }

    pub fn with_clocks(clock_a: Arc<dyn Clock>, clock_b: Arc<dyn Clock>) -> Self {
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
        let link_a = proposer.into_link();
        let link_b = peer.into_link();
        Self {
            a: Node::build(a, signer_a, link_a, 1, clock_a),
            b: Node::build(b, signer_b, link_b, 2, clock_b),
        }
    }
}

/// A throwaway client certificate whose fingerprint is what a directory pins; the loopback never
/// parses it.
pub fn certificate(seed: u8) -> ClientCertificate {
    ClientCertificate::from_der(vec![0x30, 0x82, seed, seed, seed, seed])
}

/// The server for `node`, pinning `certificate` to `peer`.
pub fn server_for(
    node: &Node,
    peer: &Party,
    certificate: &ClientCertificate,
    carriers: Arc<CarrierTable>,
) -> (LinkServer, Arc<PinnedCertificates>) {
    let directory = Arc::new(PinnedCertificates::new());
    directory.pin(certificate.fingerprint(), peer.clone());
    (
        LinkServer::new(node.host.clone(), directory.clone(), carriers),
        directory,
    )
}

pub fn world() -> World {
    World::new()
}

/// A genuine, signed `propose` from `from` (key seed `seed`) to `to`, with a fresh nonce byte.
pub fn proposal(
    seed: u8,
    from: &Party,
    to: &Party,
    nonce: u8,
) -> (spot_tenant_link::HandshakeMessage, SeedSigner) {
    let signer = SeedSigner::from_seed([seed; 32], from.clone());
    let peer_key = SeedSigner::from_seed([200u8; 32], to.clone()).public_key();
    let ports = Ports::pinned(to, peer_key);
    let audit = MemoryAuditLog::new();
    let draft = LinkDraft::new(
        LinkId::new_v7(1_700_000_000_000, [nonce; 10]),
        from.clone(),
        to.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(from),
    )
    .unwrap();
    let (_, message) = Handshake::propose(
        &signer,
        &audit,
        &ports.policy(),
        draft,
        consent(from, "user:a", 1_100),
        1_100,
        Nonce::from_bytes([nonce; 16]),
    )
    .unwrap();
    (message, signer)
}
