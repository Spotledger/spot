//! Script elevation (WP-EXT-06.T8): default deny, one door, audit first.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::prelude::{Clock, Name, OffsetDateTime, Query, RecordId, Rows, Value, Version};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass,
    Invocation,
};
use spot_functions::{Caller, Context, FunctionHandler};
use spot_server_scripts::elevation::{
    ElevatedKind, ElevationFlag, ElevationWiring, FIELD_ELEVATED, FIELD_ELEVATION_REASON,
    FakeElevationAudit, Saver, apply_plan, check_save, read_flag,
};
use spot_server_scripts::kinds::{
    ApiScript, ApiScriptHandler, ApiVerbs, FakeKindStore, KindDeps, MissedRun, ScheduledScript,
};
use spot_server_scripts::{
    ELEVATION_AUDIT_FAILED, ELEVATION_FORBIDDEN, ELEVATION_NOT_ALLOWED, ELEVATION_REASON_REQUIRED,
    LIMIT_EXCEEDED, ScriptAccessFactory, ScriptDocumentAccess, ScriptLimits, host_surface,
};
use spot_ts::{EngineConfig, ScriptEngine};

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

struct FixedClock;

impl Clock for FixedClock {
    fn now_utc(&self) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

/// One restricted document the platform may read and ordinary users may not.
#[derive(Default)]
struct World {
    restricted: Mutex<Option<(String, Document)>>,
    actors: Mutex<Vec<String>>,
    opened: AtomicU32,
    writes: AtomicU32,
}

struct Factory(Arc<World>);

impl ScriptAccessFactory for Factory {
    fn open(&self, invocation: Invocation) -> Box<dyn ScriptDocumentAccess> {
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        Box::new(Access {
            world: Arc::clone(&self.0),
            invocation,
        })
    }
}

struct Access {
    world: Arc<World>,
    invocation: Invocation,
}

fn nope() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "serverscript.store_failed",
        "not in the fake",
    )
}

impl ScriptDocumentAccess for Access {
    fn invocation(&self) -> &Invocation {
        &self.invocation
    }
    fn meta<'a>(&'a mut self, _d: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async { Err(nope()) })
    }
    fn get<'a>(&'a mut self, _d: &'a str, id: RecordId) -> BoxFuture<'a, AppResult<Document>> {
        let is_platform = matches!(self.invocation.actor, Actor::System { .. });
        self.world.actors.lock().unwrap().push(if is_platform {
            "platform".to_owned()
        } else {
            "user".to_owned()
        });
        let found = self
            .world
            .restricted
            .lock()
            .unwrap()
            .as_ref()
            .filter(|(key, _)| *key == id.to_string())
            .map(|(_, doc)| doc.clone());
        Box::pin(async move {
            if !is_platform {
                return Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "no read right on this document",
                ));
            }
            found.ok_or_else(|| AppError::new(ErrorClass::NotFound, "document.not_found", "none"))
        })
    }
    fn query<'a>(&'a mut self, _q: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async { Err(nope()) })
    }
    fn insert(&mut self, _d: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async { Err(nope()) })
    }
    fn update<'a>(
        &'a mut self,
        _d: &'a str,
        _i: RecordId,
        _p: DocumentPatch,
        _e: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(nope()) })
    }
    fn call<'a>(
        &'a mut self,
        _d: &'a str,
        _i: RecordId,
        method: &'a str,
        _a: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.world.writes.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(json!({ "called": method })) })
    }
}

struct Rig {
    world: Arc<World>,
    deps: KindDeps,
    audit: Arc<FakeElevationAudit>,
    wiring: ElevationWiring,
    secret: String,
}

fn rig() -> Rig {
    let world = Arc::new(World::default());
    let id = RecordId::new_v7();
    let mut fields = BTreeMap::new();
    fields.insert(n("rate"), Value::Text("42".to_owned()));
    let doc = Document::loaded(
        n("rate_table"),
        id,
        Version::FIRST,
        None,
        fields,
        BTreeMap::new(),
    );
    *world.restricted.lock().unwrap() = Some((id.to_string(), doc));
    let factory: Arc<dyn ScriptAccessFactory> = Arc::new(Factory(Arc::clone(&world)));
    let audit = Arc::new(FakeElevationAudit::new());
    let deps = KindDeps {
        engine: ScriptEngine::new(EngineConfig {
            surface: host_surface(),
            ..EngineConfig::default()
        }),
        access: Arc::clone(&factory),
        clock: Arc::new(FixedClock),
        store: Arc::new(FakeKindStore::new()),
        limits: ScriptLimits::default(),
        currencies: None,
    };
    Rig {
        wiring: ElevationWiring {
            factory,
            audit: audit.clone(),
        },
        world,
        deps,
        audit,
        secret: id.to_string(),
    }
}

const READ_RATE: &str = r#"export function on_call({ args }) {
  const doc = host.get({ doctype: "rate_table", id: args.id });
  return { rate: doc.rate };
}"#;

fn script(name: &str, source: &str, elevated: bool) -> ApiScript {
    ApiScript {
        record_id: None,
        name: name.to_owned(),
        module: "m".to_owned(),
        function: name.to_owned(),
        verbs: ApiVerbs::Post,
        arguments_schema: None,
        source: source.to_owned(),
        timeout_ms: 200,
        enabled: true,
        compiled_hash: None,
        disabled_reason: None,
        elevation: elevated.then(|| ElevationFlag {
            reason: "reads the restricted rate table for quotes".to_owned(),
            by: Some("admin".to_owned()),
            at: None,
        }),
    }
}

fn ctx(user: &RecordId) -> Context {
    Context::new("elev-test", Caller::User(user.to_string()))
}

// -------------------------------------------------------------------------------------------
// The run

#[tokio::test]
async fn an_ordinary_script_cannot_read_what_its_user_cannot() {
    let rig = rig();
    let handler = ApiScriptHandler::new(script("plain", READ_RATE, false), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    let error = handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect_err("denied");
    assert_eq!(error.details["cause"], "script.thrown");
    assert!(
        rig.audit.recorded().is_empty(),
        "nothing elevated, nothing audited"
    );
}

#[tokio::test]
async fn a_flagged_script_without_wiring_still_runs_as_its_user() {
    let rig = rig();
    let handler = ApiScriptHandler::new(script("flagged", READ_RATE, true), rig.deps.clone());
    let user = RecordId::new_v7();
    handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect_err("default deny: no wiring, no elevation");
    assert!(rig.audit.recorded().is_empty());
}

#[tokio::test]
async fn an_elevated_script_reads_what_the_user_cannot_and_is_audited() {
    let rig = rig();
    let handler = ApiScriptHandler::new(script("rates", READ_RATE, true), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    let out = handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect("the platform may read it");
    assert_eq!(out, json!({ "rate": "42" }));

    let runs = rig.audit.recorded();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].script, "rates");
    assert_eq!(runs[0].kind, ElevatedKind::Api);
    assert_eq!(runs[0].triggering_user, user.to_string());
    assert_eq!(runs[0].outcome, "ok");
    assert_eq!(runs[0].host_calls, 1);
    assert_eq!(runs[0].written_documents, 0);
    assert_eq!(rig.audit.reserved().len(), 1);
    // The audit never carries the script or what it read.
    let rendered = format!("{runs:?}");
    assert!(!rendered.contains("42") && !rendered.contains("export function"));
}

#[tokio::test]
async fn an_elevated_write_is_counted() {
    let rig = rig();
    let source = r#"export function on_call({ args }) {
      host.call({ doctype: "rate_table", id: args.id, method: "touch" });
    }"#;
    let handler = ApiScriptHandler::new(script("toucher", source, true), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect("runs");
    assert_eq!(rig.audit.recorded()[0].written_documents, 1);
}

#[tokio::test]
async fn failed_and_killed_elevated_runs_are_still_audited() {
    let rig = rig();
    let user = RecordId::new_v7();
    let failing = ApiScriptHandler::new(
        script(
            "boom",
            r#"export function on_call() { throw new Error("boom"); }"#,
            true,
        ),
        rig.deps.clone(),
    )
    .with_elevation(rig.wiring.clone());
    failing
        .call(&ctx(&user), json!({}))
        .await
        .expect_err("fails");
    let mut slow = script(
        "spin",
        "export function on_call() { while (true) {} }",
        true,
    );
    slow.timeout_ms = 50;
    let slow = ApiScriptHandler::new(slow, rig.deps.clone()).with_elevation(rig.wiring.clone());
    slow.call(&ctx(&user), json!({})).await.expect_err("killed");

    let runs = rig.audit.recorded();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].outcome, "script.thrown");
    assert_eq!(runs[1].outcome, LIMIT_EXCEEDED);
}

#[tokio::test]
async fn a_sink_that_cannot_reserve_stops_the_run_before_anything_is_written() {
    let rig = rig();
    rig.audit.set_fail_reserve(true);
    let source = r#"export function on_call({ args }) {
      host.call({ doctype: "rate_table", id: args.id, method: "touch" });
    }"#;
    let handler = ApiScriptHandler::new(script("toucher", source, true), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    let error = handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect_err("no audit, no elevated run");
    assert_eq!(error.code, ELEVATION_AUDIT_FAILED);
    assert_eq!(rig.world.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        rig.world.opened.load(Ordering::SeqCst),
        0,
        "no access was even opened"
    );
}

#[tokio::test]
async fn a_sink_that_cannot_record_fails_the_run() {
    let rig = rig();
    rig.audit.set_fail_record(true);
    let handler = ApiScriptHandler::new(script("rates", READ_RATE, true), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    let error = handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect_err("an unaudited run is a failed run");
    assert_eq!(error.code, ELEVATION_AUDIT_FAILED);
}

#[tokio::test]
async fn elevation_does_not_leak_into_the_next_run() {
    let rig = rig();
    let elevated = ApiScriptHandler::new(script("rates", READ_RATE, true), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let ordinary = ApiScriptHandler::new(script("plain", READ_RATE, false), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    elevated
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect("elevated reads");
    ordinary
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect_err("the next ordinary run has the user's rights");
}

#[tokio::test]
async fn script_code_cannot_choose_who_it_acts_as() {
    let rig = rig();
    let source = r#"export function on_call({ args }) {
      const attempts = [
        { doctype: "rate_table", id: args.id, user: "system" },
        { doctype: "rate_table", id: args.id, actor: "platform" },
        { doctype: "rate_table", id: args.id, as: "system", system: true },
      ];
      let reached = 0;
      for (const attempt of attempts) {
        try { host.get(attempt); reached += 1; } catch (e) {}
      }
      return { reached };
    }"#;
    let handler = ApiScriptHandler::new(script("sneaky", source, false), rig.deps.clone())
        .with_elevation(rig.wiring.clone());
    let user = RecordId::new_v7();
    let out = handler
        .call(&ctx(&user), json!({ "id": rig.secret }))
        .await
        .expect("the script runs");
    assert_eq!(out, json!({ "reached": 0 }));
    assert!(
        rig.world.actors.lock().unwrap().iter().all(|a| a == "user"),
        "only the user ever touched the document"
    );
}

// -------------------------------------------------------------------------------------------
// The save rules

fn doc(fields: &[(&str, Value)]) -> Document {
    let map: BTreeMap<Name, Value> = fields.iter().map(|(k, v)| (n(k), v.clone())).collect();
    Document::loaded(
        n("api_script"),
        RecordId::new_v7(),
        Version::FIRST,
        None,
        map,
        BTreeMap::new(),
    )
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

const REASON: &str = "reads the restricted rate table for quotes";

fn saver(may: bool) -> Saver {
    Saver {
        user: "someone".to_owned(),
        may_elevate: may,
        at: "2026-11-13T10:00:00Z".to_owned(),
    }
}

fn elevated_doc(source: &str, reason: &str) -> Document {
    doc(&[
        ("script", text(source)),
        (FIELD_ELEVATED, Value::Bool(true)),
        (FIELD_ELEVATION_REASON, text(reason)),
    ])
}

#[test]
fn a_user_without_the_permission_cannot_turn_elevation_on() {
    let error = check_save(&elevated_doc("a", REASON), None, &saver(false), false).unwrap_err();
    assert_eq!(error.fields[0].code, ELEVATION_FORBIDDEN);
    let previous = doc(&[("script", text("a"))]);
    let error = check_save(
        &elevated_doc("a", REASON),
        Some(&previous),
        &saver(false),
        false,
    )
    .unwrap_err();
    assert_eq!(error.fields[0].code, ELEVATION_FORBIDDEN);
}

#[test]
fn a_holder_elevates_with_a_reason_and_the_save_is_stamped() {
    let plan = check_save(&elevated_doc("a", REASON), None, &saver(true), false).unwrap();
    assert!(plan.elevated && !plan.reset);
    assert_eq!(
        plan.stamp,
        Some(("someone".to_owned(), "2026-11-13T10:00:00Z".to_owned()))
    );
    let mut saved = elevated_doc("a", REASON);
    apply_plan(&mut saved, &plan).unwrap();
    assert!(
        read_flag(&saved)
            .unwrap()
            .is_some_and(|f| f.by.as_deref() == Some("someone"))
    );
}

#[test]
fn the_reason_must_be_twenty_to_five_hundred_characters() {
    for reason in ["", "too short", &"x".repeat(501)] {
        let error = check_save(&elevated_doc("a", reason), None, &saver(true), false).unwrap_err();
        assert_eq!(
            error.fields[0].code, ELEVATION_REASON_REQUIRED,
            "{reason:?}"
        );
    }
    check_save(
        &elevated_doc("a", &"x".repeat(500)),
        None,
        &saver(true),
        false,
    )
    .unwrap();
}

#[test]
fn editing_the_source_without_the_permission_resets_the_flag() {
    let previous = elevated_doc("old source", REASON);
    let plan = check_save(
        &elevated_doc("new source", REASON),
        Some(&previous),
        &saver(false),
        false,
    )
    .unwrap();
    assert!(!plan.elevated && plan.reset);
    let mut saved = elevated_doc("new source", REASON);
    apply_plan(&mut saved, &plan).unwrap();
    assert!(read_flag(&saved).unwrap().is_none());
}

#[test]
fn nobody_without_the_permission_can_change_the_reason_of_an_elevated_script() {
    let previous = elevated_doc("same", REASON);
    let error = check_save(
        &elevated_doc("same", "a different but still long enough reason"),
        Some(&previous),
        &saver(false),
        false,
    )
    .unwrap_err();
    assert_eq!(error.fields[0].code, ELEVATION_FORBIDDEN);
    // Turning it off needs no permission.
    let off = doc(&[
        ("script", text("same")),
        (FIELD_ELEVATED, Value::Bool(false)),
    ]);
    assert!(
        !check_save(&off, Some(&previous), &saver(false), false)
            .unwrap()
            .elevated
    );
}

#[test]
fn a_scheduled_script_carrying_the_flag_is_refused() {
    let error = check_save(&elevated_doc("a", REASON), None, &saver(true), true).unwrap_err();
    assert_eq!(error.fields[0].code, ELEVATION_NOT_ALLOWED);
    let mut fields = BTreeMap::new();
    for (k, v) in [
        ("name", text("nightly")),
        ("cron", text("0 0 2 * * *")),
        ("run_as", text("batch")),
        ("script", text("export function on_run() {}")),
        (FIELD_ELEVATED, Value::Bool(true)),
        (FIELD_ELEVATION_REASON, text(REASON)),
    ] {
        fields.insert(n(k), v);
    }
    let record = Document::loaded(
        n("scheduled_script"),
        RecordId::new_v7(),
        Version::FIRST,
        None,
        fields,
        BTreeMap::new(),
    );
    let error = ScheduledScript::from_document(&record).unwrap_err();
    assert_eq!(error.fields[0].code, ELEVATION_NOT_ALLOWED);
    let _ = MissedRun::Skip;
}

// -------------------------------------------------------------------------------------------
// No other path

#[test]
fn only_the_elevated_access_acts_as_the_platform_and_it_needs_the_marker() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut implementors = Vec::new();
    let mut elevation_constructions = 0;
    let mut stack = vec![dir];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).unwrap() {
            let entry = entry.unwrap().path();
            if entry.is_dir() {
                stack.push(entry);
            } else if entry.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&entry).unwrap();
                for line in text.lines() {
                    if let Some(rest) = line.trim().strip_prefix("impl ScriptDocumentAccess for ") {
                        implementors.push(rest.split([' ', '<', '{']).next().unwrap().to_owned());
                    }
                    if line.contains("Elevation(())") && !line.contains("pub struct") {
                        elevation_constructions += 1;
                    }
                }
            }
        }
    }
    implementors.sort();
    assert_eq!(
        implementors,
        vec!["ElevatedAccess", "HookAccess", "ServiceAccess"],
        "a new access must be reviewed against the elevation rules"
    );
    assert_eq!(
        elevation_constructions, 1,
        "the Elevation marker is built in exactly one place (Elevation::granted)"
    );
    let source = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/elevation.rs"),
    )
    .unwrap();
    assert!(source.contains("pub(crate) fn granted"));
    assert!(source.contains("_proof: &Elevation"));
}
