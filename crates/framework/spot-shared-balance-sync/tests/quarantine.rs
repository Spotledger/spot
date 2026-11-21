#![allow(clippy::expect_used, clippy::unwrap_used)]
//! The `QuarantineSink` adapter: every refused entry becomes exactly one immutable item in the
//! platform's quarantine queue, keyed on `(node, entry id)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_offline_identity::{MemoryAuditSink, NodeId as RegistryNodeId, NodeRegistry, TenantId};
use spot_quarantine::{
    InMemoryStore, ItemFilter, ItemId, NewItem, QuarantineError, QuarantineItem, QuarantineStore,
    State, StoreFuture,
};
use spot_replica_apply::ReceivedRecord;
use spot_shared_balance::{
    AllocatedQuota, BalanceId, Batch, Confirmation, Confirmer, Entry, EntryId, EntryKind, Hub,
    Millis, NodeId, OnlineOnly, QuarantineSink, RejectReason,
};
use spot_shared_balance_sync::{
    HeadOffice, NodeIdentity, QuarantineAdapter, entry_of, receive_batch,
};

const CARD: &str = "card";
const OTHER: &str = "other";

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

fn batch(who: &NodeId, entries: Vec<Entry>) -> Batch {
    Batch {
        node: who.clone(),
        entries,
        complete: false,
    }
}

fn debit(who: &NodeId, seq: u64, balance: &str, amount: i64) -> Entry {
    entry(who, seq, balance, EntryKind::Debit, amount, 10)
}

/// Head office with a card, a second balance, and one quota for the node on the card.
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

/// The confirmer of an online-only node that never reaches head office. The settle path under
/// test does not call it; it is here so the policy is a real one.
struct Unreachable;

impl Confirmer for Unreachable {
    fn confirm(&self, _entry: &Entry, _now: Millis) -> Confirmation {
        Confirmation::Unreachable
    }
}

/// Every item the store holds, in insertion order.
async fn items(store: &Arc<dyn QuarantineStore>) -> Vec<QuarantineItem> {
    store.list(&ItemFilter::all()).await.expect("list")
}

/// The entry a stored item carries.
async fn held_entry(store: &Arc<dyn QuarantineStore>, id: ItemId) -> Entry {
    let record = store.record(id).await.expect("record").expect("payload");
    entry_of(&record).expect("a balance entry payload")
}

#[tokio::test]
async fn one_item_per_refused_entry_with_its_reason_and_nothing_lost() {
    let mine = node("branch-01");
    let mut hub = hub(&mine);
    let registry = registry_with(&["branch-01"]);
    let mut audit = MemoryAuditSink::default();
    let store: Arc<dyn QuarantineStore> = Arc::new(InMemoryStore::new());
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));

    // One entry per RejectReason a quota node can meet, plus one accepted entry.
    let unknown = debit(&mine, 1, "missing", 10);
    let credit = entry(&mine, 2, CARD, EntryKind::Credit, 10, 10);
    let no_grant = debit(&mine, 3, OTHER, 10);
    let over_quota = debit(&mine, 4, CARD, 150);
    let after_expiry = entry(&mine, 5, CARD, EntryKind::Debit, 10, 1_000);
    let accepted = debit(&mine, 6, CARD, 20);
    let quota_batch = batch(
        &mine,
        vec![
            unknown.clone(),
            credit.clone(),
            no_grant.clone(),
            over_quota.clone(),
            accepted.clone(),
        ],
    );
    let quota = AllocatedQuota;
    let first = receive_batch(
        &NodeIdentity::authenticated(mine.clone()),
        quota_batch.clone(),
        Millis(5),
        &mut adapter,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &quota,
            audit: &mut audit,
        },
    )
    .expect("quota delivery");
    assert_eq!(first.applied.len(), 1);
    assert_eq!(first.quarantined.len(), 4);
    assert_eq!(adapter.pending(), 4);

    // Delivery at the grant's expiry, not the node's `at`, is what expires an entry:
    // this one carries the same times as before but arrives at head office too late.
    let late_batch = batch(&mine, vec![after_expiry.clone()]);
    let late = receive_batch(
        &NodeIdentity::authenticated(mine.clone()),
        late_batch.clone(),
        Millis(1_000),
        &mut adapter,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &quota,
            audit: &mut audit,
        },
    )
    .expect("late delivery");
    assert!(late.applied.is_empty(), "an expired grant applies nothing");
    assert_eq!(late.quarantined.len(), 1);
    assert_eq!(adapter.pending(), 5);

    // Insufficient needs a policy that checks the balance itself: online-only.
    let insufficient = debit(&mine, 7, OTHER, 150);
    let online_batch = batch(&mine, vec![insufficient.clone()]);
    let online = OnlineOnly::new(Unreachable);
    let second = receive_batch(
        &NodeIdentity::authenticated(mine.clone()),
        online_batch.clone(),
        Millis(5),
        &mut adapter,
        HeadOffice {
            registry: &registry,
            hub: &mut hub,
            policy: &online,
            audit: &mut audit,
        },
    )
    .expect("online-only delivery");
    assert_eq!(second.quarantined.len(), 1);

    let created = adapter.flush(1_700_000_000_000_000).await.expect("flush");
    assert_eq!(created.len(), 6, "one item per refused entry");
    assert_eq!(adapter.pending(), 0);
    // Flushing by hand is the older path: the hub is told the queue holds the entries only when
    // the caller confirms them (`receive_and_flush` does this itself).
    for id in adapter.take_confirmed() {
        assert!(hub.confirm_held(&id));
    }
    assert!(hub.offered_unconfirmed().is_empty());

    let held = items(&store).await;
    assert_eq!(held.len(), 6);

    // Each item carries its entry, its reason code and its node.
    let expected = [
        (unknown.id.clone(), RejectReason::UnknownBalance),
        (credit.id.clone(), RejectReason::NotPermitted),
        (no_grant.id.clone(), RejectReason::NoGrant),
        (over_quota.id.clone(), RejectReason::OverQuota),
        (after_expiry.id.clone(), RejectReason::AfterExpiry),
        (insufficient.id.clone(), RejectReason::Insufficient),
    ];
    for (entry_id, reason) in expected {
        let item = held
            .iter()
            .find(|item| {
                item.record_ref.name.as_str() == format!("shared_balance_entry:{entry_id}")
            })
            .unwrap_or_else(|| panic!("an item for {entry_id}"));
        assert_eq!(item.reason_code, reason.code(), "reason of {entry_id}");
        assert_eq!(item.source.as_str(), "branch-01", "the node of {entry_id}");
        assert_eq!(item.state, State::Held);
        assert_eq!(
            held_entry(&store, item.id).await,
            match entry_id.seq {
                1 => unknown.clone(),
                2 => credit.clone(),
                3 => no_grant.clone(),
                4 => over_quota.clone(),
                5 => after_expiry.clone(),
                _ => insufficient.clone(),
            },
            "the payload is the entry that arrived"
        );
    }

    // A replay changes nothing: the hub recognises the entries and the store holds one item each.
    for (batch, policy) in [
        (
            quota_batch,
            &quota as &dyn spot_shared_balance::BalancePolicy,
        ),
        (
            late_batch,
            &quota as &dyn spot_shared_balance::BalancePolicy,
        ),
        (
            online_batch,
            &online as &dyn spot_shared_balance::BalancePolicy,
        ),
    ] {
        let replay = receive_batch(
            &NodeIdentity::authenticated(mine.clone()),
            batch,
            Millis(5),
            &mut adapter,
            HeadOffice {
                registry: &registry,
                hub: &mut hub,
                policy,
                audit: &mut audit,
            },
        )
        .expect("replay");
        assert!(
            replay.quarantined.is_empty(),
            "nothing is quarantined twice"
        );
        adapter.flush(1_700_000_000_000_000).await.expect("flush");
    }
    let after = items(&store).await;
    assert_eq!(after, held, "a replay adds no item and changes none");

    // Every submitted entry is in exactly one place, and the sums add up: no money lost or
    // created by the split between applied and held.
    let card = BalanceId::new(CARD);
    let other = BalanceId::new(OTHER);
    let mut submitted = 0_i64;
    let mut applied = 0_i64;
    let mut withheld = 0_i64;
    for entry in [
        &unknown,
        &credit,
        &no_grant,
        &over_quota,
        &after_expiry,
        &accepted,
        &insufficient,
    ] {
        submitted += entry.amount;
        let in_ledger = hub
            .ledger(&entry.balance)
            .is_some_and(|ledger| ledger.contains(&entry.id));
        let in_queue = held.iter().any(|item| {
            item.record_ref.name.as_str() == format!("shared_balance_entry:{}", entry.id)
        });
        assert!(in_ledger ^ in_queue, "{} is in exactly one place", entry.id);
        if in_ledger {
            applied += entry.amount;
        } else {
            withheld += entry.amount;
        }
    }
    assert_eq!(applied, 20, "the one entry the quota covered");
    assert_eq!(applied + withheld, submitted);
    assert_eq!(hub.balance(&card), Some(180), "200 less the accepted 20");
    assert_eq!(hub.balance(&other), Some(100), "nothing else was applied");
}

#[tokio::test]
async fn the_same_entry_is_never_stored_twice() {
    let mine = node("branch-01");
    let store: Arc<dyn QuarantineStore> = Arc::new(InMemoryStore::new());
    let mut adapter = QuarantineAdapter::new(Arc::clone(&store));
    let refused = debit(&mine, 1, CARD, 10);

    adapter
        .quarantine(&refused, RejectReason::OverQuota)
        .unwrap();
    adapter
        .quarantine(&refused, RejectReason::OverQuota)
        .unwrap();
    assert_eq!(
        adapter.pending(),
        1,
        "the same (node, entry id) queues once"
    );
    let created = adapter.flush(0).await.expect("flush");
    assert_eq!(created.len(), 1);
    assert_eq!(items(&store).await.len(), 1);

    // A second adapter over the same store finds the held item and adds nothing.
    let mut again = QuarantineAdapter::new(Arc::clone(&store));
    again.quarantine(&refused, RejectReason::OverQuota).unwrap();
    let created = again.flush(0).await.expect("flush");
    assert!(created.is_empty());
    assert_eq!(items(&store).await.len(), 1);
}

#[tokio::test]
async fn a_failed_flush_keeps_the_refusals_for_the_next_attempt() {
    let mine = node("branch-01");
    let inner = Arc::new(InMemoryStore::new());
    let flaky: Arc<dyn QuarantineStore> = Arc::new(FlakyStore {
        inner: Arc::clone(&inner),
        failures: AtomicUsize::new(1),
    });
    let mut adapter = QuarantineAdapter::new(Arc::clone(&flaky));

    adapter
        .quarantine(&debit(&mine, 1, CARD, 10), RejectReason::OverQuota)
        .unwrap();
    adapter
        .quarantine(&debit(&mine, 2, CARD, 10), RejectReason::AfterExpiry)
        .unwrap();
    let error = adapter.flush(0).await.expect_err("the store fails once");
    assert_eq!(error.code(), "quarantine.store_failed");
    assert_eq!(adapter.pending(), 2, "nothing is lost on a failed flush");
    assert_eq!(inner.len(), 0);

    let created = adapter.flush(0).await.expect("the retry succeeds");
    assert_eq!(created.len(), 2);
    assert_eq!(adapter.pending(), 0);
    assert_eq!(inner.len(), 2);
}

/// A store that refuses `failures` inserts, then delegates to the in-memory reference.
struct FlakyStore {
    inner: Arc<InMemoryStore>,
    failures: AtomicUsize,
}

impl QuarantineStore for FlakyStore {
    fn insert<'a>(
        &'a self,
        item: NewItem,
        record: ReceivedRecord,
    ) -> StoreFuture<'a, QuarantineItem> {
        Box::pin(async move {
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
