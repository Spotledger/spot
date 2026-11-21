//! Integration tests for the Decision node handler: it waits on a human task, parks the Case when
//! no assignee resolves, and the engine adapter resumes a wait.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;

use common::{block_on, now, spec, world};
use spot_bpm_engine::{CaseId, Engine, HandlerRegistry, InMemoryCaseStore, InMemoryTimerPort};
use spot_bpm_model::from_text;
use spot_bpm_tasks::{
    Decision, DecisionNodeHandler, EngineAdapter, InMemoryTaskStore, MapDirectory, NoApprovers,
    RecordingEngine, TaskEngine, TaskService, TaskStore,
};
use spot_dag::NodeId;
use spot_dag_exec::{NodeStatus, RunStatus};

const APPROVAL: &str = r#"
name = "approval"
app = "tasks_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "approve"
kind = "decision"
title = "Approve the order?"
assignee = "user:alice"
outcomes = ["approve", "reject"]

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "approve"

[[edges]]
from = "approve"
to = "done"
"#;

const NO_ASSIGNEE: &str = r#"
name = "no_assignee"
app = "tasks_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "approve"
kind = "decision"
title = "Approve the order?"
assignee = "role:empty"
outcomes = ["approve", "reject"]

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "approve"

[[edges]]
from = "approve"
to = "done"
"#;

const EVENT_WAIT: &str = r#"
name = "wait_event"
app = "tasks_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "pause"
kind = "wait"
wait = "event"
event = "paid"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "pause"

[[edges]]
from = "pause"
to = "done"
"#;

fn node(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

#[test]
fn the_decision_node_waits_and_resumes() {
    let store = Arc::new(InMemoryTaskStore::new());
    let recorder = Arc::new(RecordingEngine::new());
    let directory = Arc::new(MapDirectory::new());
    let service = Arc::new(
        TaskService::new(store.clone(), recorder.clone()).with_directory(directory.clone()),
    );
    let handler = DecisionNodeHandler::new(service.clone(), directory, Arc::new(NoApprovers));
    let mut registry = HandlerRegistry::logic_default();
    registry.register(handler);

    let engine = Engine::new(InMemoryCaseStore::new(), InMemoryTimerPort::new(), registry);
    let version = from_text(APPROVAL).unwrap();
    engine.publish(&version).unwrap();
    let case = CaseId::new("case1").unwrap();

    block_on(async {
        engine
            .start(&version, case.clone(), "key1".to_owned(), None, now())
            .await
            .unwrap();
        for _ in 0..6 {
            let report = engine.step(&case, now()).await.unwrap();
            if !report.made_progress() {
                break;
            }
        }
        let record = engine.load(&case).await.unwrap();
        assert_eq!(record.run.status, RunStatus::Running);
        assert_eq!(
            record.run.node(&node("approve")).unwrap().status,
            NodeStatus::Running,
            "the Decision node is waiting"
        );

        // The handler opened a task assigned to alice.
        let task = store.find_by_node("case1", "approve").unwrap().unwrap();
        assert_eq!(task.assignees, BTreeSet::from(["alice".to_owned()]));
        assert!(task.accepts(&Decision::Approve));

        // Completing it resumes the Case through the recording engine.
        let completed = service
            .complete(&task.id, "alice", Decision::Approve, None, now())
            .await
            .unwrap();
        assert_eq!(completed.status, spot_bpm_tasks::TaskStatus::Completed);
        assert_eq!(recorder.count(), 1);

        // The signal the service would send resumes the engine's wait.
        engine
            .signal(&case, &node("approve"), None, now())
            .await
            .unwrap();
        for _ in 0..6 {
            let report = engine.step(&case, now()).await.unwrap();
            if !report.made_progress() {
                break;
            }
        }
        let record = engine.load(&case).await.unwrap();
        assert_eq!(record.run.status, RunStatus::Completed);
    });
}

#[test]
fn an_unresolved_assignee_parks_the_case() {
    let store = Arc::new(InMemoryTaskStore::new());
    let recorder = Arc::new(RecordingEngine::new());
    let directory = Arc::new(MapDirectory::new());
    let service =
        Arc::new(TaskService::new(store.clone(), recorder).with_directory(directory.clone()));
    let handler = DecisionNodeHandler::new(service, directory, Arc::new(NoApprovers));
    let mut registry = HandlerRegistry::logic_default();
    registry.register(handler);

    let engine = Engine::new(InMemoryCaseStore::new(), InMemoryTimerPort::new(), registry);
    let version = from_text(NO_ASSIGNEE).unwrap();
    engine.publish(&version).unwrap();
    let case = CaseId::new("case1").unwrap();

    block_on(async {
        engine
            .start(&version, case.clone(), "key1".to_owned(), None, now())
            .await
            .unwrap();
        for _ in 0..6 {
            let report = engine.step(&case, now()).await.unwrap();
            if !report.made_progress() {
                break;
            }
        }
        let record = engine.load(&case).await.unwrap();
        assert_eq!(
            record.run.node(&node("approve")).unwrap().status,
            NodeStatus::Failed,
            "no assignee parks the node with bpm_tasks.no_assignee"
        );
        assert!(
            store.find_by_node("case1", "approve").unwrap().is_none(),
            "no task was created"
        );
    });
}

#[test]
fn the_engine_adapter_resumes_a_wait() {
    let engine = Arc::new(Engine::new(
        InMemoryCaseStore::new(),
        InMemoryTimerPort::new(),
        HandlerRegistry::logic_default(),
    ));
    let version = from_text(EVENT_WAIT).unwrap();
    engine.publish(&version).unwrap();
    let case = CaseId::new("case_wait").unwrap();

    block_on(async {
        engine
            .start(&version, case.clone(), "key1".to_owned(), None, now())
            .await
            .unwrap();
        for _ in 0..6 {
            let report = engine.step(&case, now()).await.unwrap();
            if !report.made_progress() {
                break;
            }
        }
        let record = engine.load(&case).await.unwrap();
        assert_eq!(
            record.run.node(&node("pause")).unwrap().status,
            NodeStatus::Running
        );

        let adapter = EngineAdapter::new(engine.clone());
        adapter.signal(&case, "pause", None, now()).await.unwrap();
        for _ in 0..6 {
            let report = engine.step(&case, now()).await.unwrap();
            if !report.made_progress() {
                break;
            }
        }
        let record = engine.load(&case).await.unwrap();
        assert_eq!(record.run.status, RunStatus::Completed);
    });
}

/// Keeps the shared fixtures' `spec` helper referenced (the handler tests build specs inline).
#[test]
fn a_spec_still_opens_through_the_service() {
    let w = world();
    let task = w
        .service
        .open_task(spec("t1", "case1", "approve", &["alice"]))
        .unwrap();
    assert!(task.is_open());
}
