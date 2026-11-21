//! Behaviour tests for the delivery handler (unit tests).

#![allow(clippy::unwrap_used)]

use http::{HeaderMap, HeaderValue};
use std::sync::Arc;

use crate::delivery::{DeliveryLog, DeliveryOutcome, classify_reason, parse_retry_after};
use crate::fixtures::{
    DenyFields, Rig, Step, delivery_job, response, response_retry_after, response_with_body,
    webhook,
};
use crate::model::{BulkheadConfig, CircuitBreakerConfig, DeliveryStatus};
use crate::signing::verify_signature;
use crate::webhook::WebhookStore;
use spot_core::Clock;
use spot_egress::EgressError;
use spot_jobs::store::JobFilter;

#[test]
fn retry_after_parses_seconds() {
    let mut headers = HeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("120"));
    assert_eq!(parse_retry_after(&headers), Some(120));
    headers.insert("retry-after", HeaderValue::from_static("soon"));
    assert_eq!(parse_retry_after(&headers), None);
}

#[test]
fn classify_reason_takes_the_prefix() {
    assert_eq!(classify_reason("transport: boom"), "transport");
    assert_eq!(classify_reason("plain"), "plain");
}

async fn job_count(rig: &Rig) -> usize {
    rig.jobs.list(JobFilter::default()).await.unwrap().len()
}

#[tokio::test]
async fn a_2xx_delivers_and_signs_the_exact_body() {
    let rig = Rig::new(vec![Step::Respond(response(200))]);
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    let outcome = rig.handler().run(&job).await;
    assert_eq!(outcome, DeliveryOutcome::Delivered);

    let recorded = rig.egress.recorded();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].url, "https://example.com/hook");
    let signature = recorded[0]
        .headers
        .get("x-spot-signature")
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        verify_signature(
            rig.secret.as_bytes(),
            signature,
            &recorded[0].body,
            rig.clock.now_utc(),
            None,
        )
        .unwrap(),
        "the signature must verify over the exact body"
    );
    assert_eq!(recorded[0].headers.get("x-spot-event-id").unwrap(), "evt-1");
    let stored = rig.deliveries.get(job.delivery_id).await.unwrap().unwrap();
    assert_eq!(stored.status, DeliveryStatus::Delivered);
}

#[tokio::test]
async fn protected_fields_are_omitted_from_the_body() {
    let rig = Rig::with_visibility(
        vec![Step::Respond(response(200))],
        CircuitBreakerConfig::default(),
        BulkheadConfig { max_concurrent: 4 },
        Arc::new(DenyFields {
            denied: vec!["salary".into()],
        }),
    );
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let mut job = delivery_job(&webhook);
    job.after_image = serde_json::json!({"status": "Done", "salary": "123.00"});
    assert_eq!(rig.handler().run(&job).await, DeliveryOutcome::Delivered);
    let body = String::from_utf8_lossy(&rig.egress.recorded()[0].body).into_owned();
    assert!(body.contains("status"));
    assert!(
        !body.contains("salary"),
        "a hidden field must not go out: {body}"
    );
}

#[tokio::test]
async fn retry_after_schedules_a_successor_at_the_requested_time() {
    let rig = Rig::new(vec![Step::Respond(response_retry_after(429, 5))]);
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    let outcome = rig.handler().run(&job).await;
    assert!(
        matches!(outcome, DeliveryOutcome::Rescheduled { .. }),
        "{outcome:?}"
    );
    let stored = rig.deliveries.get(job.delivery_id).await.unwrap().unwrap();
    assert_eq!(
        stored.next_retry_at,
        Some(rig.clock.now_utc() + time::Duration::seconds(5))
    );
    assert_eq!(job_count(&rig).await, 1, "a successor job is enqueued");
}

#[tokio::test]
async fn a_server_error_retries_with_the_engine() {
    let rig = Rig::new(vec![Step::Respond(response(500))]);
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    let outcome = rig.handler().run(&job).await;
    assert!(
        matches!(outcome, DeliveryOutcome::Retry { .. }),
        "{outcome:?}"
    );
    let stored = rig.deliveries.get(job.delivery_id).await.unwrap().unwrap();
    assert_eq!(stored.status, DeliveryStatus::Pending);
    assert!(stored.next_retry_at.is_none());
}

#[tokio::test]
async fn a_410_disables_the_webhook() {
    let rig = Rig::new(vec![Step::Respond(response(410))]);
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    assert!(matches!(
        rig.handler().run(&job).await,
        DeliveryOutcome::Dead { .. }
    ));
    let stored = rig.store.get(webhook.id).await.unwrap().unwrap();
    assert!(!stored.enabled);
    assert_eq!(stored.state, crate::model::WebhookState::Disabled);
    assert!(stored.state_reason.is_some());
}

#[tokio::test]
async fn an_egress_denial_is_blocked_without_retry() {
    let rig = Rig::new(vec![Step::Fail(EgressError::PolicyDenied)]);
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    let outcome = rig.handler().run(&job).await;
    assert!(
        matches!(outcome, DeliveryOutcome::Blocked { .. }),
        "{outcome:?}"
    );
    let stored = rig.deliveries.get(job.delivery_id).await.unwrap().unwrap();
    assert_eq!(stored.status, DeliveryStatus::Blocked);
    assert_eq!(job_count(&rig).await, 0, "a denial is never retried");
}

#[tokio::test]
async fn exhausted_attempts_dead_letter() {
    let rig = Rig::new(vec![
        Step::Respond(response(500)),
        Step::Respond(response(500)),
    ]);
    let mut webhook = webhook("https://example.com/hook");
    webhook.max_attempts = 2;
    let webhook = rig.register(webhook).await;
    let job = delivery_job(&webhook);
    let handler = rig.handler();
    assert!(matches!(
        handler.run(&job).await,
        DeliveryOutcome::Retry { .. }
    ));
    assert!(matches!(
        handler.run(&job).await,
        DeliveryOutcome::Dead { .. }
    ));
    let stored = rig.deliveries.get(job.delivery_id).await.unwrap().unwrap();
    assert_eq!(stored.status, DeliveryStatus::DeadLetter);
    assert_eq!(stored.attempts, 2);
}

#[tokio::test]
async fn secrets_are_never_in_the_delivery_record() {
    let rig = Rig::new(vec![Step::Respond(response_with_body(
        200,
        "here is the-signing-secret-value in a body",
    ))]);
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    assert_eq!(rig.handler().run(&job).await, DeliveryOutcome::Delivered);
    let stored = rig.deliveries.get(job.delivery_id).await.unwrap().unwrap();
    let serialized = serde_json::to_string(&stored).unwrap();
    assert!(
        !serialized.contains(&rig.secret),
        "the record leaked the secret"
    );
    let webhook = rig.store.get(webhook.id).await.unwrap().unwrap();
    let serialized = serde_json::to_string(&webhook).unwrap();
    assert!(
        !serialized.contains(&rig.secret),
        "the webhook leaked the secret"
    );
}

#[tokio::test]
async fn the_circuit_breaker_opens_then_recovers() {
    let rig = Rig::with_limits(
        vec![
            Step::Fail(EgressError::ResolutionFailed),
            Step::Respond(response(200)),
        ],
        CircuitBreakerConfig {
            failure_threshold: 1,
            open_secs: 10,
            half_open_probes: 1,
        },
        BulkheadConfig { max_concurrent: 4 },
    );
    let webhook = rig.register(webhook("https://example.com/hook")).await;
    let job = delivery_job(&webhook);
    let handler = rig.handler();

    assert!(matches!(
        handler.run(&job).await,
        DeliveryOutcome::Retry { .. }
    ));
    // The breaker is open: the next attempt is refused before any request is made.
    assert!(matches!(
        handler.run(&job).await,
        DeliveryOutcome::Rescheduled { .. }
    ));
    assert_eq!(
        rig.egress.recorded().len(),
        1,
        "no second request while open"
    );

    rig.clock.advance(std::time::Duration::from_secs(11));
    assert_eq!(handler.run(&job).await, DeliveryOutcome::Delivered);
    assert_eq!(rig.egress.recorded().len(), 2);
}
