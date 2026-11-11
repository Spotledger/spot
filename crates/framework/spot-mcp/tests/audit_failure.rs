//! A failed audit write is never ignored: write-capable tools fail closed, read-only tools
//! raise a counter and an alert, and the failed records are retried from a bounded queue.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_audit_chain::AuditRecord;
use spot_mcp::audit::{AuditAlert, AuditAlerts, AuditFailurePolicy, AuditPhase, AuditSink};
use spot_mcp::error::McpError;
use spot_mcp::principal::Credential;
use spot_mcp::{BoxFuture, MemoryAudit};
use time::OffsetDateTime;

use support::{DOCTYPE, doc_ids, error_of, request, result_of, server_with_sink};

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
}

/// A sink that can be switched off, remembering what it accepted.
struct FlakySink {
    down: AtomicBool,
    inner: MemoryAudit,
}

impl FlakySink {
    fn new(down: bool) -> Arc<Self> {
        Arc::new(Self {
            down: AtomicBool::new(down),
            inner: MemoryAudit::new(),
        })
    }
    fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

impl AuditSink for FlakySink {
    fn record<'a>(&'a self, record: AuditRecord) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(McpError::internal("sink down"));
            }
            self.inner.record(record).await
        })
    }
}

#[derive(Default)]
struct Alerts(Mutex<Vec<AuditAlert>>);

impl AuditAlerts for Alerts {
    fn alert(&self, alert: &AuditAlert) {
        self.0.lock().unwrap().push(alert.clone());
    }
}

fn read_call(id: i64) -> String {
    request(
        id,
        "tools/call",
        json!({ "name": format!("get_{DOCTYPE}"), "arguments": { "id": doc_ids()[0].to_string() } }),
    )
}

fn write_call(id: i64) -> String {
    request(
        id,
        "tools/call",
        json!({ "name": format!("create_{DOCTYPE}_draft"), "arguments": { "title": "secret-title" } }),
    )
}

async fn call(server: &spot_mcp::Server, body: &str, secs: i64) -> Vec<Json> {
    server
        .handle_text(body, Some(&Credential::bearer("carol")), at(secs))
        .await
}

#[tokio::test]
async fn the_default_policy_fails_closed_for_writes() {
    assert_eq!(
        AuditFailurePolicy::default(),
        AuditFailurePolicy::FailClosedForWrites
    );
}

#[tokio::test]
async fn a_write_is_refused_when_its_intent_cannot_be_recorded() {
    for policy in [
        AuditFailurePolicy::FailClosed,
        AuditFailurePolicy::FailClosedForWrites,
    ] {
        let sink = FlakySink::new(true);
        let alerts = Arc::new(Alerts::default());
        let (server, _m) = server_with_sink(sink.clone(), policy, alerts.clone()).await;
        let messages = call(&server, &write_call(1), 0).await;
        let error = error_of(&messages);
        assert_eq!(error["data"]["code"], "mcp.audit_unavailable");
        assert!(!error.to_string().contains("secret-title"));
        assert_eq!(server.audit_failures().failures_total(), 1);
        let raised = alerts.0.lock().unwrap().clone();
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].phase, AuditPhase::Intent);
        assert!(raised[0].refused);
        assert!(!format!("{raised:?}").contains("secret-title"));
    }
}

#[tokio::test]
async fn a_write_that_was_refused_did_not_run() {
    let sink = FlakySink::new(true);
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::FailClosedForWrites,
        Arc::new(Alerts::default()),
    )
    .await;
    let _ = call(&server, &write_call(1), 0).await;
    sink.set_down(false);
    // The intent was never recorded and the call never ran: nothing was written at all.
    assert!(sink.inner.is_empty());
}

#[tokio::test]
async fn a_write_records_intent_then_completion() {
    let sink = FlakySink::new(false);
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::FailClosedForWrites,
        Arc::new(Alerts::default()),
    )
    .await;
    let messages = call(&server, &write_call(1), 0).await;
    assert_eq!(result_of(&messages)["isError"], false);
    let records = sink.inner.records();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records[0].payload.get("phase"),
        Some(&spot_audit_chain::PayloadValue::Text("intent".to_owned()))
    );
    assert_eq!(
        records[1].payload.get("ok"),
        Some(&spot_audit_chain::PayloadValue::Bool(true))
    );
}

#[tokio::test]
async fn a_read_proceeds_loudly_under_fail_closed_for_writes() {
    let sink = FlakySink::new(true);
    let alerts = Arc::new(Alerts::default());
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::FailClosedForWrites,
        alerts.clone(),
    )
    .await;
    let messages = call(&server, &read_call(1), 0).await;
    assert_eq!(result_of(&messages)["isError"], false);
    assert_eq!(server.audit_failures().failures_total(), 1);
    assert_eq!(server.audit_failures().queued(), 1);
    let raised = alerts.0.lock().unwrap().clone();
    assert_eq!(raised.len(), 1);
    assert_eq!(raised[0].phase, AuditPhase::Completion);
    assert!(!raised[0].refused);
}

#[tokio::test]
async fn a_failed_read_record_is_retried_once_the_sink_is_back() {
    let sink = FlakySink::new(true);
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::FailClosedForWrites,
        Arc::new(Alerts::default()),
    )
    .await;
    let _ = call(&server, &read_call(1), 0).await;
    sink.set_down(false);
    // Too soon: the backoff has not elapsed, so the retry is not due.
    let _ = call(&server, &read_call(2), 0).await;
    assert_eq!(sink.inner.len(), 1);
    // Later: the queued record is flushed before the new call is recorded.
    let _ = call(&server, &read_call(3), 120).await;
    assert_eq!(sink.inner.len(), 3);
    assert_eq!(server.audit_failures().queued(), 0);
}

#[tokio::test]
async fn a_read_is_refused_under_fail_closed() {
    let sink = FlakySink::new(true);
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::FailClosed,
        Arc::new(Alerts::default()),
    )
    .await;
    let messages = call(&server, &read_call(1), 0).await;
    assert_eq!(error_of(&messages)["data"]["code"], "mcp.audit_unavailable");
}

#[tokio::test]
async fn alert_only_never_refuses() {
    let sink = FlakySink::new(true);
    let alerts = Arc::new(Alerts::default());
    let (server, _m) =
        server_with_sink(sink.clone(), AuditFailurePolicy::AlertOnly, alerts.clone()).await;
    let read = call(&server, &read_call(1), 0).await;
    assert_eq!(result_of(&read)["isError"], false);
    let write = call(&server, &write_call(2), 0).await;
    assert_eq!(result_of(&write)["isError"], false);
    assert_eq!(server.audit_failures().failures_total(), 3);
    assert!(alerts.0.lock().unwrap().iter().all(|a| !a.refused));
}

#[tokio::test]
async fn a_write_whose_completion_cannot_be_recorded_keeps_its_result() {
    // The intent succeeds, then the sink dies before the completion record.
    struct DiesAfterOne(FlakySink);
    impl AuditSink for DiesAfterOne {
        fn record<'a>(&'a self, record: AuditRecord) -> BoxFuture<'a, Result<(), McpError>> {
            Box::pin(async move {
                let result = self.0.record(record).await;
                self.0.set_down(true);
                result
            })
        }
    }
    let sink = Arc::new(DiesAfterOne(FlakySink {
        down: AtomicBool::new(false),
        inner: MemoryAudit::new(),
    }));
    let alerts = Arc::new(Alerts::default());
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::FailClosedForWrites,
        alerts.clone(),
    )
    .await;
    let messages = call(&server, &write_call(1), 0).await;
    assert_eq!(result_of(&messages)["isError"], false);
    assert_eq!(server.audit_failures().queued(), 1);
    assert_eq!(alerts.0.lock().unwrap()[0].phase, AuditPhase::Completion);
}

#[tokio::test]
async fn the_retry_queue_is_bounded_and_drops_the_oldest() {
    let sink = FlakySink::new(true);
    let (server, _m) = server_with_sink(
        sink.clone(),
        AuditFailurePolicy::AlertOnly,
        Arc::new(Alerts::default()),
    )
    .await;
    let server = Arc::try_unwrap(server)
        .ok()
        .expect("sole owner")
        .with_audit_retry_capacity(2);
    for i in 0..5 {
        let _ = call(&server, &read_call(i), 0).await;
    }
    assert_eq!(server.audit_failures().failures_total(), 5);
    assert_eq!(server.audit_failures().queued(), 2);
    assert_eq!(server.audit_failures().dropped_total(), 3);
}
