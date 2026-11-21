#![allow(clippy::expect_used, clippy::unwrap_used)]
//! `receive_and_flush`: a delivery is acknowledgeable only after the refused entries are held in
//! the quarantine queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_offline_identity::{MemoryAuditSink, NodeId as RegistryNodeId, NodeRegistry, TenantId};
use spot_quarantine::{
    InMemoryStore, ItemFilter, ItemId, NewItem, QuarantineError, QuarantineItem, QuarantineStore,
    State, StoreFuture,
};
use spot_replica_apply::ReceivedRecord;
use spot_shared_balance::{
    AllocatedQuota, BalanceId, Batch, Entry, EntryId, EntryKind, Hub, Millis, NodeId,
    QuarantineSink, RejectReason,
};
use spot_shared_balance_sync::{
    Acknowledgeable, HeadOffice, NodeIdentity, QuarantineAdapter, SyncError, codes,
    receive_and_flush,
};

const CARD: &str = "card";
const OTHER: &str = "other";
const NOW: i64 = 1_700_000_000_000_000;

fn node(id: &str) -> NodeId {
    NodeId::new(id)
}

fn registry_with(ids: &[&str]) -> NodeRegistry {
    let mut registry = NodeRegistry::new(TenantId::new("acme").expect("tenant"));
    for id in ids {
        registry.register(RegistryNodeId::new(id).expect("registry node"));
    }
    registry
}

fn entry(who: &NodeId, seq: u64, balance: &str, kind: EntryKind, amount: i64, at: u64) -> Entry {
    Entry::new(
        EntryId {
            node: who.clone(),
            seq,
        },
        BalanceId::new(balance),
        kind,
        amount,
        Millis(at),
    )
    .expect("entry")
}

fn debit(who: &NodeId, seq: u64, balance: &str, amount: i64) -> Entry {
    entry(who, seq, balance, EntryKind::Debit, amount, 10)
}

fn batch(who: &NodeId, entries: Vec<Entry>) -> Batch {
    Batch {
        node: who.clone(),
        entries,
        complete: false,
    }
}

/// Head office with a card holding 200, a second balance, and one quota on the card.
fn hub(mine: &NodeId) -> Hub {
    let mut hub = Hub::new(node("head-office"));
    hub.open_balance(BalanceId::new(CARD), 200, Millis(0))
        .expect("open card");
    hub.open_balance(BalanceId::new(OTHER), 100, Millis(0))
        .expect("open other");
    hub.grant_quota(mine, &BalanceId::new(CARD), 100, Millis(1_000), Millis(0))
        .expect("grant");
    hub
}

/// A store that refuses `failures` inserts, then delegates to the in-memory reference, and
/// counts every insert attempt.
struct FlakyStore {
    inner: Arc<InMemoryStore>,
    failures: AtomicUsize,
    attempts: Arc<AtomicUsize>,
}

impl FlakyStore {
    fn build(
        failures: usize,
    ) -> (
        Arc<InMemoryStore>,
        Arc<AtomicUsize>,
        Arc<dyn QuarantineStore>,
    ) {
        let inner = Arc::new(InMemoryStore::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let store: Arc<dyn QuarantineStore> = Arc::new(FlakyStore {
            inner: Arc::clone(&inner),
            failures: AtomicUsize::new(failures),
            attempts: Arc::clone(&attempts),
        });
        (inner, attempts, store)
    }
}

impl QuarantineStore for FlakyStore {
    fn insert<'a>(
        &'a self,
        item: NewItem,
        record: ReceivedRecord,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(async move {
            self.attempts.fetch_add(1, Ordering::SeqCst);
            let left = self.failures.load(Ordering::SeqCst);
            if left > 0 {
                self.failures.store(left - 1, Ordering::SeqCst);
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

/// One scenario's pieces.
struct Site {
    mine: NodeId,
    hub: Hub,
    registry: NodeRegistry,
    audit: MemoryAuditSink,
}

impl Site {
    fn new() -> Self {
        let mine = node("branch-01");
        Self {
            hub: hub(&mine),
            registry: registry_with(&["branch-01"]),
            audit: MemoryAuditSink::default(),
            mine,
        }
    }

    /// Delivers `batch` as `who` the transport authenticated.
    async fn deliver(
        &mut self,
        who: &NodeId,
        batch: Batch,
        adapter: &mut QuarantineAdapter,
    ) -> Result<Acknowledgeable, SyncError> {
        let quota = AllocatedQuota;
        receive_and_flush(
            &NodeIdentity::authenticated(who.clone()),
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

/// A batch of one accepted entry and three the quota refuses, with the amounts that matter.
struct Mixed {
    batch: Batch,
    all: Vec<Entry>,
    refused: Vec<Entry>,
}

fn mixed(mine: &NodeId) -> Mixed {
    let accepted = debit(mine, 1, CARD, 20);
    let over_quota = debit(mine, 2, CARD, 150);
    let no_grant = debit(mine, 3, OTHER, 10);
    let unknown = debit(mine, 4, "missing", 5);
    let all = vec![
        accepted,
        over_quota.clone(),
        no_grant.clone(),
        unknown.clone(),
    ];
    Mixed {
        batch: batch(mine, all.clone()),
        all,
        refused: vec![over_quota, no_grant, unknown],
    }
}

fn held_name(entry: &Entry) -> String {
    format!("shared_balance_entry:{}", entry.id)
}

/// Every submitted entry is on a ledger or held in the queue, never both and never neither, and
/// the two sums add up to what was submitted.
async fn assert_conserved(hub: &Hub, store: &Arc<dyn QuarantineStore>, all: &[Entry]) {
    let held = store.list(&ItemFilter::all()).await.expect("list");
    let (mut submitted, mut applied, mut withheld) = (0_i64, 0_i64, 0_i64);
    for entry in all {
        submitted += entry.amount;
        let in_ledger = hub
            .ledger(&entry.balance)
            .is_some_and(|ledger| ledger.contains(&entry.id));
        let in_queue = held
            .iter()
            .any(|item| item.record_ref.name.as_str() == held_name(entry));
        assert!(in_ledger ^ in_queue, "{} is in exactly one place", entry.id);
        if in_ledger {
            applied += entry.amount;
        } else {
            withheld += entry.amount;
        }
    }
    assert_eq!(applied + withheld, submitted, "no money lost or created");
}

#[tokio::test]
async fn a_failed_flush_is_not_acknowledgeable_and_the_redelivery_flushes_exactly_the_pending_items()
 {
    let mut site = Site::new();
    let (inner, _attempts, store) = FlakyStore::build(1);
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let sent = mixed(&site.mine);
    let mine = site.mine.clone();

    let error = site
        .deliver(&mine, sent.batch.clone(), &mut adapter)
        .await
        .expect_err("the store fails the first flush");
    assert!(matches!(error, SyncError::Quarantine(_)));
    assert_eq!(error.code(), codes::QUARANTINE_FAILED);
    assert_eq!(adapter.pending(), 3, "every refusal is still pending");
    assert_eq!(inner.len(), 0, "nothing reached the queue");
    // The accepted entry is applied; it is the refused ones that must not be lost.
    assert_eq!(site.hub.balance(&BalanceId::new(CARD)), Some(180));

    // The redelivery of the same batch through the same adapter succeeds.
    let ack = site
        .deliver(&mine, sent.batch.clone(), &mut adapter)
        .await
        .expect("the redelivery flushes");
    assert_eq!(ack.node(), &mine);
    assert_eq!(adapter.pending(), 0);
    assert_eq!(inner.len(), 3, "exactly one item per refused entry");

    let held = store.list(&ItemFilter::all()).await.expect("list");
    for refused in &sent.refused {
        let items: Vec<_> = held
            .iter()
            .filter(|item| item.record_ref.name.as_str() == held_name(refused))
            .collect();
        assert_eq!(items.len(), 1, "one item for {}", refused.id);
        assert_eq!(items[0].state, State::Held);
        assert_eq!(items[0].first_seen, NOW);
    }
    assert_conserved(&site.hub, &store, &sent.all).await;

    // A further redelivery is a harmless no-op that is acknowledgeable too.
    let again = site
        .deliver(&mine, sent.batch.clone(), &mut adapter)
        .await
        .expect("a repeat");
    assert!(again.reconciliation().quarantined.is_empty());
    assert_eq!(inner.len(), 3, "no duplicates");
    assert_conserved(&site.hub, &store, &sent.all).await;
}

#[tokio::test]
async fn a_clean_batch_flushes_nothing_and_is_acknowledgeable() {
    let mut site = Site::new();
    let (inner, attempts, store) = FlakyStore::build(0);
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let mine = site.mine.clone();
    let clean = batch(
        &mine,
        vec![debit(&mine, 1, CARD, 20), debit(&mine, 2, CARD, 30)],
    );

    let ack = site
        .deliver(&mine, clean.clone(), &mut adapter)
        .await
        .expect("a clean delivery");
    assert_eq!(ack.reconciliation().applied.len(), 2);
    assert!(ack.reconciliation().quarantined.is_empty());
    assert_eq!(attempts.load(Ordering::SeqCst), 0, "nothing was flushed");
    assert_eq!(inner.len(), 0);
    assert_eq!(adapter.pending(), 0);
    assert_eq!(site.hub.balance(&BalanceId::new(CARD)), Some(150));
    assert_conserved(
        &site.hub,
        &store,
        &[debit(&mine, 1, CARD, 20), debit(&mine, 2, CARD, 30)],
    )
    .await;

    let reconciliation = ack.into_reconciliation();
    assert_eq!(reconciliation.applied.len(), 2);
}

#[tokio::test]
async fn a_refused_sender_never_reaches_the_flush() {
    let mut site = Site::new();
    let (inner, attempts, store) = FlakyStore::build(0);
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let mine = site.mine.clone();

    // An earlier refusal is pending in the adapter; a refused batch must not flush it.
    adapter
        .quarantine(&debit(&mine, 99, CARD, 10), RejectReason::OverQuota)
        .unwrap();
    assert_eq!(adapter.pending(), 1);

    // The batch names another node than the transport authenticated.
    let intruder = node("branch-02");
    let forged = batch(&intruder, vec![debit(&intruder, 1, CARD, 5)]);
    let error = site
        .deliver(&mine, forged, &mut adapter)
        .await
        .expect_err("sender mismatch");
    assert_eq!(error.code(), codes::SENDER_MISMATCH);
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        0,
        "the flush was never reached"
    );
    assert_eq!(adapter.pending(), 1, "the earlier refusal is still pending");

    // A node the hub does not know is refused the same way.
    let stranger = node("branch-77");
    let unknown = batch(&stranger, vec![debit(&stranger, 1, CARD, 5)]);
    let error = site
        .deliver(&stranger, unknown, &mut adapter)
        .await
        .expect_err("not registered");
    assert_eq!(error.code(), codes::NODE_NOT_REGISTERED);
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
    assert_eq!(inner.len(), 0);
    assert_eq!(
        site.hub.balance(&BalanceId::new(CARD)),
        Some(200),
        "nothing applied"
    );
    assert_eq!(site.audit.records.len(), 2, "both refusals are audited");
}
