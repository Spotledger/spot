//! WP-AI-04.T3: an audit outage is loud in production. The default alert sink emits a
//! structured error event (rate limited) and the failure counters, the dropped counter and the
//! retry-queue gauge are readable as Prometheus text.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_audit_chain::AuditRecord;
use spot_mcp::BoxFuture;
use spot_mcp::audit::{
    ALERT_INTERVAL_MICROS, AuditAlert, AuditAlerts, AuditFailurePolicy, AuditPhase, AuditSink,
    TelemetryAlerts,
};
use spot_mcp::error::McpError;
use spot_mcp::metrics::{
    AUDIT_FAILURES_TOTAL, AUDIT_RECORDS_DROPPED_TOTAL, AUDIT_RETRY_QUEUE_DEPTH,
};
use spot_mcp::principal::Credential;
use time::OffsetDateTime;
use tracing_subscriber::fmt::MakeWriter;

use support::{DOCTYPE, doc_ids, request, server_with_sink};

struct DownSink;

impl AuditSink for DownSink {
    fn record<'a>(&'a self, _record: AuditRecord) -> BoxFuture<'a, Result<(), McpError>> {
        Box::pin(async { Err(McpError::internal("sink down")) })
    }
}

#[derive(Clone, Default)]
struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Buf {
    type Writer = Buf;
    fn make_writer(&'a self) -> Buf {
        self.clone()
    }
}

fn at(secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + secs).unwrap()
}

fn metric(text: &str, name: &str) -> f64 {
    text.lines()
        .find_map(|l| l.strip_prefix(name).and_then(|r| r.trim().parse().ok()))
        .unwrap_or(0.0)
}

fn alert(tool: &str, phase: AuditPhase, at_micros: i64) -> AuditAlert {
    AuditAlert {
        tool: tool.to_owned(),
        phase,
        policy: AuditFailurePolicy::AlertOnly,
        refused: false,
        at_micros,
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

#[tokio::test]
async fn a_failing_sink_raises_one_alert_and_moves_the_exported_metrics() {
    // Rendering installs the process-wide recorder, so it comes before the first counter.
    let before = spot_telemetry::render_prometheus();
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let alerts = Arc::new(TelemetryAlerts::new());
    let (server, _m) = server_with_sink(
        Arc::new(DownSink),
        AuditFailurePolicy::FailClosedForWrites,
        alerts.clone(),
    )
    .await;
    // A write is refused (intent fails): one alert. A read proceeds (completion fails): one more.
    let who = Credential::bearer("carol");
    let _ = server.handle_text(&write_call(1), Some(&who), at(0)).await;
    let out: Vec<Json> = server.handle_text(&read_call(2), Some(&who), at(0)).await;
    assert!(!out.is_empty());

    let after = spot_telemetry::render_prometheus();
    assert!(
        metric(&after, AUDIT_FAILURES_TOTAL) - metric(&before, AUDIT_FAILURES_TOTAL) >= 2.0,
        "{after}"
    );
    assert!(after.contains(AUDIT_RETRY_QUEUE_DEPTH), "{after}");
    assert!(metric(&after, AUDIT_RETRY_QUEUE_DEPTH) >= 1.0, "{after}");
    // No label at all, so no value of any argument can leak through one.
    for line in after.lines().filter(|l| l.starts_with("mcp_audit")) {
        assert!(!line.contains('{') && !line.contains("secret"), "{line}");
    }

    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("ERROR"), "{logged}");
    assert!(logged.contains("spot_mcp::audit"), "{logged}");
    assert!(
        logged.contains(&format!("create_{DOCTYPE}_draft")),
        "{logged}"
    );
    assert!(logged.contains("refused=true"), "{logged}");
    assert!(!logged.contains("secret-title"), "{logged}");
    // Two different (tool, phase) pairs: two events, nothing suppressed.
    assert_eq!(
        logged
            .matches("the MCP audit record could not be written")
            .count(),
        2
    );
    assert_eq!(alerts.suppressed_total(), 0);
}

#[tokio::test]
async fn dropped_records_are_exported() {
    // Rendering installs the process-wide recorder, so it comes before the first counter.
    let before = spot_telemetry::render_prometheus();
    let (server, _m) = server_with_sink(
        Arc::new(DownSink),
        AuditFailurePolicy::AlertOnly,
        Arc::new(TelemetryAlerts::new()),
    )
    .await;
    let server = Arc::try_unwrap(server)
        .ok()
        .expect("sole owner")
        .with_audit_retry_capacity(1);
    let who = Credential::bearer("carol");
    for i in 0..3 {
        let _ = server.handle_text(&read_call(i), Some(&who), at(0)).await;
    }
    let after = spot_telemetry::render_prometheus();
    assert!(
        metric(&after, AUDIT_RECORDS_DROPPED_TOTAL) - metric(&before, AUDIT_RECORDS_DROPPED_TOTAL)
            >= 2.0,
        "{after}"
    );
}

#[test]
fn identical_alerts_are_limited_to_one_per_ten_seconds_and_counted() {
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(buf.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let alerts = TelemetryAlerts::new();
    let events = |buf: &Buf| {
        String::from_utf8(buf.0.lock().unwrap().clone())
            .unwrap()
            .matches("could not be written")
            .count()
    };

    alerts.alert(&alert("t", AuditPhase::Intent, 0));
    alerts.alert(&alert("t", AuditPhase::Intent, 1_000_000));
    alerts.alert(&alert("t", AuditPhase::Intent, ALERT_INTERVAL_MICROS - 1));
    assert_eq!(events(&buf), 1);
    assert_eq!(alerts.suppressed_total(), 2);

    // Another phase or another tool is not the same alert.
    alerts.alert(&alert("t", AuditPhase::Completion, 1));
    alerts.alert(&alert("u", AuditPhase::Intent, 1));
    assert_eq!(events(&buf), 3);

    // After the interval it is emitted again and reports how many were held back.
    alerts.alert(&alert("t", AuditPhase::Intent, ALERT_INTERVAL_MICROS));
    assert_eq!(events(&buf), 4);
    let logged = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("suppressed_since_last=2"), "{logged}");

    // A clock that steps back never silences an alert for good.
    alerts.alert(&alert("t", AuditPhase::Intent, -50_000_000));
    assert_eq!(events(&buf), 5);
}
