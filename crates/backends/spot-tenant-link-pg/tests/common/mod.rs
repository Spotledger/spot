//! Shared setup: a scratch database with a pool, two tenant schemas, and real handshakes that
//! produce the finished links the store keeps. Every instant is fixed; nothing reads a clock.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::{Arc, Mutex};

use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod, Runtime as PoolRuntime};
use spot_audit_chain::AuditRecord;
use spot_iam_access::{AuthMethod, OperationName, StepUpPolicy};
use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use spot_tenancy::TenantId;
use spot_tenant_link::{
    AuditSink, CONSENT_OPERATION, Carrier, Consent, ConsentPolicy, Direction, Error, FieldList,
    FieldName, Grant, Handshake, HandshakeMessage, HandshakePolicy, IdentitySigner, Link,
    LinkDraft, LinkId, MemoryNonceLog, MemoryPeerKeyDirectory, Nonce, Offer, Party, PeerKey,
    PolicyStepUp, SchemaRange, SeedSigner, StepUpProof, Topic, TopicKind, Trust,
};
use spot_tenant_link_pg::{PgLinkRegistry, PgLinkStore};

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
        .application_name("spot-tenant-link-pg-test");
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

pub fn tenant_id(n: u8) -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [n; 10])
}

pub fn tenant(n: u8) -> Party {
    Party::Tenant(tenant_id(n))
}

pub fn link_id(seed: u8) -> LinkId {
    LinkId::new_v7(1_700_000_000_000, [seed; 10])
}

pub fn nonce(seed: u8) -> Nonce {
    Nonce::from_bytes([seed; 16])
}

pub fn topic(name: &str) -> Topic {
    Topic::new(name).unwrap()
}

pub fn grant_on(name: &str, direction: Direction, fields: &[&str], expires: u64) -> Grant {
    Grant {
        topic: topic(name),
        kind: TopicKind::Message,
        direction,
        schema_range: SchemaRange::new(1, 2).unwrap(),
        fields: FieldList::only(fields.iter().map(|f| FieldName::new(*f).unwrap())),
        expires,
        require_peer_region: None,
        notify: true,
    }
}

pub fn grant(direction: Direction) -> Grant {
    grant_on("bpm.message", direction, &["message.body"], 100_000)
}

pub fn offer(side: &Party, direction: Direction) -> Offer {
    Offer::new(side.clone(), vec![grant(direction)]).unwrap()
}

/// A consent at `at` with a strong sign-in ten seconds earlier, through the real step-up gate.
pub fn consent(party: &Party, actor: &str, at: u64) -> Consent {
    Consent::new(
        party.clone(),
        actor,
        at,
        proof(at),
        &PolicyStepUp::new(StepUpPolicy::default()),
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

pub fn proof(at: u64) -> StepUpProof {
    StepUpProof {
        operation: OperationName::new(CONSENT_OPERATION),
        method: AuthMethod::PasswordTotp,
        authenticated_at: i64::try_from(at / 1_000).unwrap() - 10,
    }
}

/// The two finished copies of one handshake between tenant 1 (proposer) and tenant 2, and the
/// five signed messages in order: propose, accept, the proposer's key, the peer's key, activate.
pub struct Linked {
    pub a: Link,
    pub b: Link,
    pub messages: Vec<HandshakeMessage>,
}

pub fn linked() -> Linked {
    let a = tenant(1);
    let b = tenant(2);
    let key_a = SeedSigner::from_seed([1u8; 32], a.clone());
    let key_b = SeedSigner::from_seed([2u8; 32], b.clone());
    let audit_a = spot_tenant_link::MemoryAuditLog::new();
    let audit_b = spot_tenant_link::MemoryAuditLog::new();
    let draft = LinkDraft::new(
        link_id(1),
        a.clone(),
        b.clone(),
        Carrier::Direct,
        1_000,
        None,
        offer(&a, Direction::Send),
    )
    .unwrap();
    let ports_a = Ports::pinned(&b, key_b.public_key());
    let ports_b = Ports::pinned(&a, key_a.public_key());
    let policy_a = ports_a.policy();
    let policy_b = ports_b.policy();
    let (mut proposer, propose) = Handshake::propose(
        &key_a,
        &audit_a,
        &policy_a,
        draft,
        consent(&a, "user:a", 1_100),
        1_100,
        nonce(1),
    )
    .unwrap();
    let mut peer = Handshake::from_proposal(&key_b, &audit_b, &policy_b, &propose, 1_200).unwrap();
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
    Linked {
        a: proposer.into_link(),
        b: peer.into_link(),
        messages: vec![propose, accept, key_message, key_back, activate],
    }
}

/// A scratch database, a shared pool, and the registry on the same database.
pub struct Env {
    pub db: TestDb,
    pub rt: Arc<tokio::runtime::Runtime>,
    pub pool: Pool,
}

/// `None` (printing `SKIPPED`) when `SPOT_TEST_PG_URL` is unset.
pub async fn env(rt: Arc<tokio::runtime::Runtime>) -> Option<Env> {
    let db = TestDb::create().await?;
    let pool = pool(&db.config(), 8);
    Some(Env { db, rt, pool })
}

impl Env {
    /// Creates tenant `n`'s schema (the provisioner's job) and returns a prepared store.
    pub fn store(&self, n: u8) -> PgLinkStore {
        self.provision_schema(n);
        let store = PgLinkStore::new(self.pool.clone(), Arc::clone(&self.rt), tenant_id(n));
        self.rt
            .block_on(store.prepare())
            .expect("the link tables are created");
        store
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

    pub fn registry(&self) -> Arc<PgLinkRegistry> {
        let registry = Arc::new(PgLinkRegistry::new(self.pool.clone()));
        self.rt
            .block_on(registry.prepare())
            .expect("the registry is created");
        registry
    }

    /// A fresh pool and store for tenant `n`, as after a process restart.
    pub fn restarted_store(&self, n: u8) -> PgLinkStore {
        PgLinkStore::new(
            pool(&self.db.config(), 4),
            Arc::clone(&self.rt),
            tenant_id(n),
        )
    }

    /// One text column over a raw connection with tenant `n`'s schema as the search path.
    pub fn raw_count(&self, n: u8, table: &str) -> i64 {
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

    pub fn columns(&self, schema: &str, table: &str) -> Vec<String> {
        self.rt.block_on(async {
            let client = self.db.raw_client().await;
            client
                .query(
                    "SELECT column_name FROM information_schema.columns \
                     WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                    &[&schema, &table],
                )
                .await
                .expect("read the columns")
                .iter()
                .map(|row| row.get(0))
                .collect()
        })
    }
}

/// An audit chain that records what it is given and can be made to fail.
#[derive(Default)]
pub struct Chain {
    records: Mutex<Vec<AuditRecord>>,
    fail: Mutex<bool>,
}

impl Chain {
    pub fn set_failing(&self, failing: bool) {
        *self.fail.lock().unwrap() = failing;
    }

    pub fn records(&self) -> Vec<AuditRecord> {
        self.records.lock().unwrap().clone()
    }

    pub fn kinds(&self) -> Vec<String> {
        self.records().iter().map(|r| r.kind.clone()).collect()
    }
}

impl AuditSink for Chain {
    fn append(&self, record: AuditRecord) -> Result<u64, Error> {
        if *self.fail.lock().unwrap() {
            return Err(Error::AuditFailed {
                action: "test chain down",
            });
        }
        let mut records = self.records.lock().unwrap();
        records.push(record);
        Ok(u64::try_from(records.len()).unwrap())
    }
}
