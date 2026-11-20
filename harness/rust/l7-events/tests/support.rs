//! Shared by the suites: a canned store for the seeded failures, and the run-and-check helper.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::sync::Arc;

use spot_events_contract::{Actor, EventMeta, PayloadDocument, Topic};
use spot_l7_engine::runner::EngineAccess;
use spot_l7_engine::world::World;
use spot_l7_events::{ExpectedTopic, conformance_registry, expected_for_engine_world};
use spot_l7_invariants::history::History;
use spot_l7_invariants::invariant::{BoxFuture, CheckContext, Report, RunMeta, StoreAccess};
use spot_l7_invariants::oracle::Oracle;
use spot_storage_contract::{
    Change, ErrorKind, FeedPosition, Name, NodeRef, OutboxRecord, Query, RecordId, Rows,
    StorageError, StorageResult, Version,
};
use time::OffsetDateTime;

/// A store that holds an outbox and nothing else.
pub struct Canned(pub Vec<OutboxRecord>);

impl StoreAccess for Canned {
    fn query<'a>(&'a self, _query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async { Err(StorageError::new(ErrorKind::Unsupported, "canned store")) })
    }
    fn outbox(&self) -> BoxFuture<'_, StorageResult<Vec<OutboxRecord>>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
    fn changes(&self) -> BoxFuture<'_, StorageResult<Vec<Change>>> {
        Box::pin(async { Ok(Vec::new()) })
    }
    fn next_number<'a>(&'a self, _series: &'a str) -> BoxFuture<'a, StorageResult<u64>> {
        Box::pin(async { Ok(1) })
    }
}

pub fn record(
    n: u64,
    topic: &str,
    subject: RecordId,
    version: Option<u64>,
    data: serde_json::Value,
) -> OutboxRecord {
    let meta = EventMeta {
        occurred_at: OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
        actor: Actor::System {
            component: Name::new("l7_events").unwrap(),
        },
        command_id: None,
        causation_id: None,
        correlation_id: RecordId::new_v7(),
        trace_id: None,
    };
    let document = PayloadDocument { meta, data };
    let topic = Topic::any(topic).unwrap();
    let event = document
        .to_outbox_event(
            RecordId::new_v7(),
            &topic,
            NodeRef {
                entity: Name::new("task").unwrap(),
                id: subject,
            },
            version.map(Version),
            1,
        )
        .unwrap();
    OutboxRecord {
        position: FeedPosition::new(0, n),
        transaction: RecordId::new_v7(),
        ordinal: 0,
        event,
    }
}

/// One clean event per expected topic.
pub fn clean_outbox() -> Vec<OutboxRecord> {
    let id = RecordId::new_v7();
    expected_for_engine_world()
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let data = match t.topic {
                "doc_updated" => serde_json::json!({"name": "a", "changed_fields": ["subject"]}),
                "doc_renamed" => serde_json::json!({"old_name": "a", "new_name": "b"}),
                _ => serde_json::json!({"name": "a"}),
            };
            record(i as u64 + 1, t.topic, id, Some(i as u64 + 1), data)
        })
        .collect()
}

pub fn meta() -> RunMeta {
    RunMeta {
        seed: 1,
        backend: "canned".to_owned(),
        profile: "smoke".to_owned(),
        workers: 1,
    }
}

/// The ids of the invariants that failed on `outbox`.
pub async fn failed_on(
    outbox: Vec<OutboxRecord>,
    secrets: Vec<String>,
    expected: Vec<ExpectedTopic>,
) -> Vec<String> {
    let world = World::new();
    let ctx = CheckContext::new(
        Arc::new(Canned(outbox)),
        world.catalog.clone(),
        Arc::new(History::new(1, 1, "smoke")),
        Arc::new(Oracle::new()),
    );
    let report: Report = conformance_registry(secrets, expected)
        .check_all(&ctx, meta())
        .await;
    report
        .results
        .iter()
        .filter(|r| !r.passed)
        .map(|r| r.id.clone())
        .collect()
}

/// Checks the outbox of a finished engine run with the six invariants.
pub async fn check_run<B: spot_storage_contract::StorageBackend>(
    world: &Arc<World>,
    run: &spot_l7_engine::driver::Run<B>,
) -> Report {
    let access = Arc::new(EngineAccess::new(run.engine.clone(), world.catalog.clone()));
    let ctx = CheckContext::new(
        access,
        world.catalog.clone(),
        Arc::new(run.history.clone()),
        Arc::new(Oracle::new()),
    );
    conformance_registry(run.oracle.secrets.clone(), expected_for_engine_world())
        .check_all(&ctx, run.meta.clone())
        .await
}
