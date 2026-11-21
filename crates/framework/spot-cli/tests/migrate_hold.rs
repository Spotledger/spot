//! The side-effect hold hosting: the gate port, the replay and discard of a release, the
//! fail-closed behaviour and the lifecycle operations over the real hold service (WP-MIG-08.T4).
//!
//! The in-memory ledger runs the cases that need no database. The PostgreSQL cases run against a
//! real server through `SPOT_TEST_PG_URL` and skip with a printed reason when it is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use spot_cli::services::effect_hold::{
    EffectPerformer, GateFuture, HoldParts, Pass, Settings, build, lifecycle_context,
};
use spot_core::{Clock, ManualClock, SiteId};
use spot_effect_hold::{
    AuditKind, EffectClass, EffectRecord, EffectState, InMemoryLedger, LedgerStore, RecordingAudit,
    UnavailableLedger,
};
use spot_migration_ops::hold::{MANUAL_HOLD_ID, ReleaseMode, hold, release, status};
use spot_migration_ops::{Channel, LifecycleError};
use spot_permissions::Principal;
use time::macros::datetime;

#[derive(Default)]
struct Sink(Mutex<Vec<String>>);

impl EffectPerformer for Sink {
    fn perform<'a>(&'a self, effect: &'a EffectRecord) -> GateFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.0.lock().unwrap().push(effect.key.clone());
            Ok(())
        })
    }
}

impl Sink {
    fn seen(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(datetime!(2026-11-12 09:00:00 UTC)))
}

fn parts(site: &str, ledger: Arc<dyn LedgerStore>, audit: Arc<RecordingAudit>) -> HoldParts {
    build(
        SiteId::new(site).unwrap(),
        ledger,
        clock(),
        &Settings {
            // Fast replay in tests.
            replay_per_class_per_second: 10_000,
            ..Settings::default()
        },
        audit,
    )
}

fn operator(parts: &HoldParts) -> spot_migration_ops::LifecycleContext {
    lifecycle_context(parts, clock(), Principal::system(), Channel::Cli).unwrap()
}

async fn open_gate(parts: &HoldParts) {
    // Stamped with the host clock's instant: a snapshot older than the staleness bound is held.
    parts
        .gate
        .load(spot_cli::services::effect_hold::micros(clock().as_ref()))
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_site_lets_effects_pass() {
    let p = parts(
        "alpha",
        Arc::new(InMemoryLedger::new()),
        Arc::new(RecordingAudit::new()),
    );
    open_gate(&p).await;
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Pass
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_records_and_does_not_perform_and_a_key_is_recorded_once() {
    let ledger = Arc::new(InMemoryLedger::new());
    let p = parts("alpha", ledger.clone(), Arc::new(RecordingAudit::new()));
    open_gate(&p).await;
    hold(&operator(&p), MANUAL_HOLD_ID, "verify").await.unwrap();
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Held
    );
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Held
    );
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Webhook, "k2").await,
        Pass::Held
    );
    let held = ledger
        .list_effects(&p.site, &[EffectState::Held])
        .await
        .unwrap();
    assert_eq!(held.len(), 2, "the same key is recorded once");
    // A fiscal transmission of an issued document is never held.
    assert_eq!(
        p.effect_gate
            .try_pass(&EffectClass::FiscalTransmission, "f1")
            .await,
        Pass::Pass
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replay_performs_each_effect_once_in_order_after_release() {
    let p = parts(
        "alpha",
        Arc::new(InMemoryLedger::new()),
        Arc::new(RecordingAudit::new()),
    );
    let mail = Arc::new(Sink::default());
    p.performers.register("mail", mail.clone());
    open_gate(&p).await;
    let ctx = operator(&p);
    hold(&ctx, MANUAL_HOLD_ID, "verify").await.unwrap();
    for key in ["m1", "m2", "m3"] {
        assert_eq!(
            p.effect_gate.try_pass(&EffectClass::Mail, key).await,
            Pass::Held
        );
    }
    assert!(mail.seen().is_empty(), "nothing leaves while held");
    release(&ctx, MANUAL_HOLD_ID, ReleaseMode::Replay)
        .await
        .unwrap();
    assert_eq!(mail.seen(), ["m1", "m2", "m3"]);
    // Open again: effects pass, and nothing is replayed twice.
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "m4").await,
        Pass::Pass
    );
    assert_eq!(mail.seen().len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_class_without_a_performer_keeps_the_hold() {
    let p = parts(
        "alpha",
        Arc::new(InMemoryLedger::new()),
        Arc::new(RecordingAudit::new()),
    );
    open_gate(&p).await;
    let ctx = operator(&p);
    hold(&ctx, MANUAL_HOLD_ID, "verify").await.unwrap();
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Webhook, "w1").await,
        Pass::Held
    );
    let refused = release(&ctx, MANUAL_HOLD_ID, ReleaseMode::Replay).await;
    assert_eq!(refused, Err(LifecycleError::HoldActive));
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "m1").await,
        Pass::Held
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_discard_is_audited_with_counts_and_needs_a_second_approver() {
    let audit = Arc::new(RecordingAudit::new());
    let p = parts("alpha", Arc::new(InMemoryLedger::new()), audit.clone());
    let mail = Arc::new(Sink::default());
    p.performers.register("mail", mail.clone());
    open_gate(&p).await;
    let ctx = lifecycle_context(
        &p,
        clock(),
        Principal::with_roles("alice", &["Tenant Administrator"]),
        Channel::Cli,
    )
    .unwrap();
    hold(&ctx, MANUAL_HOLD_ID, "verify").await.unwrap();
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "m1").await,
        Pass::Held
    );
    let alone = ReleaseMode::Discard {
        approver: Principal::with_roles("alice", &["Tenant Administrator"]),
        reason: "wrong copy".to_owned(),
    };
    assert_eq!(
        release(&ctx, MANUAL_HOLD_ID, alone).await,
        Err(LifecycleError::NotPermitted)
    );
    let two = ReleaseMode::Discard {
        approver: Principal::with_roles("bob", &["Tenant Administrator"]),
        reason: "wrong copy".to_owned(),
    };
    release(&ctx, MANUAL_HOLD_ID, two).await.unwrap();
    assert!(
        mail.seen().is_empty(),
        "a discarded effect is never performed"
    );
    assert_eq!(audit.events_of_kind(AuditKind::Discard).len(), 1);
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "m2").await,
        Pass::Pass
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_ledger_behaves_as_held() {
    let p = parts(
        "alpha",
        Arc::new(UnavailableLedger),
        Arc::new(RecordingAudit::new()),
    );
    // The state was never read, and cannot be: every external class is held.
    p.gate.load(0).await;
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Held
    );
    assert_eq!(
        p.effect_gate.try_pass(&EffectClass::BankApi, "k2").await,
        Pass::Held
    );
    // The state of the hold is an error, never "open".
    let ctx = operator(&p);
    assert_eq!(
        status(&ctx, p.inspector.as_ref()).await,
        Err(LifecycleError::Store)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restart_during_a_hold_keeps_it() {
    let ledger: Arc<dyn LedgerStore> = Arc::new(InMemoryLedger::new());
    let first = parts("alpha", ledger.clone(), Arc::new(RecordingAudit::new()));
    open_gate(&first).await;
    hold(&operator(&first), MANUAL_HOLD_ID, "verify")
        .await
        .unwrap();
    // A new process over the same ledger: the gate reads the hold back.
    let second = parts("alpha", ledger, Arc::new(RecordingAudit::new()));
    assert_eq!(
        second.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Held,
        "an unread state is held"
    );
    open_gate(&second).await;
    assert_eq!(
        second.effect_gate.try_pass(&EffectClass::Mail, "k2").await,
        Pass::Held
    );
    let view = status(&operator(&second), second.inspector.as_ref())
        .await
        .unwrap();
    assert!(view.held);
    assert_eq!(view.migration_id.as_deref(), Some(MANUAL_HOLD_ID));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_in_one_site_does_not_hold_another() {
    let ledger: Arc<dyn LedgerStore> = Arc::new(InMemoryLedger::new());
    let a = parts("alpha", ledger.clone(), Arc::new(RecordingAudit::new()));
    let b = parts("bravo", ledger, Arc::new(RecordingAudit::new()));
    open_gate(&a).await;
    open_gate(&b).await;
    hold(&operator(&a), MANUAL_HOLD_ID, "verify").await.unwrap();
    assert_eq!(
        a.effect_gate.try_pass(&EffectClass::Mail, "k").await,
        Pass::Held
    );
    assert_eq!(
        b.effect_gate.try_pass(&EffectClass::Mail, "k").await,
        Pass::Pass
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pg_ledger_keeps_a_hold_across_a_restart() {
    use spot_effect_hold_pg::PgLedger;
    use spot_postgres::PgBackend;
    use spot_postgres::testing::TestDb;
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let backend = PgBackend::connect_unchecked(db.config()).unwrap();
    let ledger = Arc::new(PgLedger::new(backend.pool()));
    ledger.prepare().await.unwrap();
    let first = parts("alpha", ledger.clone(), Arc::new(RecordingAudit::new()));
    open_gate(&first).await;
    hold(&operator(&first), MANUAL_HOLD_ID, "verify")
        .await
        .unwrap();
    assert_eq!(
        first.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Held
    );
    assert_eq!(
        first.effect_gate.try_pass(&EffectClass::Mail, "k1").await,
        Pass::Held
    );
    let restarted = parts(
        "alpha",
        Arc::new(PgLedger::new(backend.pool())),
        Arc::new(RecordingAudit::new()),
    );
    open_gate(&restarted).await;
    assert_eq!(
        restarted
            .effect_gate
            .try_pass(&EffectClass::Mail, "k2")
            .await,
        Pass::Held
    );
    let sink = Arc::new(Sink::default());
    restarted.performers.register("mail", sink.clone());
    release(&operator(&restarted), MANUAL_HOLD_ID, ReleaseMode::Replay)
        .await
        .unwrap();
    assert_eq!(
        sink.seen(),
        ["k1", "k2"],
        "k1 was recorded once and replays once"
    );
}
