//! The `invoke` order, step by step, with in-memory fakes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use spot_mgmt_api::handlers::{HandlerError, HandlerRegistry, handler_fn};
use spot_mgmt_api::ports::{ApprovalCheck, ApprovalRequest, AuditOutcome, BoxFuture};
use spot_mgmt_ops::OperationSpec;
use tokio::sync::Notify;

#[tokio::test]
async fn a_signed_command_is_accepted_runs_and_is_audited() {
    let h = Harness::new();
    let (status, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let done = h.wait_state(&id, "succeeded").await;
    assert_eq!(done["data"]["result"]["state"], "ok");
    assert_eq!(done["data"]["operation"], "site.backup.run");
    assert!(
        done["data"]["audit_ref"]
            .as_str()
            .unwrap()
            .starts_with("audit-")
    );
    let records = h.audit.records();
    assert!(records.iter().any(|r| r.outcome == AuditOutcome::Allowed
        && r.principal.as_deref() == Some("alice")
        && r.command_id.as_deref() == Some(id.as_str())));
    assert!(records.iter().any(|r| r.outcome == AuditOutcome::Succeeded));
}

#[tokio::test]
async fn a_read_operation_needs_the_certificate_only() {
    let h = Harness::new();
    let (status, _) = h.invoke(h.unsigned("site.status", status_params())).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn an_unsigned_command_above_read_is_refused() {
    let h = Harness::new();
    let (status, body) = h
        .invoke(h.unsigned("site.backup.run", backup_params()))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&body), "mgmt.signature_invalid");
}

#[tokio::test]
async fn an_unsigned_unknown_operation_cannot_probe_the_catalogue() {
    let h = Harness::new();
    let (status, body) = h.invoke(h.unsigned("nope.nothing", json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(code(&body), "mgmt.signature_invalid");
}

#[tokio::test]
async fn no_client_certificate_is_unauthenticated_and_an_unlisted_one_is_forbidden() {
    let h = Harness::new();
    let body = h.signed("site.backup.run", backup_params());
    let (status, answer) = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(body.clone()),
            Some("a"),
            None,
        )
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::UNAUTHORIZED, "mgmt.unauthenticated")
    );
    let (status, answer) = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(body),
            Some("b"),
            Some(mallory()),
        )
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::FORBIDDEN, "mgmt.forbidden")
    );
    let records = h.audit.records();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].certificate_fingerprint, None);
    assert_eq!(
        records[1].certificate_fingerprint.as_deref(),
        Some("fp-mallory")
    );
}

#[tokio::test]
async fn the_body_limit_applies_before_parse() {
    let h = Harness::new();
    let huge = "x".repeat(64 * 1024 + 1);
    let (status, body) = h.invoke(huge).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(code(&body), "mgmt.body_too_large");
}

#[tokio::test]
async fn the_wrong_audience_expired_and_future_commands_are_refused() {
    let h = Harness::new();
    let wrong = h.signed_with(
        "site.backup.run",
        backup_params(),
        "another-site",
        NOW - 5,
        300,
    );
    let (status, body) = h.invoke(wrong).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "mgmt.signature_invalid")
    );

    let expired = h.signed_with("site.backup.run", backup_params(), SITE, NOW - 1000, 300);
    let (status, body) = h.invoke(expired).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "mgmt.signature_expired")
    );

    let future = h.signed_with("site.backup.run", backup_params(), SITE, NOW + 3600, 300);
    let (_, body) = h.invoke(future).await;
    assert_eq!(code(&body), "mgmt.signature_expired");

    let long = h.signed_with("site.backup.run", backup_params(), SITE, NOW - 5, 601);
    let (_, body) = h.invoke(long).await;
    assert_eq!(code(&body), "mgmt.signature_expired");
}

#[tokio::test]
async fn the_same_nonce_twice_is_a_replay() {
    let h = Harness::new();
    let body = h.signed("site.backup.run", backup_params());
    let (first, _) = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(body.clone()),
            Some("one"),
            Some(alice()),
        )
        .await;
    assert_eq!(first, StatusCode::ACCEPTED);
    // A different idempotency key: this is a new request carrying an old envelope.
    let (second, answer) = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(body),
            Some("two"),
            Some(alice()),
        )
        .await;
    assert_eq!(
        (second, code(&answer)),
        (StatusCode::CONFLICT, "mgmt.replayed")
    );
}

#[tokio::test]
async fn a_retransmission_with_the_same_key_returns_the_first_answer() {
    let h = Harness::new();
    let body = h.signed("site.backup.run", backup_params());
    let send = |key: &'static str, b: String| {
        h.send(
            "POST",
            "/mgmt/v1/commands",
            Some(b),
            Some(key),
            Some(alice()),
        )
    };
    let (s1, a1) = send("same", body.clone()).await;
    let (s2, a2) = send("same", body).await;
    assert_eq!((s1, s2), (StatusCode::ACCEPTED, StatusCode::ACCEPTED));
    assert_eq!(a1["data"]["command_id"], a2["data"]["command_id"]);
    // The same key with another body is not a retransmission.
    let other = h.signed("site.backup.run", backup_params());
    let (s3, a3) = send("same", other).await;
    assert_eq!((s3, code(&a3)), (StatusCode::CONFLICT, "mgmt.replayed"));
}

#[tokio::test]
async fn a_missing_idempotency_key_is_a_parameter_error() {
    let h = Harness::new();
    let body = h.signed("site.backup.run", backup_params());
    let (status, answer) = h
        .send("POST", "/mgmt/v1/commands", Some(body), None, Some(alice()))
        .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(answer["error"]["details"]["parameter"], "Idempotency-Key");
}

#[tokio::test]
async fn a_down_nonce_store_refuses_and_nothing_runs() {
    let h = Harness::build(Options {
        nonces: Arc::new(DownNonces),
        ..Options::default()
    });
    let (status, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::INTERNAL_SERVER_ERROR, "mgmt.internal")
    );
    let (status, body) = h
        .send(
            "GET",
            "/mgmt/v1/commands/anything",
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "mgmt.unknown_command")
    );
}

#[tokio::test]
async fn a_tampered_body_does_not_verify() {
    let h = Harness::new();
    let text = h.signed("site.backup.run", backup_params());
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    value["command"]["params"]["target"] = json!("elsewhere");
    let (status, body) = h.invoke(value.to_string()).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNAUTHORIZED, "mgmt.signature_invalid")
    );
}

#[tokio::test]
async fn unknown_operations_and_parameters_are_refused_without_echoing_values() {
    let h = Harness::new();
    let (status, body) = h.invoke(h.signed("site.nothing.here", json!({}))).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::NOT_FOUND, "mgmt.unknown_operation")
    );

    let params = json!({ "site": "site-1", "target": "local", "surprise": "SECRETVALUE99" });
    let (status, body) = h.invoke(h.signed("site.backup.run", params)).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::UNPROCESSABLE_ENTITY, "mgmt.invalid_parameter")
    );
    assert_eq!(body["error"]["details"]["parameter"], "surprise");
    assert!(!body.to_string().contains("SECRETVALUE99"));

    let bad = json!({ "site": "SITE_UPPER_SECRETVALUE", "target": "local" });
    let (status, body) = h.invoke(h.signed("site.backup.run", bad)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!body.to_string().contains("SECRETVALUE"));

    let missing = json!({ "site": "site-1" });
    let (_, body) = h.invoke(h.signed("site.backup.run", missing)).await;
    assert_eq!(body["error"]["details"]["parameter"], "target");
}

#[tokio::test]
async fn every_refusal_is_audited_without_secrets() {
    let h = Harness::new();
    let signed = h.signed("site.backup.run", backup_params());
    let value: serde_json::Value = serde_json::from_str(&signed).unwrap();
    let signature = value["signatures"][0]["signature"]
        .as_str()
        .unwrap()
        .to_owned();
    let nonce = value["command"]["nonce"].as_str().unwrap().to_owned();
    // Refused three ways: no certificate, unlisted certificate, replay.
    let _ = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(signed.clone()),
            Some("a"),
            None,
        )
        .await;
    let _ = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(signed.clone()),
            Some("b"),
            Some(mallory()),
        )
        .await;
    let _ = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(signed.clone()),
            Some("c"),
            Some(alice()),
        )
        .await;
    let _ = h
        .send(
            "POST",
            "/mgmt/v1/commands",
            Some(signed),
            Some("d"),
            Some(alice()),
        )
        .await;
    let records = h.audit.records();
    let refused: Vec<_> = records
        .iter()
        .filter(|r| r.outcome == AuditOutcome::Refused)
        .collect();
    assert_eq!(refused.len(), 3);
    assert!(refused.iter().all(|r| r.reason.is_some()));
    let dump = serde_json::to_string(&records).unwrap();
    assert!(!dump.contains(&signature));
    assert!(!dump.contains(&nonce));
}

#[tokio::test]
async fn a_failing_audit_sink_turns_success_above_read_into_internal_and_nothing_runs() {
    let ran = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&ran);
    let mut handlers = HandlerRegistry::new();
    for id in ["site.backup.run", "site.status"] {
        let counter = Arc::clone(&counter);
        handlers
            .register(
                id,
                handler_fn(move |_c, _r| {
                    let counter = Arc::clone(&counter);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({}))
                    }
                }),
                false,
            )
            .unwrap();
    }
    let h = Harness::build(Options {
        audit_down: true,
        handlers: Some(handlers),
        ..Options::default()
    });
    let (status, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::INTERNAL_SERVER_ERROR, "mgmt.internal")
    );
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(ran.load(Ordering::SeqCst), 0);
    // A read operation is still answered.
    let (status, _) = h.invoke(h.unsigned("site.status", status_params())).await;
    assert_eq!(status, StatusCode::ACCEPTED);
}

#[tokio::test]
async fn two_destructive_commands_cannot_run_at_once() {
    let gate = Arc::new(Notify::new());
    let mut handlers = HandlerRegistry::new();
    let waiting = Arc::clone(&gate);
    handlers
        .register(
            "site.maintenance.enter",
            handler_fn(move |_c, _r| {
                let waiting = Arc::clone(&waiting);
                async move {
                    waiting.notified().await;
                    Ok(json!({}))
                }
            }),
            false,
        )
        .unwrap();
    let h = Harness::build(Options {
        handlers: Some(handlers),
        ..Options::default()
    });
    let params = json!({ "site": "site-1", "reason": "upgrade" });
    let (first, body) = h
        .invoke(h.signed("site.maintenance.enter", params.clone()))
        .await;
    assert_eq!(first, StatusCode::ACCEPTED);
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let (second, answer) = h
        .invoke(h.signed("site.maintenance.enter", params.clone()))
        .await;
    assert_eq!(
        (second, code(&answer)),
        (StatusCode::CONFLICT, "mgmt.conflict")
    );
    assert_eq!(answer["error"]["retryable"], true);
    gate.notify_one();
    h.wait_state(&id, "succeeded").await;
    let (third, _) = h.invoke(h.signed("site.maintenance.enter", params)).await;
    assert_eq!(third, StatusCode::ACCEPTED);
    gate.notify_one();
}

struct NeedsMigrationApproval {
    accept: bool,
}

impl ApprovalCheck for NeedsMigrationApproval {
    fn required(&self, spec: &OperationSpec) -> Vec<String> {
        if spec.id.as_str() == "site.migrate.apply" {
            vec!["migration".to_owned()]
        } else {
            Vec::new()
        }
    }

    fn verify<'a>(&'a self, _request: ApprovalRequest<'a>) -> BoxFuture<'a, Result<(), ()>> {
        Box::pin(std::future::ready(if self.accept {
            Ok(())
        } else {
            Err(())
        }))
    }
}

fn with_approvals(body: &str, approvals: &[&str]) -> String {
    let mut value: serde_json::Value = serde_json::from_str(body).unwrap();
    value["approvals"] = json!(approvals);
    value.to_string()
}

#[tokio::test]
async fn approvals_are_required_and_checked() {
    let params = json!({ "site": "site-1", "plan_hash": "a".repeat(64) });
    let h = Harness::build(Options {
        approvals: Arc::new(NeedsMigrationApproval { accept: true }),
        ..Options::default()
    });
    let signed = h.signed("site.migrate.apply", params.clone());
    let (status, body) = h.invoke(signed).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "mgmt.approval_required")
    );
    let signed = h.signed("site.migrate.apply", params.clone());
    let (status, _) = h.invoke(with_approvals(&signed, &["record-1"])).await;
    assert_eq!(status, StatusCode::ACCEPTED);

    let strict = Harness::build(Options {
        approvals: Arc::new(NeedsMigrationApproval { accept: false }),
        ..Options::default()
    });
    let signed = strict.signed("site.migrate.apply", params);
    let (status, body) = strict.invoke(with_approvals(&signed, &["forged"])).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::FORBIDDEN, "mgmt.approval_required")
    );
}

#[tokio::test]
async fn cancel_is_cooperative_and_only_for_handlers_that_declare_it() {
    let mut handlers = HandlerRegistry::new();
    handlers
        .register(
            "site.backup.run",
            handler_fn(|context, _r| async move {
                context.progress("waiting", 10).await;
                context.cancelled().await;
                Err(HandlerError::cancelled())
            }),
            true,
        )
        .unwrap();
    handlers
        .register(
            "site.maintenance.enter",
            handler_fn(|context, _r| async move {
                context.cancelled().await;
                Ok(json!({}))
            }),
            false,
        )
        .unwrap();
    let h = Harness::build(Options {
        handlers: Some(handlers),
        ..Options::default()
    });
    let (_, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let (status, _) = h
        .send(
            "POST",
            &format!("/mgmt/v1/commands/{id}/cancel"),
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let done = h.wait_state(&id, "cancelled").await;
    assert_eq!(done["data"]["error_code"], "mgmt.cancelled");

    let params = json!({ "site": "site-1", "reason": "x" });
    let (_, body) = h.invoke(h.signed("site.maintenance.enter", params)).await;
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let (status, answer) = h
        .send(
            "POST",
            &format!("/mgmt/v1/commands/{id}/cancel"),
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::FORBIDDEN, "mgmt.refused_by_policy")
    );
}

#[tokio::test]
async fn an_oversized_result_fails_the_command() {
    let mut handlers = HandlerRegistry::new();
    handlers
        .register(
            "site.backup.run",
            handler_fn(|_c, _r| async { Ok(json!({ "blob": "x".repeat(70_000) })) }),
            false,
        )
        .unwrap();
    let h = Harness::build(Options {
        handlers: Some(handlers),
        ..Options::default()
    });
    let (_, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let done = h.wait_state(&id, "failed").await;
    assert_eq!(done["data"]["error_code"], "mgmt.internal");
}

#[tokio::test]
async fn a_command_of_another_operator_is_unknown_to_a_third_party() {
    let h = Harness::new();
    let (_, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let (status, answer) = h
        .send(
            "GET",
            &format!("/mgmt/v1/commands/{id}"),
            None,
            None,
            Some(mallory()),
        )
        .await;
    // Mallory is not on the allow-list at all.
    assert_eq!(
        (status, code(&answer)),
        (StatusCode::FORBIDDEN, "mgmt.forbidden")
    );
}
