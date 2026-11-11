//! Idempotency properties of the apply path (WP-EDG-06.T1, ADR-062 G4).
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Owners, backend, fields, item_document, load, name, record};
use futures::future::BoxFuture;
use spot_replica_apply::{
    ApplyCtx, ApplyOutcome, Reason, RecordingAudit, ReplicaAudit, ReplicaMarker,
};
use spot_storage_contract::{DocumentData, RecordId, Value};

/// How many distinct records the fast property run applies. The brief asks for a million; the
/// environment variable lets a developer run a different size locally, and the million-record
/// soak is the `#[ignore]`d test below.
fn fast_records() -> usize {
    std::env::var("REPLICA_APPLY_PROPERTY_RECORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000)
}

/// A deterministic pseudo-random permutation (a Fisher-Yates shuffle over an LCG), so the run
/// is reproducible and no RNG dependency is needed.
fn shuffle(ids: &mut [RecordId], mut state: u64) {
    for i in (1..ids.len()).rev() {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let j = ((state >> 33) as usize) % (i + 1);
        ids.swap(i, j);
    }
}

/// An item record at version 1 for `id`, with one child row.
fn item(id: RecordId) -> spot_replica_apply::ReceivedRecord {
    record(item_document(id, "Widget", "1.00", 1), 1, 1, vec![])
}

/// An item record at version 1 for `id`, with no child rows (the memory-lean soak record).
fn lean_item(id: RecordId) -> spot_replica_apply::ReceivedRecord {
    let doc = DocumentData {
        entity: name("item"),
        id,
        fields: fields(&[
            ("title", Value::Text("Widget".to_owned())),
            ("qty", Value::Null),
            ("origin_node", Value::Null),
            ("origin_id", Value::Null),
            ("origin_version", Value::Int(1)),
        ]),
        children: BTreeMap::new(),
    };
    record(doc, 1, 1, vec![])
}

/// An audit sink that only counts, so a million-record soak does not hold a million markers.
#[derive(Default)]
struct CountingAudit {
    count: AtomicUsize,
}

impl CountingAudit {
    fn len(&self) -> usize {
        self.count.load(Ordering::Relaxed)
    }
}

impl ReplicaAudit for CountingAudit {
    fn replicated<'a>(&'a self, _marker: &'a ReplicaMarker) -> BoxFuture<'a, Result<(), Reason>> {
        self.count.fetch_add(1, Ordering::Relaxed);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn idempotency_at_scale() {
    let n = fast_records();
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ApplyCtx::new(&owners, &audit, 0);

    let mut ids: Vec<RecordId> = (0..n).map(|_| RecordId::new_v7()).collect();

    // First pass: every record is new and stores.
    shuffle(&mut ids, 0x5eed_1234);
    for &id in &ids {
        let outcome = common::apply_once(&backend, &item(id), &ctx).await;
        assert_eq!(
            outcome,
            ApplyOutcome::Stored,
            "first pass {id}: {outcome:?}"
        );
    }
    assert_eq!(audit.len(), n, "one marker per stored record");

    // Second pass in a different order: every record is a duplicate and changes nothing.
    shuffle(&mut ids, 0xfeed_5678);
    for &id in &ids {
        let outcome = common::apply_once(&backend, &item(id), &ctx).await;
        assert_eq!(outcome, ApplyOutcome::Duplicate, "second pass {id}");
    }
    assert_eq!(audit.len(), n, "a duplicate writes no new marker");

    // A sample of the records is present exactly once.
    for &id in ids.iter().step_by((n / 100).max(1)).take(100) {
        assert!(load(&backend, "item", id).await.is_some(), "{id} is stored");
    }
}

#[tokio::test]
#[ignore = "a soak: a million records, about ten minutes in a debug build; run with --ignored"]
async fn a_million_records_apply_once_and_are_idempotent() {
    let n = 1_000_000;
    let backend = backend();
    let owners = Owners::mirror();
    let audit = CountingAudit::default();
    let ctx = ApplyCtx::new(&owners, &audit, 0);

    let mut ids: Vec<RecordId> = (0..n).map(|_| RecordId::new_v7()).collect();

    shuffle(&mut ids, 0x5eed_1234);
    for &id in &ids {
        let outcome = common::apply_once(&backend, &lean_item(id), &ctx).await;
        assert_eq!(
            outcome,
            ApplyOutcome::Stored,
            "first pass {id}: {outcome:?}"
        );
    }
    assert_eq!(audit.len(), n);

    shuffle(&mut ids, 0xfeed_5678);
    for &id in &ids {
        assert_eq!(
            common::apply_once(&backend, &lean_item(id), &ctx).await,
            ApplyOutcome::Duplicate,
            "second pass {id}"
        );
    }
    assert_eq!(audit.len(), n, "a duplicate writes no new marker");

    for &id in ids.iter().step_by(n / 100).take(100) {
        assert!(load(&backend, "item", id).await.is_some(), "{id} is stored");
    }
}

#[tokio::test]
async fn idempotency_ignores_the_stream_sequence() {
    // The apply key is (name, version), not the sequence: a redelivered record with a different
    // sequence is still a duplicate.
    let backend = backend();
    let owners = Owners::mirror();
    let audit = RecordingAudit::new();
    let ctx = ApplyCtx::new(&owners, &audit, 0);
    let id = RecordId::new_v7();

    let mut first = item(id);
    first.seq = 1;
    assert!(common::apply_once(&backend, &first, &ctx).await.is_stored());

    let mut redelivered = item(id);
    redelivered.seq = 99;
    assert_eq!(
        common::apply_once(&backend, &redelivered, &ctx).await,
        ApplyOutcome::Duplicate
    );
}
