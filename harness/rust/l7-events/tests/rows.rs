//! The audit and job-enqueued events against their rows: a missing event and an extra event both
//! fail the check, and a clean pairing passes.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_l7_engine::world::World;
use spot_l7_events::rows::{AUDIT_TOPIC, JOB_TOPIC, ROWS, RowSource, RowsMatch};
use spot_l7_invariants::history::History;
use spot_l7_invariants::invariant::{BoxFuture, CheckContext, Invariant, Violation};
use spot_l7_invariants::oracle::Oracle;
use spot_storage_contract::{OutboxRecord, RecordId, StorageResult};
use support::{Canned, record};

struct Rows {
    audit: Vec<String>,
    jobs: Vec<String>,
}

impl RowSource for Rows {
    fn audit_rows(&self) -> BoxFuture<'_, StorageResult<Vec<String>>> {
        Box::pin(async move { Ok(self.audit.clone()) })
    }
    fn job_rows(&self) -> BoxFuture<'_, StorageResult<Vec<String>>> {
        Box::pin(async move { Ok(self.jobs.clone()) })
    }
}

fn audit_event(n: u64, subject: RecordId, version: u64) -> OutboxRecord {
    record(
        n,
        AUDIT_TOPIC,
        subject,
        Some(version),
        serde_json::json!({"operation": "create"}),
    )
}

fn job_event(n: u64, key: &str) -> OutboxRecord {
    record(
        n,
        JOB_TOPIC,
        RecordId::new_v7(),
        None,
        serde_json::json!({"job": "mail.send", "args": {}, "idempotency_key": key}),
    )
}

async fn violations(outbox: Vec<OutboxRecord>, rows: Rows) -> Vec<Violation> {
    let world = World::new();
    let ctx = CheckContext::new(
        Arc::new(Canned(outbox)),
        world.catalog.clone(),
        Arc::new(History::new(1, 1, "smoke")),
        Arc::new(Oracle::new()),
    );
    RowsMatch::new(Arc::new(rows)).check(&ctx).await
}

fn world_pair() -> (Vec<OutboxRecord>, Rows) {
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    let outbox = vec![
        audit_event(1, a, 1),
        audit_event(2, b, 3),
        job_event(3, "close-1"),
    ];
    let rows = Rows {
        audit: vec![format!("{a}:1"), format!("{b}:3")],
        jobs: vec!["close-1".to_owned()],
    };
    (outbox, rows)
}

#[tokio::test]
async fn matching_events_and_rows_pass() {
    let (outbox, rows) = world_pair();
    assert_eq!(violations(outbox, rows).await, Vec::new());
}

#[tokio::test]
async fn an_audit_row_whose_event_is_missing_fails() {
    let (mut outbox, rows) = world_pair();
    outbox.remove(0);
    let found = violations(outbox, rows).await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].invariant, ROWS);
    assert_eq!(found[0].entity, AUDIT_TOPIC);
    assert_eq!(found[0].detail, "row_without_event");
}

#[tokio::test]
async fn a_job_event_without_a_row_fails() {
    let (mut outbox, rows) = world_pair();
    outbox.push(job_event(9, "phantom"));
    let found = violations(outbox, rows).await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].entity, JOB_TOPIC);
    assert_eq!(found[0].id, "phantom");
    assert_eq!(found[0].detail, "event_without_row");
}

#[tokio::test]
async fn a_duplicated_event_fails() {
    let (mut outbox, rows) = world_pair();
    outbox.push(job_event(10, "close-1"));
    let found = violations(outbox, rows).await;
    assert!(
        found.iter().any(|v| v.detail == "duplicate_event"),
        "{found:?}"
    );
}

#[tokio::test]
async fn the_seventh_invariant_registers_beside_the_six() {
    let (_, rows) = world_pair();
    let registry = spot_l7_events::conformance_registry_with_rows(vec![], vec![], Arc::new(rows));
    assert_eq!(registry.len(), 7);
    assert!(registry.ids().contains(&ROWS));
}
