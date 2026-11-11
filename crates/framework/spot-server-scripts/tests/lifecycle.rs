//! End-to-end: the document engine runs Server Scripts as ordinary lifecycle steps, on the
//! embedded TypeScript engine, through the real `HookContext` and the host bridge.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{Actor, AppResult, BoxFuture, Document, ErrorClass, Invocation};
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{
    AllowAllAuthorizer, Authorizer, Engine, QueryPlan, V7Ids, WriteChange,
};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::{Action, Decision, DocFacts, Principal};
use spot_server_scripts::{
    FakeScriptStore, FieldView, ScriptController, ScriptEvent, ScriptLimits, ScriptLog,
    ServerScript, ServerScriptController, host_surface,
};
use spot_storage_contract::Query;
use spot_storage_fake::FakeBackend;
use spot_ts::{EngineConfig, ScriptEngine};

// -------------------------------------------------------------------------------------------
// Fixtures

fn text_field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn decimal_field(name: &str) -> FieldSpec {
    let mut spec = FieldSpec::new(name, FieldType::Decimal);
    spec.scale = Some(2);
    spec
}

fn test_meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut note = Definition::new("note");
    note.fields.push(text_field("title"));
    note.fields.push(text_field("hidden_note"));
    note.fields.push(decimal_field("total"));
    note.fields.push(decimal_field("qty"));

    let mut customer = Definition::new("customer");
    customer.fields.push(text_field("name"));
    customer.fields.push(decimal_field("balance"));

    let mut server_script = Definition::new("server_script");
    server_script.fields.push({
        let mut spec = text_field("name");
        spec.unique = true;
        spec
    });
    server_script
        .fields
        .push(FieldSpec::new("enabled", FieldType::Bool));
    server_script.fields.push(text_field("doctype"));
    server_script.fields.push({
        let mut spec = FieldSpec::new("event", FieldType::Select);
        spec.options = Some(
            "before_validate\nvalidate\nbefore_save\nafter_save\nbefore_submit\non_submit\nbefore_cancel\non_cancel\nafter_commit"
                .to_owned(),
        );
        spec
    });
    server_script
        .fields
        .push(FieldSpec::new("script", FieldType::LongText));
    server_script.fields.push(text_field("compiled_hash"));
    server_script
        .fields
        .push(FieldSpec::new("last_error", FieldType::LongText));
    server_script
        .fields
        .push(FieldSpec::new("timeout_ms", FieldType::Int));
    server_script
        .fields
        .push(FieldSpec::new("priority", FieldType::Int));
    server_script
        .fields
        .push(FieldSpec::new("last_run_count", FieldType::Int));
    server_script.fields.push(text_field("disabled_reason"));

    Registry::new(vec![note, customer, server_script])
        .expect("the definitions compile")
        .effective()
}

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
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

fn user_inv(id: RecordId) -> Invocation {
    Invocation {
        actor: Actor::User { id, via: None },
        ..inv()
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

fn script_engine() -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: host_surface(),
        ..EngineConfig::default()
    })
}

fn note_draft(title: &str) -> Document {
    let mut draft = Document::new(n("note"), RecordId::new_v7());
    draft.set(n("title"), Value::Text(title.to_owned()));
    draft
}

async fn build(
    controller: Arc<ScriptController>,
    authorizer: Option<Arc<dyn Authorizer>>,
) -> Engine<FakeBackend> {
    let mut registry = ControllerRegistry::new();
    registry.register("server_script", controller, ControllerSource::TypeScript);
    let seams = registry.install().expect("no step conflicts");
    let mut engine = Engine::new(
        FakeBackend::new(),
        test_meta(),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    if let Some(authorizer) = authorizer {
        engine = engine.with_authorizer(authorizer);
    }
    engine.prepare().await.expect("the store prepares");
    engine
}

fn controller(scripts: Vec<ServerScript>) -> Arc<ScriptController> {
    controller_with(scripts, ScriptLimits::default())
}

fn controller_with(scripts: Vec<ServerScript>, limits: ScriptLimits) -> Arc<ScriptController> {
    let store = Arc::new(FakeScriptStore::with_scripts(scripts.clone()));
    Arc::new(
        ScriptController::new("note", scripts, script_engine(), store)
            .expect("the controller builds")
            .with_limits(limits)
            .with_log(Arc::new(ScriptLog::new(64))),
    )
}

async fn stored_title(engine: &Engine<FakeBackend>, id: RecordId) -> String {
    let doc = engine
        .peek("note", id)
        .await
        .expect("peek works")
        .expect("the note is stored");
    match doc.get("title") {
        Value::Text(t) => t.clone(),
        other => panic!("title is {other:?}"),
    }
}

// -------------------------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn a_before_save_script_applies_its_patch() {
    let controller = controller(vec![script(
        "derive_title",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { return { patch: { title: "derived" } }; }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft("original"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "derived");
    let runs = controller.log().runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].is_ok());
}

#[tokio::test]
async fn a_validate_script_may_not_change_the_document() {
    let controller = controller(vec![script(
        "bad_validate",
        ScriptEvent::Validate,
        r#"export function on_validate(doc) { return { patch: { title: "nope" } }; }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let error = engine
        .create(&inv(), note_draft("original"))
        .await
        .expect_err("the phase violation fails the save");
    assert_eq!(error.code, "serverscript.phase_violation");
}

#[tokio::test]
async fn a_typed_throw_becomes_field_errors() {
    let controller = controller(vec![script(
        "check_qty",
        ScriptEvent::Validate,
        r#"export function on_validate(doc) {
             host.throw({ code: "selling.qty_not_positive", message: "Quantity must be positive.",
                          fields: [{ field: "qty", code: "selling.qty_not_positive", message: "Quantity must be positive." }] });
           }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let error = engine
        .create(&inv(), note_draft("original"))
        .await
        .expect_err("the typed throw fails the save");
    assert_eq!(error.class, ErrorClass::Validation);
    assert_eq!(error.fields[0].code, "selling.qty_not_positive");
    assert_eq!(error.fields[0].field.as_str(), "qty");
}

#[tokio::test]
async fn a_runaway_script_is_killed_and_the_save_fails() {
    let controller = controller(vec![script(
        "runaway",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { while (true) {} }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let error = engine
        .create(&inv(), note_draft("original"))
        .await
        .expect_err("the runaway is killed");
    assert_eq!(error.code, "serverscript.limit_exceeded");
    assert_eq!(error.details["cause"], "script.timeout");
}

#[tokio::test]
async fn a_script_cannot_reach_the_host() {
    let controller = controller(vec![script(
        "probe",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) {
             const names = ["process", "require", "fetch"];
             const leaked = names.filter((name) => typeof globalThis[name] !== "undefined");
             let evalRefused = false;
             try { globalThis.eval("1 + 1"); } catch (error) { evalRefused = true; }
             return { patch: { title: (leaked.length === 0 && evalRefused) ? "safe" : "unsafe:" + leaked.join(",") } };
           }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "safe");
}

#[tokio::test]
async fn a_disabled_script_does_not_run() {
    let mut disabled = script(
        "off",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { return { patch: { title: "ran" } }; }"#,
    );
    disabled.enabled = false;
    let controller = controller(vec![disabled]);
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft("original"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "original");
    assert!(controller.log().is_empty());
}

#[tokio::test]
async fn scripts_run_in_priority_then_name_order() {
    let mut first = script(
        "a_first",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { host.log({ level: "info", message: "a sees:" + (doc.title || "empty") }); return { patch: { title: (doc.title || "") + "a" } }; }"#,
    );
    first.priority = 0;
    let mut second = script(
        "b_second",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { host.log({ level: "info", message: "b sees:" + (doc.title || "empty") }); return { patch: { title: (doc.title || "") + "b" } }; }"#,
    );
    second.priority = 1;
    let controller = controller(vec![second, first]);
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft(""))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "ab");
}

#[tokio::test]
async fn a_script_cannot_see_a_field_the_user_may_not_read() {
    struct HidingView;
    impl FieldView for HidingView {
        fn readable<'a>(
            &'a self,
            _inv: &'a Invocation,
            _def: &'a spot_app_api::DocTypeDef,
            doc: &'a Document,
        ) -> BoxFuture<'a, AppResult<Document>> {
            Box::pin(async move {
                let fields: BTreeMap<Name, Value> = doc
                    .fields()
                    .iter()
                    .filter(|(k, _)| k.as_str() != "hidden_note")
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let mut out = Document::new(doc.doctype().clone(), doc.id());
                for (k, v) in fields {
                    out.set(k, v);
                }
                Ok(out)
            })
        }
    }

    let scripts = vec![script(
        "peek",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) {
             return { patch: { title: ("hidden_note" in doc) ? "leaked" : "hidden" } };
           }"#,
    )];
    let store = Arc::new(FakeScriptStore::with_scripts(scripts.clone()));
    let controller = Arc::new(
        ScriptController::new("note", scripts, script_engine(), store)
            .expect("the controller builds")
            .with_view(Arc::new(HidingView)),
    );
    let engine = build(controller.clone(), None).await;
    let mut draft = note_draft("x");
    draft.set(n("hidden_note"), Value::Text("secret".to_owned()));
    let created = engine.create(&inv(), draft).await.expect("saved").value;
    assert_eq!(stored_title(&engine, created.id()).await, "hidden");
}

#[tokio::test]
async fn a_script_runs_with_the_users_rights_and_a_denied_write_rolls_back() {
    /// Denies writing `balance` on `customer`; hides nothing on read.
    struct DenyBalance;
    impl Authorizer for DenyBalance {
        fn principal_for<'a>(&'a self, _inv: &'a Invocation) -> BoxFuture<'a, Principal> {
            AllowAllAuthorizer.principal_for(_inv)
        }
        fn decide<'a>(
            &'a self,
            principal: &'a Principal,
            action: Action,
            facts: &'a DocFacts,
        ) -> BoxFuture<'a, Decision> {
            AllowAllAuthorizer.decide(principal, action, facts)
        }
        fn authorize<'a>(
            &'a self,
            principal: &'a Principal,
            action: Action,
            doc: &'a Document,
        ) -> BoxFuture<'a, AppResult<()>> {
            AllowAllAuthorizer.authorize(principal, action, doc)
        }
        fn authorize_write<'a>(
            &'a self,
            principal: &'a Principal,
            action: Action,
            doc: &'a Document,
            changes: &'a [WriteChange],
        ) -> BoxFuture<'a, AppResult<()>> {
            Box::pin(async move {
                if changes.iter().any(|c| c.set && c.path == "balance") {
                    return Err(spot_app_api::AppError::new(
                        ErrorClass::PermissionDenied,
                        "permission.denied",
                        "You may not change the balance.".to_owned(),
                    ));
                }
                AllowAllAuthorizer
                    .authorize_write(principal, action, doc, changes)
                    .await
            })
        }
        fn readable<'a>(
            &'a self,
            principal: &'a Principal,
            doc: Document,
            def: &'a spot_app_api::DocTypeDef,
        ) -> BoxFuture<'a, AppResult<Document>> {
            AllowAllAuthorizer.readable(principal, doc, def)
        }
        fn readable_query<'a>(
            &'a self,
            principal: &'a Principal,
            query: &'a Query,
        ) -> BoxFuture<'a, AppResult<Option<QueryPlan>>> {
            AllowAllAuthorizer.readable_query(principal, query)
        }
        fn is_unrestricted(&self) -> bool {
            false
        }
    }

    // The customer is created through the same engine (the same backend) as the note.
    let customer_id = RecordId::new_v7();
    let source = format!(
        r#"export function on_after_save(doc) {{
             host.update({{ doctype: "customer", id: "{}", values: {{ balance: "5.00" }} }});
           }}"#,
        customer_id
    );
    let controller = controller(vec![script(
        "update_customer",
        ScriptEvent::AfterSave,
        &source,
    )]);
    let engine = build(controller.clone(), Some(Arc::new(DenyBalance))).await;
    let mut customer = Document::new(n("customer"), customer_id);
    customer.set(n("name"), Value::Text("Acme".to_owned()));
    engine
        .create(&inv(), customer)
        .await
        .expect("customer saved");

    let error = engine
        .create(&user_inv(RecordId::new_v7()), note_draft("x"))
        .await
        .expect_err("the denied write fails the save");
    // The host surfaced the permission refusal as a thrown HostError; the save rolled back.
    assert!(
        error.to_string().contains("may not change the balance"),
        "unexpected error: {error}"
    );
    let customer = engine
        .peek("customer", customer_id)
        .await
        .expect("peek")
        .expect("stored");
    assert_eq!(
        customer.get("balance"),
        &Value::Null,
        "the balance was not written"
    );
}

#[tokio::test]
async fn an_after_commit_failure_disables_the_script_after_the_threshold() {
    let controller = controller_with(
        vec![script(
            "notify",
            ScriptEvent::AfterCommit,
            r#"export function on_after_commit(doc) { throw { code: "notify.failed", message: "no" }; }"#,
        )],
        ScriptLimits {
            failure_threshold: 1,
            ..ScriptLimits::default()
        },
    );
    let engine = build(controller.clone(), None).await;
    // The committed failure does not roll back the save.
    engine.create(&inv(), note_draft("x")).await.expect("saved");
    let disabled = controller.disabled_scripts();
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0].0, "notify");
}

#[tokio::test]
async fn host_round_and_format_money_use_exact_decimals() {
    let controller = controller(vec![script(
        "money",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) {
             return { patch: { total: host.round({ value: "1.005", scale: 2, mode: "half_up" }) } };
           }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect("saved")
        .value;
    let doc = engine
        .peek("note", created.id())
        .await
        .expect("peek")
        .expect("stored");
    assert_eq!(
        doc.get("total"),
        &Value::Decimal(spot_money::rounding::parse_decimal("1.01").unwrap())
    );
}

#[tokio::test]
async fn a_script_reads_another_document_through_host_get() {
    let customer_id = RecordId::new_v7();
    let source = format!(
        r#"export function on_before_save({{ doc }}) {{
             const other = host.get({{ doctype: "customer", id: "{}", fields: ["name"] }});
             return {{ patch: {{ title: other.name }} }};
           }}"#,
        customer_id
    );
    let controller = controller(vec![script(
        "read_customer",
        ScriptEvent::BeforeSave,
        &source,
    )]);
    let engine = build(controller.clone(), None).await;
    let mut customer = Document::new(n("customer"), customer_id);
    customer.set(n("name"), Value::Text("Acme".to_owned()));
    engine
        .create(&inv(), customer)
        .await
        .expect("customer saved");
    let created = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "Acme");
}

#[tokio::test]
async fn a_script_inserts_a_document_through_host_insert() {
    let controller = controller(vec![script(
        "make_customer",
        ScriptEvent::AfterSave,
        r#"export function on_after_save({ doc }) {
             host.insert({ doctype: "customer", values: { name: doc.title } });
           }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft("Acme"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "Acme");
    // The customer exists (the insert ran through the full lifecycle as the same user).
    let query = spot_storage_contract::Query::from(
        &n("customer"),
        &n("c"),
        spot_storage_contract::QueryPath::Transactional,
    );
    let _ = query;
}

#[tokio::test]
async fn compile_on_save_reports_diagnostics_with_positions_and_stores_the_hash() {
    let store = Arc::new(FakeScriptStore::new());
    let compile = Arc::new(ServerScriptController::new(script_engine(), store.clone()));
    let mut registry = ControllerRegistry::new();
    registry.register("server_script", compile, ControllerSource::Native);
    let seams = registry.install().expect("no conflicts");
    let engine = Engine::new(
        FakeBackend::new(),
        test_meta(),
        seams,
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("the engine builds");
    engine.prepare().await.expect("prepared");

    let mut broken = Document::new(n("server_script"), RecordId::new_v7());
    broken.set(n("name"), Value::Text("broken".to_owned()));
    broken.set(n("doctype"), Value::Text("note".to_owned()));
    broken.set(n("event"), Value::Text("validate".to_owned()));
    broken.set(n("enabled"), Value::Bool(true));
    broken.set(
        n("script"),
        Value::Text("export function on_validate(doc) { this is not ts".to_owned()),
    );
    let error = engine
        .create(&inv(), broken)
        .await
        .expect_err("the broken script is refused");
    assert_eq!(error.class, ErrorClass::Validation);
    assert!(!error.fields.is_empty());
    assert_eq!(error.fields[0].field.as_str(), "script");

    let mut good = Document::new(n("server_script"), RecordId::new_v7());
    good.set(n("name"), Value::Text("good".to_owned()));
    good.set(n("doctype"), Value::Text("note".to_owned()));
    good.set(n("event"), Value::Text("validate".to_owned()));
    good.set(n("enabled"), Value::Bool(true));
    good.set(
        n("script"),
        Value::Text("export function on_validate(doc) { const x: number = 1; }".to_owned()),
    );
    let saved = engine
        .create(&inv(), good)
        .await
        .expect("the good script saves")
        .value;
    let stored = engine
        .peek("server_script", saved.id())
        .await
        .expect("peek")
        .expect("stored");
    assert!(matches!(stored.get("compiled_hash"), Value::Text(h) if h.len() == 64));
}

#[tokio::test]
async fn an_invocation_is_deterministic_across_reruns() {
    let source = r#"export function on_before_save({ doc }) {
        let acc = 0;
        for (let i = 0; i < 1000; i++) { acc = (acc + Math.floor(host.now() % 7)) % 1000; }
        return { patch: { title: "n" + acc + "-" + Math.floor(Math.random() * 1000) } };
    }"#;
    let mut results = Vec::new();
    for _ in 0..2 {
        let controller = controller(vec![script(
            "deterministic",
            ScriptEvent::BeforeSave,
            source,
        )]);
        let engine = build(controller, None).await;
        let created = engine
            .create(&inv(), note_draft("x"))
            .await
            .expect("saved")
            .value;
        results.push(stored_title(&engine, created.id()).await);
    }
    assert_eq!(results[0], results[1]);
}

#[tokio::test]
async fn a_script_runs_a_cross_document_query() {
    let controller = controller(vec![script(
        "roll_up",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) {
             const rows = host.query({ doctype: "customer", fields: ["name"], filter: { name: "Acme" }, limit: 10 });
             return { patch: { title: rows.length === 1 ? rows[0].name : "none:" + rows.length } };
           }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let mut customer = Document::new(n("customer"), RecordId::new_v7());
    customer.set(n("name"), Value::Text("Acme".to_owned()));
    engine
        .create(&inv(), customer)
        .await
        .expect("customer saved");
    let created = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "Acme");
}

#[tokio::test]
async fn a_script_calls_a_whitelisted_method_through_host_call() {
    struct PingCaller;
    impl spot_server_scripts::MethodCaller for PingCaller {
        fn call<'a>(
            &'a self,
            _inv: &'a Invocation,
            _doctype: &'a str,
            _id: RecordId,
            method: &'a str,
            _args: serde_json::Value,
        ) -> BoxFuture<'a, AppResult<serde_json::Value>> {
            Box::pin(async move { Ok(serde_json::json!({ "method": method })) })
        }
    }

    let scripts = vec![script(
        "call_ping",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) {
             const out = host.call({ doctype: "note", id: doc.id, method: "ping" });
             return { patch: { title: out.method } };
           }"#,
    )];
    let store = Arc::new(FakeScriptStore::with_scripts(scripts.clone()));
    let controller = Arc::new(
        ScriptController::new("note", scripts, script_engine(), store)
            .expect("the controller builds")
            .with_method_caller(Arc::new(PingCaller)),
    );
    let engine = build(controller.clone(), None).await;
    let created = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect("saved")
        .value;
    assert_eq!(stored_title(&engine, created.id()).await, "ping");
}

#[tokio::test]
async fn a_memory_bomb_is_killed() {
    let controller = controller(vec![script(
        "bomb",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { const a = []; while (true) { a.push("x".repeat(65536)); } }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let error = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect_err("the memory bomb is killed");
    assert_eq!(error.code, "serverscript.limit_exceeded");
}

#[tokio::test]
async fn a_host_call_flood_is_killed() {
    let controller = controller(vec![script(
        "flood",
        ScriptEvent::BeforeSave,
        r#"export function on_before_save({ doc }) { for (let i = 0; i < 100000; i++) { host.round({ value: "1.5", scale: 0 }); } }"#,
    )]);
    let engine = build(controller.clone(), None).await;
    let error = engine
        .create(&inv(), note_draft("x"))
        .await
        .expect_err("the host-call flood is killed");
    assert_eq!(error.code, "serverscript.limit_exceeded");
    assert_eq!(error.details["cause"], "script.too_many_host_calls");
}

// -------------------------------------------------------------------------------------------
// The corpus: twenty realistic before-save scripts, each run twice with the same result.

#[tokio::test]
async fn a_corpus_of_twenty_realistic_scripts_behaves_identically_on_rerun() {
    // (name, input title, input qty, source of the body returning a title, expected title)
    let corpus: [(&str, &str, &str, &str, &str); 20] = [
        (
            "default_title",
            "",
            "1",
            r#"return doc.title ? doc.title : "Untitled";"#,
            "Untitled",
        ),
        (
            "trim",
            "  spaced  ",
            "1",
            "return doc.title.trim();",
            "spaced",
        ),
        (
            "upper",
            "invoice",
            "1",
            "return doc.title.toUpperCase();",
            "INVOICE",
        ),
        (
            "lower",
            "INVOICE",
            "1",
            "return doc.title.toLowerCase();",
            "invoice",
        ),
        (
            "prefix",
            "100",
            "1",
            r#"return "SO-" + doc.title;"#,
            "SO-100",
        ),
        (
            "suffix",
            "100",
            "1",
            r#"return doc.title + "/24";"#,
            "100/24",
        ),
        (
            "pad",
            "7",
            "1",
            r#"return doc.title.padStart(5, "0");"#,
            "00007",
        ),
        (
            "slug",
            "Hello World",
            "1",
            r#"return doc.title.toLowerCase().split(" ").join("-");"#,
            "hello-world",
        ),
        (
            "first_word",
            "alpha beta",
            "1",
            r#"return doc.title.split(" ")[0];"#,
            "alpha",
        ),
        (
            "length",
            "abcd",
            "1",
            "return String(doc.title.length);",
            "4",
        ),
        (
            "qty_string",
            "x",
            "3",
            r#"return doc.title + ":" + doc.qty;"#,
            "x:3",
        ),
        ("qty_is_text", "x", "3", "return typeof doc.qty;", "string"),
        (
            "bucket_small",
            "x",
            "3",
            r#"return Number(doc.qty) < 10 ? "small" : "large";"#,
            "small",
        ),
        (
            "bucket_large",
            "x",
            "30",
            r#"return Number(doc.qty) < 10 ? "small" : "large";"#,
            "large",
        ),
        (
            "cents",
            "x",
            "3",
            r#"return String(Number(doc.qty) * 250) + " cents";"#,
            "750 cents",
        ),
        (
            "hidden_empty",
            "x",
            "1",
            r#"return doc.hidden_note === undefined || doc.hidden_note === null ? "none" : "some";"#,
            "none",
        ),
        (
            "join_fields",
            "a",
            "2",
            r#"return [doc.title, doc.qty].join("|");"#,
            "a|2",
        ),
        ("repeat", "ab", "1", "return doc.title.repeat(3);", "ababab"),
        (
            "reverse",
            "abc",
            "1",
            r#"return doc.title.split("").reverse().join("");"#,
            "cba",
        ),
        (
            "checksum",
            "abc",
            "1",
            "let s = 0; for (const c of doc.title) { s += c.charCodeAt(0); } return String(s);",
            "294",
        ),
    ];
    for (name, title, qty, body, expected) in corpus {
        let source = format!(
            "export function on_before_save({{ doc }}) {{ const t = (() => {{ {body} }})(); return {{ patch: {{ title: t }} }}; }}"
        );
        let mut seen = Vec::new();
        for _ in 0..2 {
            let controller = controller(vec![script(name, ScriptEvent::BeforeSave, &source)]);
            let engine = build(controller, None).await;
            let mut draft = note_draft(title);
            draft.set(
                n("qty"),
                Value::Decimal(spot_money::rounding::parse_decimal(qty).unwrap()),
            );
            let created = engine
                .create(&inv(), draft)
                .await
                .unwrap_or_else(|e| panic!("{name} failed: {e:?}"))
                .value;
            seen.push(stored_title(&engine, created.id()).await);
        }
        assert_eq!(seen[0], seen[1], "{name} is not deterministic");
        assert_eq!(seen[0], expected, "{name} gave the wrong title");
    }
}
