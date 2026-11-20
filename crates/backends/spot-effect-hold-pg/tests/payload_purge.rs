//! The payload purge on PostgreSQL (WP-MIG-08.T7): a purge clears the references and queues their
//! handles in one statement, a handle another effect still references is not offered for deletion,
//! the drain deletes the bytes and completes the queue, and a drain that fails part-way leaves the
//! rest queued. Needs `SPOT_TEST_PG_URL` (skipped otherwise).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::*;
use spot_effect_hold::{
    EffectState, InMemoryPayloadStore, LedgerStore, NewEffect, PayloadRef, RetentionPolicy,
    drain_payload_deletes, purge_expired_with_payloads,
};
use spot_postgres::testing::TestDb;

fn stored(
    site: &spot_core::SiteId,
    key: &str,
    handle: &str,
    m: &spot_effect_hold::MigrationId,
) -> NewEffect {
    let mut new = effect(site, key, 1, m);
    new.effect.payload_ref = PayloadRef::stored(handle, "hash", 10);
    new
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_purge_queues_handles_and_a_shared_handle_waits_for_its_last_reference() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 8, "hold_payload_purge").await;
    let s = site("payload-pg.example");
    let m = mig("mig-1");
    let bytes = InMemoryPayloadStore::new();
    for handle in ["own", "shared"] {
        bytes.put(handle, b"body".to_vec());
    }
    let mut ids = Vec::new();
    for (key, handle, at) in [
        ("a", "own", 10),
        ("b", "shared", 10),
        ("c", "shared", 5_000_000_000_000),
    ] {
        let id = ledger
            .append_effect(stored(&s, key, handle, &m))
            .await
            .unwrap();
        ledger
            .transition_effect_at(id, EffectState::Held, EffectState::Dispatched, None, at)
            .await
            .unwrap();
        ids.push(id);
    }
    let policy = RetentionPolicy::new(1, 1).unwrap();
    let now = 3_000_000_000_000;
    let report = purge_expired_with_payloads(&ledger, &bytes, &s, policy, now, 100)
        .await
        .unwrap();
    assert_eq!((report.purged, report.payloads_deleted), (2, 1));
    assert!(!bytes.contains("own"));
    assert!(bytes.contains("shared"), "effect c still references it");

    let later = 10_000_000_000_000;
    let last = purge_expired_with_payloads(&ledger, &bytes, &s, policy, later, 100)
        .await
        .unwrap();
    assert_eq!((last.purged, last.payloads_deleted), (1, 1));
    assert!(bytes.is_empty());
    assert!(
        ledger
            .pending_payload_deletes(&s, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_drain_leaves_the_handles_queued() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let ledger = ledger(&db, 8, "hold_payload_queue").await;
    let s = site("payload-pg-2.example");
    let m = mig("mig-1");
    for key in ["a", "b"] {
        let id = ledger
            .append_effect(stored(&s, key, &format!("blob-{key}"), &m))
            .await
            .unwrap();
        ledger
            .transition_effect_at(id, EffectState::Held, EffectState::Discarded, None, 10)
            .await
            .unwrap();
    }
    let cleared = ledger
        .purge_payloads(&s, &[EffectState::Discarded], 100, 100)
        .await
        .unwrap();
    assert_eq!(cleared, 2);
    assert_eq!(
        ledger.pending_payload_deletes(&s, 10).await.unwrap(),
        vec!["blob-a".to_owned(), "blob-b".to_owned()]
    );
    // A limit of one drains one handle; the other stays queued.
    let bytes = InMemoryPayloadStore::new();
    assert_eq!(
        drain_payload_deletes(&ledger, &bytes, &s, 1).await.unwrap(),
        1
    );
    assert_eq!(
        ledger.pending_payload_deletes(&s, 10).await.unwrap(),
        vec!["blob-b".to_owned()]
    );
    // Another site's queue is separate.
    let other = site("payload-pg-3.example");
    assert!(
        ledger
            .pending_payload_deletes(&other, 10)
            .await
            .unwrap()
            .is_empty()
    );
}
