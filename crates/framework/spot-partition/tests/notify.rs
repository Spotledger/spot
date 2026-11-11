//! The running-low signal reaches the outbox in the allocation's own transaction (WP-EDG-03.T7).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::BoxFuture;
use spot_app_api::prelude::{Name, RecordId};
use spot_partition::{
    BranchSeries, LeaseId, LowWater, MemorySeriesStore, OutboxWriter, PartitionError, RangeLease,
    RunningLowNotifier, TxOutbox,
};
use spot_storage_contract::SeriesKey;
use spot_storage_contract::{
    ChangeFeed, FeedPosition, Isolation, OutboxEvent, StorageBackend, Transaction, TxOptions,
    contract_tests,
};
use spot_storage_fake::{FakeBackend, FakeTx};
use spot_sync_protocol::NodeId;
use time::OffsetDateTime;

fn at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn node() -> NodeId {
    NodeId::new("edge-khi#branch_07").unwrap()
}

fn lease(id: u64, from: u64, to: u64) -> RangeLease {
    RangeLease::new(
        SeriesKey::new("SINV-").unwrap(),
        from,
        to,
        node(),
        LeaseId::new(id),
    )
    .unwrap()
}

/// A lease of ten numbers; half is low, so the fifth allocation crosses the mark.
fn series(store: &Arc<MemorySeriesStore>, lease: RangeLease) -> BranchSeries {
    BranchSeries::install(lease, LowWater::half(), store.clone()).unwrap()
}

async fn begin(backend: &FakeBackend) -> FakeTx {
    backend
        .begin(
            Arc::new(contract_tests::catalog().unwrap()),
            TxOptions::new(
                Name::new("partition_notify").unwrap(),
                Isolation::ReadCommitted,
            ),
        )
        .await
        .unwrap()
}

async fn events(backend: &FakeBackend) -> Vec<OutboxEvent> {
    backend
        .read_outbox(FeedPosition::START, 1_000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        .collect()
}

/// Allocates `count` numbers in one committed transaction each.
async fn allocate_committed(
    notifier: &mut RunningLowNotifier<TxOutbox>,
    series: &mut BranchSeries,
    backend: &FakeBackend,
    count: usize,
) {
    for _ in 0..count {
        let mut tx = begin(backend).await;
        let got = notifier
            .allocate(series, &mut tx, RecordId::new_v7(), at())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        notifier.committed(&got);
    }
}

fn notifier() -> RunningLowNotifier<TxOutbox> {
    RunningLowNotifier::new(Arc::new(TxOutbox))
}

#[tokio::test]
async fn crossing_the_low_water_mark_enqueues_exactly_one_event_with_counts_only() {
    let backend = FakeBackend::new();
    let store = Arc::new(MemorySeriesStore::new());
    let mut series = series(&store, lease(1, 1, 10));
    let mut notifier = notifier();

    allocate_committed(&mut notifier, &mut series, &backend, 4).await;
    assert!(
        events(&backend).await.is_empty(),
        "6 of 10 left: not low yet"
    );

    allocate_committed(&mut notifier, &mut series, &backend, 1).await;
    let sent = events(&backend).await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].topic.as_str(), "sys_partition_range_low");
    let payload: Json = serde_json::from_str(&sent[0].payload).unwrap();
    assert_eq!(
        payload["data"],
        serde_json::json!({
            "series": "SINV-", "node": "edge-khi#branch_07", "remaining": 5, "total": 10,
        })
    );
    assert_eq!(
        payload["data"].as_object().unwrap().len(),
        4,
        "no other field"
    );
}

#[tokio::test]
async fn a_second_allocation_in_the_same_lease_enqueues_none() {
    let backend = FakeBackend::new();
    let store = Arc::new(MemorySeriesStore::new());
    let mut series = series(&store, lease(1, 1, 10));
    let mut notifier = notifier();
    allocate_committed(&mut notifier, &mut series, &backend, 9).await;
    assert_eq!(events(&backend).await.len(), 1);
}

#[tokio::test]
async fn two_low_allocations_in_one_transaction_enqueue_one_event() {
    let backend = FakeBackend::new();
    let store = Arc::new(MemorySeriesStore::new());
    let mut series = series(&store, lease(1, 1, 10));
    let mut notifier = notifier();
    allocate_committed(&mut notifier, &mut series, &backend, 4).await;

    let mut tx = begin(&backend).await;
    let a = notifier
        .allocate(&mut series, &mut tx, RecordId::new_v7(), at())
        .await
        .unwrap();
    let b = notifier
        .allocate(&mut series, &mut tx, RecordId::new_v7(), at())
        .await
        .unwrap();
    assert!(a.announced() && !b.announced());
    tx.commit().await.unwrap();
    notifier.committed(&a);
    notifier.committed(&b);
    assert_eq!(events(&backend).await.len(), 1);
}

#[tokio::test]
async fn a_rolled_back_allocation_leaves_no_event_and_keeps_the_notice_armed() {
    let backend = FakeBackend::new();
    let store = Arc::new(MemorySeriesStore::new());
    let mut series = series(&store, lease(1, 1, 10));
    let mut notifier = notifier();
    allocate_committed(&mut notifier, &mut series, &backend, 4).await;

    let mut tx = begin(&backend).await;
    let got = notifier
        .allocate(&mut series, &mut tx, RecordId::new_v7(), at())
        .await
        .unwrap();
    assert!(got.announced(), "enqueued inside the transaction");
    tx.rollback().await.unwrap();
    notifier.rolled_back(&got);
    assert!(
        events(&backend).await.is_empty(),
        "no event after the rollback"
    );

    // The number is never reissued, and the next committed allocation still announces.
    allocate_committed(&mut notifier, &mut series, &backend, 1).await;
    assert_eq!(events(&backend).await.len(), 1);
}

#[tokio::test]
async fn a_new_lease_rearms_the_notice() {
    let backend = FakeBackend::new();
    let store = Arc::new(MemorySeriesStore::new());
    let mut first = series(&store, lease(1, 1, 4));
    let mut notifier = notifier();
    allocate_committed(&mut notifier, &mut first, &backend, 4).await;
    assert_eq!(events(&backend).await.len(), 1);

    let mut second = series(&store, lease(2, 5, 8));
    allocate_committed(&mut notifier, &mut second, &backend, 4).await;
    assert_eq!(events(&backend).await.len(), 2, "one per lease");
}

/// A writer whose outbox is unavailable.
struct Refusing;

impl OutboxWriter<FakeTx> for Refusing {
    fn write<'a>(
        &'a self,
        _tx: &'a mut FakeTx,
        _event: &'a OutboxEvent,
    ) -> BoxFuture<'a, Result<(), PartitionError>> {
        Box::pin(async {
            Err(PartitionError::Store {
                reason: "the outbox is unavailable".to_owned(),
            })
        })
    }
}

#[tokio::test]
async fn a_failing_outbox_write_does_not_stop_numbering_and_is_reported_once_per_lease() {
    let backend = FakeBackend::new();
    let store = Arc::new(MemorySeriesStore::new());
    let mut series = series(&store, lease(1, 1, 10));
    let mut notifier = RunningLowNotifier::new(Arc::new(Refusing));

    let mut numbers = Vec::new();
    for _ in 0..8 {
        let mut tx = begin(&backend).await;
        let got = notifier
            .allocate(&mut series, &mut tx, RecordId::new_v7(), at())
            .await
            .unwrap();
        tx.commit().await.unwrap();
        numbers.push(got.number);
    }
    assert_eq!(numbers, [1, 2, 3, 4, 5, 6, 7, 8], "numbering never stopped");
    assert_eq!(notifier.failures(), 4, "counted at every low allocation");
    let report = notifier.take_report().expect("reported once");
    assert_eq!(report.code(), "partition.range_low_not_recorded");
    assert!(notifier.take_report().is_none(), "logged once per lease");
    assert!(events(&backend).await.is_empty());
}
