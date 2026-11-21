//! Shared fixtures for the `spot-bpm-tasks` integration tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_tasks::{
    AssigneeSpec, Decision, InMemoryTaskStore, MapDirectory, Priority, RecordingEngine, RunLabel,
    TaskId, TaskOption, TaskService, TaskSpec,
};

/// Runs an immediately-ready future to completion on the current thread.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// The fixed start instant the tests use (2023-11-14T22:13:20Z).
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// `now()` plus `seconds`.
pub fn plus(seconds: u64) -> OffsetDateTime {
    now() + std::time::Duration::from_secs(seconds)
}

/// A world with an in-memory store, a recording engine and a directory where `mgr` manages
/// `alice`.
pub struct World {
    /// The task store.
    pub store: Arc<InMemoryTaskStore>,
    /// The recording engine.
    pub engine: Arc<RecordingEngine>,
    /// The directory.
    pub directory: Arc<MapDirectory>,
    /// The service.
    pub service: Arc<TaskService>,
}

/// Builds the shared world.
pub fn world() -> World {
    let store = Arc::new(InMemoryTaskStore::new());
    let engine = Arc::new(RecordingEngine::new());
    let directory = Arc::new(
        MapDirectory::new()
            .with_role("manager", "mgr")
            .with_manager("alice", "mgr"),
    );
    let service =
        Arc::new(TaskService::new(store.clone(), engine.clone()).with_directory(directory.clone()));
    World {
        store,
        engine,
        directory,
        service,
    }
}

/// The default options: approve and reject.
pub fn options() -> Vec<TaskOption> {
    vec![
        TaskOption {
            label: "Approve".to_owned(),
            decision: Decision::Approve,
            branch: "approve".to_owned(),
        },
        TaskOption {
            label: "Reject".to_owned(),
            decision: Decision::Reject,
            branch: "reject".to_owned(),
        },
    ]
}

/// A minimal task spec assigned to `assignees`.
pub fn spec(id: &str, case: &str, node: &str, assignees: &[&str]) -> TaskSpec {
    TaskSpec {
        id: TaskId::new(id).unwrap(),
        case: case.to_owned(),
        node: node.to_owned(),
        process: "approval".to_owned(),
        title: "Approve the order?".to_owned(),
        doctype: None,
        subject: None,
        created_by: None,
        distinct_from_creator: false,
        duty: None,
        scope: None,
        assignee_spec: AssigneeSpec::Any(assignees.iter().map(|a| (*a).to_owned()).collect()),
        assignees: assignees.iter().map(|a| (*a).to_owned()).collect(),
        roles: BTreeSet::new(),
        options: options(),
        form: serde_json::json!({ "secret": "do-not-leak" }),
        priority: Priority::Normal,
        due: None,
        sla: None,
        created_at: now(),
    }
}

/// The shipped run label.
pub fn label() -> RunLabel {
    RunLabel::shipped()
}
