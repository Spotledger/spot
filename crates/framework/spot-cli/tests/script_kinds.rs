//! API scripts and scheduled scripts, hosted (WP-EXT-06.T7): the admin controllers, the documents
//! reader, the runtime and the two seams, driven through the real document engine on the
//! in-memory backend. The same cases over PostgreSQL and HTTP are the l3-http `script_kinds`
//! case; this file needs no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use serde_json::json;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, Document, ErrorClass, Invocation};
use spot_cli::services::function::{FunctionDeclSource, FunctionSources};
use spot_cli::services::jobs::{DynamicSchedule, ScheduleSource, ScheduleSources};
use spot_cli::services::script_kinds::{
    EngineAccessFactory, KindDocs, KindSource, KindsHandle, KindsRuntime, Settings,
    register_controllers,
};
use spot_controllers::ControllerRegistry;
use spot_core::{Clock, SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_functions::{FunctionDecl, FunctionHandler, FunctionKey, HttpVerb};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_server_scripts::ScriptLimits;
use spot_storage_contract::Collation;
use spot_storage_fake::FakeBackend;

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

fn unique_site() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("script-kinds-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn common(def: &mut Definition) {
    def.fields.push({
        let mut spec = text("name");
        spec.unique = true;
        spec
    });
    def.fields.push(FieldSpec::new("enabled", FieldType::Bool));
    def.fields
        .push(FieldSpec::new("script", FieldType::LongText));
    def.fields
        .push(FieldSpec::new("timeout_ms", FieldType::Int));
    def.fields.push(text("compiled_hash"));
    def.fields
        .push(FieldSpec::new("last_error", FieldType::LongText));
    def.fields.push(text("disabled_reason"));
}

fn select(name: &str, options: &str) -> FieldSpec {
    let mut spec = FieldSpec::new(name, FieldType::Select);
    spec.options = Some(options.to_owned());
    spec
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut api = Definition::new("api_script");
    common(&mut api);
    api.fields.push(text("module"));
    api.fields.push(text("function"));
    api.fields.push(select("verbs", "GET\nPOST\nGET and POST"));
    api.fields
        .push(FieldSpec::new("arguments_schema", FieldType::LongText));

    let mut scheduled = Definition::new("scheduled_script");
    common(&mut scheduled);
    scheduled.fields.push(text("cron"));
    scheduled.fields.push(text("time_zone"));
    scheduled
        .fields
        .push(select("missed_run", "skip\nrun_once"));
    scheduled
        .fields
        .push(select("queue", "default\nshort\nlong"));
    scheduled.fields.push(text("run_as"));
    scheduled
        .fields
        .push(FieldSpec::new("last_run_count", FieldType::Int));

    let mut user = Definition::new("user");
    user.fields.push(text("user_name"));
    user.fields.push(text("state"));

    Registry::new(vec![api, scheduled, user])
        .expect("the definitions compile")
        .effective()
}

fn system() -> Invocation {
    Invocation {
        site: SiteId::new("script-kinds-test").expect("a site id"),
        actor: Actor::System {
            component: n("script_kinds_tests"),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    }
}

struct Host {
    engine: Arc<Engine<FakeBackend>>,
    meta: Arc<spot_meta::EffectiveMeta>,
    site: String,
}

async fn host() -> Host {
    let site = unique_site();
    let meta = meta();
    let mut registry = ControllerRegistry::new();
    register_controllers(&mut registry, &site);
    let seams = registry.install().expect("no step conflicts");
    let engine = Engine::new(
        FakeBackend::new(),
        Arc::clone(&meta),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("the store prepares");
    Host {
        engine: Arc::new(engine),
        meta,
        site,
    }
}

fn runtime(host: &Host, settings: Settings) -> KindsRuntime {
    let engine = Arc::clone(&host.engine);
    let for_docs = Arc::clone(&engine);
    let catalog = Arc::new(
        host.meta
            .catalog(Collation::new("und").expect("a collation"))
            .expect("the catalog compiles"),
    );
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let source: Arc<dyn KindSource> = Arc::new(KindDocs::new(
        SiteId::new("script-kinds-test").expect("a site id"),
        Arc::new(move || Some(Arc::clone(&for_docs))),
        Arc::clone(&clock),
        catalog,
    ));
    KindsRuntime::new(
        SiteId::new("script-kinds-test").expect("a site id"),
        settings,
        ScriptLimits::default(),
        source,
        Arc::new(EngineAccessFactory::new(Arc::new(move || {
            Some(Arc::clone(&engine))
        }))),
        clock,
        spot_cli::services::scripts::slot(&host.site)
            .script_engine()
            .clone(),
    )
}

async fn create_api(host: &Host, name: &str, module: &str, function: &str) -> Document {
    let mut doc = Document::new(n("api_script"), RecordId::new_v7());
    for (field, value) in [
        ("name", Value::Text(name.to_owned())),
        ("enabled", Value::Bool(true)),
        ("module", Value::Text(module.to_owned())),
        ("function", Value::Text(function.to_owned())),
        ("verbs", Value::Text("POST".to_owned())),
        (
            "script",
            Value::Text("export function on_call({ args }) { return { ok: true }; }".to_owned()),
        ),
        ("timeout_ms", Value::Int(200)),
    ] {
        doc.set(n(field), value);
    }
    host.engine
        .create(&system(), doc)
        .await
        .expect("the script saves")
        .value
}

// -------------------------------------------------------------------------------------------
// The seams, with fake providers

struct FakeFunctions(Vec<(FunctionDecl, Arc<dyn FunctionHandler>)>);

impl FunctionDeclSource for FakeFunctions {
    fn declarations(&self) -> Vec<(FunctionDecl, Arc<dyn FunctionHandler>)> {
        self.0.clone()
    }
}

struct Noop;

impl FunctionHandler for Noop {
    fn call<'a>(
        &'a self,
        _ctx: &'a spot_functions::Context,
        _args: serde_json::Value,
    ) -> spot_functions::HandlerFuture<'a> {
        Box::pin(async { Ok(json!({})) })
    }
}

fn decl(module: &str, name: &str) -> (FunctionDecl, Arc<dyn FunctionHandler>) {
    (
        FunctionDecl {
            key: FunctionKey::from_parts("scripting", module, name),
            verbs: Some(vec![HttpVerb::Post]),
            guest: false,
            limit: None,
            arguments: None,
        },
        Arc::new(Noop),
    )
}

#[test]
fn a_function_source_is_merged_and_a_conflict_between_sources_is_refused() {
    let sources = FunctionSources::new(Arc::new(SystemClock::new()));
    sources
        .set("a", Arc::new(FakeFunctions(vec![decl("m", "f")])))
        .expect("one source is fine");
    // The same id replaces its source: no conflict with itself.
    sources
        .set("a", Arc::new(FakeFunctions(vec![decl("m", "g")])))
        .expect("replacing is fine");
    let error = sources
        .set("b", Arc::new(FakeFunctions(vec![decl("m", "g")])))
        .expect_err("two sources, one function");
    assert!(error.contains("scripting/m/g"), "{error}");
    // The overlay that was serving is unchanged by the refused rebuild.
    sources
        .set("b", Arc::new(FakeFunctions(vec![decl("m", "other")])))
        .expect("a different function is fine");
}

struct FakeSchedules(Vec<DynamicSchedule>);

impl ScheduleSource for FakeSchedules {
    fn schedules(&self) -> Vec<DynamicSchedule> {
        self.0.clone()
    }
}

#[test]
fn schedule_sources_are_listed_in_id_order_and_replaced_by_id() {
    let sources = ScheduleSources::new();
    assert!(sources.current().is_empty());
    sources.set("a", Arc::new(FakeSchedules(Vec::new())));
    sources.set("a", Arc::new(FakeSchedules(Vec::new())));
    assert!(sources.current().is_empty());
}

// -------------------------------------------------------------------------------------------
// The runtime over the engine

#[tokio::test]
async fn a_saved_script_is_loaded_and_a_disabled_one_stops_being_served() {
    let host = host().await;
    let runtime = runtime(&host, Settings::default());
    runtime.refresh().await.expect("loads");
    assert_eq!(runtime.status().api_scripts, 0);

    let saved = create_api(&host, "double", "maths", "double").await;
    assert!(
        matches!(saved.get("compiled_hash"), Value::Text(h) if !h.is_empty()),
        "compile on save stores the hash"
    );
    runtime.refresh().await.expect("loads");
    assert_eq!(runtime.status().api_scripts, 1);
    assert!(runtime.generation() >= 1);

    let mut patch = spot_app_api::DocumentPatch::default();
    patch.fields.insert(n("enabled"), Value::Bool(false));
    host.engine
        .update(
            &system(),
            "api_script",
            saved.id(),
            patch,
            saved.version().unwrap(),
        )
        .await
        .expect("disabled");
    runtime.refresh().await.expect("loads");
    assert_eq!(runtime.status().api_scripts, 0);
}

#[tokio::test]
async fn a_script_that_does_not_compile_cannot_be_saved() {
    let host = host().await;
    let mut doc = Document::new(n("api_script"), RecordId::new_v7());
    for (field, value) in [
        ("name", "broken"),
        ("module", "m"),
        ("function", "f"),
        ("verbs", "POST"),
        ("script", "export function other() {}"),
    ] {
        doc.set(n(field), Value::Text(value.to_owned()));
    }
    doc.set(n("enabled"), Value::Bool(true));
    let error = host
        .engine
        .create(&system(), doc)
        .await
        .expect_err("no on_call export");
    assert_eq!(error.class, ErrorClass::Validation);
    assert!(error.fields.iter().any(|f| f.message.contains("on_call")));
}

#[tokio::test]
async fn two_scripts_on_one_key_are_both_left_out_and_the_status_says_so() {
    let host = host().await;
    create_api(&host, "first", "m", "f").await;
    create_api(&host, "second", "m", "f").await;
    create_api(&host, "other", "m", "g").await;
    let runtime = runtime(&host, Settings::default());
    runtime.refresh().await.expect("loads");
    let status = runtime.status();
    assert_eq!(status.api_scripts, 1);
    let skipped: Vec<&str> = status.skipped.iter().map(|s| s.name.as_str()).collect();
    assert!(skipped.contains(&"first") && skipped.contains(&"second"));
    assert!(
        status
            .skipped
            .iter()
            .all(|s| s.reason.contains("serverscript.function_conflict"))
    );
}

#[tokio::test]
async fn scripts_over_max_scripts_are_skipped_and_reported() {
    let host = host().await;
    create_api(&host, "a", "m", "a").await;
    create_api(&host, "b", "m", "b").await;
    let settings = Settings {
        max_scripts: 1,
        ..Settings::default()
    };
    let runtime = runtime(&host, settings);
    runtime.refresh().await.expect("loads");
    let status = runtime.status();
    assert_eq!(status.api_scripts, 1);
    assert_eq!(status.skipped.len(), 1);
    assert!(status.skipped[0].reason.contains("max_scripts"));
}

#[tokio::test]
async fn a_restart_picks_the_scripts_up_again() {
    let host = host().await;
    create_api(&host, "double", "maths", "double").await;
    let first = runtime(&host, Settings::default());
    first.refresh().await.expect("loads");
    drop(first);
    let second = runtime(&host, Settings::default());
    assert_eq!(
        second.status().api_scripts,
        0,
        "nothing before the first load"
    );
    second.refresh().await.expect("loads");
    assert_eq!(second.status().api_scripts, 1);
}

#[tokio::test]
async fn an_unchanged_set_keeps_its_registries() {
    let host = host().await;
    create_api(&host, "double", "maths", "double").await;
    let runtime = runtime(&host, Settings::default());
    runtime.refresh().await.expect("loads");
    let generation = runtime.generation();
    runtime.refresh().await.expect("loads again");
    assert_eq!(
        runtime.generation(),
        generation,
        "nothing changed, nothing rebuilt"
    );
}

#[tokio::test]
async fn the_handle_reloads_the_running_runtime() {
    let host = host().await;
    let handle = KindsHandle::detached();
    let runtime = Arc::new(runtime(&host, Settings::default()));
    handle.set_ready(Arc::clone(&runtime));
    create_api(&host, "double", "maths", "double").await;
    handle.reload().await;
    assert_eq!(handle.status().api_scripts, 1);
}
