//! Shared setup: a scratch database, a bridge on a lazily built pool, the stores, and the task
//! fixtures. Tests never read a clock; every instant is a whole number of seconds.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use deadpool_postgres::Pool;
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_tasks::{
    AssigneeSpec, Decision, Priority, Task, TaskAction, TaskEvent, TaskId, TaskOption, TaskSpec,
};
use spot_bpm_tasks_pg::PgTaskStore;
use spot_pg_bridge::{Bridge, BridgeConfig};
use spot_postgres::testing::TestDb;
use spot_postgres::{PgBackend, PgConfig};

pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub db: TestDb,
}

/// A scratch database, or `None` (printing `SKIPPED:`) when no server is configured.
pub fn fixture() -> Option<Fixture> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let db = rt.block_on(TestDb::create())?;
    Some(Fixture { rt, db })
}

static NAMES: AtomicUsize = AtomicUsize::new(0);

pub fn pool_of(config: PgConfig, size: usize) -> Pool {
    let mut config = config;
    config.pool_max_size = size;
    PgBackend::connect_unchecked(config)
        .expect("backend")
        .pool()
}

/// A bridge on its own pool of 12 (in flight 8).
pub fn open_bridge(config: PgConfig) -> (Arc<Bridge>, Pool) {
    let pool = pool_of(config, 12);
    let bridge = Arc::new(
        Bridge::new(
            BridgeConfig {
                name: format!("bpm_tasks_pg_{}", NAMES.fetch_add(1, Ordering::SeqCst)),
                workers: 2,
                max_in_flight: 8,
                queue_capacity: 64,
                default_deadline: Duration::from_secs(5),
                max_deadline: Duration::from_secs(20),
            },
            pool.clone(),
        )
        .expect("bridge"),
    );
    (bridge, pool)
}

/// A store for `tenant` on a fresh bridge, with its tables created.
pub fn open_store(config: PgConfig, tenant: &str) -> (Arc<Bridge>, Arc<PgTaskStore>) {
    let (bridge, pool) = open_bridge(config);
    let store = Arc::new(PgTaskStore::new(Arc::clone(&bridge), &pool, tenant).expect("store"));
    store.prepare().expect("prepare");
    (bridge, store)
}

/// A second store for another tenant on the same bridge.
pub fn sibling(bridge: &Arc<Bridge>, config: PgConfig, tenant: &str) -> Arc<PgTaskStore> {
    let pool = pool_of(config, 12);
    Arc::new(PgTaskStore::new(Arc::clone(bridge), &pool, tenant).expect("store"))
}

/// 2023-11-14T22:13:20Z.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// `now()` plus `seconds`.
pub fn plus(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000 + seconds).unwrap()
}

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

/// A task for `(case, node)` named `id`, assigned to `assignees`, with `roles`.
pub fn task_with(id: &str, case: &str, node: &str, assignees: &[&str], roles: &[&str]) -> Task {
    Task::open(TaskSpec {
        id: TaskId::new(id).unwrap(),
        case: case.to_owned(),
        node: node.to_owned(),
        process: "approval".to_owned(),
        title: "Approve the order?".to_owned(),
        doctype: Some("Sales Order".to_owned()),
        subject: Some("SO-0001".to_owned()),
        created_by: Some("zed".to_owned()),
        distinct_from_creator: true,
        duty: None,
        scope: None,
        assignee_spec: AssigneeSpec::Any(assignees.iter().map(|a| (*a).to_owned()).collect()),
        assignees: assignees.iter().map(|a| (*a).to_owned()).collect(),
        roles: roles
            .iter()
            .map(|r| (*r).to_owned())
            .collect::<BTreeSet<_>>(),
        options: options(),
        form: serde_json::json!({ "note": "form-payload-marker" }),
        priority: Priority::Normal,
        due: None,
        sla: None,
        created_at: now(),
    })
}

pub fn task(id: &str, case: &str, node: &str, assignees: &[&str]) -> Task {
    task_with(id, case, node, assignees, &[])
}

pub fn created(task: &Task) -> TaskEvent {
    TaskEvent::new(&task.id, TaskAction::Created, "system:engine", now())
}

pub fn claimed(task: &Task, by: &str, at: i64) -> TaskEvent {
    TaskEvent::new(&task.id, TaskAction::Claimed, by, plus(at))
}
