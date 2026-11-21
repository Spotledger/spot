#![allow(clippy::expect_used, clippy::unwrap_used)]
//! A refused entry's hub mark is confirmed only once the queue holds the entry: a
//! failed flush, a lost adapter or a restart never leaves an entry held nowhere, and a redelivery
//! offers it again.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use spot_offline_identity::{MemoryAuditSink, NodeId as RegistryNodeId, NodeRegistry, TenantId};
use spot_quarantine::{
    InMemoryStore, ItemFilter, ItemId, NewItem, QuarantineError, QuarantineItem, QuarantineStore,
    StoreFuture,
};
use spot_replica_apply::ReceivedRecord;
use spot_shared_balance::{
    AllocatedQuota, BalanceId, Batch, Entry, EntryId, EntryKind, Hub, Mark, Millis, NodeId,
};
use spot_shared_balance_sync::{
    Acknowledgeable, HeadOffice, NodeIdentity, QuarantineAdapter, SyncError, receive_and_flush,
};

const CARD: &str = "card";
const NOW: i64 = 1_700_000_000_000_000;

fn node(id: &str) -> NodeId {
    NodeId::new(id)
}

fn debit(who: &NodeId, seq: u64, balance: &str, amount: i64) -> Entry {
    Entry::new(
        EntryId {
            node: who.clone(),
            seq,
        },
        BalanceId::new(balance),
        EntryKind::Debit,
        amount,
        Millis(10),
    )
    .expect("entry")
}

/// A store that fails inserts when `fail` says so (by attempt number), else delegates.
struct ScriptedStore {
    inner: Arc<InMemoryStore>,
    attempts: AtomicUsize,
    fail: Box<dyn Fn(usize) -> bool + Send + Sync>,
}

impl ScriptedStore {
    fn build(
        fail: impl Fn(usize) -> bool + Send + Sync + 'static,
    ) -> (Arc<InMemoryStore>, Arc<dyn QuarantineStore>) {
        let inner = Arc::new(InMemoryStore::new());
        let store: Arc<dyn QuarantineStore> = Arc::new(ScriptedStore {
            inner: Arc::clone(&inner),
            attempts: AtomicUsize::new(0),
            fail: Box::new(fail),
        });
        (inner, store)
    }
}

impl QuarantineStore for ScriptedStore {
    fn insert<'a>(
        &'a self,
        item: NewItem,
        record: ReceivedRecord,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(async move {
            let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
            if (self.fail)(attempt) {
                return Err(QuarantineError::Store {
                    why: "the test store is failing".to_owned(),
                });
            }
            self.inner.insert(item, record).await
        })
    }

    fn item<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<QuarantineItem>> {
        self.inner.item(id)
    }

    fn record<'a>(&'a self, id: ItemId) -> StoreFuture<'a, Option<ReceivedRecord>> {
        self.inner.record(id)
    }

    fn update<'a>(&'a self, item: &'a QuarantineItem) -> StoreFuture<'a, ()> {
        self.inner.update(item)
    }

    fn list<'a>(&'a self, filter: &'a ItemFilter) -> StoreFuture<'a, Vec<QuarantineItem>> {
        self.inner.list(filter)
    }

    fn find<'a>(
        &'a self,
        source: &'a spot_sync_protocol::SourceId,
        name: &'a spot_sync_protocol::RecordName,
        version: u64,
    ) -> StoreFuture<'a, Option<QuarantineItem>> {
        self.inner.find(source, name, version)
    }
}

struct Site {
    mine: NodeId,
    hub: Hub,
    registry: NodeRegistry,
    audit: MemoryAuditSink,
}

impl Site {
    fn new() -> Self {
        let mine = node("branch-01");
        let mut hub = Hub::new(node("head-office"));
        hub.open_balance(BalanceId::new(CARD), 200, Millis(0))
            .expect("open card");
        hub.grant_quota(&mine, &BalanceId::new(CARD), 100, Millis(1_000), Millis(0))
            .expect("grant");
        let mut registry = NodeRegistry::new(TenantId::new("acme").expect("tenant"));
        registry.register(RegistryNodeId::new("branch-01").expect("registry node"));
        Self {
            mine,
            hub,
            registry,
            audit: MemoryAuditSink::default(),
        }
    }

    async fn deliver(
        &mut self,
        batch: Batch,
        adapter: &mut QuarantineAdapter,
    ) -> Result<Acknowledgeable, SyncError> {
        let quota = AllocatedQuota;
        receive_and_flush(
            &NodeIdentity::authenticated(self.mine.clone()),
            batch,
            Millis(5),
            adapter,
            HeadOffice {
                registry: &self.registry,
                hub: &mut self.hub,
                policy: &quota,
                audit: &mut self.audit,
            },
            NOW,
        )
        .await
    }
}

/// Entries the quota refuses: more than the 100 granted, or on a balance the hub lacks.
fn refused_entries(mine: &NodeId, count: u64) -> Vec<Entry> {
    (1..=count)
        .map(|seq| debit(mine, seq, "missing", 5))
        .collect()
}

fn batch_of(mine: &NodeId, entries: Vec<Entry>) -> Batch {
    Batch {
        node: mine.clone(),
        entries,
        complete: false,
    }
}

#[tokio::test]
async fn a_failed_flush_acknowledges_nothing_and_leaves_the_marks_offered() {
    let mut site = Site::new();
    let (inner, store) = ScriptedStore::build(|attempt| attempt == 0);
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let entries = refused_entries(&site.mine, 3);
    let delivery = batch_of(&site.mine, entries.clone());

    let error = site
        .deliver(delivery.clone(), &mut adapter)
        .await
        .expect_err("the first insert fails");
    assert_eq!(
        error.code(),
        spot_shared_balance_sync::codes::QUARANTINE_FAILED
    );
    assert_eq!(inner.len(), 0);
    assert_eq!(adapter.pending(), 3);
    for entry in &entries {
        assert_eq!(site.hub.mark(&entry.id), Some(Mark::Offered));
    }
    assert_eq!(site.hub.offered_unconfirmed().len(), 3);

    // The redelivery through the same adapter flushes them and the marks turn to held.
    let ack = site
        .deliver(delivery, &mut adapter)
        .await
        .expect("the redelivery");
    assert_eq!(ack.reconciliation().handled().len(), 3);
    assert_eq!(inner.len(), 3);
    assert!(site.hub.offered_unconfirmed().is_empty());
    for entry in &entries {
        assert!(site.hub.is_quarantined(&entry.id));
    }
}

#[tokio::test]
async fn a_flush_that_fails_part_way_confirms_what_the_queue_holds_and_no_more() {
    let mut site = Site::new();
    // The first two inserts work, the third fails.
    let (inner, store) = ScriptedStore::build(|attempt| attempt == 2);
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let entries = refused_entries(&site.mine, 4);
    let delivery = batch_of(&site.mine, entries.clone());

    site.deliver(delivery.clone(), &mut adapter)
        .await
        .expect_err("the third insert fails");
    assert_eq!(inner.len(), 2);
    assert!(site.hub.is_quarantined(&entries[0].id));
    assert!(site.hub.is_quarantined(&entries[1].id));
    assert_eq!(site.hub.mark(&entries[2].id), Some(Mark::Offered));
    assert_eq!(site.hub.mark(&entries[3].id), Some(Mark::Offered));

    let _ = site.deliver(delivery, &mut adapter).await.expect("retry");
    assert_eq!(inner.len(), 4, "each entry is queued once");
    assert!(site.hub.offered_unconfirmed().is_empty());
}

#[tokio::test]
async fn a_lost_adapter_is_replaced_and_the_redelivery_offers_it_the_same_entries() {
    let mut site = Site::new();
    let (inner, store) = ScriptedStore::build(|attempt| attempt == 0);
    let entries = refused_entries(&site.mine, 3);
    let delivery = batch_of(&site.mine, entries.clone());

    {
        let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
        site.deliver(delivery.clone(), &mut adapter)
            .await
            .expect_err("the flush fails");
        assert_eq!(adapter.pending(), 3);
        // The adapter is dropped here with its three items still pending (a restart).
    }
    assert_eq!(inner.len(), 0);
    assert_eq!(site.hub.offered_unconfirmed().len(), 3);

    let mut fresh = QuarantineAdapter::new(Arc::clone(&store));
    let ack = site
        .deliver(delivery, &mut fresh)
        .await
        .expect("a new adapter takes the redelivery");
    assert_eq!(ack.reconciliation().quarantined.len(), 3);
    assert_eq!(inner.len(), 3, "the entries reached the queue");
    assert!(site.hub.offered_unconfirmed().is_empty());
}

#[tokio::test]
async fn a_repeat_after_held_does_not_call_the_sink() {
    let mut site = Site::new();
    let (inner, store) = ScriptedStore::build(|_| false);
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let delivery = batch_of(&site.mine, refused_entries(&site.mine, 2));

    let _ = site.deliver(delivery.clone(), &mut adapter).await.unwrap();
    assert_eq!(inner.len(), 2);

    // A new adapter that is never flushed would show a sink call as a pending item.
    let mut watcher = QuarantineAdapter::new(Arc::clone(&store));
    let ack = site.deliver(delivery, &mut watcher).await.unwrap();
    assert_eq!(ack.reconciliation().duplicates.len(), 2);
    assert!(ack.reconciliation().quarantined.is_empty());
    assert_eq!(watcher.pending(), 0, "the sink was not offered them again");
    assert_eq!(inner.len(), 2);
}

#[tokio::test]
async fn a_hub_whose_confirmation_was_lost_catches_up_from_the_queue() {
    // The queue holds the entry but the hub's mark is only offered (the confirmation never
    // reached it). The redelivery recognises the item and confirms without storing it twice.
    let mut site = Site::new();
    let (inner, store) = ScriptedStore::build(|_| false);
    let entries = refused_entries(&site.mine, 1);
    let delivery = batch_of(&site.mine, entries.clone());

    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let _ = site.deliver(delivery.clone(), &mut adapter).await.unwrap();
    assert_eq!(inner.len(), 1);

    // Simulate the lost confirmation: a hub that offered the entry but never confirmed it.
    let mut fresh_site = Site::new();
    let mut second = QuarantineAdapter::new(Arc::clone(&store));
    let _ = fresh_site
        .deliver(delivery, &mut second)
        .await
        .expect("a hub with no mark");
    assert_eq!(
        inner.len(),
        1,
        "the queue's item is recognised, not stored again"
    );
    assert!(fresh_site.hub.is_quarantined(&entries[0].id));
}

/// A small deterministic generator, so a failing seed reproduces.
struct Lcg(AtomicU64);

impl Lcg {
    fn next(&self) -> u64 {
        let next = self
            .0
            .load(Ordering::SeqCst)
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0.store(next, Ordering::SeqCst);
        next >> 33
    }
}

#[tokio::test]
async fn six_seeds_of_random_flush_failures_and_lost_adapters_lose_nothing_and_store_nothing_twice()
{
    for seed in 1_u64..=6 {
        let rng = Arc::new(Lcg(AtomicU64::new(seed.wrapping_mul(0x9E37_79B9))));
        let mut site = Site::new();
        let failing = Arc::clone(&rng);
        let (inner, store) = ScriptedStore::build(move |_| failing.next().is_multiple_of(3));
        let entries = refused_entries(&site.mine, 12);
        let wanted: BTreeSet<EntryId> = entries.iter().map(|e| e.id.clone()).collect();
        let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
        let mut acknowledged: BTreeSet<EntryId> = BTreeSet::new();

        for _ in 0..200 {
            let remaining: Vec<Entry> = entries
                .iter()
                .filter(|e| !acknowledged.contains(&e.id))
                .cloned()
                .collect();
            if remaining.is_empty() {
                break;
            }
            // The adapter is lost now and then, with whatever it had pending.
            if rng.next().is_multiple_of(4) {
                adapter = QuarantineAdapter::new(Arc::clone(&store));
            }
            if let Ok(ack) = site
                .deliver(batch_of(&site.mine, remaining), &mut adapter)
                .await
            {
                acknowledged.extend(ack.reconciliation().handled());
            }
        }
        assert_eq!(
            acknowledged, wanted,
            "seed {seed}: every entry acknowledged"
        );
        let queued = inner.list(&ItemFilter::all()).await.expect("list");
        assert_eq!(
            queued.len(),
            wanted.len(),
            "seed {seed}: each refused entry is queued once, none lost"
        );
        assert!(site.hub.offered_unconfirmed().is_empty(), "seed {seed}");
    }
}
