//! WP-MIG-08.T7: the purge deletes the stored payload bytes, not only the references. The bytes
//! are gone after a purge, a payload shared by two holds stays until the last reference goes, a
//! crash between clearing the references and deleting the bytes leaves nothing that references
//! missing bytes (and the next purge finishes the job), and the audit record carries counts only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::{NOW, effect, policies};
use futures::future::BoxFuture;
use spot_core::SiteId;
use spot_effect_hold::{
    AuditKind, Dispatcher, EffectClass, EffectState, Error, HoldService, InMemoryLedger,
    InMemoryPayloadStore, LedgerStore, MigrationId, NewEffect, Notifier, PayloadRef, PayloadStore,
    RecordingAudit, RecordingDispatcher, RecordingNotifier, Result, RetentionPolicy,
    drain_payload_deletes,
};

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

async fn finished(
    ledger: &InMemoryLedger,
    s: &SiteId,
    key: &str,
    handle: &str,
    state: EffectState,
    at: i64,
) {
    let mut new = effect(s, EffectClass::Mail, key, "node:a");
    new.payload_ref = PayloadRef::stored(handle, "hash", 10);
    let id = ledger
        .append_effect(NewEffect {
            effect: new,
            migration_id: MigrationId::new("mig-1").unwrap(),
        })
        .await
        .unwrap();
    ledger
        .transition_effect_at(id, EffectState::Held, state, None, at)
        .await
        .unwrap();
}

fn service(
    s: &SiteId,
    ledger: Arc<InMemoryLedger>,
    audit: Arc<RecordingAudit>,
    payloads: Option<Arc<dyn PayloadStore>>,
) -> HoldService {
    let svc = HoldService::new(
        s.clone(),
        ledger,
        policies(),
        audit,
        Arc::new(RecordingDispatcher::new()) as Arc<dyn Dispatcher>,
        Arc::new(RecordingNotifier::new()) as Arc<dyn Notifier>,
    );
    match payloads {
        Some(store) => svc.with_payload_store(store),
        None => svc,
    }
}

fn policy() -> RetentionPolicy {
    RetentionPolicy::new(1, 1).unwrap()
}

#[tokio::test]
async fn the_bytes_are_gone_after_a_purge_and_the_audit_has_counts_only() {
    let s = site("payload-1.example");
    let ledger = Arc::new(InMemoryLedger::new());
    let audit = Arc::new(RecordingAudit::new());
    let bytes = Arc::new(InMemoryPayloadStore::new());
    bytes.put("blob-a", b"mail body a".to_vec());
    bytes.put("blob-b", b"mail body b".to_vec());
    bytes.put("blob-held", b"still needed".to_vec());
    finished(&ledger, &s, "a", "blob-a", EffectState::Dispatched, 1).await;
    finished(&ledger, &s, "b", "blob-b", EffectState::Discarded, 1).await;
    // A held effect of the same age keeps its reference and its bytes.
    let mut held = effect(&s, EffectClass::Mail, "h", "node:a");
    held.payload_ref = PayloadRef::stored("blob-held", "hash", 10);
    ledger
        .append_effect(NewEffect {
            effect: held,
            migration_id: MigrationId::new("mig-1").unwrap(),
        })
        .await
        .unwrap();

    let svc = service(&s, ledger.clone(), audit.clone(), Some(bytes.clone()));
    let report = svc.purge_expired(policy(), NOW, 100).await.unwrap();
    assert_eq!((report.purged, report.payloads_deleted), (2, 2));
    assert!(!bytes.contains("blob-a") && !bytes.contains("blob-b"));
    assert!(bytes.contains("blob-held"));

    let events = audit.events_of_kind(AuditKind::Purged);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].counts.get("purged"), Some(&2));
    assert_eq!(events[0].counts.get("payloads_deleted"), Some(&2));
    assert_eq!(events[0].counts.len(), 2);
    let text = format!("{:?}", events[0]);
    assert!(!text.contains("blob-") && !text.contains("mail body"));

    // Idempotent: nothing more to clear or delete.
    let again = svc.purge_expired(policy(), NOW, 100).await.unwrap();
    assert_eq!((again.purged, again.payloads_deleted), (0, 0));
}

#[tokio::test]
async fn a_payload_shared_by_two_holds_is_kept_until_the_last_reference_goes() {
    let s = site("payload-2.example");
    let ledger = Arc::new(InMemoryLedger::new());
    let bytes = Arc::new(InMemoryPayloadStore::new());
    bytes.put("shared", b"one body, two effects".to_vec());
    // The released effect is old enough, the discarded one is young (kept longer).
    finished(&ledger, &s, "old", "shared", EffectState::Dispatched, 1).await;
    finished(
        &ledger,
        &s,
        "young",
        "shared",
        EffectState::Discarded,
        NOW - 1_000,
    )
    .await;
    let svc = service(
        &s,
        ledger.clone(),
        Arc::new(RecordingAudit::new()),
        Some(bytes.clone()),
    );
    let first = svc.purge_expired(policy(), NOW, 100).await.unwrap();
    assert_eq!((first.purged, first.payloads_deleted), (1, 0));
    assert!(bytes.contains("shared"), "the second hold still needs it");

    // Later the second reference expires too: now the bytes go.
    let later = NOW + 3 * 86_400_000_000;
    let last = svc.purge_expired(policy(), later, 100).await.unwrap();
    assert_eq!((last.purged, last.payloads_deleted), (1, 1));
    assert!(!bytes.contains("shared"));
}

/// A byte store that fails its first delete, and checks at delete time that no effect in the
/// ledger still references the handle (nothing may reference bytes that are being deleted).
struct FlakyStore {
    inner: InMemoryPayloadStore,
    ledger: Arc<InMemoryLedger>,
    site: SiteId,
    fail_once: AtomicBool,
}

impl PayloadStore for FlakyStore {
    fn delete<'a>(&'a self, handle: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let all = [
                EffectState::Held,
                EffectState::Releasing,
                EffectState::Dispatched,
                EffectState::Discarded,
                EffectState::NeedsConfirmation,
            ];
            for record in self.ledger.list_effects(&self.site, &all).await? {
                if let PayloadRef::Stored { handle: h, .. } = &record.payload_ref {
                    assert_ne!(h, handle, "an effect references bytes being deleted");
                }
            }
            if self.fail_once.swap(false, Ordering::SeqCst) {
                return Err(Error::LedgerUnavailable {
                    reason: "the payload store went away".to_owned(),
                });
            }
            self.inner.delete(handle).await
        })
    }
}

#[tokio::test]
async fn a_crash_between_the_steps_leaves_no_reference_to_missing_bytes_and_the_next_purge_finishes()
 {
    let s = site("payload-3.example");
    let ledger = Arc::new(InMemoryLedger::new());
    let flaky = Arc::new(FlakyStore {
        inner: InMemoryPayloadStore::new(),
        ledger: ledger.clone(),
        site: s.clone(),
        fail_once: AtomicBool::new(true),
    });
    flaky.inner.put("blob-x", b"x".to_vec());
    finished(&ledger, &s, "x", "blob-x", EffectState::Dispatched, 1).await;
    let svc = service(
        &s,
        ledger.clone(),
        Arc::new(RecordingAudit::new()),
        Some(flaky.clone()),
    );

    // The byte store fails after the references were cleared: the call fails, the reference is
    // already gone (nothing points at the bytes) and the handle stays queued for deletion.
    assert!(svc.purge_expired(policy(), NOW, 100).await.is_err());
    assert!(
        flaky.inner.contains("blob-x"),
        "orphan bytes remain for now"
    );
    assert_eq!(
        ledger.pending_payload_deletes(&s, 10).await.unwrap(),
        vec!["blob-x".to_owned()]
    );

    // The next purge has nothing to clear, drains the queue and deletes the orphan.
    let report = svc.purge_expired(policy(), NOW, 100).await.unwrap();
    assert_eq!((report.purged, report.payloads_deleted), (0, 1));
    assert!(!flaky.inner.contains("blob-x"));
    assert!(
        ledger
            .pending_payload_deletes(&s, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn without_a_payload_store_the_handles_stay_queued_until_one_drains_them() {
    let s = site("payload-4.example");
    let ledger = Arc::new(InMemoryLedger::new());
    finished(&ledger, &s, "a", "blob-q", EffectState::Dispatched, 1).await;
    let svc = service(&s, ledger.clone(), Arc::new(RecordingAudit::new()), None);
    let report = svc.purge_expired(policy(), NOW, 100).await.unwrap();
    assert_eq!((report.purged, report.payloads_deleted), (1, 0));
    assert_eq!(
        ledger.pending_payload_deletes(&s, 10).await.unwrap(),
        vec!["blob-q".to_owned()]
    );
    let bytes = InMemoryPayloadStore::new();
    bytes.put("blob-q", b"q".to_vec());
    assert_eq!(
        drain_payload_deletes(ledger.as_ref(), &bytes, &s, 10)
            .await
            .unwrap(),
        1
    );
    assert!(bytes.is_empty());
    // Another site's queue is not touched.
    let other = site("payload-5.example");
    assert!(
        ledger
            .pending_payload_deletes(&other, 10)
            .await
            .unwrap()
            .is_empty()
    );
}
