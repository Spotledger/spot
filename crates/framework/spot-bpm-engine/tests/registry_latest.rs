//! `ProcessRegistry::latest` returns only published versions; `get` returns any registered one,
//! which is how a Case pinned to a retired version stays resolvable (WP-BPM-09.T5).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::task::{Context, Poll, Waker};

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{
    CaseId, CaseStore, EngineError, HandlerRegistry, Harness, ProcessDefinition, ProcessRegistry,
};
use spot_bpm_model::from_text;

/// Runs a future to completion on the current thread (every in-memory future is ready).
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

fn definition(name: &str, version: u32, status: &str) -> ProcessDefinition {
    let text = format!(
        r#"
name = "{name}"
app = "registry_test"
version = {version}
status = "{status}"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "done"
"#
    );
    ProcessDefinition::compile(&from_text(&text).unwrap()).unwrap()
}

#[test]
fn latest_skips_a_retired_version_and_get_still_returns_it() {
    let registry = ProcessRegistry::new();
    registry.publish(definition("order", 1, "published"));
    registry.publish(definition("order", 2, "published"));
    registry.publish(definition("order", 3, "retired"));
    assert_eq!(registry.latest("order").unwrap().number(), 2);
    assert_eq!(registry.get("order", 3).unwrap().number(), 3);
}

#[test]
fn latest_is_none_when_only_a_retired_version_is_registered() {
    let registry = ProcessRegistry::new();
    registry.publish(definition("order", 1, "retired"));
    assert!(registry.latest("order").is_none());
    assert_eq!(registry.get("order", 1).unwrap().number(), 1);
}

#[test]
fn a_draft_is_never_latest() {
    let registry = ProcessRegistry::new();
    registry.publish(definition("order", 1, "published"));
    registry.publish(definition("order", 2, "draft"));
    assert_eq!(registry.latest("order").unwrap().number(), 1);
    let only_draft = ProcessRegistry::new();
    only_draft.publish(definition("order", 1, "draft"));
    assert!(only_draft.latest("order").is_none());
    assert!(only_draft.get("order", 1).is_some());
}

#[test]
fn another_name_is_unaffected() {
    let registry = ProcessRegistry::new();
    registry.publish(definition("order", 1, "retired"));
    registry.publish(definition("invoice", 4, "published"));
    registry.publish(definition("invoice", 5, "retired"));
    assert!(registry.latest("order").is_none());
    assert_eq!(registry.latest("invoice").unwrap().number(), 4);
    assert!(registry.latest("missing").is_none());
}

const PARENT: &str = r#"
name = "parent"
app = "registry_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "sub"
kind = "sub_process"
process = "child"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "sub"

[[edges]]
from = "sub"
to = "done"
"#;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

#[test]
fn a_sub_process_naming_no_version_never_starts_a_retired_one() {
    block_on(async {
        let harness = Harness::new(HandlerRegistry::logic_default());
        let engine = harness.engine();
        engine
            .processes()
            .publish(definition("child", 1, "published"));
        engine
            .processes()
            .publish(definition("child", 2, "retired"));
        let parent = from_text(PARENT).unwrap();
        let id = CaseId::new("parent1").unwrap();
        engine
            .start(&parent, id.clone(), "p1", None, now())
            .await
            .unwrap();
        engine.run_until_idle(&id, now(), 20).await.unwrap();
        let child = engine
            .store()
            .find_by_key("parent1:sub")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(child.run.dag_version, 1, "the highest published version");
    });
}

#[test]
fn a_sub_process_whose_only_version_is_retired_is_unknown() {
    block_on(async {
        let harness = Harness::new(HandlerRegistry::logic_default());
        let engine = harness.engine();
        engine
            .processes()
            .publish(definition("child", 1, "retired"));
        let parent = from_text(PARENT).unwrap();
        let id = CaseId::new("parent1").unwrap();
        engine
            .start(&parent, id.clone(), "p1", None, now())
            .await
            .unwrap();
        let error = engine.run_until_idle(&id, now(), 20).await.unwrap_err();
        assert!(
            matches!(error, EngineError::ChildProcessUnknown { .. }),
            "{error:?}"
        );
    });
}
