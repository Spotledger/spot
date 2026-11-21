//! Personal-field protection and erasure over a real PostgreSQL wrapped-key table (WP-RTB-01.T8):
//! the pieces `services/journal_privacy.rs` assembles, built the same way, driven directly.
//!
//! - a personal field is sealed under a per-subject key held wrapped in PostgreSQL, and the line the
//!   relay would see holds no clear value;
//! - erasing the subject destroys the key (a tombstone in the table): the same subject never gets a
//!   second key, so the sealed value can never be read again;
//! - a requester without the permission is refused and the refusal is audited;
//! - two tenants with the same subject id have separate key spaces: erasing in one leaves the other.
//!
//! Skips cleanly (printing `SKIPPED:`) without `SPOT_TEST_PG_URL`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use spot_journal_pii::SubjectKeys;
use spot_journal_pii_secrets::testing::{CounterRng, FixedClock, MemoryAudit};
use spot_journal_pii_secrets::{AdapterConfig, ProviderSubjectKeys};
use spot_journal_pii_secrets_pg::{DDL, PgWrappedKeyStore};
use spot_journal_privacy::{
    AdminAction, AdminAudit, CatalogueFields, ErasePermission, Eraser, FieldDecl, Privacy,
    SubjectHasher,
};
use spot_journal_protect::{SharedKeys, SharedNonces};
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgTransaction};
use spot_secrets::MemoryKeyring;
use spot_storage_contract::{
    Catalog, Collation, Isolation, Name, StorageBackend, Transaction, TxOptions,
};

const PERSON: &str = "01900000-0000-7000-8000-0000000000aa";
const SECRET: &str = "grace.hopper@navy.example";

struct Env {
    _db: TestDb,
    #[allow(dead_code)]
    backend: PgBackend,
    bridge: Arc<Bridge>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    let catalog = Arc::new(Catalog::new(1, Collation::new("und").unwrap(), vec![]).unwrap());
    backend.prepare(&catalog).await.unwrap();
    // `spot migrate` creates the table from `schema::extra_tables`; a test database gets it from
    // the store crate's published DDL.
    let mut options = TxOptions::new(
        Name::new("journal_privacy_ddl").unwrap(),
        Isolation::ReadCommitted,
    );
    options.max_writes = 100;
    let mut tx = backend.begin(Arc::clone(&catalog), options).await.unwrap();
    for statement in DDL.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        tx.raw_execute_with(PgTransaction::READ, statement, &[])
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    // A bridge starts its own runtime: build it on a plain thread.
    let for_bridge = backend.clone();
    let bridge = std::thread::spawn(move || {
        let config = BridgeConfig {
            name: "journal_privacy_test".to_owned(),
            workers: 2,
            max_in_flight: 4,
            queue_capacity: 16,
            default_deadline: Duration::from_secs(10),
            max_deadline: Duration::from_secs(30),
        };
        Bridge::for_backend(config, &for_bridge).unwrap()
    })
    .join()
    .unwrap();
    Some(Env {
        _db: db,
        backend,
        bridge: Arc::new(bridge),
    })
}

type Keys = ProviderSubjectKeys<Arc<MemoryKeyring>, PgWrappedKeyStore, MemoryAudit>;

fn keys_for(env: &Env, tenant: &str, ring: &Arc<MemoryKeyring>) -> Arc<Keys> {
    let config = AdapterConfig::new(tenant, b"a-stable-tenant-salt".to_vec());
    Arc::new(
        ProviderSubjectKeys::new(
            config,
            Arc::clone(ring),
            PgWrappedKeyStore::new(Arc::clone(&env.bridge)),
            MemoryAudit::new(),
            Box::new(CounterRng::new(7)),
            Box::new(FixedClock(1_700_000_000_000)),
        )
        .unwrap(),
    )
}

fn decls() -> Vec<FieldDecl> {
    vec![FieldDecl {
        entity: "contact".to_owned(),
        path: "email".to_owned(),
        subject: Some("person".to_owned()),
        purpose: "notifications".to_owned(),
        retention_class: "contact_5y".to_owned(),
        erasable: true,
        secret: false,
    }]
}

fn line() -> Vec<u8> {
    json!({
        "event_id": "01900000-0000-7000-8000-000000100001",
        "topic": "doc_state", "schema_version": 1, "source_id": "acme.example",
        "position": {"generation": 1, "seq": 1},
        "transaction": "01900000-0000-7000-8000-000000200001", "ordinal": 0,
        "subject": {"entity": "contact", "id": PERSON}, "subject_version": 1,
        "meta": {"occurred_at": "2026-01-01T00:00:00Z",
                 "actor": {"kind": "system", "component": "scheduler"},
                 "correlation_id": "01900000-0000-7000-8000-000000300001"},
        "data": {"op": "insert", "entity_schema": 1,
                 "image": {"person": PERSON, "email": SECRET, "city": "Leeds"}},
    })
    .to_string()
    .into_bytes()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

struct Dpo;

impl ErasePermission for Dpo {
    fn may_erase(&self, requester: &str) -> bool {
        requester == "dpo"
    }
}

struct Hash(Arc<Keys>);

impl SubjectHasher for Hash {
    fn hash(&self, subject: &str) -> String {
        self.0.subject_hash(subject)
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<(AdminAction, String, String)>>);

impl AdminAudit for Records {
    fn record(
        &self,
        action: &AdminAction,
        hash: &str,
        requester: &str,
    ) -> Result<(), &'static str> {
        self.0
            .lock()
            .unwrap()
            .push((action.clone(), hash.to_owned(), requester.to_owned()));
        Ok(())
    }
}

fn privacy_and_eraser(keys: &Arc<Keys>, audit: &Arc<Records>) -> (Privacy, Eraser) {
    let fields = CatalogueFields::new(&decls(), &Default::default()).unwrap();
    let shared: SharedKeys = Arc::<Keys>::clone(keys);
    let nonces: SharedNonces = Arc::new(Mutex::new(spot_journal_pii::CounterNonces::new()));
    let privacy = Privacy::new(Arc::clone(&fields), Arc::clone(&shared), nonces);
    let eraser = Eraser::new(
        fields,
        shared,
        Arc::new(Dpo),
        Arc::new(Hash(Arc::clone(keys))),
        audit.clone(),
    );
    (privacy, eraser)
}

/// Runs `work` on a plain thread: the blocking bridge to PostgreSQL refuses a call made from a
/// runtime thread, `block_in_place` included.
fn off_runtime<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| scope.spawn(work).join().unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sealed_field_is_unreadable_after_the_subject_is_erased_and_no_second_key_appears() {
    let Some(env) = env().await else {
        return;
    };
    let ring = Arc::new(MemoryKeyring::new("journal-privacy-test").unwrap());
    let keys = keys_for(&env, "acme", &ring);
    let audit = Arc::new(Records::default());
    let (privacy, eraser) = privacy_and_eraser(&keys, &audit);
    let sealed = off_runtime(|| privacy.protector().protect(&line())).unwrap();
    assert!(
        !contains(&sealed, SECRET),
        "the relay sees no clear personal value"
    );
    assert!(contains(&sealed, "Leeds"), "other fields are untouched");
    let erased = off_runtime(|| eraser.erase_subject(PERSON, "dpo")).unwrap();
    assert_eq!(erased.erased_fields, 1);
    assert!(!erased.is_partial());
    assert!(
        off_runtime(|| keys.key_for(PERSON)).unwrap().is_none(),
        "a tombstone: the key is gone for good"
    );
    // A destroyed subject never gets a second key: protecting again fails instead of minting one.
    assert!(off_runtime(|| privacy.protector().protect(&line())).is_err());
    assert!(off_runtime(|| keys.key_for(PERSON)).unwrap().is_none());
    let records = audit.0.lock().unwrap().clone();
    assert_eq!(
        records[0].0,
        AdminAction::SubjectErased { retained_fields: 0 }
    );
    assert_ne!(records[0].1, PERSON, "the audit target is a salted hash");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unprivileged_erase_is_refused_and_audited_and_keeps_the_key() {
    let Some(env) = env().await else {
        return;
    };
    let ring = Arc::new(MemoryKeyring::new("journal-privacy-test").unwrap());
    let keys = keys_for(&env, "acme", &ring);
    let audit = Arc::new(Records::default());
    let (privacy, eraser) = privacy_and_eraser(&keys, &audit);
    off_runtime(|| privacy.protector().protect(&line())).unwrap();
    let refused = off_runtime(|| eraser.erase_subject(PERSON, "clerk")).unwrap_err();
    assert_eq!(refused.code(), "journal_privacy.permission_denied");
    assert!(off_runtime(|| keys.key_for(PERSON)).unwrap().is_some());
    assert!(matches!(
        audit.0.lock().unwrap()[0].0,
        AdminAction::SubjectEraseRefused { .. }
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn erasing_in_one_tenant_leaves_the_same_subject_id_in_another() {
    let Some(env) = env().await else {
        return;
    };
    let ring_a = Arc::new(MemoryKeyring::new("tenant-a").unwrap());
    let ring_b = Arc::new(MemoryKeyring::new("tenant-b").unwrap());
    let keys_a = keys_for(&env, "tenant_a", &ring_a);
    let keys_b = keys_for(&env, "tenant_b", &ring_b);
    let audit = Arc::new(Records::default());
    let (privacy_a, eraser_a) = privacy_and_eraser(&keys_a, &audit);
    let (privacy_b, _) = privacy_and_eraser(&keys_b, &audit);
    off_runtime(|| {
        privacy_a.protector().protect(&line()).unwrap();
        privacy_b.protector().protect(&line()).unwrap();
        eraser_a.erase_subject(PERSON, "dpo").unwrap();
    });
    assert!(off_runtime(|| keys_a.key_for(PERSON)).unwrap().is_none());
    assert!(
        off_runtime(|| keys_b.key_for(PERSON)).unwrap().is_some(),
        "separate key spaces per tenant"
    );
}
