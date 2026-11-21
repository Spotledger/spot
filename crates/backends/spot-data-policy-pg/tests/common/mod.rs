//! Shared setup: a scratch database, a bridge and pool, a record source over the in-memory store
//! (so effects can be counted and made to fail), and a harness trait both stores implement so one
//! table of cases runs against the in-memory reference and PostgreSQL.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_data_policy::store::{
    ApplyItem, Checkpoint, Effect, ItemOutcome, MemoryStore, OutcomeStatus, RetentionStore,
};
use spot_data_policy::{Action, DataPolicyError, DeletionProof, Record};
use spot_data_policy_pg::{PgRetentionStore, RecordSource};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_tenancy::TenantId;

pub const POOL: usize = 6;

pub fn tenant(n: u8) -> TenantId {
    TenantId::new_v7(1_700_000_000_000, [n; 10])
}

pub fn record(id: &str) -> Record {
    Record {
        id: id.to_owned(),
        doctype: "invoice".to_owned(),
        doc_class: None,
        jurisdiction: None,
        fields: Default::default(),
        created: 1,
        closed: None,
        events: Default::default(),
        references: Vec::new(),
    }
}

pub fn outcome(run: &str, id: &str, status: OutcomeStatus) -> ItemOutcome {
    ItemOutcome {
        run_id: run.to_owned(),
        record_id: id.to_owned(),
        rule_id: "r1".to_owned(),
        action: Some(Action::Delete),
        status,
        reason: None,
        attempt: 1,
        at: 10,
        proof_digest: None,
    }
}

pub fn delete_item(run: &str, id: &str) -> ApplyItem {
    let proof = DeletionProof::compute(&record(id), "r1", 10);
    let mut out = outcome(run, id, OutcomeStatus::Applied);
    out.proof_digest = Some(proof.digest_hex());
    ApplyItem {
        effect: Effect::Delete {
            record_id: id.to_owned(),
        },
        outcome: out,
        audit: None,
        proof: Some(proof),
    }
}

/// The record source the PostgreSQL store is tested with: the in-memory store behind a lock,
/// shared with the test so it can count effects, and a switch that makes the effect fail.
#[derive(Clone)]
pub struct SharedSource {
    pub inner: Arc<Mutex<MemoryStore>>,
    pub fail: Arc<AtomicBool>,
    pub fail_reads: Arc<AtomicBool>,
    pub delay: Arc<Mutex<Duration>>,
}

impl SharedSource {
    pub fn with(records: &[&str]) -> Self {
        let store =
            MemoryStore::from_records(records.iter().map(|id| record(id)).collect()).unwrap();
        Self {
            inner: Arc::new(Mutex::new(store)),
            fail: Arc::new(AtomicBool::new(false)),
            fail_reads: Arc::new(AtomicBool::new(false)),
            delay: Arc::new(Mutex::new(Duration::ZERO)),
        }
    }

    pub fn effects(&self) -> usize {
        self.inner.lock().unwrap().effects().len()
    }

    pub fn count(&self) -> usize {
        self.inner.lock().unwrap().record_count()
    }
}

impl RecordSource for SharedSource {
    fn records(&self) -> Result<Vec<Record>, DataPolicyError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(DataPolicyError::StoreFailed {
                reason: "the records read was made to fail",
            });
        }
        self.inner.lock().unwrap().records()
    }

    fn record(&self, id: &str) -> Result<Option<Record>, DataPolicyError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(DataPolicyError::StoreFailed {
                reason: "the record read was made to fail",
            });
        }
        self.inner.lock().unwrap().record(id)
    }

    fn apply_effect(&mut self, item: &ApplyItem) -> Result<(), DataPolicyError> {
        let delay = *self.delay.lock().unwrap();
        if !delay.is_zero() {
            std::thread::sleep(delay);
        }
        if self.fail.load(Ordering::SeqCst) {
            return Err(DataPolicyError::StoreFailed {
                reason: "the effect was made to fail",
            });
        }
        self.inner.lock().unwrap().apply_item(ApplyItem {
            effect: item.effect.clone(),
            outcome: item.outcome.clone(),
            audit: item.audit.clone(),
            proof: None,
        })
    }
}

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (with a printed message) when no server is configured.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

pub fn bridge(fx: &Fixture, in_flight: usize) -> Arc<Bridge> {
    let mut config = fx.db.config();
    config.pool_max_size = POOL;
    let backend = PgBackend::connect_unchecked(config).expect("backend");
    Arc::new(
        Bridge::for_backend(
            BridgeConfig {
                name: "data_policy_pg_test".to_owned(),
                workers: 2,
                max_in_flight: in_flight,
                queue_capacity: 8,
                default_deadline: Duration::from_secs(10),
                max_deadline: Duration::from_secs(30),
            },
            &backend,
        )
        .expect("bridge"),
    )
}

/// Creates the tenant's schema (the host does this; the ledger only creates its tables).
pub fn create_schema(fx: &Fixture, tenant: &TenantId) {
    fx.rt.block_on(async {
        let client = fx.db.raw_client().await;
        client
            .batch_execute(&format!("CREATE SCHEMA \"{}\"", tenant.schema_name()))
            .await
            .expect("schema");
    });
}

pub fn pg_store(
    fx: &Fixture,
    bridge: &Arc<Bridge>,
    tenant: &TenantId,
    source: SharedSource,
) -> PgRetentionStore<SharedSource> {
    let store = PgRetentionStore::new(Arc::clone(bridge), POOL, *tenant, source).expect("store");
    store.prepare().expect("prepare");
    let _ = fx;
    store
}

/// What the parity cases need from a store under test.
pub trait Harness {
    fn store(&mut self) -> &mut dyn RetentionStore;
    fn effects(&self) -> usize;
    fn live(&self) -> usize;
}

pub struct MemoryHarness(pub MemoryStore);

impl Harness for MemoryHarness {
    fn store(&mut self) -> &mut dyn RetentionStore {
        &mut self.0
    }
    fn effects(&self) -> usize {
        self.0.effects().len()
    }
    fn live(&self) -> usize {
        self.0.record_count()
    }
}

pub struct PgHarness {
    pub store: PgRetentionStore<SharedSource>,
    pub source: SharedSource,
}

impl Harness for PgHarness {
    fn store(&mut self) -> &mut dyn RetentionStore {
        &mut self.store
    }
    fn effects(&self) -> usize {
        self.source.effects()
    }
    fn live(&self) -> usize {
        self.source.count()
    }
}

pub fn checkpoint(run: &str, next: usize) -> Checkpoint {
    let mut c = Checkpoint::new(run, [7; 32]);
    c.next_index = next;
    c.read = next as u64;
    c.applied = next as u64;
    c
}
