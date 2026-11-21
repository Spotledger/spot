//! The shipper over the real relay, the signed manifest and an in-memory object store: the
//! journal it writes verifies, a wrong key is caught, two sites on one store stay apart, and no
//! personal value reaches the relay in clear.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use serde_json::json;
use spot_events_contract::envelope::Position;
use spot_journal_pii::{CounterNonces, FieldSpec, KeyHandle, MemoryKeys, PersonalFields};
use spot_journal_protect::{
    ProtectTransform, SharedFields, SharedKeys, SharedNonces, StaticFields,
};
use spot_journal_relay::{Durability, RelayConfig};
use spot_journal_segment::SealLimits;
use spot_journal_ship::memory::{MemoryLease, MemoryOutbox, MemoryWatermarks, RecordingAlarms};
use spot_journal_ship::{
    ChainChecker, ChainState, JournalKeys, OutboxEntry, PassThrough, Ports, ProgressHandle,
    Protector, ShipFault, Shipper, ShipperConfig, Step, StoreChecker, StoreRelayFactory,
    TenantScoped,
};
use spot_journal_store::{
    JournalStore, MemoryStore, ObjectEntry, ObjectMeta, StoreError, StoreFuture, StoredObject,
};
use spot_secrets::MemoryKeyring;

/// A handle on one in-memory store that several relays, checkers and sites share.
#[derive(Clone)]
struct Shared(Arc<MemoryStore>);

impl JournalStore for Shared {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        self.0.put_new(key, bytes)
    }
    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        self.0.put_replace(key, bytes, expected_etag)
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        self.0.get(key)
    }
    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        self.0.head(key)
    }
    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        self.0.list(prefix, after, limit)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.0.delete(key)
    }
}

type Factory = StoreRelayFactory<TenantScoped<Shared>>;

struct Site {
    shipper: Shipper<Factory>,
    handle: ProgressHandle,
    outbox: Arc<MemoryOutbox>,
    keys: Arc<JournalKeys>,
    now: i64,
}

impl Site {
    async fn drain(&mut self) {
        for _ in 0..40 {
            self.now += 1_000;
            if matches!(self.shipper.step(self.now).await, Step::Idle) {
                return;
            }
        }
        panic!("the shipper did not settle");
    }
}

async fn site(tenant: &str, store: &Shared, protector: Arc<dyn Protector>, batch: usize) -> Site {
    let keys = Arc::new(
        JournalKeys::derive(Arc::new(
            MemoryKeyring::new(&format!("key-{tenant}")).unwrap(),
        ))
        .await
        .unwrap(),
    );
    let make_store = {
        let store = store.clone();
        let tenant = tenant.to_owned();
        Arc::new(move || {
            TenantScoped::new(store.clone(), &tenant)
                .map_err(|_| ShipFault::Config("the tenant id is unusable"))
        })
    };
    let signer_keys = Arc::clone(&keys);
    let mut config =
        RelayConfig::new(tenant, tenant, Durability::RemoteSync).with_tail_keys(keys.tail_keys());
    // Seal after every batch so the verifier has segments to look at.
    config.limits = SealLimits {
        max_bytes: 1,
        max_age_millis: 0,
    };
    let factory = StoreRelayFactory::new(
        make_store.clone(),
        Arc::new(move || signer_keys.signer()),
        keys.verifying(),
        config,
    );
    let checker = StoreChecker::new(
        TenantScoped::new(store.clone(), tenant).unwrap(),
        keys.verifying(),
        tenant,
        tenant,
    )
    .unwrap();
    let outbox = Arc::new(MemoryOutbox::new());
    let mut cfg = ShipperConfig::new(tenant, "remote-sync", true);
    cfg.batch_max_entries = batch;
    let (shipper, handle) = Shipper::new(
        cfg,
        Ports {
            outbox: outbox.clone(),
            marks: Arc::new(MemoryWatermarks::new()),
            lease: Arc::new(MemoryLease::held()),
            protector,
            alarms: Arc::new(RecordingAlarms::new()),
            factory,
            checker: Some(Arc::new(checker)),
            backup: None,
        },
    )
    .unwrap();
    Site {
        shipper,
        handle,
        outbox,
        keys,
        now: 1_700_000_000_000,
    }
}

async fn all_keys(store: &Shared, prefix: &str) -> Vec<String> {
    store
        .list(prefix, None, 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.key)
        .collect()
}

#[tokio::test]
async fn a_shipped_journal_has_segments_and_verifies() {
    let store = Shared(Arc::new(MemoryStore::new()));
    let mut acme = site("acme", &store, Arc::new(PassThrough), 20).await;
    acme.outbox.push_range(1, 1, 50);
    acme.drain().await;
    assert_eq!(
        acme.handle.durable(),
        Position {
            generation: 1,
            seq: 50
        }
    );
    let keys = all_keys(&store, "acme/").await;
    assert!(
        keys.iter().any(|k| k.starts_with("acme/segments/")),
        "{keys:?}"
    );
    assert!(
        keys.iter().any(|k| k.starts_with("acme/manifest/")),
        "{keys:?}"
    );
    let checker = StoreChecker::new(
        TenantScoped::new(store.clone(), "acme").unwrap(),
        acme.keys.verifying(),
        "acme",
        "acme",
    )
    .unwrap();
    assert_eq!(checker.check(1).await.unwrap(), ChainState::Clean);
}

#[tokio::test]
async fn a_verifier_with_the_wrong_key_calls_the_chain_damaged() {
    let store = Shared(Arc::new(MemoryStore::new()));
    let mut acme = site("acme", &store, Arc::new(PassThrough), 20).await;
    acme.outbox.push_range(1, 1, 10);
    acme.drain().await;
    let stranger = JournalKeys::derive(Arc::new(MemoryKeyring::new("another-tenant-key").unwrap()))
        .await
        .unwrap();
    let checker = StoreChecker::new(
        TenantScoped::new(store.clone(), "acme").unwrap(),
        stranger.verifying(),
        "acme",
        "acme",
    )
    .unwrap();
    assert!(matches!(
        checker.check(1).await.unwrap(),
        ChainState::Damaged { .. }
    ));
}

#[tokio::test]
async fn two_sites_on_one_store_cannot_reach_each_others_prefix() {
    let store = Shared(Arc::new(MemoryStore::new()));
    let mut acme = site("acme", &store, Arc::new(PassThrough), 20).await;
    let mut globex = site("globex", &store, Arc::new(PassThrough), 20).await;
    acme.outbox.push_range(1, 1, 12);
    globex.outbox.push_range(1, 1, 7);
    acme.drain().await;
    globex.drain().await;
    let acme_keys = all_keys(&store, "acme/").await;
    let globex_keys = all_keys(&store, "globex/").await;
    assert!(!acme_keys.is_empty() && !globex_keys.is_empty());
    assert!(acme_keys.iter().all(|k| k.starts_with("acme/")));
    assert!(globex_keys.iter().all(|k| k.starts_with("globex/")));
    // The scoped view of acme refuses every operation on a globex key and on a globex listing.
    let scoped = TenantScoped::new(store.clone(), "acme").unwrap();
    let foreign = globex_keys[0].clone();
    assert!(matches!(
        scoped.get(&foreign).await,
        Err(StoreError::KeyInvalid(_))
    ));
    assert!(matches!(
        scoped.head(&foreign).await,
        Err(StoreError::KeyInvalid(_))
    ));
    assert!(matches!(
        scoped.put_new(&foreign, vec![1]).await,
        Err(StoreError::KeyInvalid(_))
    ));
    assert!(matches!(
        scoped.delete(&foreign).await,
        Err(StoreError::KeyInvalid(_))
    ));
    assert!(matches!(
        scoped.list("globex/", None, 10).await,
        Err(StoreError::KeyInvalid(_))
    ));
    // Each site's chain verifies under its own key and its own prefix only.
    for (tenant, site) in [("acme", &acme), ("globex", &globex)] {
        let checker = StoreChecker::new(
            TenantScoped::new(store.clone(), tenant).unwrap(),
            site.keys.verifying(),
            tenant,
            tenant,
        )
        .unwrap();
        assert_eq!(
            checker.check(1).await.unwrap(),
            ChainState::Clean,
            "{tenant}"
        );
    }
}

const PERSON: &str = "01900000-0000-7000-8000-0000000000aa";

fn contact_fields() -> SharedFields {
    let fields = PersonalFields::new("contact")
        .unwrap()
        .with_field(FieldSpec::new("email", "person", "notifications", "contact_5y").unwrap());
    Arc::new(StaticFields::new().with(fields))
}

fn transform() -> ProtectTransform {
    let keys = MemoryKeys::new(KeyHandle::new([9; 32]));
    keys.insert(PERSON, KeyHandle::new([10; 32]));
    let keys: SharedKeys = Arc::new(keys);
    let nonces: SharedNonces = Arc::new(Mutex::new(CounterNonces::new()));
    ProtectTransform::new(contact_fields(), keys, nonces)
}

fn state_line(n: u64, email: &str) -> OutboxEntry {
    let line = json!({
        "event_id": format!("01900000-0000-7000-8000-{:012x}", 1_000_000 + n),
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": "acme.example",
        "position": {"generation": 1, "seq": n},
        "transaction": format!("01900000-0000-7000-8000-{:012x}", 2_000_000 + n),
        "ordinal": 0,
        "subject": {"entity": "contact", "id": PERSON},
        "subject_version": n,
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": format!("01900000-0000-7000-8000-{:012x}", 3_000_000 + n),
        },
        "data": {"op": "update", "entity_schema": 1, "image": {"person": PERSON, "email": email, "city": "Leeds"}},
    })
    .to_string();
    OutboxEntry {
        position: Position {
            generation: 1,
            seq: n,
        },
        line: line.into_bytes(),
    }
}

/// Records every line the inner protector produced.
struct Spy {
    inner: Arc<dyn Protector>,
    seen: Mutex<Vec<Vec<u8>>>,
}

impl Protector for Spy {
    fn protect(&self, line: &[u8]) -> Result<Vec<u8>, ShipFault> {
        let out = self.inner.protect(line)?;
        self.seen.lock().unwrap().push(out.clone());
        Ok(out)
    }
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[tokio::test]
async fn a_personal_field_reaches_neither_the_relay_nor_the_store_in_clear() {
    let secret = "grace.hopper@navy.example";
    for (protected, expect_clear) in [(true, false), (false, true)] {
        let store = Shared(Arc::new(MemoryStore::new()));
        let inner: Arc<dyn Protector> = if protected {
            Arc::new(transform())
        } else {
            Arc::new(PassThrough)
        };
        let spy = Arc::new(Spy {
            inner,
            seen: Mutex::new(Vec::new()),
        });
        let mut acme = site("acme", &store, spy.clone(), 20).await;
        for n in 1..=5 {
            acme.outbox.push(state_line(n, secret));
        }
        acme.drain().await;
        let seen = spy.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 5);
        let relay_saw_clear = seen.iter().any(|line| contains(line, secret));
        assert_eq!(relay_saw_clear, expect_clear, "protected = {protected}");
        // Whatever is in the store (tail chunks, segments, manifest) holds no clear value either.
        for key in all_keys(&store, "acme/").await {
            let object = store.get(&key).await.unwrap().unwrap();
            if protected {
                assert!(!contains(&object.bytes, secret), "{key}");
            }
        }
    }
}
