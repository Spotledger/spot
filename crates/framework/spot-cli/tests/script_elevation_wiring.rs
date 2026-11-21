//! Script elevation wired into the host (WP-EXT-06.T10): the permission name the grammar accepts,
//! the reserved script DocTypes, the admin controllers that carry the elevation gate, the gate
//! through the real document engine on the in-memory backend, a runtime that elevates a flagged
//! script and audits it (and one that does not), and `host.query` through `Engine::query_as`.
//! The library's own rules are `spot-server-scripts/tests/elevation.rs`; the over-PostgreSQL cases
//! are the l3-http `scripts` cases. This file needs no database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, AppError, AppResult, BoxFuture, Document, ErrorClass, Invocation};
use spot_cli::permissions::AppPerms;
use spot_cli::services::script_kinds::{EngineQueries, NoQueries};
use spot_cli::services::scripts::elevation::{
    ChainElevationAudit, ElevationEntry, ElevationLog, PermissionCheck,
};
use spot_cli::services::scripts::{
    RESERVED, Runtime, Settings, SlotState, is_scriptable, register_admin, register_controllers,
    slot,
};
use spot_controllers::ControllerRegistry;
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_server_scripts::elevation::{ElevationFlag, ElevationWiring, PERMISSION_ELEVATE};
use spot_server_scripts::{
    AllowAllView, FakeScriptStore, MethodCaller, ScriptEvent, ScriptStore, ServerScript,
    ServiceAccessFactory,
};
use spot_storage_contract::{Expr, Query, QueryPath, Selection};
use spot_storage_fake::FakeBackend;

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

fn unique_site() -> String {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!("elevation-wiring-{}", NEXT.fetch_add(1, Ordering::SeqCst))
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields.push(text("title"));

    let mut script = Definition::new("server_script");
    script.fields.push({
        let mut spec = text("name");
        spec.unique = true;
        spec
    });
    script
        .fields
        .push(FieldSpec::new("enabled", FieldType::Bool));
    script.fields.push(text("doctype"));
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
    script.fields.push(text("compiled_hash"));
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
    script.fields.push(text("disabled_reason"));
    script
        .fields
        .push(FieldSpec::new("elevated", FieldType::Bool));
    script
        .fields
        .push(FieldSpec::new("elevation_reason", FieldType::LongText));
    script.fields.push(text("elevated_by"));
    script.fields.push(text("elevated_at"));

    Registry::new(vec![note, script])
        .expect("the definitions compile")
        .effective()
}

fn system_inv() -> Invocation {
    Invocation {
        site: SiteId::new("elevation.test").expect("a site id"),
        actor: Actor::System {
            component: n("elevation_tests"),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::UNIX_EPOCH,
    }
}

fn user_inv(id: RecordId) -> Invocation {
    Invocation {
        actor: Actor::User { id, via: None },
        ..system_inv()
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

/// Holds the permission for exactly the users it was given.
struct Holders(BTreeSet<RecordId>);

impl PermissionCheck for Holders {
    fn holds<'a>(&'a self, inv: &'a Invocation, permission: &'a str) -> BoxFuture<'a, bool> {
        let held = permission == PERMISSION_ELEVATE
            && matches!(&inv.actor, Actor::User { id, .. } if self.0.contains(id));
        Box::pin(async move { held })
    }
}

#[derive(Default)]
struct MemoryLog {
    entries: Mutex<Vec<ElevationEntry>>,
}

impl ElevationLog for MemoryLog {
    fn append<'a>(&'a self, entry: ElevationEntry) -> BoxFuture<'a, AppResult<()>> {
        self.entries.lock().unwrap().push(entry);
        Box::pin(async { Ok(()) })
    }
}

fn has_code(error: &AppError, code: &str) -> bool {
    error.code == code || error.fields.iter().any(|f| f.code == code)
}

// -------------------------------------------------------------------------------------------
// Names and reserved DocTypes

#[test]
fn the_elevate_permission_is_a_name_the_grammar_accepts_and_the_scripting_app_declares_it() {
    assert!(
        spot_permissions::AppPermissionName::parse(PERMISSION_ELEVATE).is_ok(),
        "a dotted name would be refused when the app installs"
    );
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/scripting");
    let manifest = spot_cli::appdir::manifest(&dir).expect("the scripting manifest parses");
    let mut perms = AppPerms::default();
    let mut declared = 0;
    for (name, decl) in manifest.app_permissions() {
        spot_cli::services::controllers::declare_app_permission(
            &mut perms.catalogue,
            "wiring.test",
            name.as_str(),
            spot_permissions::AppPermissionDecl {
                app: decl.app,
                title: decl.title,
                description: decl.description,
            },
        )
        .expect("the declaration is accepted");
        if name.as_str() == PERMISSION_ELEVATE {
            declared += 1;
        }
    }
    assert_eq!(declared, 1, "the scripting app declares it once");
    let grants =
        std::fs::read_to_string(dir.join(spot_cli::services::controllers::GRANTS_FILE)).unwrap();
    assert!(
        spot_permissions::load_app_grants(&grants).is_ok(),
        "the grants file loads"
    );
    assert!(grants.contains(PERMISSION_ELEVATE));
    // The platform declares none of the script permissions: they belong to the scripting app.
    assert!(
        spot_cli::services::controllers::platform_permissions()
            .iter()
            .all(|(name, _)| name.as_str() != PERMISSION_ELEVATE)
    );
}

#[test]
fn the_three_script_doctypes_are_reserved() {
    for doctype in ["server_script", "api_script", "scheduled_script"] {
        assert!(RESERVED.contains(&doctype), "{doctype} is reserved");
        assert!(!is_scriptable(doctype));
    }
    assert!(is_scriptable("invoice"));
}

// -------------------------------------------------------------------------------------------
// The admin controllers carry the gate

#[test]
fn the_admin_controllers_of_all_three_script_doctypes_carry_the_elevation_step() {
    let site = unique_site();
    let mut registry = ControllerRegistry::new();
    register_admin(&mut registry, &site);
    spot_cli::services::script_kinds::register_controllers(&mut registry, &site);
    let set = registry.set();
    for doctype in ["server_script", "api_script", "scheduled_script"] {
        let controllers = set.controllers_for(doctype);
        assert_eq!(controllers.len(), 1, "one controller for {doctype}");
        let wanted = format!("{doctype}:elevation");
        let steps = controllers[0].controller.steps();
        let gate = steps
            .iter()
            .find(|s| s.name.as_str() == wanted)
            .unwrap_or_else(|| panic!("{doctype} has no `{wanted}` step"));
        let compile = steps
            .iter()
            .filter(|s| s.name.as_str() != wanted)
            .map(|s| s.priority)
            .min()
            .unwrap_or(0);
        assert!(
            gate.priority < compile.min(0),
            "the gate of {doctype} runs before every other save step"
        );
    }
    // The registry installs: no step of the three controllers collides with another.
    registry.install().expect("no step conflicts");
}

// -------------------------------------------------------------------------------------------
// The gate through the engine

struct GateHost {
    engine: Engine<FakeBackend>,
}

async fn gate_host(holders: BTreeSet<RecordId>) -> GateHost {
    let site = unique_site();
    let slot = slot(&site);
    let store = Arc::new(FakeScriptStore::new());
    let runtime = Runtime::new(
        Settings::default(),
        slot.script_engine().clone(),
        store as Arc<dyn ScriptStore>,
        Arc::new(AllowAllView),
        Arc::new(NoCaller),
    )
    .with_permissions(Arc::new(Holders(holders)));
    slot.set(SlotState::Ready(Arc::new(runtime)));
    let mut registry = ControllerRegistry::new();
    register_admin(&mut registry, &site);
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
    GateHost { engine }
}

fn script_doc(name: &str, elevated: bool) -> Document {
    let mut doc = Document::new(n("server_script"), RecordId::new_v7());
    doc.set(n("name"), Value::Text(name.to_owned()));
    doc.set(n("enabled"), Value::Bool(true));
    doc.set(n("doctype"), Value::Text("note".to_owned()));
    doc.set(n("event"), Value::Text("validate".to_owned()));
    doc.set(
        n("script"),
        Value::Text("export function on_validate({ doc }) {}".to_owned()),
    );
    if elevated {
        doc.set(n("elevated"), Value::Bool(true));
        doc.set(
            n("elevation_reason"),
            Value::Text("Reads the tariff table on behalf of every caller".to_owned()),
        );
    }
    doc
}

#[tokio::test]
async fn the_flag_set_by_a_holder_is_saved_and_stamped_and_by_anyone_else_is_refused() {
    let holder = RecordId::new_v7();
    let other = RecordId::new_v7();
    let host = gate_host(BTreeSet::from([holder])).await;

    let refused = host
        .engine
        .create(&user_inv(other), script_doc("tariffs", true))
        .await
        .expect_err("a user without the permission cannot elevate");
    assert!(
        has_code(&refused, spot_server_scripts::ELEVATION_FORBIDDEN),
        "{refused:?}"
    );

    let saved = host
        .engine
        .create(&user_inv(holder), script_doc("tariffs", true))
        .await
        .expect("a holder may elevate")
        .value;
    let stored = host
        .engine
        .peek("server_script", saved.id())
        .await
        .expect("peek")
        .expect("stored");
    assert_eq!(stored.get("elevated"), &Value::Bool(true));
    match stored.get("elevated_by") {
        Value::Text(by) => assert!(!by.is_empty(), "the stamp names who"),
        other => panic!("the stamp is {other:?}"),
    }

    // An unflagged script saves for anyone: the gate only guards the flag.
    host.engine
        .create(&user_inv(other), script_doc("plain", false))
        .await
        .expect("an unflagged script saves");
}

// -------------------------------------------------------------------------------------------
// A runtime that elevates, and one that does not

struct Elevating {
    engine: Arc<Engine<FakeBackend>>,
    log: Arc<MemoryLog>,
}

/// A site whose `note` has one script, flagged elevated when `flagged`, and a runtime wired for
/// elevation when `wired`.
async fn elevating(flagged: bool, wired: bool) -> Elevating {
    let site = unique_site();
    let slot = slot(&site);
    let settings = Settings::default();
    let script = ServerScript {
        record_id: None,
        name: "tariffs".to_owned(),
        doctype: "note".to_owned(),
        event: ScriptEvent::Validate,
        source: "export function on_validate({ doc }) {}".to_owned(),
        compiled_hash: None,
        timeout_ms: 200,
        priority: 0,
        enabled: true,
        disabled_reason: None,
    };
    let store = Arc::new(FakeScriptStore::with_scripts([script]));
    if flagged {
        store.set_elevation(
            "tariffs",
            Some(ElevationFlag {
                reason: "Reads the tariff table on behalf of every caller".to_owned(),
                by: Some("an administrator".to_owned()),
                at: None,
            }),
        );
    }
    let mut registry = ControllerRegistry::new();
    register_controllers(&mut registry, &site, &settings, ["note", "server_script"]);
    let seams = registry.install().expect("no step conflicts");
    let engine = Arc::new(
        Engine::new(
            FakeBackend::new(),
            meta(),
            seams,
            Arc::new(SystemClock::new()),
            Arc::new(V7Ids),
        )
        .expect("the engine builds"),
    );
    engine.prepare().await.expect("the store prepares");
    let log = Arc::new(MemoryLog::default());
    let mut runtime = Runtime::new(
        settings,
        slot.script_engine().clone(),
        store as Arc<dyn ScriptStore>,
        Arc::new(AllowAllView),
        Arc::new(NoCaller),
    );
    if wired {
        runtime = runtime.with_elevation(ElevationWiring {
            factory: Arc::new(ServiceAccessFactory::new(
                Arc::clone(&engine) as Arc<dyn DocumentService>,
                Arc::new(NoQueries),
            )),
            audit: Arc::new(ChainElevationAudit::new(
                Arc::clone(&log) as Arc<dyn ElevationLog>
            )),
        });
    }
    slot.set(SlotState::Ready(Arc::new(runtime)));
    Elevating { engine, log }
}

fn note(title: &str) -> Document {
    let mut doc = Document::new(n("note"), RecordId::new_v7());
    doc.set(n("title"), Value::Text(title.to_owned()));
    doc
}

fn stages(log: &MemoryLog) -> Vec<&'static str> {
    log.entries
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.stage)
        .collect()
}

#[tokio::test]
async fn a_flagged_script_on_a_wired_runtime_runs_elevated_and_is_audited_once() {
    let host = elevating(true, true).await;
    let user = RecordId::new_v7();
    host.engine
        .create(&user_inv(user), note("first"))
        .await
        .expect("the script runs and the note saves");
    assert_eq!(stages(&host.log), ["reserved", "finished"]);
    let entries = host.log.entries.lock().unwrap().clone();
    assert_eq!(entries[0].run.script, "tariffs");
    assert_eq!(entries[1].run.outcome, "ok");
    assert_eq!(
        entries[1].run.triggering_user,
        user.to_string(),
        "the audit names the user whose action triggered the run"
    );
}

#[tokio::test]
async fn an_unflagged_script_is_never_elevated_and_writes_no_audit() {
    let host = elevating(false, true).await;
    host.engine
        .create(&user_inv(RecordId::new_v7()), note("first"))
        .await
        .expect("the script runs as its user");
    assert!(stages(&host.log).is_empty());
}

#[tokio::test]
async fn a_flagged_script_on_a_runtime_that_was_not_wired_runs_as_its_user() {
    let host = elevating(true, false).await;
    host.engine
        .create(&user_inv(RecordId::new_v7()), note("first"))
        .await
        .expect("default deny: the script still runs, as its user");
    assert!(stages(&host.log).is_empty());
}

// -------------------------------------------------------------------------------------------
// host.query

#[tokio::test]
async fn host_query_goes_through_the_engine_query_as() {
    let site = unique_site();
    let mut registry = ControllerRegistry::new();
    register_admin(&mut registry, &site);
    let seams = registry.install().expect("no step conflicts");
    let engine = Arc::new(
        Engine::new(
            FakeBackend::new(),
            meta(),
            seams,
            Arc::new(SystemClock::new()),
            Arc::new(V7Ids),
        )
        .expect("the engine builds"),
    );
    engine.prepare().await.expect("the store prepares");
    for title in ["a", "b"] {
        engine
            .create(&system_inv(), note(title))
            .await
            .expect("a note saves");
    }
    let mut query = Query::from(&n("note"), &n("n"), QueryPath::Transactional);
    query.select.push(Selection {
        expr: Expr::field(&n("n"), &n("title")),
        alias: n("title"),
    });

    use spot_server_scripts::ScriptQueries;
    let present = {
        let engine = Arc::clone(&engine);
        EngineQueries::new(Arc::new(move || Some(Arc::clone(&engine))))
    };
    let rows = present
        .query(&system_inv(), &query)
        .await
        .expect("the platform reads the notes");
    assert_eq!(rows.rows.len(), 2);

    let absent: EngineQueries<FakeBackend> = EngineQueries::new(Arc::new(|| None));
    let refused = absent
        .query(&system_inv(), &query)
        .await
        .expect_err("no engine, no query");
    assert_eq!(refused.code, spot_server_scripts::STORE_FAILED);
}
