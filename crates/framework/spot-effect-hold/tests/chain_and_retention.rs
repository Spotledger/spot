//! The hold's audit on the tamper-evident chain and the retention purge (WP-MIG-08.T6): one chain
//! entry per transition in order, the chain verifies, the body is the event and holds no payload, a
//! failing chain store fails the transition (a hold that began stays held), two services append to
//! one chain, a head that keeps moving fails; and the purge keeps held and releasing effects, ages
//! released and discarded ones by their own limits, is idempotent, bounded, keeps the dedupe key,
//! reads no clock and is audited with a count.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use common::{DAY_MICROS, NOW, effect, policies};
use spot_audit_chain::{
    AppendOutcome, AuditError, AuditStore, ChainEntry, Checkpoint, CheckpointPolicy, InMemoryStore,
    KeyRing, LocalSigner, StoreFuture, Verifier,
};
use spot_core::SiteId;
use spot_effect_hold::{
    AuditEvent, AuditKind, AuditSink, ChainAuditSink, Dispatcher, EffectClass, EffectState, Error,
    HoldService, InMemoryLedger, LedgerStore, Notifier, PayloadRef, RecordingDispatcher,
    RecordingNotifier, ReleasePlan, RetentionPolicy, chain_scope, codes,
};

const CANARY: &str = "CANARY-payload-handle-5f3a";

/// A store several sinks share (the chain's store is owned by value).
#[derive(Clone)]
struct Shared(Arc<InMemoryStore>);

impl AuditStore for Shared {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.0.head(site)
    }
    fn append_if_head<'a>(
        &'a self,
        site: &'a str,
        expected_seq: u64,
        entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.0.append_if_head(site, expected_seq, entry)
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        self.0.range(site, from, to)
    }
    fn put_checkpoint<'a>(&'a self, checkpoint: Checkpoint) -> StoreFuture<'a, ()> {
        self.0.put_checkpoint(checkpoint)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.0.latest_checkpoint(site)
    }
}

/// A store that never lets an append land: the head always "moved".
struct AlwaysMoved(InMemoryStore, AtomicU32);

impl AuditStore for AlwaysMoved {
    fn head<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        self.0.head(site)
    }
    fn append_if_head<'a>(
        &'a self,
        _site: &'a str,
        _expected_seq: u64,
        _entry: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(AppendOutcome::HeadMoved) })
    }
    fn range<'a>(&'a self, site: &'a str, from: u64, to: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        self.0.range(site, from, to)
    }
    fn put_checkpoint<'a>(&'a self, c: Checkpoint) -> StoreFuture<'a, ()> {
        self.0.put_checkpoint(c)
    }
    fn latest_checkpoint<'a>(&'a self, site: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        self.0.latest_checkpoint(site)
    }
}

/// A store that fails every operation.
struct Down;

fn down() -> AuditError {
    AuditError::Store("the chain store is down".to_owned())
}

impl AuditStore for Down {
    fn head<'a>(&'a self, _s: &'a str) -> StoreFuture<'a, Option<ChainEntry>> {
        Box::pin(async { Err(down()) })
    }
    fn append_if_head<'a>(
        &'a self,
        _s: &'a str,
        _e: u64,
        _x: ChainEntry,
    ) -> StoreFuture<'a, AppendOutcome> {
        Box::pin(async { Err(down()) })
    }
    fn range<'a>(&'a self, _s: &'a str, _f: u64, _t: u64) -> StoreFuture<'a, Vec<ChainEntry>> {
        Box::pin(async { Err(down()) })
    }
    fn put_checkpoint<'a>(&'a self, _c: Checkpoint) -> StoreFuture<'a, ()> {
        Box::pin(async { Err(down()) })
    }
    fn latest_checkpoint<'a>(&'a self, _s: &'a str) -> StoreFuture<'a, Option<Checkpoint>> {
        Box::pin(async { Err(down()) })
    }
}

fn signer(seed: u8) -> (Arc<LocalSigner>, KeyRing) {
    let signer = Arc::new(LocalSigner::from_seed([seed; 32]));
    let mut ring = KeyRing::new();
    ring.add(&signer.public_key()).unwrap();
    (signer, ring)
}

fn service(site: &SiteId, ledger: Arc<dyn LedgerStore>, audit: Arc<dyn AuditSink>) -> HoldService {
    HoldService::new(
        site.clone(),
        ledger,
        policies(),
        audit,
        Arc::new(RecordingDispatcher::new()) as Arc<dyn Dispatcher>,
        Arc::new(RecordingNotifier::new()) as Arc<dyn Notifier>,
    )
}

fn site(name: &str) -> SiteId {
    SiteId::new(name).unwrap()
}

// ---- the chain sink -------------------------------------------------------------------------

#[tokio::test]
async fn one_entry_per_transition_in_order_the_chain_verifies_and_the_body_is_the_event() {
    let s = site("chain-1.example");
    let store = Shared(Arc::new(InMemoryStore::new()));
    let (signer, ring) = signer(1);
    let sink = Arc::new(ChainAuditSink::new(
        s.clone(),
        store.clone(),
        signer,
        CheckpointPolicy { every: 0 },
    ));
    let svc = service(&s, Arc::new(InMemoryLedger::new()), sink.clone());
    svc.load(NOW).await;
    svc.begin("mig-1", "verify", NOW, Some(NOW + DAY_MICROS))
        .await
        .unwrap();
    let mut mail = effect(&s, EffectClass::Mail, "k1", "node:a");
    mail.payload_ref = PayloadRef::stored(CANARY, "hash", 10);
    svc.gate().admit(&mail, NOW).await;
    svc.release("mig-1", ReleasePlan::new(), NOW + 1)
        .await
        .unwrap();

    let scope = chain_scope(&s);
    assert_eq!(scope, "hold:chain-1.example");
    let entries = store.range(&scope, 1, u64::MAX).await.unwrap();
    let kinds: Vec<&str> = entries.iter().map(|e| e.record.kind.as_str()).collect();
    assert_eq!(kinds, ["hold.begin", "hold.release"]);
    for entry in &entries {
        let text = format!("{entry:?}");
        assert!(
            !text.contains(CANARY),
            "a payload handle reached the chain: {text}"
        );
        assert!(!text.contains("k1"), "an effect key reached the chain");
        assert_eq!(entry.record.actor, "system");
    }
    let report = Verifier::new(&ring)
        .verify_range(&store, &scope, 1, u64::MAX)
        .await
        .unwrap();
    assert!(report.is_ok(), "{report:?}");
}

#[tokio::test]
async fn a_failing_chain_store_fails_the_transition_and_the_hold_stays_held() {
    let s = site("chain-2.example");
    let (signer, _ring) = signer(2);
    let sink = Arc::new(ChainAuditSink::new(
        s.clone(),
        Down,
        signer,
        CheckpointPolicy { every: 0 },
    ));
    let ledger = Arc::new(InMemoryLedger::new());
    let svc = service(&s, ledger.clone(), sink);
    let error = svc.begin("mig-1", "x", NOW, None).await.unwrap_err();
    assert_eq!(error.code(), codes::AUDIT_FAILED);
    // Fail closed: the site is held even though the audit could not be written.
    assert!(!matches!(
        ledger.get_hold(&s).await.unwrap(),
        spot_effect_hold::HoldState::Open
    ));
}

#[tokio::test]
async fn two_services_appending_to_one_sites_chain_both_land() {
    let s = site("chain-3.example");
    let store = Shared(Arc::new(InMemoryStore::new()));
    let (signer, ring) = signer(3);
    let make = || {
        Arc::new(ChainAuditSink::new(
            s.clone(),
            store.clone(),
            signer.clone(),
            CheckpointPolicy { every: 0 },
        ))
    };
    let (a, b) = (make(), make());
    let ev = |n: i64| AuditEvent::new(AuditKind::Begin, s.clone(), None, n);
    let (ra, rb) = tokio::join!(a.record(ev(1)), b.record(ev(2)));
    ra.unwrap();
    rb.unwrap();
    let scope = chain_scope(&s);
    assert_eq!(store.range(&scope, 1, u64::MAX).await.unwrap().len(), 2);
    assert!(
        Verifier::new(&ring)
            .verify_range(&store, &scope, 1, u64::MAX)
            .await
            .unwrap()
            .is_ok()
    );
}

#[tokio::test]
async fn a_head_that_keeps_moving_fails_after_bounded_retries_and_another_site_is_refused() {
    let s = site("chain-4.example");
    let (signer, _ring) = signer(4);
    let moved = AlwaysMoved(InMemoryStore::new(), AtomicU32::new(0));
    let sink = ChainAuditSink::new(
        s.clone(),
        moved,
        signer.clone(),
        CheckpointPolicy { every: 0 },
    );
    let error = sink
        .record(AuditEvent::new(AuditKind::Begin, s.clone(), None, 1))
        .await
        .unwrap_err();
    assert_eq!(error.code(), codes::AUDIT_FAILED);
    let ok = ChainAuditSink::new(
        s.clone(),
        InMemoryStore::new(),
        signer,
        CheckpointPolicy { every: 0 },
    );
    let other = AuditEvent::new(AuditKind::Begin, site("someone-else.example"), None, 1);
    assert!(matches!(
        ok.record(other).await,
        Err(Error::AuditFailed { .. })
    ));
}

// ---- retention ----------------------------------------------------------------------------------

#[test]
fn the_retention_policy_is_bounded() {
    assert_eq!(RetentionPolicy::default().released_days(), 7);
    assert_eq!(RetentionPolicy::default().discarded_days(), 30);
    for (a, b) in [(0, 30), (7, 0), (3651, 30), (7, 3651)] {
        let error = RetentionPolicy::new(a, b).unwrap_err();
        assert_eq!(error.code(), codes::RETENTION_INVALID);
    }
    assert!(RetentionPolicy::new(1, 3650).is_ok());
}

/// An effect with a stored payload, moved to `state` at `at`.
async fn finished(
    ledger: &InMemoryLedger,
    s: &SiteId,
    key: &str,
    state: EffectState,
    at: i64,
) -> spot_effect_hold::EffectId {
    let mut new = common::effect(s, EffectClass::Mail, key, "node:a");
    new.payload_ref = PayloadRef::stored(CANARY, "hash", 10);
    let id = ledger
        .append_effect(spot_effect_hold::NewEffect {
            effect: new,
            migration_id: spot_effect_hold::MigrationId::new("mig-1").unwrap(),
        })
        .await
        .unwrap();
    if state != EffectState::Held {
        ledger
            .transition_effect_at(id, EffectState::Held, state, None, at)
            .await
            .unwrap();
    }
    id
}

async fn absent(ledger: &InMemoryLedger, s: &SiteId, id: spot_effect_hold::EffectId) -> bool {
    let all = [
        EffectState::Held,
        EffectState::Releasing,
        EffectState::Dispatched,
        EffectState::Discarded,
        EffectState::NeedsConfirmation,
    ];
    ledger
        .list_effects(s, &all)
        .await
        .unwrap()
        .iter()
        .find(|r| r.id == id)
        .map(|r| r.payload_ref == PayloadRef::Absent)
        .unwrap()
}

#[tokio::test]
async fn purge_keeps_held_and_releasing_whatever_their_age_and_uses_each_limit_with_exact_boundaries()
 {
    let s = site("purge-1.example");
    let ledger = Arc::new(InMemoryLedger::new());
    let audit = Arc::new(spot_effect_hold::RecordingAudit::new());
    let svc = service(&s, ledger.clone(), audit.clone());
    let policy = RetentionPolicy::default();
    let now = NOW;
    let released_limit = now - 7 * DAY_MICROS;
    let discarded_limit = now - 30 * DAY_MICROS;

    let held = finished(&ledger, &s, "held", EffectState::Held, 0).await;
    let releasing = finished(&ledger, &s, "releasing", EffectState::Releasing, 1).await;
    let released_at_limit =
        finished(&ledger, &s, "r-at", EffectState::Dispatched, released_limit).await;
    let released_past = finished(
        &ledger,
        &s,
        "r-past",
        EffectState::Dispatched,
        released_limit - 1,
    )
    .await;
    let discarded_at_limit =
        finished(&ledger, &s, "d-at", EffectState::Discarded, discarded_limit).await;
    let discarded_past = finished(
        &ledger,
        &s,
        "d-past",
        EffectState::Discarded,
        discarded_limit - 1,
    )
    .await;
    // A discarded effect is kept longer than a released one: 10 days old is past the released limit
    // but not the discarded one.
    let discarded_young = finished(
        &ledger,
        &s,
        "d-young",
        EffectState::Discarded,
        now - 10 * DAY_MICROS,
    )
    .await;

    let report = svc.purge_expired(policy, now, 1_000).await.unwrap();
    assert_eq!(report.purged, 2);
    assert!(!report.more);
    assert!(absent(&ledger, &s, released_past).await);
    assert!(absent(&ledger, &s, discarded_past).await);
    for kept in [
        held,
        releasing,
        released_at_limit,
        discarded_at_limit,
        discarded_young,
    ] {
        assert!(!absent(&ledger, &s, kept).await);
    }

    // The purge itself is on the audit trail, with a count and nothing else.
    let events = audit.events_of_kind(AuditKind::Purged);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].counts.get("purged"), Some(&2));
    assert_eq!(events[0].counts.len(), 1);

    // Idempotent; and a duplicate of a purged effect is still refused as one (the dedupe key stays).
    assert_eq!(
        svc.purge_expired(policy, now, 1_000).await.unwrap().purged,
        0
    );
    let again = ledger
        .append_effect(spot_effect_hold::NewEffect {
            effect: common::effect(&s, EffectClass::Mail, "r-past", "node:a"),
            migration_id: spot_effect_hold::MigrationId::new("mig-1").unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(again, released_past);
}

#[tokio::test]
async fn purge_respects_its_limit_reports_more_and_never_touches_another_site() {
    let s = site("purge-2.example");
    let other = site("purge-3.example");
    let ledger = Arc::new(InMemoryLedger::new());
    let svc = service(
        &s,
        ledger.clone(),
        Arc::new(spot_effect_hold::RecordingAudit::new()),
    );
    for key in ["a", "b", "c"] {
        finished(&ledger, &s, key, EffectState::Dispatched, 1).await;
    }
    let foreign = finished(&ledger, &other, "x", EffectState::Dispatched, 1).await;
    let policy = RetentionPolicy::new(1, 1).unwrap();
    let first = svc.purge_expired(policy, NOW, 2).await.unwrap();
    assert_eq!((first.purged, first.more), (2, true));
    let second = svc.purge_expired(policy, NOW, 2).await.unwrap();
    assert_eq!((second.purged, second.more), (1, false));
    assert!(
        !absent(&ledger, &other, foreign).await,
        "another site's reference was purged"
    );
    // A zero limit purges nothing.
    assert_eq!(svc.purge_expired(policy, NOW, 0).await.unwrap().purged, 0);
}

#[tokio::test]
async fn a_ledger_that_cannot_purge_says_so() {
    let s = site("purge-4.example");
    let svc = service(
        &s,
        Arc::new(spot_effect_hold::UnavailableLedger),
        Arc::new(spot_effect_hold::RecordingAudit::new()),
    );
    let error = svc
        .purge_expired(RetentionPolicy::default(), NOW, 10)
        .await
        .unwrap_err();
    assert_eq!(error.code(), codes::LEDGER_UNAVAILABLE);
}
