//! The script corpus (WP-EXT-06.T9): thirty document-event, API and scheduled scripts of the kinds
//! consultants write, run through the real engine on the in-memory backend, each with a golden
//! outcome in its `case.toml`. See the project process notes for the format and for how to
//! add a case.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{Clock, Name, OffsetDateTime, RecordId, Value};
use spot_app_api::{Actor, AppError, AppResult, BoxFuture, Document, ErrorClass, Invocation};
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_functions::{Caller, Context, FunctionHandler};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_scheduler::{RunSlot, SlotRef};
use spot_server_scripts::elevation::{ElevationFlag, ElevationWiring, FakeElevationAudit};
use spot_server_scripts::kinds::{
    ApiScript, ApiScriptHandler, ApiVerbs, FakeKindStore, KindDeps, MissedRun, ScheduledScript,
    ScheduledScriptRunner, UserDirectory, UserRecord,
};
use spot_server_scripts::{
    FakeScriptStore, MethodCaller, ScriptAccessFactory, ScriptController, ScriptDocumentAccess,
    ScriptEvent, ScriptLimits, ScriptLog, ScriptQueries, ServerScript, ServiceAccessFactory,
    host_surface, value_json,
};
use spot_storage_contract::{
    Collation, Expr, Isolation, Query, QueryExecutor, QueryPath, Rows, Selection, StorageBackend,
    TextMatch, TextMode, Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;
use spot_ts::{EngineConfig, ScriptEngine};

// -------------------------------------------------------------------------------------------
// The world every case runs in

const DECIMALS: [&str; 8] = [
    "qty",
    "rate",
    "discount",
    "amount",
    "tax_rate",
    "total",
    "credit_limit",
    "outstanding",
];

fn n(name: &str) -> Name {
    Name::new(name).expect("a storage name")
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

fn decimal(name: &str) -> FieldSpec {
    let mut spec = FieldSpec::new(name, FieldType::Decimal);
    spec.scale = Some(2);
    spec
}

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut order = Definition::new("sales_order");
    for field in [
        "title",
        "customer",
        "order_date",
        "delivery_date",
        "status",
        "price_list",
        "address",
        "notes",
    ] {
        order.fields.push(text(field));
    }
    for field in ["qty", "rate", "discount", "amount", "tax_rate", "total"] {
        order.fields.push(decimal(field));
    }
    let mut customer = Definition::new("customer");
    for field in ["name", "price_list", "address", "tier"] {
        customer.fields.push(text(field));
    }
    customer.fields.push(decimal("credit_limit"));
    customer.fields.push(decimal("outstanding"));
    let mut shipment = Definition::new("shipment");
    shipment.fields.push(text("sales_order"));
    let mut log = Definition::new("log_entry");
    log.fields.push(text("message"));
    log.fields.push(text("reference"));
    let mut rates = Definition::new("rate_table");
    rates.fields.push(text("region"));
    rates.fields.push(decimal("rate"));
    Registry::new(vec![order, customer, shipment, log, rates])
        .expect("the corpus definitions compile")
        .effective()
}

fn system() -> Invocation {
    Invocation {
        site: SiteId::new("corpus-test").expect("a site id"),
        actor: Actor::System {
            component: n("corpus"),
        },
        command_id: None,
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

fn as_user(id: RecordId) -> Invocation {
    Invocation {
        actor: Actor::User { id, via: None },
        ..system()
    }
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

fn script_engine() -> ScriptEngine {
    ScriptEngine::new(EngineConfig {
        surface: host_surface(),
        ..EngineConfig::default()
    })
}

/// Counts `host.call`s of the document-event path.
#[derive(Default)]
struct CountingCaller(AtomicU32);

impl MethodCaller for CountingCaller {
    fn call<'a>(
        &'a self,
        _inv: &'a Invocation,
        _doctype: &'a str,
        _id: RecordId,
        method: &'a str,
        _args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok(json!({ "called": method })) })
    }
}

/// `host.query` is not offered to API and scheduled scripts (the engine has no query for an
/// invocation); a case that needs it says so with `unsupported`.
struct NoQueries;

impl ScriptQueries for NoQueries {
    fn query<'a>(
        &'a self,
        _inv: &'a Invocation,
        _query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async {
            Err(AppError::new(
                ErrorClass::Internal,
                "serverscript.store_failed",
                "host.query is not available to this kind of script",
            ))
        })
    }
}

/// Reading `rate_table` is denied to every user and open to the platform: the restriction an
/// elevated script gets past and an ordinary one does not.
struct Restricted {
    inner: Arc<dyn ScriptAccessFactory>,
}

struct RestrictedAccess {
    inner: Box<dyn ScriptDocumentAccess>,
}

impl ScriptAccessFactory for Restricted {
    fn open(&self, invocation: Invocation) -> Box<dyn ScriptDocumentAccess> {
        Box::new(RestrictedAccess {
            inner: self.inner.open(invocation),
        })
    }
}

impl ScriptDocumentAccess for RestrictedAccess {
    fn invocation(&self) -> &Invocation {
        self.inner.invocation()
    }
    fn can_call(&self) -> bool {
        self.inner.can_call()
    }
    fn meta<'a>(
        &'a mut self,
        doctype: &'a str,
    ) -> BoxFuture<'a, AppResult<Arc<spot_app_api::DocTypeDef>>> {
        self.inner.meta(doctype)
    }
    fn get<'a>(&'a mut self, doctype: &'a str, id: RecordId) -> BoxFuture<'a, AppResult<Document>> {
        let user = matches!(self.inner.invocation().actor, Actor::User { .. });
        if user && doctype == "rate_table" {
            return Box::pin(async {
                Err(AppError::new(
                    ErrorClass::PermissionDenied,
                    "permission.denied",
                    "no read right on this document",
                ))
            });
        }
        self.inner.get(doctype, id)
    }
    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        self.inner.query(query)
    }
    fn insert(&mut self, doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        self.inner.insert(doc)
    }
    fn update<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        patch: spot_app_api::DocumentPatch,
        expected: spot_app_api::prelude::Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        self.inner.update(doctype, id, patch, expected)
    }
    fn call<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.inner.call(doctype, id, method, args)
    }
}

struct Users(Vec<(String, UserRecord)>);

impl UserDirectory for Users {
    fn find<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<Option<UserRecord>>> {
        let found = self
            .0
            .iter()
            .find(|(u, _)| u == name)
            .map(|(_, r)| r.clone());
        Box::pin(async move { Ok(found) })
    }
}

// -------------------------------------------------------------------------------------------
// Cases

struct Case {
    name: String,
    spec: Json,
    source: String,
}

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/corpus")
}

fn discover() -> Vec<Case> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(corpus_dir())
        .expect("the corpus directory exists")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| {
            let name = dir.file_name().unwrap().to_string_lossy().into_owned();
            let spec_text = std::fs::read_to_string(dir.join("case.toml"))
                .unwrap_or_else(|e| panic!("{name}: no case.toml: {e}"));
            let table: toml::Table =
                toml::from_str(&spec_text).unwrap_or_else(|e| panic!("{name}: case.toml: {e}"));
            let spec = serde_json::to_value(&table).expect("TOML is JSON-shaped");
            let source = std::fs::read_to_string(dir.join("script.ts"))
                .unwrap_or_else(|e| panic!("{name}: no script.ts: {e}"));
            Case { name, spec, source }
        })
        .collect()
}

/// What running a case produced, in the terms `[expect]` talks about.
#[derive(Clone, Debug, PartialEq)]
struct Outcome {
    ok: bool,
    error_code: Option<String>,
    error_fields: Vec<String>,
    fields: BTreeMap<String, Json>,
    result: Option<Json>,
    host_calls: u32,
    method_calls: u32,
    documents: Vec<(String, String, String, usize)>,
    audit_entries: usize,
    message: Option<String>,
    /// The message of the refusal, for the failure report.
    error_message: Option<String>,
}

impl Outcome {
    fn empty() -> Outcome {
        Outcome {
            ok: false,
            error_code: None,
            error_fields: Vec::new(),
            fields: BTreeMap::new(),
            result: None,
            host_calls: 0,
            method_calls: 0,
            documents: Vec::new(),
            audit_entries: 0,
            message: None,
            error_message: None,
        }
    }

    fn fail(&mut self, error: &AppError) {
        self.ok = false;
        self.error_code = Some(error.code.clone());
        self.error_fields = error.fields.iter().map(|f| f.field.to_string()).collect();
        self.error_message = Some(error.message.clone());
    }
}

struct World {
    engine: Arc<Engine<FakeBackend>>,
    meta: Arc<spot_meta::EffectiveMeta>,
    keys: BTreeMap<String, RecordId>,
}

fn substitute(value: &Json, keys: &BTreeMap<String, RecordId>) -> Json {
    match value {
        Json::String(s) => {
            let mut out = s.clone();
            for (key, id) in keys {
                out = out.replace(&format!("${{{key}}}"), &id.to_string());
            }
            Json::String(out)
        }
        Json::Array(items) => Json::Array(items.iter().map(|v| substitute(v, keys)).collect()),
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute(v, keys)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn build_doc(doctype: &str, id: RecordId, fields: &Json) -> Document {
    let mut doc = Document::new(n(doctype), id);
    if let Some(map) = fields.as_object() {
        for (name, value) in map {
            let stored = match value {
                Json::String(s) if DECIMALS.contains(&name.as_str()) => {
                    Value::Decimal(s.parse().expect("a decimal string"))
                }
                Json::String(s) => Value::Text(s.clone()),
                Json::Bool(b) => Value::Bool(*b),
                Json::Number(num) => Value::Int(num.as_i64().expect("an integer")),
                other => panic!("unsupported field value {other}"),
            };
            doc.set(n(name), stored);
        }
    }
    doc
}

async fn count_docs(world: &World, doctype: &str, field: &str, value: &str) -> usize {
    let catalog = Arc::new(
        world
            .meta
            .catalog(Collation::new("und").expect("a collation"))
            .expect("the catalog compiles"),
    );
    let entity = n(doctype);
    let alias = n("t");
    let id = n("id");
    let mut query = Query::from(&entity, &alias, QueryPath::Report);
    query.select.push(Selection {
        expr: Expr::field(&alias, &id),
        alias: id,
    });
    query.filter = Some(Expr::Text(
        Box::new(Expr::field(&alias, &n(field))),
        TextMatch {
            mode: TextMode::Equals,
            pattern: value.to_owned(),
            case_insensitive: false,
        },
    ));
    let options = TxOptions::new(n("corpus_count"), Isolation::ReadCommitted).read_only();
    let mut tx = world
        .engine
        .backend()
        .begin(catalog, options)
        .await
        .expect("a transaction");
    let rows = tx.query(&query).await.expect("the query runs");
    let _ = tx.rollback().await;
    rows.rows.len()
}

async fn world(case: &Case, controller: Option<Arc<ScriptController>>) -> World {
    let meta = meta();
    let mut registry = ControllerRegistry::new();
    if let Some(controller) = controller {
        registry.register("server_script", controller, ControllerSource::TypeScript);
    }
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
    let engine = Arc::new(engine);
    let mut keys = BTreeMap::new();
    for before in case.spec["before"].as_array().into_iter().flatten() {
        let key = before["key"]
            .as_str()
            .expect("a seed needs a key")
            .to_owned();
        let id = RecordId::new_v7();
        keys.insert(key, id);
        let doctype = before["doctype"].as_str().expect("a seed needs a doctype");
        let doc = build_doc(doctype, id, &substitute(&before["fields"], &keys));
        engine.create(&system(), doc).await.expect("the seed saves");
    }
    World { engine, meta, keys }
}

async fn check_documents(world: &World, case: &Case, out: &mut Outcome) {
    for expected in case.spec["expect"]["documents"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let doctype = expected["doctype"].as_str().expect("doctype");
        let field = expected["field"].as_str().expect("field");
        let value = substitute(&expected["value"], &world.keys);
        let value = value.as_str().expect("a text value").to_owned();
        let found = count_docs(world, doctype, field, &value).await;
        out.documents
            .push((doctype.to_owned(), field.to_owned(), value, found));
    }
}

async fn run_doc_event(case: &Case) -> Outcome {
    let spec = &case.spec;
    let event = ScriptEvent::parse(spec["event"].as_str().expect("event")).expect("a known event");
    let script = ServerScript {
        record_id: None,
        name: "corpus".to_owned(),
        doctype: "sales_order".to_owned(),
        event,
        source: case.source.clone(),
        compiled_hash: None,
        timeout_ms: spec["timeout_ms"].as_u64().unwrap_or(200),
        priority: 0,
        enabled: true,
        disabled_reason: None,
    };
    let store = Arc::new(FakeScriptStore::with_scripts([script.clone()]));
    let log = Arc::new(ScriptLog::new(16));
    let caller = Arc::new(CountingCaller::default());
    let controller = Arc::new(
        ScriptController::new("sales_order", vec![script], script_engine(), store)
            .expect("the controller builds")
            .with_limits(ScriptLimits::default())
            .with_log(Arc::clone(&log))
            .with_method_caller(Arc::clone(&caller) as Arc<dyn MethodCaller>),
    );
    let world = world(case, Some(controller)).await;
    let id = RecordId::new_v7();
    let user = RecordId::new_v7();
    let doc = build_doc(
        "sales_order",
        id,
        &substitute(&spec["input"]["fields"], &world.keys),
    );
    let mut out = Outcome::empty();
    match world.engine.create(&as_user(user), doc).await {
        Ok(_) => {
            out.ok = true;
            if let Some(saved) = world.engine.peek("sales_order", id).await.expect("peek") {
                for name in case.spec["expect"]["fields"]
                    .as_object()
                    .map(|m| m.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default()
                {
                    out.fields
                        .insert(name.clone(), value_json(saved.get(&name)));
                }
            }
        }
        Err(error) => out.fail(&error),
    }
    out.host_calls = log.runs().iter().map(|r| r.host_calls).sum();
    out.method_calls = caller.0.load(Ordering::SeqCst);
    check_documents(&world, case, &mut out).await;
    out
}

fn kind_deps(
    world: &World,
    restricted: bool,
    limits: ScriptLimits,
) -> (KindDeps, Arc<dyn ScriptAccessFactory>) {
    let service: Arc<dyn ScriptAccessFactory> = Arc::new(ServiceAccessFactory::new(
        Arc::clone(&world.engine) as Arc<dyn DocumentService>,
        Arc::new(NoQueries),
    ));
    let factory: Arc<dyn ScriptAccessFactory> = if restricted {
        Arc::new(Restricted { inner: service })
    } else {
        service
    };
    let deps = KindDeps {
        engine: script_engine(),
        access: Arc::clone(&factory),
        clock: Arc::new(FixedClock),
        store: Arc::new(FakeKindStore::new()),
        limits,
        currencies: None,
    };
    (deps, factory)
}

async fn run_api(case: &Case) -> Outcome {
    let spec = &case.spec;
    let world = world(case, None).await;
    let restricted = spec["restricted"].as_bool().unwrap_or(false);
    let (deps, factory) = kind_deps(&world, restricted, ScriptLimits::default());
    let elevated = spec["elevated"].as_bool().unwrap_or(false);
    let script = ApiScript {
        record_id: None,
        name: "corpus".to_owned(),
        module: "corpus".to_owned(),
        function: "run".to_owned(),
        verbs: ApiVerbs::Post,
        arguments_schema: spec.get("schema").map(|s| substitute(s, &world.keys)),
        source: case.source.clone(),
        timeout_ms: spec["timeout_ms"].as_u64().unwrap_or(200),
        enabled: true,
        compiled_hash: None,
        disabled_reason: None,
        elevation: elevated.then(|| ElevationFlag {
            reason: "the corpus reads the restricted rate table".to_owned(),
            by: None,
            at: None,
        }),
    };
    let audit = Arc::new(FakeElevationAudit::new());
    let mut handler = ApiScriptHandler::new(script, deps);
    if elevated {
        handler = handler.with_elevation(ElevationWiring {
            factory,
            audit: audit.clone(),
        });
    }
    let user = RecordId::new_v7();
    let args = substitute(&spec["input"]["args"], &world.keys);
    let mut out = Outcome::empty();
    match handler
        .call(
            &Context::new("corpus-test", Caller::User(user.to_string())),
            args,
        )
        .await
    {
        Ok(result) => {
            out.ok = true;
            out.result = Some(result);
        }
        Err(error) => out.fail(&error),
    }
    out.audit_entries = audit.recorded().len();
    check_documents(&world, case, &mut out).await;
    out
}

async fn run_scheduled(case: &Case) -> Outcome {
    let spec = &case.spec;
    let world = world(case, None).await;
    let (deps, _) = kind_deps(&world, false, ScriptLimits::default());
    let store = Arc::new(FakeKindStore::new());
    let deps = KindDeps {
        store: store.clone(),
        ..deps
    };
    let run_as = UserRecord {
        id: RecordId::new_v7(),
        enabled: true,
    };
    let script = ScheduledScript {
        record_id: None,
        name: "corpus".to_owned(),
        cron: spot_manifest::Cron::parse("0 0 2 * * *").expect("a cron"),
        time_zone: "UTC".to_owned(),
        missed_run: MissedRun::Skip,
        queue: spot_manifest::Queue::Default,
        run_as: "batch".to_owned(),
        source: case.source.clone(),
        timeout_ms: spec["timeout_ms"].as_u64().unwrap_or(200),
        enabled: true,
        compiled_hash: None,
        disabled_reason: None,
    };
    let runner = ScheduledScriptRunner::new(
        SiteId::new("corpus-test").expect("a site id"),
        vec![script],
        deps,
        Arc::new(Users(vec![("batch".to_owned(), run_as)])),
    );
    let slot = RunSlot {
        schedule: "corpus".to_owned(),
        scheduled_for: jiff::Timestamp::from_second(1_700_000_000).expect("a timestamp"),
        deduplication_key: "scripting:corpus:2023-11-14T22:13:20Z".to_owned(),
        slot_ref: SlotRef::new("scripting", "corpus"),
    };
    let mut out = Outcome::empty();
    match runner.run("corpus", &slot).await {
        Ok(run) => {
            out.ok = true;
            out.host_calls = run.host_calls;
            out.message = run.message;
        }
        Err(error) => out.fail(&error),
    }
    check_documents(&world, case, &mut out).await;
    out
}

async fn run_case(case: &Case) -> Outcome {
    match case.spec["kind"].as_str().expect("a case needs a kind") {
        "doc_event" => run_doc_event(case).await,
        "api" => run_api(case).await,
        "scheduled" => run_scheduled(case).await,
        other => panic!("{}: unknown kind {other}", case.name),
    }
}

fn check(case: &Case, out: &Outcome) -> Vec<String> {
    let expect = &case.spec["expect"];
    let mut problems = Vec::new();
    let mut said = false;
    if let Some(ok) = expect["ok"].as_bool() {
        said = true;
        if out.ok != ok {
            problems.push(format!(
                "ok: expected {ok}, got {} ({:?} {:?} {:?})",
                out.ok, out.error_code, out.error_message, out.error_fields
            ));
        }
    }
    if let Some(code) = expect["error_code"].as_str() {
        said = true;
        if out.error_code.as_deref() != Some(code) {
            problems.push(format!(
                "error_code: expected {code}, got {:?}",
                out.error_code
            ));
        }
    }
    if let Some(prefix) = expect["error_code_prefix"].as_str() {
        said = true;
        if !out
            .error_code
            .as_deref()
            .is_some_and(|c| c.starts_with(prefix))
        {
            problems.push(format!(
                "error_code: expected prefix {prefix}, got {:?}",
                out.error_code
            ));
        }
    }
    if let Some(fields) = expect["error_fields"].as_array() {
        said = true;
        let wanted: Vec<String> = fields
            .iter()
            .filter_map(|f| f.as_str().map(str::to_owned))
            .collect();
        if out.error_fields != wanted {
            problems.push(format!(
                "error_fields: expected {wanted:?}, got {:?}",
                out.error_fields
            ));
        }
    }
    if let Some(fields) = expect["fields"].as_object() {
        said = true;
        for (name, value) in fields {
            if out.fields.get(name) != Some(value) {
                problems.push(format!(
                    "field {name}: expected {value}, got {:?}",
                    out.fields.get(name)
                ));
            }
        }
    }
    if let Some(result) = expect.get("result") {
        said = true;
        if out.result.as_ref() != Some(result) {
            problems.push(format!("result: expected {result}, got {:?}", out.result));
        }
    }
    if let Some(calls) = expect["host_calls"].as_u64() {
        said = true;
        if u64::from(out.host_calls) != calls {
            problems.push(format!(
                "host_calls: expected {calls}, got {}",
                out.host_calls
            ));
        }
    }
    if let Some(calls) = expect["method_calls"].as_u64() {
        said = true;
        if u64::from(out.method_calls) != calls {
            problems.push(format!(
                "method_calls: expected {calls}, got {}",
                out.method_calls
            ));
        }
    }
    if let Some(entries) = expect["audit_entries"].as_u64() {
        said = true;
        if out.audit_entries as u64 != entries {
            problems.push(format!(
                "audit_entries: expected {entries}, got {}",
                out.audit_entries
            ));
        }
    }
    if let Some(message) = expect["message"].as_str() {
        said = true;
        if out.message.as_deref() != Some(message) {
            problems.push(format!(
                "message: expected {message}, got {:?}",
                out.message
            ));
        }
    }
    for (index, expected) in expect["documents"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        said = true;
        let count = expected["count"].as_u64().unwrap_or(1) as usize;
        match out.documents.get(index) {
            Some((_, _, _, found)) if *found == count => {}
            other => problems.push(format!(
                "documents[{index}]: expected {count}, got {other:?}"
            )),
        }
    }
    if !said {
        problems.push("the case states no expectation".to_owned());
    }
    problems
}

// -------------------------------------------------------------------------------------------
// The suite

#[test]
fn the_corpus_has_thirty_cases_and_ten_negative_ones() {
    let cases = discover();
    assert!(cases.len() >= 30, "{} cases", cases.len());
    let negative = cases
        .iter()
        .filter(|c| c.spec["negative"].as_bool().unwrap_or(false))
        .count();
    assert!(negative >= 10, "{negative} negative cases");
    for case in &cases {
        for key in ["kind", "pattern"] {
            assert!(case.spec.get(key).is_some(), "{}: no `{key}`", case.name);
        }
        assert!(
            case.spec.get("expect").is_some(),
            "{}: no [expect]",
            case.name
        );
    }
}

#[tokio::test]
async fn every_case_meets_its_golden_outcome() {
    let mut failures = Vec::new();
    for case in discover() {
        let out = run_case(&case).await;
        let problems = check(&case, &out);
        if !problems.is_empty() {
            failures.push(format!("{}: {}", case.name, problems.join("; ")));
        }
    }
    assert!(
        failures.is_empty(),
        "corpus failures:\n{}",
        failures.join("\n")
    );
}

#[tokio::test]
async fn an_unsupported_case_documents_what_is_missing() {
    for case in discover() {
        if let Some(why) = case.spec.get("unsupported") {
            assert!(
                why.as_str().is_some_and(|w| w.len() > 10),
                "{}: `unsupported` must say what is missing",
                case.name
            );
        }
    }
}

#[tokio::test]
async fn a_run_repeated_twenty_times_gives_the_same_result_and_host_call_count() {
    let cases = discover();
    for name in [
        "derive_fields_from_customer",
        "line_and_document_totals",
        "api_calculation_endpoint",
    ] {
        let case = cases
            .iter()
            .find(|c| c.name == name)
            .expect("the case exists");
        let first = run_case(case).await;
        for _ in 0..19 {
            assert_eq!(run_case(case).await, first, "{name} is not deterministic");
        }
    }
}

#[tokio::test]
async fn the_clock_and_the_random_seed_come_from_the_invocation() {
    let source = "export function on_call() { return { now: Date.now(), roll: Math.random() }; }";
    let case = Case {
        name: "clock".to_owned(),
        spec: json!({ "kind": "api", "pattern": "clock", "input": { "args": {} }, "expect": { "ok": true } }),
        source: source.to_owned(),
    };
    let first = run_case(&case).await;
    let second = run_case(&case).await;
    assert!(first.ok);
    assert_eq!(
        first.result, second.result,
        "the same invocation fixes both values"
    );
    assert_eq!(
        first.result.as_ref().unwrap()["now"],
        json!(0),
        "the injected clock is the epoch"
    );
}

#[tokio::test]
async fn concurrent_scripts_do_not_see_each_others_globals() {
    let write = Case {
        name: "writer".to_owned(),
        spec: json!({ "kind": "api", "pattern": "isolation", "input": { "args": {} }, "expect": { "ok": true } }),
        source: "export function on_call() { globalThis.leak = \"A\"; return { mine: globalThis.leak }; }".to_owned(),
    };
    let read = Case {
        name: "reader".to_owned(),
        spec: json!({ "kind": "api", "pattern": "isolation", "input": { "args": {} }, "expect": { "ok": true } }),
        source: "export function on_call() { return { seen: typeof globalThis.leak }; }".to_owned(),
    };
    let (a, b) = tokio::join!(run_case(&write), run_case(&read));
    assert_eq!(a.result, Some(json!({ "mine": "A" })));
    assert_eq!(b.result, Some(json!({ "seen": "undefined" })));
    let again = run_case(&read).await;
    assert_eq!(
        again.result,
        Some(json!({ "seen": "undefined" })),
        "a later run is clean too"
    );
}
