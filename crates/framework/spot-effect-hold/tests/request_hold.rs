//! Request-scoped holds (WP-BPM-15.T2, ADR-080 section 6.7): hold, release at a commit point,
//! discard on undo, the original order, a crash between release and acknowledgement, the
//! never-held class, and the interplay with a migration hold.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{NOW, World, effect, world};
use futures::future::BoxFuture;
use spot_effect_hold::HoldService;
use spot_effect_hold::{
    AuditKind, AuditSink, Dispatcher, EffectRecord, HoldState, InMemoryLedger, Notifier,
    RecordingAudit, RecordingNotifier, Result as HoldResult,
};
use spot_effect_hold::{
    EffectClass, EffectState, HoldScope, LedgerStore, ReleasePlan, RequestAdmission, RequestId,
    UnavailableLedger,
};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

fn request(id: &str) -> RequestId {
    RequestId::new(id).unwrap()
}

async fn hold(w: &World, req: &RequestId, class: EffectClass, key: &str, source: &str) {
    let admission = w
        .service
        .admit_request(req, &effect(&w.site, class, key, source))
        .await
        .unwrap();
    assert!(matches!(admission, RequestAdmission::Held(_)));
}

#[tokio::test]
async fn effects_are_held_and_released_at_the_commit_point_in_original_order() {
    let w = world("req-order.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    hold(&w, &r, EffectClass::Webhook, "k2", "node:b").await;
    hold(&w, &r, EffectClass::Mail, "k3", "node:a").await;
    assert_eq!(
        w.dispatcher.unique_count(),
        0,
        "nothing leaves before the commit"
    );

    let report = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert!(report.completed && report.deferred_by.is_none());
    let keys: Vec<String> = w.dispatcher.calls().into_iter().map(|c| c.key).collect();
    assert_eq!(keys, ["k1", "k2", "k3"], "original order across sources");
    assert!(w.service.request_pending(&r).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_release_is_limited_to_the_region_since_the_last_commit_point() {
    let w = world("req-region.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    hold(&w, &r, EffectClass::Mail, "k2", "node:b").await;
    let first: BTreeSet<String> = ["node:a".to_owned()].into();
    let report = w
        .service
        .release_request(&r, ReleasePlan::new(), Some(&first), NOW)
        .await
        .unwrap();
    assert_eq!(report.dispatched.len(), 1);
    assert_eq!(w.dispatcher.calls()[0].key, "k1");
    let left = w.service.request_pending(&r).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].key, "k2");
}

#[tokio::test]
async fn a_discard_on_undo_drops_everything_and_notifies_where_the_class_says() {
    let w = world("req-discard.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    hold(&w, &r, EffectClass::BankApi, "k2", "node:a").await;
    let report = w
        .service
        .discard_request(&r, "request undone", NOW)
        .await
        .unwrap();
    assert_eq!((report.discarded, report.notified), (2, 1));
    assert!(report.in_flight.is_empty());
    assert_eq!(w.dispatcher.unique_count(), 0, "nothing left the building");
    assert_eq!(w.notifier.notices().len(), 1);
    assert_eq!(w.notifier.notices()[0].key, "k2");
    // A release after the discard has nothing to send, and never replays a discarded effect.
    let again = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert!(again.dispatched.is_empty() && again.completed);
    assert_eq!(w.dispatcher.unique_count(), 0);
}

#[tokio::test]
async fn a_failed_dispatch_stops_the_walk_so_nothing_overtakes_it() {
    let w = world("req-fail.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    hold(&w, &r, EffectClass::Mail, "k2", "node:a").await;
    hold(&w, &r, EffectClass::Mail, "k3", "node:a").await;
    w.dispatcher.fail_key("k2");
    let error = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.dispatch_failed");
    let keys: Vec<String> = w.dispatcher.calls().into_iter().map(|c| c.key).collect();
    assert_eq!(keys, ["k1"], "k3 did not overtake the failed k2");
    let pending = w.service.request_pending(&r).await.unwrap();
    assert_eq!(pending.len(), 2);
    // k2 may have been sent: it stays `Releasing` so a later discard cannot claim it.
    assert_eq!(pending[0].state, EffectState::Releasing);
    assert_eq!(pending[1].state, EffectState::Held);
    let discard = w
        .service
        .discard_request(&r, "cancelled", NOW)
        .await
        .unwrap();
    assert_eq!(discard.in_flight, vec![pending[0].id]);
    assert_eq!(discard.discarded, 1);
    // The note carries a stable code, not the dispatcher's text.
    assert_eq!(
        pending[0].note.as_deref(),
        Some("dispatch failed: effect_hold.dispatch_failed")
    );
}

#[tokio::test]
async fn a_crash_between_release_and_acknowledgement_re_releases_idempotently() {
    let w = world("req-crash.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    hold(&w, &r, EffectClass::Mail, "k2", "node:a").await;
    // Simulate the crash: k1 was handed to the transport but never acknowledged.
    let pending = w.service.request_pending(&r).await.unwrap();
    w.ledger
        .transition_effect(
            pending[0].id,
            EffectState::Held,
            EffectState::Releasing,
            None,
        )
        .await
        .unwrap();
    let first = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert_eq!(
        first.dispatched.len(),
        2,
        "the Releasing effect is re-driven"
    );
    // A second release after the acknowledgement sends nothing more.
    let second = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert!(second.dispatched.is_empty() && second.completed);
    assert_eq!(
        w.dispatcher.unique_count(),
        2,
        "each key went out exactly once"
    );
}

#[tokio::test]
async fn a_discard_never_claims_an_effect_that_may_have_left() {
    let w = world("req-inflight.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    hold(&w, &r, EffectClass::Mail, "k2", "node:a").await;
    let pending = w.service.request_pending(&r).await.unwrap();
    w.ledger
        .transition_effect(
            pending[0].id,
            EffectState::Held,
            EffectState::Releasing,
            None,
        )
        .await
        .unwrap();
    let report = w
        .service
        .discard_request(&r, "cancelled", NOW)
        .await
        .unwrap();
    assert_eq!(report.discarded, 1);
    assert_eq!(report.in_flight, vec![pending[0].id]);
}

#[tokio::test]
async fn a_never_held_class_passes_through_and_marks_the_point_of_no_return() {
    let w = world("req-fiscal.example");
    let r = request("r1");
    let admission = w
        .service
        .admit_request(
            &r,
            &effect(&w.site, EffectClass::FiscalTransmission, "f1", "node:a"),
        )
        .await
        .unwrap();
    assert_eq!(admission, RequestAdmission::Proceed { irreversible: true });
    assert!(w.service.request_pending(&r).await.unwrap().is_empty());
}

#[tokio::test]
async fn several_requests_hold_the_same_class_and_release_independently() {
    let w = world("req-many.example");
    let (a, b) = (request("a"), request("b"));
    hold(&w, &a, EffectClass::Mail, "ka", "node:x").await;
    hold(&w, &b, EffectClass::Mail, "kb", "node:x").await;
    w.service
        .release_request(&a, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert_eq!(w.dispatcher.calls().len(), 1);
    assert_eq!(w.dispatcher.calls()[0].key, "ka");
    let left = w.service.request_pending(&b).await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].scope(), HoldScope::Request(b));
}

#[tokio::test]
async fn a_live_migration_hold_wins_over_a_request_release() {
    let w = world("req-mig.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    w.service
        .begin("mig-1", "upgrade", NOW, None)
        .await
        .unwrap();

    let deferred = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert!(!deferred.completed && deferred.dispatched.is_empty());
    assert_eq!(
        deferred.deferred_by.as_ref().map(|m| m.as_str()),
        Some("mig-1")
    );
    assert_eq!(w.dispatcher.unique_count(), 0);

    // The migration's own release never touches the request's rows.
    w.service
        .release("mig-1", ReleasePlan::new(), NOW)
        .await
        .unwrap();
    assert_eq!(w.dispatcher.unique_count(), 0);
    assert_eq!(w.service.request_pending(&r).await.unwrap().len(), 1);

    // Once the migration is verified the commit point can be retried.
    let report = w
        .service
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert!(report.completed);
    assert_eq!(w.dispatcher.unique_count(), 1);
}

#[tokio::test]
async fn a_migration_discard_leaves_request_holds_alone_and_ids_are_reserved() {
    let w = world("req-mig2.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::Mail, "k1", "node:a").await;
    w.service
        .begin("mig-1", "upgrade", NOW, None)
        .await
        .unwrap();
    let discard = w
        .service
        .discard("mig-1", "rolled back", NOW)
        .await
        .unwrap();
    assert_eq!(discard.discarded, 0);
    assert_eq!(w.service.request_pending(&r).await.unwrap().len(), 1);
    assert!(w.service.held_in_order().await.unwrap().is_empty());

    let error = w
        .service
        .begin("request:r1", "clash", NOW, None)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.invalid_input");
}

#[tokio::test]
async fn stale_effects_wait_for_confirmation_then_release() {
    let w = world("req-stale.example");
    let r = request("r1");
    hold(&w, &r, EffectClass::BankApi, "bank1", "node:a").await;
    let later = NOW + common::DAY_MICROS + 1;
    let waiting = w
        .service
        .release_request(&r, ReleasePlan::new(), None, later)
        .await
        .unwrap();
    assert!(!waiting.completed);
    assert_eq!(waiting.needs_confirmation.len(), 1);
    assert_eq!(w.dispatcher.unique_count(), 0);
    let confirmed = w
        .service
        .release_request(&r, ReleasePlan::new().confirm("bank1"), None, later)
        .await
        .unwrap();
    assert!(confirmed.completed);
    assert_eq!(w.dispatcher.unique_count(), 1);
}

#[tokio::test]
async fn an_unreadable_ledger_fails_closed() {
    let w = world("req-down.example");
    let down = w.service_over(Arc::new(UnavailableLedger));
    let r = request("r1");
    // A ledger that cannot record the effect is an error, never a silent `Held`: the effect is
    // neither sent nor recorded and the caller knows it.
    let error = down
        .admit_request(&r, &effect(&w.site, EffectClass::Mail, "k1", "node:a"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.ledger_unavailable");
    let error = down
        .release_request(&r, ReleasePlan::new(), None, NOW)
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.state_unknown");
    assert_eq!(w.dispatcher.unique_count(), 0);
}

#[tokio::test]
async fn unbounded_inputs_are_refused() {
    let w = world("req-bounds.example");
    let r = request("r1");
    let long = "k".repeat(300);
    let error = w
        .service
        .admit_request(&r, &effect(&w.site, EffectClass::Mail, &long, "node:a"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.invalid_input");
    let other = common::world("elsewhere.example");
    let error = w
        .service
        .admit_request(&r, &effect(&other.site, EffectClass::Mail, "k", "node:a"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.invalid_input");
    let error = w.service.discard_request(&r, "", NOW).await.unwrap_err();
    assert_eq!(error.code(), "effect_hold.invalid_input");
}

/// A dispatcher that counts **every** call (no de-duplication by key), yields so racing tasks
/// interleave, and can start a migration hold while a release is walking.
struct Counting {
    keys: Mutex<Vec<String>>,
    ledger: Arc<InMemoryLedger>,
    site: spot_core::SiteId,
    begin_migration_on_first: AtomicBool,
}

impl Counting {
    fn count(&self, key: &str) -> usize {
        self.keys
            .lock()
            .unwrap()
            .iter()
            .filter(|k| *k == key)
            .count()
    }
    fn total(&self) -> usize {
        self.keys.lock().unwrap().len()
    }
}

impl std::fmt::Debug for Counting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Counting")
    }
}

impl Dispatcher for Counting {
    fn dispatch<'a>(&'a self, effect: &'a EffectRecord) -> BoxFuture<'a, HoldResult<()>> {
        Box::pin(async move {
            tokio::task::yield_now().await;
            self.keys.lock().unwrap().push(effect.key.clone());
            tokio::task::yield_now().await;
            if self.begin_migration_on_first.swap(false, Ordering::SeqCst) {
                let held = HoldState::Held {
                    migration_id: spot_effect_hold::MigrationId::new("mig-mid").unwrap(),
                    reason: "mid-release".to_owned(),
                    since: NOW,
                    expected_by: None,
                };
                self.ledger
                    .compare_and_set_hold(&self.site, &HoldState::Open, held)
                    .await
                    .unwrap();
            }
            Ok(())
        })
    }
}

struct Race {
    service: Arc<HoldService>,
    dispatcher: Arc<Counting>,
    audit: Arc<RecordingAudit>,
    notifier: Arc<RecordingNotifier>,
    site: spot_core::SiteId,
}

fn race(name: &str) -> Race {
    let site = spot_core::SiteId::new(name).unwrap();
    let ledger = Arc::new(InMemoryLedger::new());
    let dispatcher = Arc::new(Counting {
        keys: Mutex::new(Vec::new()),
        ledger: Arc::clone(&ledger),
        site: site.clone(),
        begin_migration_on_first: AtomicBool::new(false),
    });
    let audit = Arc::new(RecordingAudit::new());
    let notifier = Arc::new(RecordingNotifier::new());
    let service = Arc::new(HoldService::new(
        site.clone(),
        ledger as Arc<dyn LedgerStore>,
        common::policies(),
        Arc::clone(&audit) as Arc<dyn AuditSink>,
        Arc::clone(&dispatcher) as Arc<dyn Dispatcher>,
        Arc::clone(&notifier) as Arc<dyn Notifier>,
    ));
    Race {
        service,
        dispatcher,
        audit,
        notifier,
        site,
    }
}

async fn admit(r: &Race, req: &RequestId, class: EffectClass, key: &str) {
    r.service
        .admit_request(req, &effect(&r.site, class, key, "node:a"))
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_callers_racing_the_same_commit_dispatch_each_effect_once() {
    let r = race("race-commit.example");
    let req = request("r1");
    for n in 0..5 {
        admit(&r, &req, EffectClass::Mail, &format!("k{n}")).await;
    }
    let mut tasks = Vec::new();
    for _ in 0..4 {
        let service = Arc::clone(&r.service);
        let req = req.clone();
        tasks.push(tokio::spawn(async move {
            service
                .release_request(&req, ReleasePlan::new(), None, NOW)
                .await
        }));
    }
    let mut dispatched = 0;
    for task in tasks {
        dispatched += task.await.unwrap().unwrap().dispatched.len();
    }
    assert_eq!(
        dispatched, 5,
        "the reports add up to one dispatch per effect"
    );
    for n in 0..5 {
        assert_eq!(
            r.dispatcher.count(&format!("k{n}")),
            1,
            "k{n} went out once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commit_racing_end_dispatches_xor_discards_every_effect() {
    for round in 0..30 {
        let r = race(&format!("race-end-{round}.example"));
        let req = request("r1");
        for n in 0..4 {
            admit(&r, &req, EffectClass::BankApi, &format!("k{n}")).await;
        }
        let (a, b) = (Arc::clone(&r.service), Arc::clone(&r.service));
        let (ra, rb) = (req.clone(), req.clone());
        let commit = tokio::spawn(async move {
            a.release_request(&ra, ReleasePlan::new(), None, NOW)
                .await
                .unwrap()
        });
        let end =
            tokio::spawn(async move { b.discard_request(&rb, "cancelled", NOW).await.unwrap() });
        let released = commit.await.unwrap();
        let discarded = end.await.unwrap();
        // Every effect is dispatched xor discarded, never both and never neither.
        assert_eq!(
            released.dispatched.len() + usize::try_from(discarded.discarded).unwrap(),
            4,
            "round {round}"
        );
        assert!(discarded.in_flight.is_empty());
        assert_eq!(r.dispatcher.total(), released.dispatched.len());
        // A discarded bank instruction raised exactly one task each; none for a sent one.
        assert_eq!(
            u64::try_from(r.notifier.notices().len()).unwrap(),
            discarded.notified
        );
        assert_eq!(discarded.notified, discarded.discarded);
        assert!(r.service.request_pending(&req).await.unwrap().is_empty());
    }
}

#[tokio::test]
async fn admission_after_end_is_refused() {
    let r = race("after-end.example");
    let req = request("r1");
    admit(&r, &req, EffectClass::Mail, "k1").await;
    r.service
        .discard_request(&req, "cancelled", NOW)
        .await
        .unwrap();
    let error = r
        .service
        .admit_request(&req, &effect(&r.site, EffectClass::Mail, "k2", "node:a"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.invalid_input");
    assert!(r.service.request_pending(&req).await.unwrap().is_empty());
    // Another request is unaffected.
    admit(&r, &request("r2"), EffectClass::Mail, "k3").await;
}

#[tokio::test]
async fn a_request_may_hold_only_so_many_effects() {
    let r = race("cap.example");
    let req = request("r1");
    for n in 0..spot_effect_hold::MAX_HELD_PER_REQUEST {
        admit(&r, &req, EffectClass::Mail, &format!("k{n}")).await;
    }
    let error = r
        .service
        .admit_request(&req, &effect(&r.site, EffectClass::Mail, "over", "node:a"))
        .await
        .unwrap_err();
    assert_eq!(error.code(), "effect_hold.invalid_input");
    // The cap is per request.
    admit(&r, &request("r2"), EffectClass::Mail, "other").await;
    // Releasing frees room.
    r.service
        .release_request(&req, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    admit(&r, &req, EffectClass::Mail, "again").await;
}

#[tokio::test]
async fn a_migration_starting_mid_release_stops_the_walk_and_is_audited() {
    let r = race("mid-mig.example");
    let req = request("r1");
    for n in 0..3 {
        admit(&r, &req, EffectClass::Mail, &format!("k{n}")).await;
    }
    r.dispatcher
        .begin_migration_on_first
        .store(true, Ordering::SeqCst);
    let report = r
        .service
        .release_request(&req, ReleasePlan::new(), None, NOW)
        .await
        .unwrap();
    assert!(!report.completed);
    assert_eq!(
        report.dispatched.len(),
        1,
        "only the effect already in flight"
    );
    assert_eq!(
        report.deferred_by.as_ref().map(|m| m.as_str()),
        Some("mig-mid")
    );
    assert_eq!(r.dispatcher.total(), 1);
    let events = r.audit.events_of_kind(AuditKind::Release);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].counts.get("deferred"), Some(&1));
}

#[tokio::test]
async fn a_failed_release_leaves_an_audit_record() {
    let w = world("fail-audit.example");
    let req = request("r1");
    hold(&w, &req, EffectClass::Mail, "k1", "node:a").await;
    w.dispatcher.fail_key("k1");
    let _ = w
        .service
        .release_request(&req, ReleasePlan::new(), None, NOW)
        .await;
    let events = w.audit.events_of_kind(AuditKind::Release);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].counts.get("failed"), Some(&1));
}
