//! One tenant joined to one external peer by an active external-carrier link, with the tenant's
//! full set of stores, a vault holding the shared secret, and fakes for the peer.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use spot_connectors::{
    ConnectorCall, ConnectorError, ConnectorHealth, ConnectorId, ConnectorInfo, ConnectorOutput,
    ConnectorPort, CredentialRef, CredentialVault, HttpRequest, HttpResponse, Vault,
};
use spot_link_exchange::{
    Conversation, IdSource, Inbox, InboxStore, Lifetimes, MemoryInbox, MemoryOutbox,
    MemoryQuarantine, MessageId, MessageState, Outbox, OutboxConfig, Retention, Submit,
};
use spot_link_external::{
    ChannelFailure, Delivery, ExternalPeer, ExternalReceiver, ExternalSender, InboundRequest,
    InboundResponse, Outbound, PeerAnswer, PeerChannel, PeerFormat, PumpReport, ReceiverPorts,
    RefusalCounter, SignatureScheme, WebhookSpec,
};
use spot_secrets::SecretString;
use spot_tenancy::TenantId;
use spot_tenant_link::{
    Carrier, Direction, ExchangePayload, FieldList, FieldName, FieldValue, Grant, IdentitySigner,
    Link, LinkId, LinkState, MemoryAuditLog, Party, SchemaRange, SeedSigner, Topic, TopicKind,
};

pub const FAR: u64 = 1_000_000_000_000_000;
pub const DAY: u64 = 86_400_000;
/// The first instant of the tests (Unix ms); the signature windows work in seconds from it.
pub const T0: u64 = 1_760_000_000_000;
pub const SECRET: &str = "CANARY-shared-secret-7f3a91";

pub fn topic() -> Topic {
    Topic::new("bpm.message").unwrap()
}

pub fn conversation(name: &str) -> Conversation {
    Conversation::new(name).unwrap()
}

pub fn grant(topic: &Topic, direction: Direction) -> Grant {
    Grant {
        topic: topic.clone(),
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

pub struct Counter(AtomicU64);

impl Counter {
    pub fn new() -> Self {
        Self(AtomicU64::new(0))
    }
}

impl IdSource for Counter {
    fn next_id(&self, now: u64) -> MessageId {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        let mut entropy = [0u8; 10];
        entropy[..8].copy_from_slice(&n.to_be_bytes());
        MessageId::new_v7(now, entropy)
    }
}

pub fn peer(outbound: Outbound, format: PeerFormat) -> ExternalPeer {
    ExternalPeer::new(
        spot_tenant_link::ExternalPeerId::new("acme-erp").unwrap(),
        "Acme ERP".to_owned(),
        CredentialRef::new("acme-shared-secret").unwrap(),
        outbound,
        WebhookSpec {
            path: "/links/acme/inbound".to_owned(),
            signature_scheme: SignatureScheme::SpotHmacSha256,
            tolerance_secs: 300,
        },
        format,
    )
}

pub fn webhook_peer() -> ExternalPeer {
    peer(
        Outbound::WebhookUrl("https://acme.example/hooks/spot".to_owned()),
        PeerFormat::JsonEnvelope,
    )
}

pub fn vault() -> Arc<Vault> {
    let mut vault = Vault::new();
    vault.insert_static(
        CredentialRef::new("acme-shared-secret").unwrap(),
        SecretString::new(SECRET),
    );
    Arc::new(vault)
}

/// The tenant's side of one external link.
pub struct World {
    pub tenant: Party,
    pub local: SeedSigner,
    pub bridge: SeedSigner,
    pub link: Link,
    pub peer: ExternalPeer,
    pub outbox: MemoryOutbox,
    pub inbox: MemoryInbox,
    pub audit: MemoryAuditLog,
    pub quarantine: MemoryQuarantine,
    pub ids: Counter,
    pub lifetimes: Lifetimes,
    pub vault: Arc<Vault>,
    pub refusals: RefusalCounter,
}

impl World {
    pub fn new() -> Self {
        Self::with(webhook_peer(), vec![grant(&topic(), Direction::Both)])
    }

    pub fn with(peer: ExternalPeer, grants: Vec<Grant>) -> Self {
        let tenant = Party::Tenant(TenantId::new_v7(1_700_000_000_000, [1; 10]));
        let local = SeedSigner::from_seed([1u8; 32], tenant.clone());
        let bridge = SeedSigner::for_external_peer([9u8; 32], "acme-erp").unwrap();
        let link = Link {
            id: LinkId::new_v7(1_700_000_000_000, [7u8; 10]),
            parties: [tenant.clone(), peer.party()],
            local: tenant.clone(),
            carrier: Carrier::External,
            state: LinkState::Active,
            created: 1_000,
            peer_keys: [Some(local.public_key()), Some(bridge.public_key())],
            grants,
            consents: Vec::new(),
            revocation: None,
            expires: None,
            version: 1,
        };
        link.validate().unwrap();
        Self {
            tenant,
            local,
            bridge,
            link,
            peer,
            outbox: MemoryOutbox::new(),
            inbox: MemoryInbox::new(),
            audit: MemoryAuditLog::new(),
            quarantine: MemoryQuarantine::new(),
            ids: Counter::new(),
            lifetimes: Lifetimes::new(Retention::from_days(3).unwrap()),
            vault: vault(),
            refusals: RefusalCounter::new(),
        }
    }

    pub fn outbox(&self) -> Outbox<'_> {
        Outbox::new(
            &self.outbox,
            &self.audit,
            &self.local,
            &self.ids,
            self.lifetimes.clone(),
        )
        .with_config(OutboxConfig {
            resend_after_ms: 1_000,
            unapplied_resend_after_ms: u64::MAX,
        })
    }

    pub fn inbox(&self) -> Inbox<'_> {
        Inbox::new(&self.inbox, &self.audit, &self.quarantine, &self.local)
    }

    pub fn enqueue(&self, conversation_name: &str, text: &str, now: u64) -> MessageId {
        self.enqueue_on(&topic(), 1, conversation_name, text, now)
            .unwrap()
    }

    pub fn enqueue_on(
        &self,
        topic: &Topic,
        schema_version: u32,
        conversation_name: &str,
        text: &str,
        now: u64,
    ) -> spot_link_exchange::Result<MessageId> {
        let conversation = conversation(conversation_name);
        let submit = Submit {
            link: &self.link,
            conversation: &conversation,
            topic,
            schema_version,
            now,
            actor: "user:alice",
            oracle: None,
        };
        let mut payload = ExchangePayload::new();
        payload.insert(
            FieldName::new("message.body").unwrap(),
            FieldValue::Text(text.to_owned()),
        );
        self.outbox()
            .enqueue_message(&submit, &payload)
            .map(|e| e.id)
    }

    pub async fn pump(&self, channel: &dyn PeerChannel, now: u64) -> PumpReport {
        let outbox = self.outbox();
        ExternalSender::new(&outbox, &self.peer, channel, &self.bridge, &self.audit)
            .pump(&self.link, now, 50, "system", None)
            .await
            .unwrap()
    }

    pub async fn receive(&self, request: &InboundRequest, now: u64) -> InboundResponse {
        self.receive_with(&*self.vault, request, now).await
    }

    /// Reads one inbound body the way the HTTP edge does: under this peer's own cap, through
    /// the receiver's reader, so no test can build a request the edge could not have built.
    pub fn read<I, C>(
        &self,
        path: &str,
        headers: Vec<(String, String)>,
        body: I,
    ) -> spot_link_external::Result<InboundRequest>
    where
        I: IntoIterator<Item = std::io::Result<C>>,
        C: AsRef<[u8]>,
    {
        let inbox = self.inbox();
        let outbox = self.outbox();
        ExternalReceiver::new(
            &self.peer,
            ReceiverPorts {
                inbox: &inbox,
                inbox_store: &self.inbox as &dyn InboxStore,
                outbox: &outbox,
                outbox_store: &self.outbox,
                bridge: &self.bridge,
                vault: &*self.vault,
                lifetimes: &self.lifetimes,
                audit: &self.audit,
                refusals: &self.refusals,
            },
        )
        .read(path, headers, body)
    }

    /// Handles one request through a vault of the test's choosing, so a test can prove a
    /// request was refused before the secret was even looked up.
    pub async fn receive_with(
        &self,
        vault: &dyn CredentialVault,
        request: &InboundRequest,
        now: u64,
    ) -> InboundResponse {
        let inbox = self.inbox();
        let outbox = self.outbox();
        ExternalReceiver::new(
            &self.peer,
            ReceiverPorts {
                inbox: &inbox,
                inbox_store: &self.inbox as &dyn InboxStore,
                outbox: &outbox,
                outbox_store: &self.outbox,
                bridge: &self.bridge,
                vault,
                lifetimes: &self.lifetimes,
                audit: &self.audit,
                refusals: &self.refusals,
            },
        )
        .handle(&self.link, request, now)
        .await
    }

    pub fn states(&self) -> Vec<MessageState> {
        self.outbox.all().unwrap().iter().map(|e| e.state).collect()
    }

    pub fn state_of(&self, id: &MessageId) -> MessageState {
        self.outbox().state(id).unwrap()
    }

    pub fn revoke(&mut self, now: u64) {
        let peer = self.peer.party();
        self.link = self
            .link
            .revoked_by(&peer, spot_tenant_link::RevokeReason::SecurityConcern, now)
            .unwrap();
    }

    pub fn stored(&self) -> usize {
        self.inbox.stored().unwrap().len()
    }

    /// Everything a log or a screen could show, as text.
    pub fn everything(&self) -> String {
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            self.audit.entries().unwrap(),
            self.outbox.all().unwrap(),
            self.inbox.stored().unwrap(),
            self.quarantine.held()
        )
    }
}

/// The cap the fixtures read under: the peer's documented default, one mebibyte.
pub fn fixture_cap() -> usize {
    usize::try_from(ExternalPeer::DEFAULT_MAX_BODY_BYTES).expect("the documented cap fits a usize")
}

/// A signed request as the peer would send it.
///
/// The body goes through the reader the route uses, under the documented cap, so a fixture
/// cannot build a request the HTTP edge could not: an over-cap body is a `BodyTooLarge` here,
/// not a buffered `Vec`.
pub fn signed(body: &str, path: &str, secret: &str, at_secs: i64) -> InboundRequest {
    signed_over(body, body, path, secret, at_secs)
}

/// A request whose signature covers `signed_body` but whose body is `body`: the tampered case.
pub fn signed_over(
    signed_body: &str,
    body: &str,
    path: &str,
    secret: &str,
    at_secs: i64,
) -> InboundRequest {
    let signature =
        spot_webhooks::SigningKey::new(secret.as_bytes()).sign(at_secs, signed_body.as_bytes());
    InboundRequest::from_stream(
        path.to_owned(),
        vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            (
                "X-Spot-Signature".to_owned(),
                format!("t={at_secs},v1={signature}"),
            ),
        ],
        [Ok::<_, std::io::Error>(body.as_bytes().to_vec())],
        fixture_cap(),
    )
    .expect("a fixture body fits the peer's documented cap")
}

pub fn from_peer(world: &World, body: &str, now: u64) -> InboundRequest {
    signed(
        body,
        &world.peer.inbound.path,
        SECRET,
        i64::try_from(now / 1_000).unwrap(),
    )
}

/// A body of `total` bytes delivered in chunks of at most `chunk` bytes, counting what the
/// reader actually pulled out of it: the proof that a read stopped where it should.
pub struct Bytes {
    left: usize,
    /// Bytes handed out so far.
    pub pulled: usize,
    chunk: usize,
}

impl Bytes {
    pub fn new(total: usize, chunk: usize) -> Self {
        Self {
            left: total,
            pulled: 0,
            chunk: chunk.max(1),
        }
    }
}

impl Iterator for Bytes {
    type Item = std::io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        let take = self.left.min(self.chunk);
        self.left -= take;
        self.pulled += take;
        Some(Ok(vec![b'x'; take]))
    }
}

pub fn message_body(id: &str, conversation: &str, text: &str) -> String {
    format!(
        "{{\"id\":\"{id}\",\"conversation\":\"{conversation}\",\"topic\":\"bpm.message\",\"schema_version\":1,\"payload\":{{\"message.body\":\"{text}\"}}}}"
    )
}

/// What the fake peer received, and what it answers.
pub struct FakePeer {
    pub answers: Mutex<VecDeque<Result<PeerAnswer, ChannelFailure>>>,
    pub received: Mutex<Vec<(MessageId, String)>>,
}

impl FakePeer {
    pub fn new(answers: Vec<Result<PeerAnswer, ChannelFailure>>) -> Self {
        Self {
            answers: Mutex::new(answers.into()),
            received: Mutex::new(Vec::new()),
        }
    }

    pub fn answering(status: u16) -> Self {
        Self::new(vec![Ok(answer(status, ""))])
    }

    pub fn bodies(&self) -> Vec<String> {
        self.received
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect()
    }

    pub fn push(&self, next: Result<PeerAnswer, ChannelFailure>) {
        self.answers.lock().unwrap().push_back(next);
    }
}

pub fn answer(status: u16, body: &str) -> PeerAnswer {
    PeerAnswer {
        status,
        body: body.to_owned(),
    }
}

#[async_trait]
impl PeerChannel for FakePeer {
    async fn deliver(
        &self,
        _peer: &ExternalPeer,
        delivery: &Delivery<'_>,
    ) -> Result<PeerAnswer, ChannelFailure> {
        self.received
            .lock()
            .unwrap()
            .push((delivery.message, delivery.body.to_string()));
        let mut answers = self.answers.lock().unwrap();
        if answers.len() > 1 {
            answers.pop_front().unwrap()
        } else {
            answers.front().cloned().unwrap_or(Ok(answer(200, "")))
        }
    }
}

/// A fake connector port: scripted results for `invoke`.
pub struct FakeConnector {
    pub results: Mutex<VecDeque<Result<ConnectorOutput, ConnectorError>>>,
    pub calls: Mutex<Vec<ConnectorCall>>,
}

impl FakeConnector {
    pub fn new(results: Vec<Result<ConnectorOutput, ConnectorError>>) -> Self {
        Self {
            results: Mutex::new(results.into()),
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ConnectorPort for FakeConnector {
    async fn describe(&self, connector: &ConnectorId) -> spot_connectors::Result<ConnectorInfo> {
        Err(ConnectorError::NotFound {
            id: connector.as_str().to_owned(),
        })
    }

    async fn invoke(&self, call: ConnectorCall) -> spot_connectors::Result<ConnectorOutput> {
        self.calls.lock().unwrap().push(call);
        self.results.lock().unwrap().pop_front().unwrap()
    }

    async fn webhook(
        &self,
        _connector: &ConnectorId,
        _request: HttpRequest,
        _now_micros: i64,
    ) -> spot_connectors::Result<HttpResponse> {
        Err(ConnectorError::Unsupported {
            what: "webhook".to_owned(),
        })
    }

    async fn ping(&self, _connector: &ConnectorId) -> spot_connectors::Result<ConnectorHealth> {
        Ok(ConnectorHealth {
            healthy: true,
            detail: String::new(),
        })
    }
}

/// A vault that records whether it was asked for a secret and always fails: a receiver that
/// refuses a request before the secret is looked up leaves [`SpyVault::resolved`] false.
#[derive(Default)]
pub struct SpyVault {
    resolved: AtomicBool,
}

impl SpyVault {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any call reached the vault.
    pub fn resolved(&self) -> bool {
        self.resolved.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl CredentialVault for SpyVault {
    async fn resolve(
        &self,
        reference: &CredentialRef,
        _now_micros: i64,
    ) -> spot_connectors::Result<SecretString> {
        self.resolved.store(true, Ordering::SeqCst);
        Err(ConnectorError::SecretUnavailable {
            reference: reference.to_string(),
        })
    }
}
