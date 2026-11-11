//! A retired version that a running Case is pinned to survives a restart (WP-BPM-09.T5): it is
//! loaded for the pin, never chosen for a new start, and the Case finishes on it.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_definitions::{DefinitionStore, MemoryDefinitionStore, load_into};
use spot_bpm_engine::{
    CaseId, Engine, EngineError, HandlerRegistry, InMemoryCaseStore, InMemoryEngine,
    InMemoryTimerPort, ProcessRegistry,
};
use spot_bpm_model::VersionStatus;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A trigger and a terminal join: the terminal node's id (the marker) only makes each version's content hash differ.
fn text(name: &str, marker: &str) -> String {
    format!(
        r#"
name = "{name}"
app = "defs_test"
version = 1
status = "draft"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "{marker}"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "{marker}"
"#
    )
}

async fn publish(store: &MemoryDefinitionStore, name: &str, marker: &str, expected: Option<u32>) {
    store
        .publish(name, &text(name, marker), expected, "alice", None, now())
        .await
        .unwrap();
}

fn engine(cases: InMemoryCaseStore) -> InMemoryEngine {
    Engine::new(
        cases,
        InMemoryTimerPort::new(),
        HandlerRegistry::logic_default(),
    )
}

#[tokio::test]
async fn a_retired_pinned_version_loads_and_latest_is_the_published_one() {
    let store = MemoryDefinitionStore::new();
    publish(&store, "order", "end_one", None).await;
    publish(&store, "order", "end_two", Some(1)).await;
    store.retire("order", 1, "alice", now()).await.unwrap();
    store.set_running_pins("order", [1]);

    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &store).await.unwrap();
    assert!(report.is_clean(), "{report:?}");
    assert_eq!(
        report.loaded,
        vec![("order".to_owned(), 1), ("order".to_owned(), 2)]
    );
    let pinned = registry.get("order", 1).unwrap();
    assert_eq!(pinned.version.status, VersionStatus::Retired);
    assert_eq!(registry.latest("order").unwrap().number(), 2);
}

#[tokio::test]
async fn a_retired_and_unpinned_version_is_not_loaded() {
    let store = MemoryDefinitionStore::new();
    publish(&store, "order", "end_one", None).await;
    publish(&store, "order", "end_two", Some(1)).await;
    store.retire("order", 1, "alice", now()).await.unwrap();
    let registry = ProcessRegistry::new();
    let report = load_into(&registry, &store).await.unwrap();
    assert_eq!(report.loaded, vec![("order".to_owned(), 2)]);
    assert!(registry.get("order", 1).is_none());
}

#[tokio::test]
async fn a_case_resumes_on_its_retired_pin_after_a_restart_and_a_new_start_runs_the_new_version() {
    let store = MemoryDefinitionStore::new();
    publish(&store, "order", "end_one", None).await;

    // Before the restart: v1 is published and a Case starts on it.
    let cases = InMemoryCaseStore::new();
    let before = engine(cases.clone());
    load_into(before.processes(), &store).await.unwrap();
    let v1 = before.processes().latest("order").unwrap();
    assert_eq!(v1.number(), 1);
    let case = CaseId::new("case1").unwrap();
    before
        .start_definition(&v1, case.clone(), "k1".to_owned(), None, now())
        .await
        .unwrap();

    // v1 is retired, v2 published; the host records the Case's pin.
    publish(&store, "order", "end_two", Some(1)).await;
    store.retire("order", 1, "alice", now()).await.unwrap();
    store.set_running_pins("order", [1]);

    // The restart: a fresh registry from the store, over the same Case store.
    let after = engine(cases);
    let report = load_into(after.processes(), &store).await.unwrap();
    assert!(report.is_clean(), "{report:?}");

    let finished = after.run_until_idle(&case, now(), 10).await.unwrap();
    assert!(finished.run.is_finished());
    assert_eq!(finished.run.dag_version, 1, "the Case finishes on its pin");

    let fresh = after.processes().latest("order").unwrap();
    let record = after
        .start_definition(
            &fresh,
            CaseId::new("case2").unwrap(),
            "k2".to_owned(),
            None,
            now(),
        )
        .await
        .unwrap();
    assert_eq!(
        record.run.dag_version, 2,
        "a new start runs the new version"
    );
}

#[tokio::test]
async fn without_the_pin_a_restarted_case_cannot_resume_on_a_retired_version() {
    let store = MemoryDefinitionStore::new();
    publish(&store, "order", "end_one", None).await;
    let cases = InMemoryCaseStore::new();
    let before = engine(cases.clone());
    load_into(before.processes(), &store).await.unwrap();
    let v1 = before.processes().latest("order").unwrap();
    let case = CaseId::new("case1").unwrap();
    before
        .start_definition(&v1, case.clone(), "k1".to_owned(), None, now())
        .await
        .unwrap();
    publish(&store, "order", "end_two", Some(1)).await;
    store.retire("order", 1, "alice", now()).await.unwrap();

    let after = engine(cases);
    load_into(after.processes(), &store).await.unwrap();
    let error = after.step(&case, now()).await.unwrap_err();
    assert!(
        matches!(error, EngineError::ProcessNotFound { version: 1, .. }),
        "{error:?}"
    );
}
