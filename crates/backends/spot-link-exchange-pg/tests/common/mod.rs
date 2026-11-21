//! Shared setup: a scratch database with a pool, per-tenant schemas, and a real handshake that
//! produces the finished links the exchange uses. Every instant is fixed; nothing reads a clock.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime as PoolRuntime};
use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_link_exchange::{
    Conversation, EntryStatus, IdSource, InboxEntry, Message, MessageId, MessageParts,
    TransportParty,
};
use spot_link_exchange_pg::PgExchange;
use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use spot_tenancy::TenantId;
use spot_tenant_link::{
    CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, FieldList, FieldName, Grant,
    Handshake, HandshakePolicy, IdentitySigner, Link, LinkDraft, LinkId, MemoryAuditLog,
    MemoryNonceLog, MemoryPeerKeyDirectory, Nonce, Offer, Party, PeerKey, PolicyStepUp,
    SchemaRange, SeedSigner, StepUpProof, Topic, TopicKind, Trust,
};

pub const FAR: u64 = 1_000_000_000_000_000;
pub const DAY: u64 = 86_400_000;
pub const T0: u64 = 10_000_000;

pub fn runtime() -> Arc<tokio::runtime::Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("the test runtime builds"),
    )
}

pub fn pool(config: &PgConfig, max_size: usize) -> Pool {
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name("spot-link-exchange-pg-test");
    let manager = Manager::from_config(
        driver,
        tokio_postgres::NoTls,
        ManagerConfig {
            recycling_method: RecyclingMethod::Clean,
        },
    );
    Pool::builder(manager)
        .max_size(max_size)
        .runtime(PoolRuntime::Tokio1)
        .build()
        .expect("the pool builds")
}

/// What a carrier would hand the exchange: the party the connection authenticated.
pub fn connection(party: &Party) -> TransportParty {
    TransportParty::from_verified_connection(party.clone())
}

pub fn tenant_id(n: u8) -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [n; 10])
}

pub fn tenant(n: u8) -> Party {
    Party::Tenant(tenant_id(n))
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

fn offer(side: &Party, direction: Direction) -> Offer {
    Offer::new(side.clone(), vec![grant(direction)]).unwrap()
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

/// Tenant 1 (A, the sender) and tenant 2 (B, the receiver) joined by an active link.
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
}

impl World {
    pub fn new() -> Self {
        let a = tenant(1);
        let b = tenant(2);
        let signer_a = SeedSigner::from_seed([1u8; 32], a.clone());
        let signer_b = SeedSigner::from_seed([2u8; 32], b.clone());
        let hs_a = MemoryAuditLog::new();
        let hs_b = MemoryAuditLog::new();
        let draft = LinkDraft::new(
            LinkId::new_v7(1_700_000_000_000, [7u8; 10]),
            a.clone(),
            b.clone(),
            Carrier::Direct,
            1_000,
            None,
            offer(&a, Direction::Send),
        )
        .unwrap();
        let nonce = |n: u8| Nonce::from_bytes([n; 16]);
        let ports_a = Ports::pinned(&b, signer_b.public_key());
        let ports_b = Ports::pinned(&a, signer_a.public_key());
        let policy_a = ports_a.policy();
        let policy_b = ports_b.policy();
        let (mut proposer, propose) = Handshake::propose(
            &signer_a,
            &hs_a,
            &policy_a,
            draft,
            consent(&a, "user:a", 1_100),
            1_100,
            nonce(1),
        )
        .unwrap();
        let mut peer =
            Handshake::from_proposal(&signer_b, &hs_b, &policy_b, &propose, 1_200).unwrap();
        let accept = peer
            .accept(
                offer(&b, Direction::Receive),
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
        }
    }

    /// A message from A in `conversation` at `seq`, signed by A's key.
    pub fn message(&self, conversation: &str, seq: u64, sent_at: u64) -> Message {
        Message::sign(
            MessageParts {
                id: self.ids.next_id(sent_at),
                link: self.link_a.id,
                conversation: Conversation::new(conversation).unwrap(),
                seq,
                topic: message_topic(),
                schema_version: 1,
                sent_at,
                expires_at: sent_at + 3 * DAY,
                tombstone: false,
                replaces: None,
                payload: format!(r#"{{"message.body":{{"text":"m{seq}"}}}}"#),
            },
            &self.signer_a,
        )
        .unwrap()
    }

    pub fn entry(&self, message: Message, received_at: u64) -> InboxEntry {
        InboxEntry {
            message,
            received_at,
            status: EntryStatus::Applied,
        }
    }
}

pub struct Env {
    pub db: TestDb,
    pub rt: Arc<tokio::runtime::Runtime>,
    pub pool: Pool,
}

/// `None` (printing `SKIPPED`) when `SPOT_TEST_PG_URL` is unset.
pub async fn env(rt: Arc<tokio::runtime::Runtime>) -> Option<Env> {
    let db = TestDb::create().await?;
    let pool = pool(&db.config(), 24);
    Some(Env { db, rt, pool })
}

impl Env {
    /// Creates tenant `n`'s schema (the provisioner's job) and returns prepared stores.
    pub fn exchange(&self, n: u8) -> PgExchange {
        self.provision_schema(n);
        let exchange = PgExchange::new(self.pool.clone(), Arc::clone(&self.rt), tenant_id(n));
        self.rt
            .block_on(exchange.prepare())
            .expect("the exchange tables are created");
        exchange
    }

    pub fn provision_schema(&self, n: u8) {
        let schema = tenant_id(n).schema_name();
        self.rt.block_on(async {
            let client = self.db.raw_client().await;
            client
                .batch_execute(&format!("CREATE SCHEMA IF NOT EXISTS \"{schema}\""))
                .await
                .expect("create the tenant schema");
        });
    }

    /// A fresh pool and exchange for tenant `n`, as after a process restart.
    pub fn restarted(&self, n: u8) -> PgExchange {
        PgExchange::new(
            pool(&self.db.config(), 4),
            Arc::clone(&self.rt),
            tenant_id(n),
        )
    }

    /// `SELECT count(*)` over a raw connection with tenant `n`'s schema as the search path.
    pub fn count(&self, n: u8, table: &str) -> i64 {
        let schema = tenant_id(n).schema_name();
        self.rt.block_on(async {
            let client = self.db.raw_client().await;
            client
                .batch_execute(&format!("SET search_path TO \"{schema}\""))
                .await
                .expect("select the tenant schema");
            client
                .query_one(&format!("SELECT count(*) FROM {table}"), &[])
                .await
                .expect("count rows")
                .get(0)
        })
    }
}
