//! End-to-end: TypeScript controllers on the real document engine over the fake backend —
//! representative controllers, the permission-sensitive view, limit breaches, a malicious module,
//! source-map positions, hot swap and the L9 latency probe.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Instant;

use common::{engine_for, n, note_meta, open_engine, system_inv, ts_controller, ts_engine};
use serde_json::json;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{RecordId, Value};
use spot_app_api::{AppResult, BoxFuture, Document, Invocation};
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_server_scripts::FieldView;
use spot_ts_controllers::{TsController, TsLimits};

/// A module that only declares its DocType and an empty step/method set.
fn empty_module() -> &'static str {
    "export function contract() { return { doctype: \"note\", steps: {}, methods: {} }; }\n\
     export function run_step() { return null; }\n\
     export function run_method() { return null; }\n"
}

fn note_draft(title: Option<&str>, hidden: Option<&str>) -> Document {
    let mut draft = Document::new(n("note"), RecordId::new_v7());
    if let Some(title) = title {
        draft.set(n("title"), Value::Text(title.to_owned()));
    }
    if let Some(hidden) = hidden {
        draft.set(n("hidden"), Value::Text(hidden.to_owned()));
    }
    draft
}

async fn stored_title(
    engine: &spot_document::engine::Engine<spot_storage_fake::FakeBackend>,
    id: RecordId,
) -> String {
    let stored = engine.peek("note", id).await.unwrap().unwrap();
    match stored.get("title") {
        Value::Text(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

// --- representative controllers ---------------------------------------------------------------

#[tokio::test]
async fn a_step_can_query_through_the_host() {
    let source = r#"
export function contract() { return { doctype: "note", steps: { probe: { point: "before_save" } }, methods: {} }; }
export function run_step() {
  const rows = host.query({ doctype: "note", fields: ["id"], limit: 5 });
  return { patch: { title: Array.isArray(rows) ? "rows:" + rows.length : "bad" } };
}
export function run_method() { return null; }
"#;
    let controller = ts_controller(
        "ts_controllers",
        "controllers/q.ts",
        source,
        TsLimits::default(),
    );
    let engine = engine_for(controller, "ts_controllers");
    let created = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "rows:0");
}

#[tokio::test]
async fn a_method_can_insert_a_related_document() {
    let source = r#"
export function contract() {
  return { doctype: "note", steps: {}, methods: {
    make_child: { right: "write", args: { title: "text" }, result: { id: "id" } } } };
}
export function run_step() { return null; }
export function run_method({ method, args }) {
  const child = host.insert({ doctype: "note", values: { title: args.title } });
  return { id: child.id };
}
"#;
    let controller = ts_controller(
        "ts_controllers",
        "controllers/i.ts",
        source,
        TsLimits::default(),
    );
    let engine = engine_for(controller, "ts_controllers");
    let parent = engine
        .create(&system_inv(), note_draft(Some("parent"), None))
        .await
        .unwrap()
        .value;
    let (_, result) = engine
        .call_method(
            &system_inv(),
            "note",
            parent.id(),
            "make_child",
            json!({ "title": "child" }),
            None,
        )
        .await
        .unwrap()
        .value;
    let id = result["id"].as_str().unwrap();
    assert!(spot_ts_controllers::parse_record_id(id).is_some());
}

#[tokio::test]
async fn a_step_can_apply_a_default() {
    let source = r#"
export function contract() { return { doctype: "note", steps: { default: { point: "before_save" } }, methods: {} }; }
export function run_step({ doc }) { if (!doc.title) { return { patch: { title: "default" } }; } return null; }
export function run_method() { return null; }
"#;
    let controller = ts_controller(
        "ts_controllers",
        "controllers/d.ts",
        source,
        TsLimits::default(),
    );
    let engine = engine_for(controller, "ts_controllers");
    let created = engine
        .create(&system_inv(), note_draft(None, None))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "default");
}

// --- the permission-sensitive view ------------------------------------------------------------

/// A view that hides `hidden` by masking it, as a real host does for a field the user may not read.
struct HideHidden;

impl FieldView for HideHidden {
    fn readable<'a>(
        &'a self,
        _inv: &'a Invocation,
        _def: &'a spot_app_api::DocTypeDef,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let mut out = doc.clone();
            out.set(n("hidden"), Value::Null);
            Ok(out)
        })
    }
}

#[tokio::test]
async fn the_view_decides_what_a_handler_sees() {
    let source = r#"
export function contract() { return { doctype: "note", steps: { peek: { point: "before_save" } }, methods: {} }; }
export function run_step({ doc }) {
  if (doc.hidden === null || doc.hidden === undefined) { return { patch: { title: "safe" } }; }
  return { patch: { title: "leak" } };
}
export function run_method() { return null; }
"#;
    let limits = TsLimits::default();
    let loaded =
        spot_ts_controllers::load_in_memory("ts_controllers", "controllers/v.ts", source, limits)
            .unwrap();
    let open = Arc::new(TsController::from_parts(
        "ts_controllers",
        "controllers/v.ts",
        loaded,
        ts_engine(),
        limits,
    ));
    let engine = engine_for(
        Arc::clone(&open) as Arc<dyn spot_app_api::Controller>,
        "ts_controllers",
    );
    let created = engine
        .create(&system_inv(), note_draft(Some("draft"), Some("secret")))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "leak");

    let loaded =
        spot_ts_controllers::load_in_memory("ts_controllers", "controllers/v.ts", source, limits)
            .unwrap();
    let masked = Arc::new(
        TsController::from_parts(
            "ts_controllers",
            "controllers/v.ts",
            loaded,
            ts_engine(),
            limits,
        )
        .with_view(Arc::new(HideHidden)),
    );
    let engine = engine_for(
        masked as Arc<dyn spot_app_api::Controller>,
        "ts_controllers",
    );
    let created = engine
        .create(&system_inv(), note_draft(Some("draft"), Some("secret")))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "safe");
}

// --- limit breaches ---------------------------------------------------------------------------

async fn expect_limit(source: &str, limits: TsLimits) {
    let controller = ts_controller("ts_controllers", "controllers/l.ts", source, limits);
    let engine = engine_for(controller, "ts_controllers");
    let error = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .expect_err("the operation must fail");
    assert_eq!(
        error.code,
        spot_ts_controllers::LIMIT_EXCEEDED,
        "unexpected error: {}: {}",
        error.code,
        error.message
    );
}

#[tokio::test]
async fn a_runaway_step_is_killed_by_its_deadline() {
    expect_limit(
        "export function contract() { return { doctype: \"note\", steps: { spin: { point: \"before_save\" } }, methods: {} }; }\n\
         export function run_step() { while (true) {} }\n\
         export function run_method() { return null; }",
        TsLimits {
            default_timeout_ms: 50,
            max_timeout_ms: 50,
            ..TsLimits::default()
        },
    )
    .await;
}

#[tokio::test]
async fn a_giant_allocation_is_killed_by_the_memory_limit() {
    expect_limit(
        "export function contract() { return { doctype: \"note\", steps: { eat: { point: \"before_save\" } }, methods: {} }; }\n\
         export function run_step() { const s = \"x\".repeat(100000000); return { patch: { title: s.length } }; }\n\
         export function run_method() { return null; }",
        TsLimits {
            memory_bytes: 8 * 1024 * 1024,
            ..TsLimits::default()
        },
    )
    .await;
}

#[tokio::test]
async fn too_many_host_calls_is_killed() {
    expect_limit(
        "export function contract() { return { doctype: \"note\", steps: { chatty: { point: \"before_save\" } }, methods: {} }; }\n\
         export function run_step() { for (let i = 0; i < 50; i++) { host.log({ level: \"info\", message: \"x\" }); } return null; }\n\
         export function run_method() { return null; }",
        TsLimits {
            host_calls: 5,
            ..TsLimits::default()
        },
    )
    .await;
}

// --- a malicious module is confined -----------------------------------------------------------

async fn title_after_step(source: &str) -> String {
    let controller = ts_controller(
        "ts_controllers",
        "controllers/m.ts",
        source,
        TsLimits::default(),
    );
    let engine = engine_for(controller, "ts_controllers");
    let created = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .unwrap()
        .value;
    stored_title(&engine, created.id()).await
}

#[tokio::test]
async fn prototype_pollution_does_not_survive_one_execution() {
    let source = r#"
export function contract() { return { doctype: "note", steps: { pollute: { point: "before_save" } }, methods: {} }; }
export function run_step() {
  if (({}).polluted === true) { return { patch: { title: "dirty" } }; }
  Object.prototype.polluted = true;
  return { patch: { title: "clean" } };
}
export function run_method() { return null; }
"#;
    assert_eq!(title_after_step(source).await, "clean");
    assert_eq!(title_after_step(source).await, "clean");
}

#[tokio::test]
async fn global_this_internals_are_not_reachable() {
    let source = r#"
export function contract() { return { doctype: "note", steps: { look: { point: "before_save" } }, methods: {} }; }
export function run_step() {
  const exposed = typeof globalThis.process !== "undefined"
    || typeof fetch !== "undefined"
    || typeof XMLHttpRequest !== "undefined"
    || typeof Deno !== "undefined";
  return { patch: { title: exposed ? "exposed" : "confined" } };
}
export function run_method() { return null; }
"#;
    assert_eq!(title_after_step(source).await, "confined");
}

#[tokio::test]
async fn eval_is_refused_when_the_module_loads() {
    let source = "export function contract() { return { doctype: \"note\", steps: { run: { point: \"before_save\" } }, methods: {} }; }\n\
         export function run_step() { return { patch: { title: \"\" + eval(\"1 + 1\") } }; }\n\
         export function run_method() { return null; }";
    let loaded = spot_ts_controllers::load_in_memory(
        "ts_controllers",
        "controllers/e.ts",
        source,
        TsLimits::default(),
    );
    let problems = loaded.err().expect("eval is refused");
    assert!(
        problems
            .iter()
            .any(|p| p.code == spot_ts_controllers::COMPILE_FAILED)
    );
}

#[tokio::test]
async fn the_function_constructor_is_disabled() {
    let source = "export function contract() { return { doctype: \"note\", steps: { run: { point: \"before_save\" } }, methods: {} }; }\n\
         export function run_step() { const f = new Function(\"return 1\"); return { patch: { title: \"\" + f() } }; }\n\
         export function run_method() { return null; }";
    let loaded = spot_ts_controllers::load_in_memory(
        "ts_controllers",
        "controllers/f.ts",
        source,
        TsLimits::default(),
    );
    let problems = loaded.err().expect("the Function constructor is refused");
    assert!(
        problems
            .iter()
            .any(|p| p.code == spot_ts_controllers::COMPILE_FAILED)
    );
}

// --- source-map positions ---------------------------------------------------------------------

#[tokio::test]
async fn a_thrown_error_carries_its_original_position() {
    let source = r#"
export function contract() {
  return { doctype: "note", steps: {}, methods: { boom: { right: "read" } } };
}
export function run_step() { return null; }
export function run_method() {
  throw new TypeError("boom");
}
"#;
    let controller = ts_controller(
        "ts_controllers",
        "controllers/boom.ts",
        source,
        TsLimits::default(),
    );
    let engine = engine_for(controller, "ts_controllers");
    let created = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .unwrap()
        .value;
    let error = engine
        .call_method(&system_inv(), "note", created.id(), "boom", json!({}), None)
        .await
        .expect_err("the method throws");
    assert_eq!(error.code, "script.thrown");
    assert!(
        error.details.get("line").is_some(),
        "no position in {:?}",
        error.details
    );
}

// --- hot swap ---------------------------------------------------------------------------------

#[tokio::test]
async fn swapping_the_controller_set_changes_the_next_operation_only() {
    let module_a = r#"
export function contract() { return { doctype: "note", steps: { fill: { point: "before_save" } }, methods: {} }; }
export function run_step() { return { patch: { title: "A" } }; }
export function run_method() { return null; }
"#;
    let module_b = r#"
export function contract() { return { doctype: "note", steps: { fill: { point: "before_save" } }, methods: {} }; }
export function run_step() { return { patch: { title: "B" } }; }
export function run_method() { return null; }
"#;
    let a = ts_controller(
        "ts_controllers",
        "controllers/a.ts",
        module_a,
        TsLimits::default(),
    );
    let engine = engine_for(
        Arc::clone(&a) as Arc<dyn spot_app_api::Controller>,
        "ts_controllers",
    );
    let first = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, first.id()).await, "A");

    let b = ts_controller(
        "ts_controllers",
        "controllers/b.ts",
        module_b,
        TsLimits::default(),
    );
    let mut registry = ControllerRegistry::new();
    registry.register(
        "ts_controllers",
        Arc::clone(&b) as Arc<dyn spot_app_api::Controller>,
        ControllerSource::TypeScript,
    );
    engine.set_seams(registry.install().unwrap());
    engine.set_controllers(vec![b as Arc<dyn spot_app_api::Controller>]);
    let second = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, second.id()).await, "B");
}

#[tokio::test]
async fn the_registry_snapshot_is_immutable_across_a_swap() {
    let a = ts_controller(
        "ts_controllers",
        "controllers/a.ts",
        empty_module(),
        TsLimits::default(),
    );
    let mut registry = ControllerRegistry::new();
    registry.register(
        "a",
        Arc::clone(&a) as Arc<dyn spot_app_api::Controller>,
        ControllerSource::TypeScript,
    );
    let before = registry.set();
    registry.register(
        "b",
        Arc::clone(&a) as Arc<dyn spot_app_api::Controller>,
        ControllerSource::TypeScript,
    );
    let after = registry.set();
    assert_eq!(before.controllers_for("note").len(), 1);
    assert_eq!(after.controllers_for("note").len(), 2);
    // The old set is unchanged: an operation that began on it still sees one controller.
    assert_eq!(before.controllers_for("note").len(), 1);
}

#[tokio::test]
async fn the_engine_can_be_built_without_a_controller() {
    let registry = ControllerRegistry::new();
    let seams = registry.install().unwrap();
    let engine = open_engine(seams);
    let created = engine
        .create(&system_inv(), note_draft(Some("plain"), None))
        .await
        .unwrap()
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "plain");
    let _ = note_meta();
}

// --- L9 latency probe -------------------------------------------------------------------------

/// The L9 budget: a controller module loads (transpile, compile and describe) in under 3 s, and a
/// whitelisted call runs in under 50 ms. Ignored by default; run with `--ignored`.
#[tokio::test]
#[ignore = "L9 latency probe: run with --ignored"]
async fn startup_and_per_call_latency_are_within_budget() {
    let source = r#"
export function contract() { return { doctype: "note", steps: {}, methods: { ping: { right: "read", result: { pong: "bool" } } } }; }
export function run_step() { return null; }
export function run_method() { return { pong: true }; }
"#;
    let started = Instant::now();
    let controller = ts_controller(
        "ts_controllers",
        "controllers/p.ts",
        source,
        TsLimits::default(),
    );
    let startup = started.elapsed();
    let engine = engine_for(controller, "ts_controllers");
    let created = engine
        .create(&system_inv(), note_draft(Some("draft"), None))
        .await
        .unwrap()
        .value;

    let started = Instant::now();
    const CALLS: u32 = 50;
    for _ in 0..CALLS {
        engine
            .call_method(&system_inv(), "note", created.id(), "ping", json!({}), None)
            .await
            .unwrap();
    }
    let per_call = started.elapsed() / CALLS;
    println!("startup: {startup:?}; per call: {per_call:?}");
    assert!(startup.as_secs() < 3, "startup {startup:?} over budget");
    assert!(
        per_call.as_millis() < 50,
        "per call {per_call:?} over budget"
    );
}
