//! Server Scripts and TypeScript controllers, hosted (WP-EXT-06.T4): the hub, the admin
//! controller, the slot and the module source, driven through the real document engine on the
//! in-memory backend. The same cases over PostgreSQL are the l3-http `scripts` cases; this file
//! needs no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use spot_app_api::context::DocumentService as _;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, AppError, AppResult, BoxFuture, Document, ErrorClass, Invocation};
use spot_cli::services::scripts::{
    DirModules, Runtime, ScriptSlot, Settings, SlotState, is_scriptable, register_controllers,
    slot, typescript_factory,
};
use spot_controllers::{ControllerFactory, ControllerRef, ControllerRegistry};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_server_scripts::{
    AllowAllView, FakeScriptStore, FieldView, MethodCaller, ScriptEvent, ScriptStore, ServerScript,
};
use spot_storage_fake::FakeBackend;
use spot_ts_controllers::ModuleSource;

// -------------------------------------------------------------------------------------------
// Fixtures

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

fn unique_site() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("scripts-host-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn text_field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields.push(text_field("title"));
    note.fields.push(text_field("hidden_note"));
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    note.fields.push(total);

    let mut script = Definition::new("server_script");
    script.fields.push({
        let mut spec = text_field("name");
        spec.unique = true;
        spec
    });
    script
        .fields
        .push(FieldSpec::new("enabled", FieldType::Bool));
    script.fields.push(text_field("doctype"));
    script.fields.push({
        let mut spec = FieldSpec::new("event", FieldType::Select);
        spec.options = Some(
            "before_validate\nvalidate\nbefore_save\nafter_save\nbefore_submit\non_submit\nbefore_cancel\non_cancel\nafter_commit"
                .to_owned(),
        );
        spec
    });
    script
        .fields
        .push(FieldSpec::new("script", FieldType::LongText));
    script.fields.push(text_field("compiled_hash"));
    script
        .fields
        .push(FieldSpec::new("last_error", FieldType::LongText));
    script
        .fields
        .push(FieldSpec::new("timeout_ms", FieldType::Int));
    script
        .fields
        .push(FieldSpec::new("priority", FieldType::Int));
    script
        .fields
        .push(FieldSpec::new("last_run_count", FieldType::Int));
    script.fields.push(text_field("disabled_reason"));

    Registry::new(vec![note, script])
        .expect("the definitions compile")
        .effective()
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("scripts.test").expect("a site id"),
        actor: Actor::System {
            component: n("scripts_tests"),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    }
}

fn script(name: &str, event: ScriptEvent, source: &str) -> ServerScript {
    ServerScript {
        record_id: None,
        name: name.to_owned(),
        doctype: "note".to_owned(),
        event,
        source: source.to_owned(),
        compiled_hash: None,
        timeout_ms: 200,
        priority: 0,
        enabled: true,
        disabled_reason: None,
    }
}

struct NoCaller;

impl MethodCaller for NoCaller {
    fn call<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        _method: &'a str,
        _args: serde_json::Value,
    ) -> BoxFuture<'a, AppResult<serde_json::Value>> {
        Box::pin(async {
            Err(AppError::new(
                ErrorClass::Internal,
                "test.no_caller",
                "none",
            ))
        })
    }
}

/// One hosted site: the slot, the runtime over `store`, the engine with the hub installed.
struct Hosted {
    slot: Arc<ScriptSlot>,
    runtime: Arc<Runtime>,
    store: Arc<FakeScriptStore>,
    engine: Engine<FakeBackend>,
}

async fn hosted_with(
    scripts: Vec<ServerScript>,
    settings: Settings,
    view: Arc<dyn FieldView>,
) -> Hosted {
    let site = unique_site();
    let slot = slot(&site);
    let store = Arc::new(FakeScriptStore::with_scripts(scripts));
    let runtime = Arc::new(Runtime::new(
        settings.clone(),
        slot.script_engine().clone(),
        Arc::clone(&store) as Arc<dyn ScriptStore>,
        view,
        Arc::new(NoCaller),
    ));
    slot.set(SlotState::Ready(Arc::clone(&runtime)));
    let mut registry = ControllerRegistry::new();
    register_controllers(&mut registry, &site, &settings, ["note", "server_script"]);
    let seams = registry.install().expect("no step conflicts");
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("the store prepares");
    Hosted {
        slot,
        runtime,
        store,
        engine,
    }
}

async fn hosted(scripts: Vec<ServerScript>) -> Hosted {
    hosted_with(scripts, Settings::default(), Arc::new(AllowAllView)).await
}

fn note(title: &str, total: &str) -> Document {
    let mut draft = Document::new(n("note"), RecordId::new_v7());
    draft.set(n("title"), Value::Text(title.to_owned()));
    draft.set(n("hidden_note"), Value::Text("secret".to_owned()));
    draft.set(
        n("total"),
        Value::Decimal(total.parse().expect("a decimal")),
    );
    draft
}

const REJECT_NEGATIVE: &str = r#"
export function on_validate({ doc }) {
  if (doc.total.startsWith("-")) {
    host.throw({ code: "test.total_negative", message: "The total cannot be negative.",
                 fields: [{ field: "total", code: "test.total_negative", message: "The total cannot be negative." }] });
  }
}"#;

// -------------------------------------------------------------------------------------------
// The hub

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_that_rejects_a_negative_total_blocks_the_create_with_a_field_error() {
    let host = hosted(vec![script(
        "no_negative",
        ScriptEvent::Validate,
        REJECT_NEGATIVE,
    )])
    .await;
    let error = host
        .engine
        .create(&inv(), note("bad", "-5.00"))
        .await
        .expect_err("the script refuses the create");
    assert_eq!(error.class, ErrorClass::Validation);
    assert_eq!(error.fields[0].code, "test.total_negative");
    assert_eq!(error.fields[0].field.as_str(), "total");
    host.engine
        .create(&inv(), note("good", "5.00"))
        .await
        .expect("a positive total saves");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn editing_a_script_takes_effect_on_the_next_request_without_a_restart() {
    let host = hosted(vec![]).await;
    host.engine
        .create(&inv(), note("before", "-1.00"))
        .await
        .expect("no script yet");
    host.store.put(script(
        "no_negative",
        ScriptEvent::Validate,
        REJECT_NEGATIVE,
    ));
    // The admin controller does this when the save of the script commits.
    host.runtime.invalidate();
    host.engine
        .create(&inv(), note("after", "-1.00"))
        .await
        .expect_err("the new script is in force");
    host.store.put(ServerScript {
        enabled: false,
        ..script("no_negative", ScriptEvent::Validate, REJECT_NEGATIVE)
    });
    host.runtime.invalidate();
    host.engine
        .create(&inv(), note("disabled", "-1.00"))
        .await
        .expect("a disabled script does not run");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scripts_of_one_event_run_by_priority_then_name() {
    let source = |tag: &str| {
        format!(
            r#"export function on_before_save({{ doc }}) {{ return {{ patch: {{ title: (doc.title || "") + "{tag}" }} }}; }}"#
        )
    };
    let mut late = script("a_late", ScriptEvent::BeforeSave, &source("L"));
    late.priority = 5;
    let early_b = script("b_early", ScriptEvent::BeforeSave, &source("B"));
    let early_a = script("a_early", ScriptEvent::BeforeSave, &source("A"));
    let host = hosted(vec![late, early_b, early_a]).await;
    let created = host
        .engine
        .create(&inv(), note("", "1.00"))
        .await
        .expect("saved")
        .value;
    let stored = host
        .engine
        .peek("note", created.id())
        .await
        .expect("peek")
        .expect("stored");
    assert_eq!(stored.get("title"), &Value::Text("ABL".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_infinite_loop_is_stopped_within_the_timeout_and_the_write_fails_cleanly() {
    let mut runaway = script(
        "runaway",
        ScriptEvent::BeforeSave,
        "export function on_before_save({ doc }) { while (true) {} }",
    );
    runaway.timeout_ms = 50;
    let host = hosted(vec![runaway]).await;
    let started = Instant::now();
    let error = host
        .engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect_err("the runaway is cut off");
    assert_eq!(error.code, "serverscript.limit_exceeded");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the loop is bounded"
    );
    // The message names the script and never its content.
    assert!(!error.message.contains("while"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_timeout_a_script_asks_for_is_clamped_to_the_operator_ceiling() {
    let mut greedy = script(
        "greedy",
        ScriptEvent::BeforeSave,
        "export function on_before_save({ doc }) { while (true) {} }",
    );
    greedy.timeout_ms = 3_600_000;
    let settings = Settings {
        default_timeout_ms: 50,
        max_timeout_ms: 100,
        ..Settings::default()
    };
    let host = hosted_with(vec![greedy], settings, Arc::new(AllowAllView)).await;
    let started = Instant::now();
    host.engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect_err("cut off at the ceiling, not at an hour");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_sees_only_what_the_view_of_the_triggering_user_allows() {
    struct Hiding;
    impl FieldView for Hiding {
        fn readable<'a>(
            &'a self,
            _inv: &'a Invocation,
            _def: &'a spot_app_api::DocTypeDef,
            doc: &'a Document,
        ) -> BoxFuture<'a, AppResult<Document>> {
            Box::pin(async move {
                let kept: BTreeMap<Name, Value> = doc
                    .fields()
                    .iter()
                    .filter(|(k, _)| k.as_str() != "hidden_note")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let mut out = Document::new(doc.doctype().clone(), doc.id());
                for (k, v) in kept {
                    out.set(k, v);
                }
                Ok(out)
            })
        }
    }
    let probe = script(
        "probe",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) {
             return { patch: { title: ("hidden_note" in doc) ? "leaked" : "hidden" } };
           }"#,
    );
    let host = hosted_with(vec![probe], Settings::default(), Arc::new(Hiding)).await;
    let created = host
        .engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect("saved")
        .value;
    let stored = host
        .engine
        .peek("note", created.id())
        .await
        .expect("peek")
        .expect("stored");
    assert_eq!(stored.get("title"), &Value::Text("hidden".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_whose_scripts_are_not_loaded_refuses_writes_and_an_off_site_does_not() {
    let host = hosted(vec![]).await;
    host.slot.set(SlotState::Pending);
    let error = host
        .engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect_err("fail closed while pending");
    assert_eq!(error.code, "serverscript.store_failed");
    host.slot.set(SlotState::Off);
    host.engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect("off means no scripts");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn more_scripts_than_the_ceiling_allows_fail_the_event() {
    let scripts: Vec<ServerScript> = (0..3)
        .map(|i| {
            script(
                &format!("s{i}"),
                ScriptEvent::Validate,
                "export function on_validate({ doc }) {}",
            )
        })
        .collect();
    let settings = Settings {
        max_scripts_per_event: 2,
        ..Settings::default()
    };
    let host = hosted_with(scripts, settings, Arc::new(AllowAllView)).await;
    let error = host
        .engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect_err("three scripts exceed a ceiling of two");
    assert_eq!(error.code, "serverscript.limit_exceeded");
}

// -------------------------------------------------------------------------------------------
// The Server Script DocType

fn script_doc(name: &str, doctype: &str, source: &str) -> Document {
    let mut doc = Document::new(n("server_script"), RecordId::new_v7());
    doc.set(n("name"), Value::Text(name.to_owned()));
    doc.set(n("enabled"), Value::Bool(true));
    doc.set(n("doctype"), Value::Text(doctype.to_owned()));
    doc.set(n("event"), Value::Text("validate".to_owned()));
    doc.set(n("script"), Value::Text(source.to_owned()));
    doc
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_that_does_not_compile_cannot_be_saved_and_reports_line_and_column() {
    let host = hosted(vec![]).await;
    let error = host
        .engine
        .create(
            &inv(),
            script_doc(
                "broken",
                "note",
                "export function on_validate({ doc }) {\n  let = ;\n}",
            ),
        )
        .await
        .expect_err("a source that does not compile is refused");
    assert_eq!(error.class, ErrorClass::Validation);
    let first = &error.fields[0];
    assert_eq!(first.field.as_str(), "script");
    assert!(first.message.contains("line 2"), "{}", first.message);
    assert!(first.message.contains("column"), "{}", first.message);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_for_a_reserved_doctype_cannot_be_saved() {
    let host = hosted(vec![]).await;
    let error = host
        .engine
        .create(
            &inv(),
            script_doc(
                "sneaky",
                "session",
                "export function on_validate({ doc }) {}",
            ),
        )
        .await
        .expect_err("reserved");
    assert_eq!(error.fields[0].code, "serverscript.record_invalid");
    assert!(!is_scriptable("server_script"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_valid_script_saves_with_its_compiled_hash() {
    let host = hosted(vec![]).await;
    let created = host
        .engine
        .create(
            &inv(),
            script_doc("ok", "note", "export function on_validate({ doc }) {}"),
        )
        .await
        .expect("saved")
        .value;
    let stored = host
        .engine
        .peek("server_script", created.id())
        .await
        .expect("peek")
        .expect("stored");
    match stored.get("compiled_hash") {
        Value::Text(hash) => assert!(!hash.is_empty()),
        other => panic!("the hash is {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// The slot and the module source

#[test]
fn a_site_has_one_slot() {
    let site = unique_site();
    assert!(Arc::ptr_eq(&slot(&site), &slot(&site)));
    assert!(!Arc::ptr_eq(&slot(&site), &slot(&unique_site())));
}

fn temp_app(files: &[(&str, &str)]) -> std::path::PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "spot-scripts-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    for (path, text) in files {
        let file = dir.join(path);
        std::fs::create_dir_all(file.parent().expect("a parent")).expect("a directory");
        std::fs::write(file, text).expect("a file");
    }
    dir
}

#[test]
fn modules_are_read_from_the_app_directory_only() {
    let dir = temp_app(&[("controllers/note.ts", "export const a = 1;")]);
    let modules = DirModules::new(dir.clone());
    assert_eq!(
        modules.read("app", "controllers/note.ts").expect("read"),
        "export const a = 1;"
    );
    for bad in [
        "../note.ts",
        "/etc/hosts.ts",
        "controllers/missing.ts",
        "controllers/note.js",
        "controllers/../note.ts",
    ] {
        let error = modules.read("app", bad).expect_err("refused");
        assert_eq!(error.code, "tscontroller.module_missing", "{bad}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(unix)]
#[test]
fn a_symlinked_module_is_refused() {
    let dir = temp_app(&[("controllers/real.ts", "export const a = 1;")]);
    std::os::unix::fs::symlink(
        dir.join("controllers/real.ts"),
        dir.join("controllers/link.ts"),
    )
    .expect("a symlink");
    let error = DirModules::new(dir.clone())
        .read("app", "controllers/link.ts")
        .expect_err("a symlink is never followed");
    assert_eq!(error.code, "tscontroller.module_missing");
    let _ = std::fs::remove_dir_all(dir);
}

const TS_MODULE: &str = r#"
export function contract() {
  return {
    doctype: "note",
    steps: { derive_title: { point: "before_save" } },
    methods: { ping: { right: "read", args: {} } },
  };
}
export function run_step({ step, doc }) { return { patch: { title: "from-ts" } }; }
export function run_method() { return { pong: true }; }
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_typescript_controller_loads_from_its_directory_and_runs_as_a_lifecycle_step() {
    let dir = temp_app(&[("controllers/note.ts", TS_MODULE)]);
    let site = unique_site();
    let slot = slot(&site);
    slot.set(SlotState::Off);
    let factory = typescript_factory(&slot, "notes", dir.clone(), Settings::default().ts_limits());
    let controller = factory
        .create(&ControllerRef::parse("ts:controllers/note.ts").expect("a reference"))
        .expect("the module loads");
    assert_eq!(controller.doctype(), "note");
    assert_eq!(controller.steps().len(), 1);
    assert_eq!(controller.methods().len(), 1);

    let mut registry = ControllerRegistry::new();
    registry.register(
        "notes",
        controller,
        spot_controllers::ControllerSource::TypeScript,
    );
    let seams = registry.install().expect("no step conflicts");
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("prepared");
    let created = engine
        .create(&inv(), note("x", "1.00"))
        .await
        .expect("saved")
        .value;
    let stored = engine
        .peek("note", created.id())
        .await
        .expect("peek")
        .expect("stored");
    assert_eq!(stored.get("title"), &Value::Text("from-ts".to_owned()));
    let _ = std::fs::remove_dir_all(dir);
}
