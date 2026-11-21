//! The producers ask the side-effect hold (WP-MIG-08.T5): the worker's claim gate, the guard on the
//! external-effect job handlers (mail, webhook deliveries), and an absent hold behaving as before.
//!
//! No database: the in-memory ledger and the hold service run the cases.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use spot_cli::services::SiteCtx;
use spot_cli::services::effect_hold::{HoldParts, Settings, build};
use spot_cli::services::jobs::{HoldClaimGate, guard_job_handler, queue_gate_from};
use spot_core::{Clock, ManualClock, SiteId};
use spot_effect_hold::{EffectClass, Gate, InMemoryLedger, QueueGate, RecordingAudit};
use spot_jobs::model::{Job, JobId, JobState};
use spot_jobs::worker::{BoxFuture, ClaimGate, HandlerResult, JobContext, JobHandlerRegistry};
use time::macros::datetime;

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(datetime!(2026-11-12 09:00:00 UTC)))
}

fn parts() -> HoldParts {
    build(
        SiteId::new("alpha").unwrap(),
        Arc::new(InMemoryLedger::new()),
        clock(),
        &Settings::default(),
        Arc::new(RecordingAudit::new()),
    )
}

struct CountingLease {
    asked: AtomicU32,
    answer: bool,
}

impl ClaimGate for CountingLease {
    fn permit<'a>(&'a self) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer
        })
    }
}

fn job(name: &str) -> JobContext {
    let at = datetime!(2026-11-12 09:00:00 UTC);
    JobContext::new(
        Job {
            id: JobId(7),
            site: SiteId::new("alpha").unwrap(),
            name: name.to_owned(),
            args: serde_json::json!({}),
            queue: "default".to_owned(),
            state: JobState::Running,
            attempt: 1,
            max_attempts: 5,
            not_before: at,
            idempotency_key: Some("k-7".to_owned()),
            claim: None,
            last_error: None,
            created_at: at,
            context: serde_json::json!({}),
        },
        1,
        at,
    )
}

/// A handler that counts how often it ran (the "network call").
fn counted(registry: &mut JobHandlerRegistry, name: &str) -> Arc<AtomicU32> {
    let calls = Arc::new(AtomicU32::new(0));
    let seen = Arc::clone(&calls);
    registry.register(name.to_owned(), move |_ctx| {
        let seen = Arc::clone(&seen);
        Box::pin(async move {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(HandlerResult::Ok)
        })
    });
    calls
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_open_site_claims_as_the_lease_says() {
    let p = parts();
    p.gate.load(0).await;
    for answer in [true, false] {
        let lease = Arc::new(CountingLease {
            asked: AtomicU32::new(0),
            answer,
        });
        let gate = HoldClaimGate::new(
            lease.clone(),
            Arc::new(QueueGate::new(p.gate.clone())),
            vec!["default".to_owned(), "webhooks".to_owned()],
        );
        assert_eq!(gate.permit().await, answer);
        assert_eq!(lease.asked.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_site_claims_nothing_and_does_not_even_ask_the_lease() {
    let p = parts();
    p.gate.load(0).await;
    let lease = Arc::new(CountingLease {
        asked: AtomicU32::new(0),
        answer: true,
    });
    let gate = HoldClaimGate::new(
        lease.clone(),
        Arc::new(QueueGate::new(p.gate.clone())),
        vec!["default".to_owned()],
    );
    assert!(gate.permit().await);
    p.service.begin("m1", "verify", 10, None).await.unwrap();
    assert!(!gate.permit().await, "a held site claims no queue");
    assert_eq!(
        lease.asked.load(Ordering::SeqCst),
        1,
        "the held answer needs no lease call"
    );
    p.service
        .release("m1", spot_effect_hold::ReleasePlan::new(), 20)
        .await
        .ok();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_hold_state_claims_nothing() {
    // The gate never read the ledger: fail closed.
    let p = parts();
    let lease = Arc::new(CountingLease {
        asked: AtomicU32::new(0),
        answer: true,
    });
    let gate = HoldClaimGate::new(
        lease,
        Arc::new(QueueGate::new(p.gate.clone())),
        vec!["default".to_owned()],
    );
    assert!(!gate.permit().await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guarded_handler_runs_when_open_and_nothing_leaves_when_held() {
    let p = parts();
    p.gate.load(0).await;
    let mut registry = JobHandlerRegistry::new();
    let calls = counted(&mut registry, "platform:send_mail");
    assert!(guard_job_handler(
        &mut registry,
        "platform:send_mail",
        EffectClass::Mail,
        p.gate.clone()
    ));
    let handler = registry.get("platform:send_mail").unwrap();
    assert_eq!(
        (*handler)(job("platform:send_mail")).await.unwrap(),
        HandlerResult::Ok
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    p.service.begin("m1", "verify", 10, None).await.unwrap();
    let held = (*handler)(job("platform:send_mail")).await.unwrap();
    assert!(matches!(held, HandlerResult::Retry(_)), "{held:?}");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the send did not happen");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_state_holds_the_guarded_handler_too() {
    let p = parts();
    let mut registry = JobHandlerRegistry::new();
    let calls = counted(&mut registry, "webhooks:deliver");
    guard_job_handler(
        &mut registry,
        "webhooks:deliver",
        EffectClass::Webhook,
        p.gate.clone(),
    );
    let handler = registry.get("webhooks:deliver").unwrap();
    let result = (*handler)(job("webhooks:deliver")).await.unwrap();
    assert!(matches!(result, HandlerResult::Retry(_)));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fiscal_transmission_is_never_held() {
    let p = parts();
    p.gate.load(0).await;
    p.service.begin("m1", "verify", 10, None).await.unwrap();
    let mut registry = JobHandlerRegistry::new();
    let calls = counted(&mut registry, "fiscal:transmit");
    guard_job_handler(
        &mut registry,
        "fiscal:transmit",
        EffectClass::FiscalTransmission,
        p.gate.clone(),
    );
    let handler = registry.get("fiscal:transmit").unwrap();
    assert_eq!(
        (*handler)(job("fiscal:transmit")).await.unwrap(),
        HandlerResult::Ok
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_missing_handler_is_left_alone() {
    let p = parts();
    let mut registry = JobHandlerRegistry::new();
    assert!(!guard_job_handler(
        &mut registry,
        "platform:send_mail",
        EffectClass::Mail,
        p.gate.clone()
    ));
    assert!(registry.get("platform:send_mail").is_none());
}

#[test]
fn without_the_hold_service_there_is_no_queue_gate() {
    let mut ctx = SiteCtx::new(
        "alpha",
        Duration::from_secs(1),
        clock(),
        BTreeMap::new(),
        true,
    );
    assert!(
        queue_gate_from(&ctx).is_none(),
        "the hold disabled: jobs behave as before"
    );
    let p = parts();
    ctx.provide::<spot_effect_hold::HoldService>(p.service.clone())
        .unwrap();
    assert!(queue_gate_from(&ctx).is_some());
    let _: &Gate = &p.gate;
}
