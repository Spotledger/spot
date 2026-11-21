//! API scripts and scheduled scripts (WP-EXT-06.T6) on the real engine, over a fake document
//! access that applies a per-user read rule, so the "runs as the caller" and "no escalation"
//! properties are observable.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::prelude::{
    Clock, Name, OffsetDateTime, Query, RecordId, Rows, SiteId, Value, Version,
};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass,
    Invocation,
};
use spot_functions::{Caller, Context, FunctionHandler, HttpVerb};
use spot_manifest::Queue;
use spot_scheduler::slot::rfc3339;
use spot_scheduler::{MissedRunPolicy, RunSlot, SlotRef};
use spot_server_scripts::kinds::{
    ApiScript, ApiScriptHandler, ApiScriptRegistry, ApiVerbs, FakeKindStore, KindDeps, MissedRun,
    ScheduledScript, ScheduledScriptRunner, UserDirectory, UserRecord,
};
use spot_server_scripts::{
    FUNCTION_CONFLICT, GUEST_REFUSED, LIMIT_EXCEEDED, RECORD_INVALID, RUN_AS_UNAVAILABLE,
    ScriptAccessFactory, ScriptDocumentAccess, ScriptLimits, host_surface,
};
use spot_ts::{EngineConfig, ScriptEngine};

// -------------------------------------------------------------------------------------------
// Fixtures

struct FixedClock;

impl Clock for FixedClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

/// The world the fake access reads: documents, the users who may not read them, and a record
/// of who touched what.
#[derive(Default)]
struct World {
    docs: Mutex<BTreeMap<String, Document>>,
    cannot_read: Mutex<Vec<String>>,
    opened: AtomicU32,
    touched_by: Mutex<Vec<String>>,
}

impl World {
    fn put_note(&self, title: &str) -> String {
        let id = RecordId::new_v7();
        let mut fields = BTreeMap::new();
        fields.insert(n("title"), Value::Text(title.to_owned()));
        let doc = Document::loaded(n("note"), id, Version::FIRST, None, fields, BTreeMap::new());
        self.docs.lock().unwrap().insert(id.to_string(), doc);
        id.to_string()
    }

    fn deny(&self, user: &str) {
        self.cannot_read.lock().unwrap().push(user.to_owned());
    }
}

struct FakeFactory(Arc<World>);

impl ScriptAccessFactory for FakeFactory {
    fn open(&self, invocation: Invocation) -> Box<dyn ScriptDocumentAccess> {
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        Box::new(FakeAccess {
            world: Arc::clone(&self.0),
            invocation,
        })
    }
}

struct FakeAccess {
    world: Arc<World>,
    invocation: Invocation,
}

fn unsupported() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "serverscript.store_failed",
        "the fake offers no such operation",
    )
}

impl ScriptDocumentAccess for FakeAccess {
    fn invocation(&self) -> &Invocation {
        &self.invocation
    }

    fn meta<'a>(&'a mut self, _doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move { Err(unsupported()) })
    }

    fn get<'a>(
        &'a mut self,
        _doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        let user = match &self.invocation.actor {
            Actor::User { id, .. } => id.to_string(),
            other => format!("{other:?}"),
        };
        self.world.touched_by.lock().unwrap().push(user.clone());
        let denied = self.world.cannot_read.lock().unwrap().contains(&user);
        let found = self
            .world
            .docs
            .lock()
            .unwrap()
            .get(&id.to_string())
            .cloned();
        Box::pin(async move {
            if denied {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "no read right on this document",
                ));
            }
            found.ok_or_else(|| {
                AppError::new(
                    ErrorClass::NotFound,
                    "document.not_found",
                    "no such document",
                )
            })
        })
    }

    fn query<'a>(&'a mut self, _query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move { Err(unsupported()) })
    }

    fn insert(&mut self, _doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async move { Err(unsupported()) })
    }

    fn update<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _patch: DocumentPatch,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move { Err(unsupported()) })
    }

    fn call<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        method: &'a str,
        _args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move { Ok(json!({ "called": method })) })
    }
}

struct Users(Vec<(String, UserRecord)>);

impl UserDirectory for Users {
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<Option<UserRecord>>> {
        let found = self
            .0
            .iter()
            .find(|(user, _)| user == name)
            .map(|(_, record)| record.clone());
        Box::pin(async move { Ok(found) })
    }
}

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

fn engine() -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: host_surface(),
        ..EngineConfig::default()
    })
}

struct Rig {
    world: Arc<World>,
    store: Arc<FakeKindStore>,
    engine: ScriptEngine,
    deps: KindDeps,
}

fn rig(limits: ScriptLimits) -> Rig {
    let world = Arc::new(World::default());
    let store = Arc::new(FakeKindStore::new());
    let engine = engine();
    let deps = KindDeps {
        engine: engine.clone(),
        access: Arc::new(FakeFactory(Arc::clone(&world))),
        clock: Arc::new(FixedClock),
        store: store.clone(),
        limits,
        currencies: None,
    };
    Rig {
        world,
        store,
        engine,
        deps,
    }
}

fn api(name: &str, module: &str, function: &str, source: &str) -> ApiScript {
    ApiScript {
        record_id: None,
        name: name.to_owned(),
        module: module.to_owned(),
        function: function.to_owned(),
        verbs: ApiVerbs::GetAndPost,
        arguments_schema: None,
        source: source.to_owned(),
        timeout_ms: 200,
        enabled: true,
        compiled_hash: None,
        disabled_reason: None,
        elevation: None,
    }
}

fn scheduled(name: &str, run_as: &str, source: &str) -> ScheduledScript {
    ScheduledScript {
        record_id: None,
        name: name.to_owned(),
        cron: spot_manifest::Cron::parse("0 0 2 * * *").expect("a cron"),
        time_zone: "Asia/Karachi".to_owned(),
        missed_run: MissedRun::RunOnce,
        queue: Queue::Long,
        run_as: run_as.to_owned(),
        source: source.to_owned(),
        timeout_ms: 200,
        enabled: true,
        compiled_hash: None,
        disabled_reason: None,
    }
}

fn ctx_for(user: &RecordId) -> Context {
    Context::new("scripts-test", Caller::User(user.to_string()))
}

fn slot(schedule: &str) -> RunSlot {
    RunSlot {
        schedule: schedule.to_owned(),
        scheduled_for: jiff::Timestamp::from_second(1_700_000_000).expect("a timestamp"),
        deduplication_key: format!("scripting:{schedule}:2023-11-14T22:13:20Z"),
        slot_ref: SlotRef::new("scripting", schedule),
    }
}

fn site() -> SiteId {
    SiteId::new("scripts-test").expect("a site id")
}

fn engine_never_entered(engine: &ScriptEngine) -> bool {
    let stats = engine.stats();
    stats.compiles == 0 && stats.cache_hits == 0
}

// -------------------------------------------------------------------------------------------
// API scripts

#[tokio::test]
async fn an_api_script_returns_its_json_and_records_the_run() {
    let rig = rig(ScriptLimits::default());
    let mut script = api(
        "double",
        "maths",
        "double",
        "export function on_call({ args }) { return { doubled: args.n * 2 }; }",
    );
    script.arguments_schema = Some(
        json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}),
    );
    let handler = ApiScriptHandler::new(script, rig.deps.clone());
    let user = RecordId::new_v7();

    let result = handler
        .call(&ctx_for(&user), json!({ "n": 21 }))
        .await
        .expect("the script answers");
    assert_eq!(result, json!({ "doubled": 42 }));

    let runs = rig.store.runs();
    assert_eq!(runs.len(), 1);
    assert!(runs[0].ok);
    assert_eq!(runs[0].code, "ok");
    assert_eq!(runs[0].user, user.to_string());
}

#[tokio::test]
async fn an_api_script_sees_what_its_caller_may_read_and_no_more() {
    let rig = rig(ScriptLimits::default());
    let note = rig.world.put_note("hello");
    let handler = ApiScriptHandler::new(
        api(
            "read_note",
            "notes",
            "read",
            r#"export function on_call({ args }) {
                 const doc = host.get({ doctype: "note", id: args.id });
                 return { title: doc.title };
               }"#,
        ),
        rig.deps.clone(),
    );
    let reader = RecordId::new_v7();
    let outsider = RecordId::new_v7();
    rig.world.deny(&outsider.to_string());

    let ok = handler
        .call(&ctx_for(&reader), json!({ "id": note }))
        .await
        .expect("the reader may read");
    assert_eq!(ok, json!({ "title": "hello" }));

    let refused = handler
        .call(&ctx_for(&outsider), json!({ "id": note }))
        .await
        .expect_err("the outsider may not read: the host's refusal, not a result");
    assert_eq!(refused.details["cause"], "script.thrown");
    assert!(refused.message.contains("no read right"));

    // Each call acted as its own caller, never as anybody else.
    let touched = rig.world.touched_by.lock().unwrap().clone();
    assert_eq!(touched, vec![reader.to_string(), outsider.to_string()]);
}

#[tokio::test]
async fn a_guest_is_refused_before_the_engine_starts() {
    let rig = rig(ScriptLimits::default());
    let handler = ApiScriptHandler::new(
        api("hello", "m", "f", "export function on_call() { return 1; }"),
        rig.deps.clone(),
    );
    for caller in [Caller::Anonymous, Caller::System("scheduler".into())] {
        let error = handler
            .call(&Context::new("scripts-test", caller), json!({}))
            .await
            .expect_err("a caller with no user is refused");
        assert_eq!(error.code, GUEST_REFUSED);
        assert_eq!(error.class, ErrorClass::PermissionDenied);
    }
    // A user id that is not a record id is no identity either.
    let error = handler
        .call(
            &Context::new("scripts-test", Caller::User("not-an-id".into())),
            json!({}),
        )
        .await
        .expect_err("refused");
    assert_eq!(error.code, GUEST_REFUSED);

    assert!(engine_never_entered(&rig.engine), "the engine was entered");
    assert_eq!(rig.world.opened.load(Ordering::SeqCst), 0);
    assert!(rig.store.runs().is_empty());
}

#[tokio::test]
async fn a_bad_argument_is_refused_by_the_schema_before_the_engine() {
    let rig = rig(ScriptLimits::default());
    let mut script = api(
        "double",
        "m",
        "f",
        "export function on_call() { return 1; }",
    );
    script.arguments_schema = Some(
        json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}),
    );
    let handler = ApiScriptHandler::new(script, rig.deps.clone());
    let user = RecordId::new_v7();

    let error = handler
        .call(&ctx_for(&user), json!({ "n": "twenty" }))
        .await
        .expect_err("the schema refuses");
    assert_eq!(error.code, "function.bad_arguments");
    assert_eq!(error.class, ErrorClass::Validation);
    let missing = handler
        .call(&ctx_for(&user), json!({}))
        .await
        .expect_err("a required argument is missing");
    assert_eq!(missing.code, "function.bad_arguments");
    assert!(engine_never_entered(&rig.engine));
}

#[tokio::test]
async fn an_unknown_argument_is_refused_by_a_closed_schema_before_the_engine() {
    let rig = rig(ScriptLimits::default());
    let mut script = api(
        "double",
        "m",
        "f",
        "export function on_call({ args }) { return { doubled: args.n * 2 }; }",
    );
    script.arguments_schema = Some(json!({
        "type": "object",
        "required": ["n"],
        "additionalProperties": false,
        "properties": { "n": { "type": "integer" } }
    }));
    let handler = ApiScriptHandler::new(script, rig.deps.clone());
    let user = RecordId::new_v7();

    let error = handler
        .call(&ctx_for(&user), json!({ "n": 2, "tier": 3 }))
        .await
        .expect_err("the closed schema refuses the extra argument");
    assert_eq!(error.code, "function.bad_arguments");
    assert_eq!(error.class, ErrorClass::Validation);
    assert_eq!(
        error.explanation.as_deref(),
        Some("tier: unexpected property"),
        "the refusal names the argument"
    );
    assert!(
        engine_never_entered(&rig.engine),
        "the engine is not touched by a refused call"
    );

    let ran = handler
        .call(&ctx_for(&user), json!({ "n": 2 }))
        .await
        .expect("the same call without the extra argument runs");
    assert_eq!(ran, json!({ "doubled": 4 }));
}

#[tokio::test]
async fn a_schema_without_additional_properties_keeps_an_unlisted_argument() {
    let rig = rig(ScriptLimits::default());
    let mut script = api(
        "open",
        "m",
        "g",
        "export function on_call({ args }) { return { doubled: args.n * 2 }; }",
    );
    script.arguments_schema = Some(json!({
        "type": "object",
        "required": ["n"],
        "properties": { "n": { "type": "integer" } }
    }));
    let handler = ApiScriptHandler::new(script, rig.deps.clone());
    let user = RecordId::new_v7();
    let ran = handler
        .call(&ctx_for(&user), json!({ "n": 3, "extra": true }))
        .await
        .expect("an open schema accepts an unlisted argument");
    assert_eq!(ran, json!({ "doubled": 6 }));
}

#[tokio::test]
async fn a_script_that_loops_forever_is_killed_and_does_not_stop_the_next_call() {
    let rig = rig(ScriptLimits::default());
    let mut looping = api(
        "spin",
        "m",
        "spin",
        "export function on_call() { while (true) {} }",
    );
    looping.timeout_ms = 50;
    let looping = ApiScriptHandler::new(looping, rig.deps.clone());
    let fine = ApiScriptHandler::new(
        api(
            "fine",
            "m",
            "fine",
            "export function on_call() { return { ok: true }; }",
        ),
        rig.deps.clone(),
    );
    let user = RecordId::new_v7();

    let killed = looping
        .call(&ctx_for(&user), json!({}))
        .await
        .expect_err("the loop is killed");
    assert_eq!(killed.code, LIMIT_EXCEEDED);

    let next = fine
        .call(&ctx_for(&user), json!({}))
        .await
        .expect("the next call is unaffected");
    assert_eq!(next, json!({ "ok": true }));
    let runs = rig.store.runs();
    assert_eq!(runs.len(), 2);
    assert!(!runs[0].ok);
    assert_eq!(runs[0].code, LIMIT_EXCEEDED);
    assert!(runs[1].ok);
}

#[tokio::test]
async fn a_typed_throw_becomes_the_typed_error() {
    let rig = rig(ScriptLimits::default());
    let handler = ApiScriptHandler::new(
        api(
            "check",
            "m",
            "check",
            r#"export function on_call({ args }) {
                 host.throw({
                   code: "app.bad_amount",
                   message: "The amount is not acceptable.",
                   fields: [{ field: "amount", code: "app.bad_amount", message: "Too large." }],
                 });
               }"#,
        ),
        rig.deps.clone(),
    );
    let user = RecordId::new_v7();
    let error = handler
        .call(&ctx_for(&user), json!({}))
        .await
        .expect_err("the script throws");
    assert_eq!(error.class, ErrorClass::Validation);
    assert_eq!(error.fields[0].code, "app.bad_amount");
    assert_eq!(error.fields[0].field.to_string(), "amount");
    assert_eq!(error.details["script"], "check");
}

#[tokio::test]
async fn a_result_over_the_output_limit_is_limit_exceeded() {
    let limits = ScriptLimits {
        output_bytes: 1024,
        ..ScriptLimits::default()
    };
    let rig = rig(limits);
    let handler = ApiScriptHandler::new(
        api(
            "big",
            "m",
            "big",
            r#"export function on_call() { return { text: "x".repeat(100000) }; }"#,
        ),
        rig.deps.clone(),
    );
    let user = RecordId::new_v7();
    let error = handler
        .call(&ctx_for(&user), json!({}))
        .await
        .expect_err("too large");
    assert_eq!(error.code, LIMIT_EXCEEDED);
}

#[test]
fn the_registry_declares_one_non_guest_function_per_enabled_script() {
    let rig = rig(ScriptLimits::default());
    let mut disabled = api("off", "m", "off", "export function on_call() {}");
    disabled.enabled = false;
    let mut get_only = api(
        "lookup",
        "reports",
        "lookup",
        "export function on_call() {}",
    );
    get_only.verbs = ApiVerbs::Get;
    let built = ApiScriptRegistry::build(
        &[
            api("double", "maths", "double", "export function on_call() {}"),
            get_only,
            disabled,
        ],
        &rig.deps,
    )
    .expect("no conflict");
    let keys: Vec<String> = built.iter().map(|(decl, _)| decl.key.to_string()).collect();
    assert_eq!(
        keys,
        vec!["scripting/maths/double", "scripting/reports/lookup"]
    );
    for (decl, _) in &built {
        assert!(!decl.guest, "an API script is never guest-callable");
        assert_eq!(decl.key.app(), "scripting");
    }
    assert_eq!(built[1].0.verbs, Some(vec![HttpVerb::Get]));
    assert_eq!(built[0].0.verbs, Some(vec![HttpVerb::Get, HttpVerb::Post]));
}

#[test]
fn two_scripts_with_one_key_are_refused_naming_both() {
    let rig = rig(ScriptLimits::default());
    let scripts = [
        api("first", "m", "f", "export function on_call() {}"),
        api("second", "m", "f", "export function on_call() {}"),
        api("other", "m", "g", "export function on_call() {}"),
    ];
    let error = ApiScriptRegistry::build(&scripts, &rig.deps)
        .err()
        .expect("a conflict");
    assert_eq!(error.code, FUNCTION_CONFLICT);
    assert!(error.message.contains("`first`") && error.message.contains("`second`"));
    assert!(error.message.contains("scripting/m/f"));
    let conflicts = ApiScriptRegistry::conflicts(&scripts);
    assert_eq!(conflicts.len(), 1);
    assert_eq!(
        conflicts[0].scripts,
        vec!["first".to_owned(), "second".to_owned()]
    );
}

// -------------------------------------------------------------------------------------------
// Scheduled scripts

fn runner(
    rig: &Rig,
    scripts: Vec<ScheduledScript>,
    users: Vec<(&str, UserRecord)>,
) -> ScheduledScriptRunner {
    let users = Users(users.into_iter().map(|(n, r)| (n.to_owned(), r)).collect());
    ScheduledScriptRunner::new(site(), scripts, rig.deps.clone(), Arc::new(users))
}

fn user(enabled: bool) -> UserRecord {
    UserRecord {
        id: RecordId::new_v7(),
        enabled,
    }
}

#[test]
fn the_schedule_carries_the_cron_zone_queue_and_policy() {
    let rig = rig(ScriptLimits::default());
    let runner = runner(
        &rig,
        vec![scheduled("nightly", "batch", "export function on_run() {}")],
        vec![("batch", user(true))],
    );
    let schedules = runner.schedules();
    assert_eq!(schedules.len(), 1);
    let (schedule, policy, target) = &schedules[0];
    assert_eq!(schedule.name, "nightly");
    assert_eq!(schedule.cron.as_str(), "0 0 2 * * *");
    assert_eq!(schedule.zone.name(), "Asia/Karachi");
    assert_eq!(schedule.queue, Queue::Long);
    assert_eq!(*policy, MissedRunPolicy::RunOnce);
    assert_eq!(target.job, "scripting:run_scheduled_script");
    assert_eq!(target.args, json!({ "script": "nightly" }));
    assert_eq!(target.queue, Queue::Long);
    assert_eq!(schedule.step.as_str(), "scripting:run_scheduled_script");
}

#[tokio::test]
async fn a_scheduled_run_acts_as_its_run_as_user_and_passes_the_slot_through() {
    let rig = rig(ScriptLimits::default());
    let batch = user(true);
    let runner = runner(
        &rig,
        vec![scheduled(
            "nightly",
            "batch",
            r#"export function on_run({ ctx }) {
                 host.log({ level: "info", message: ctx.deduplication_key + "|" + ctx.scheduled_for + "|" + ctx.schedule + "|" + ctx.user });
               }"#,
        )],
        vec![("batch", batch.clone())],
    );
    let slot = slot("nightly");
    let run = runner
        .run("nightly", &slot)
        .await
        .expect("the run succeeds");
    assert!(run.ok);
    assert_eq!(run.user, batch.id.to_string());
    assert_eq!(
        run.message,
        Some(format!(
            "{}|{}|nightly|{}",
            slot.deduplication_key,
            rfc3339(slot.scheduled_for),
            batch.id
        ))
    );
    assert_eq!(rig.store.runs().len(), 1);
}

#[tokio::test]
async fn a_disabled_or_missing_run_as_user_is_refused_without_the_engine() {
    let rig = rig(ScriptLimits::default());
    let runner = runner(
        &rig,
        vec![
            scheduled("gone", "nobody", "export function on_run() {}"),
            scheduled("off", "sleeper", "export function on_run() {}"),
        ],
        vec![("sleeper", user(false))],
    );
    for name in ["gone", "off"] {
        let error = runner
            .run(name, &slot(name))
            .await
            .expect_err("no system fallback");
        assert_eq!(error.code, RUN_AS_UNAVAILABLE);
    }
    assert!(engine_never_entered(&rig.engine));
    assert_eq!(rig.world.opened.load(Ordering::SeqCst), 0);
    assert!(rig.store.runs().iter().all(|run| !run.ok));
}

#[tokio::test]
async fn a_scheduled_script_cannot_read_what_its_user_cannot() {
    let rig = rig(ScriptLimits::default());
    let note = rig.world.put_note("secret");
    let batch = user(true);
    rig.world.deny(&batch.id.to_string());
    let source = format!(
        r#"export function on_run() {{
             const doc = host.get({{ doctype: "note", id: "{note}" }});
             host.log({{ level: "info", message: doc.title }});
           }}"#
    );
    let runner = runner(
        &rig,
        vec![scheduled("peek", "batch", &source)],
        vec![("batch", batch.clone())],
    );
    let error = runner
        .run("peek", &slot("peek"))
        .await
        .expect_err("the user has no read right: no escalation");
    assert_eq!(error.details["cause"], "script.thrown");
    assert!(error.message.contains("no read right"));
    // The only actor that ever touched the document is the run-as user.
    let touched = rig.world.touched_by.lock().unwrap().clone();
    assert_eq!(touched, vec![batch.id.to_string()]);
}

#[tokio::test]
async fn three_consecutive_failures_disable_the_script_with_a_reason() {
    let limits = ScriptLimits {
        failure_threshold: 3,
        ..ScriptLimits::default()
    };
    let rig = rig(limits);
    let runner = runner(
        &rig,
        vec![scheduled(
            "broken",
            "batch",
            r#"export function on_run() { throw new Error("boom"); }"#,
        )],
        vec![("batch", user(true))],
    );
    for _ in 0..3 {
        assert!(runner.run("broken", &slot("broken")).await.is_err());
    }
    let disabled = rig.store.disabled();
    assert_eq!(disabled.len(), 1);
    assert_eq!(disabled[0].1, "broken");
    assert!(disabled[0].2.contains("3 consecutive failures"));
    assert!(disabled[0].2.contains("script.thrown"));
    assert_eq!(runner.disabled_scripts().len(), 1);
    assert!(
        runner.schedules().is_empty(),
        "a disabled script leaves the schedule"
    );

    // A fourth run is refused without running, and records nothing more.
    let refused = runner
        .run("broken", &slot("broken"))
        .await
        .expect_err("disabled");
    assert_eq!(refused.code, RECORD_INVALID);
    assert_eq!(rig.store.runs().len(), 3);
}

#[tokio::test]
async fn a_success_resets_the_failure_count() {
    let limits = ScriptLimits {
        failure_threshold: 2,
        ..ScriptLimits::default()
    };
    let rig = rig(limits);
    let runner = runner(
        &rig,
        vec![scheduled(
            "flaky",
            "batch",
            r#"export function on_run({ ctx }) {
                 if (ctx.scheduled_for.length > 0 && ctx.schedule === "flaky" && ctx.deduplication_key.endsWith("fail")) { throw new Error("no"); }
               }"#,
        )],
        vec![("batch", user(true))],
    );
    let mut failing = slot("flaky");
    failing.deduplication_key = "scripting:flaky:fail".to_owned();
    assert!(runner.run("flaky", &failing).await.is_err());
    assert!(runner.run("flaky", &slot("flaky")).await.is_ok());
    assert!(runner.run("flaky", &failing).await.is_err());
    assert!(
        rig.store.disabled().is_empty(),
        "not consecutive: still enabled"
    );
}

#[tokio::test]
async fn an_unknown_script_is_refused() {
    let rig = rig(ScriptLimits::default());
    let runner = runner(&rig, Vec::new(), Vec::new());
    let error = runner
        .run("ghost", &slot("ghost"))
        .await
        .expect_err("no such script");
    assert_eq!(error.code, RECORD_INVALID);
}
