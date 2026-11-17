//! The site's controller installation with Server Scripts and TypeScript controllers wired in
//! (WP-EXT-06.T5): `register_apps` / `register_scripts` as `controllers::build` calls them, over
//! the real document engine on the in-memory backend. No database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use spot_app_api::context::DocumentService as _;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, AppError, AppResult, BoxFuture, Document, ErrorClass, Invocation};
use spot_cli::appdir::AppSource;
use spot_cli::services::controllers::{
    install_strict, register_apps, register_scripts, scripts_settings_from,
};
use spot_cli::services::scripts::{Runtime, Settings, SlotState, slot};
use spot_controllers::{ControllerRegistry, DefaultController, NativeControllers};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_server_scripts::{
    AllowAllView, FakeScriptStore, MethodCaller, ScriptEvent, ScriptStore, ServerScript,
};
use spot_storage_fake::FakeBackend;

// ---------------------------------------------------------------------------------------------
// Fixtures

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

fn unique_site() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "controllers-scripts-{}",
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields.push(FieldSpec::new("title", FieldType::Text));
    let mut total = FieldSpec::new("total", FieldType::Decimal);
    total.scale = Some(2);
    note.fields.push(total);
    let mut script = Definition::new("server_script");
    script.fields.push({
        let mut spec = FieldSpec::new("name", FieldType::Text);
        spec.unique = true;
        spec
    });
    script
        .fields
        .push(FieldSpec::new("enabled", FieldType::Bool));
    script
        .fields
        .push(FieldSpec::new("doctype", FieldType::Text));
    script.fields.push({
        let mut spec = FieldSpec::new("event", FieldType::Select);
        spec.options = Some("validate\nbefore_save".to_owned());
        spec
    });
    script
        .fields
        .push(FieldSpec::new("script", FieldType::LongText));
    script
        .fields
        .push(FieldSpec::new("compiled_hash", FieldType::Text));
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
    script
        .fields
        .push(FieldSpec::new("disabled_reason", FieldType::Text));
    Registry::new(vec![note, script])
        .expect("the definitions compile")
        .effective()
}

fn names(meta: &spot_meta::EffectiveMeta) -> Vec<String> {
    meta.doctypes()
        .map(|v| v.name.as_str().to_owned())
        .collect()
}

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("scripts.test").expect("a site id"),
        actor: Actor::System {
            component: n("controllers_scripts_tests"),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    }
}

fn note(title: &str, total: &str) -> Document {
    let mut draft = Document::new(n("note"), RecordId::new_v7());
    draft.set(n("title"), Value::Text(title.to_owned()));
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

fn script(name: &str, source: &str) -> ServerScript {
    ServerScript {
        record_id: None,
        name: name.to_owned(),
        doctype: "note".to_owned(),
        event: ScriptEvent::Validate,
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

fn temp_app(files: &[(&str, &str)]) -> std::path::PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "spot-controllers-scripts-{}-{}",
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

fn app_toml(name: &str, controller: &str) -> String {
    format!(
        "[app]\nname = \"{name}\"\nversion = \"1.0.0\"\ntitle = \"{name}\"\ncontroller = \"{controller}\"\n"
    )
}

fn source(name: &str, dir: &std::path::Path) -> AppSource {
    AppSource {
        name: name.to_owned(),
        version: "1.0.0".to_owned(),
        dir: dir.to_path_buf(),
        definitions: Vec::new(),
    }
}

async fn engine_of(registry: &ControllerRegistry) -> Engine<FakeBackend> {
    let seams = install_strict(&registry.set()).expect("the set installs into the strict registry");
    let engine = Engine::new(
        FakeBackend::new(),
        meta(),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("the store prepares");
    engine
}

const TS_MODULE: &str = r#"
export function contract() {
  return { doctype: "note", steps: { derive_title: { point: "before_save" } }, methods: {} };
}
export function run_step({ step, doc }) { return { patch: { title: "from-ts" } }; }
"#;

fn native_table() -> NativeControllers {
    NativeControllers::new().with("notes", Arc::new(DefaultController::new("note")))
}

// ---------------------------------------------------------------------------------------------
// Server Script hubs

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_script_refuses_the_write_after_the_build_and_accepts_it_once_edited_with_no_restart() {
    let site = unique_site();
    let settings = Settings::default();
    let slot = slot(&site);
    let store = Arc::new(FakeScriptStore::with_scripts(vec![script(
        "no_negative",
        REJECT_NEGATIVE,
    )]));
    let runtime = Arc::new(Runtime::new(
        settings.clone(),
        slot.script_engine().clone(),
        Arc::clone(&store) as Arc<dyn ScriptStore>,
        Arc::new(AllowAllView),
        Arc::new(NoCaller),
    ));
    slot.set(SlotState::Ready(Arc::clone(&runtime)));

    let meta = meta();
    let mut registry = ControllerRegistry::new();
    register_scripts(
        &mut registry,
        &site,
        &settings,
        names(&meta).iter().map(String::as_str),
    );
    let engine = engine_of(&registry).await;
    let error = engine
        .create(&inv(), note("bad", "-5.00"))
        .await
        .expect_err("the script refuses the write");
    assert_eq!(error.class, ErrorClass::Validation);
    assert_eq!(error.fields[0].code, "test.total_negative");

    // The script is edited (here: removed); the admin controller does the same on commit.
    store.put(ServerScript {
        enabled: false,
        ..script("no_negative", REJECT_NEGATIVE)
    });
    runtime.invalidate();
    engine
        .create(&inv(), note("fine", "-5.00"))
        .await
        .expect("the edited script no longer refuses, with no rebuild");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reserved_doctype_gets_no_hub_and_server_script_gets_its_admin_controller() {
    let site = unique_site();
    let mut registry = ControllerRegistry::new();
    register_scripts(
        &mut registry,
        &site,
        &Settings::default(),
        ["note", "role", "server_script", "user"],
    );
    let set = registry.set();
    let apps = |doctype: &str| -> Vec<String> {
        set.controllers_for(doctype)
            .iter()
            .map(|r| r.app.clone())
            .collect()
    };
    assert!(apps("note").contains(&"server_script".to_owned()));
    assert!(apps("server_script").contains(&"server_script".to_owned()));
    // `role` and `user` are reserved: only the framework default answers for them.
    assert!(!apps("role").contains(&"server_script".to_owned()));
    assert!(!apps("user").contains(&"server_script".to_owned()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_scripts_register_no_hub_and_no_write_is_refused_as_pending() {
    let site = unique_site();
    let settings = Settings {
        enabled: false,
        ..Settings::default()
    };
    let mut registry = ControllerRegistry::new();
    register_scripts(&mut registry, &site, &settings, ["note", "server_script"]);
    let set = registry.set();
    assert!(
        set.controllers_for("note")
            .iter()
            .all(|r| r.app != "server_script")
    );
    // The slot was never filled: it is Pending. With no hub nobody asks it.
    let engine = engine_of(&registry).await;
    engine
        .create(&inv(), note("allowed", "1.00"))
        .await
        .expect("no hub, no refusal");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enabled_scripts_refuse_a_write_only_while_the_slot_is_pending() {
    let site = unique_site();
    let settings = Settings::default();
    let mut registry = ControllerRegistry::new();
    register_scripts(&mut registry, &site, &settings, ["note"]);
    let engine = engine_of(&registry).await;
    let error = engine
        .create(&inv(), note("early", "1.00"))
        .await
        .expect_err("the scripts module has not loaded yet");
    assert_eq!(error.code, "serverscript.store_failed");
    slot(&site).set(SlotState::Off);
    engine
        .create(&inv(), note("later", "1.00"))
        .await
        .expect("once the slot is filled the write goes through");
}

// ---------------------------------------------------------------------------------------------
// App controllers

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typescript_app_controller_runs_as_a_lifecycle_step() {
    let dir = temp_app(&[
        ("app.toml", &app_toml("notes_ts", "ts:controllers/note.ts")),
        ("controllers/note.ts", TS_MODULE),
    ]);
    let site = unique_site();
    slot(&site).set(SlotState::Off);
    let mut registry = ControllerRegistry::new();
    let errors = register_apps(
        &mut registry,
        &site,
        &[source("notes_ts", &dir)],
        &NativeControllers::new(),
        &Settings::default(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    let engine = engine_of(&registry).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_app_is_unaffected_and_never_given_the_typescript_factory() {
    let dir = temp_app(&[("app.toml", &app_toml("notes", "native:notes"))]);
    let site = unique_site();
    let mut registry = ControllerRegistry::new();
    let errors = register_apps(
        &mut registry,
        &site,
        &[source("notes", &dir)],
        &native_table(),
        &Settings::default(),
    );
    assert!(errors.is_empty(), "{errors:?}");
    let set = registry.set();
    assert!(set.controllers_for("note").iter().any(|r| r.app == "notes"));

    // The same reference with no such native controller is the native table's refusal.
    let mut empty = ControllerRegistry::new();
    let errors = register_apps(
        &mut empty,
        &site,
        &[source("notes", &dir)],
        &NativeControllers::new(),
        &Settings::default(),
    );
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("controller.unknown_native"),
        "{errors:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typescript_reference_is_not_looked_up_in_the_native_table() {
    // The module is missing: the TypeScript factory says so; the native table's own refusal
    // (`controller.host_unavailable` for `ts:`) never appears.
    let dir = temp_app(&[(
        "app.toml",
        &app_toml("notes_ts", "ts:controllers/missing.ts"),
    )]);
    let site = unique_site();
    let mut registry = ControllerRegistry::new();
    let errors = register_apps(
        &mut registry,
        &site,
        &[source("notes_ts", &dir)],
        &native_table(),
        &Settings::default(),
    );
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("tscontroller.module_missing"),
        "{errors:?}"
    );
    assert!(!errors[0].contains("host_unavailable"));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_problem_of_the_apps_is_reported_at_once() {
    let ts = temp_app(&[("app.toml", &app_toml("one", "ts:controllers/missing.ts"))]);
    let native = temp_app(&[("app.toml", &app_toml("two", "native:ghost"))]);
    let broken = temp_app(&[("app.toml", "this is not toml [")]);
    let ok = temp_app(&[("app.toml", &app_toml("notes", "native:notes"))]);
    let site = unique_site();
    let mut registry = ControllerRegistry::new();
    let errors = register_apps(
        &mut registry,
        &site,
        &[
            source("one", &ts),
            source("two", &native),
            source("broken", &broken),
            source("notes", &ok),
        ],
        &native_table(),
        &Settings::default(),
    );
    assert_eq!(errors.len(), 3, "{errors:?}");
    assert!(errors[0].starts_with("app `one`:"));
    assert!(errors[1].starts_with("app `two`:"));
    // The healthy app still registered.
    assert!(
        registry
            .set()
            .controllers_for("note")
            .iter()
            .any(|r| r.app == "notes")
    );
    for dir in [ts, native, broken, ok] {
        let _ = std::fs::remove_dir_all(dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_app_without_a_controller_reference_is_skipped() {
    let dir = temp_app(&[(
        "app.toml",
        "[app]\nname = \"plain\"\nversion = \"1.0.0\"\ntitle = \"plain\"\n",
    )]);
    let mut registry = ControllerRegistry::new();
    let errors = register_apps(
        &mut registry,
        &unique_site(),
        &[source("plain", &dir)],
        &NativeControllers::new(),
        &Settings::default(),
    );
    assert!(errors.is_empty());
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------------------------
// Settings

#[test]
fn the_scripts_settings_of_the_host_configuration_default_decode_and_refuse() {
    fn with(table: Option<&str>) -> spot_cli::Result<Settings> {
        let value = table.map(|text| text.parse::<toml::Value>().unwrap());
        scripts_settings_from(value.as_ref())
    }
    assert_eq!(with(None).unwrap(), Settings::default());
    assert!(!with(Some("enabled = false")).unwrap().enabled);
    let unknown = with(Some("surprise = 1")).unwrap_err();
    assert!(unknown.0.contains("services.scripts"), "{}", unknown.0);
    let wrong = with(Some("refresh_secs = \"secret-looking\"")).unwrap_err();
    assert!(!wrong.0.contains("secret-looking"));
    let range = with(Some("max_timeout_ms = 0")).unwrap_err();
    assert!(range.0.contains("max_timeout_ms"), "{}", range.0);
}
