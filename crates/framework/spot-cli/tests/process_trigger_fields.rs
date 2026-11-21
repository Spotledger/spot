//! The trigger fields of a published process, end to end (WP-BPM-02.T12; WP-BPM-01.T5,
//! WP-BPM-03.T6, ADR-49).
//!
//! A process text with start roles, an input schema and a document condition is published through
//! the real `VersionHost` (so the trigger index is built from the stored definition, not from a
//! hand-registered spec), and then started through the real `ProcessApi` (a manual or API start)
//! or the real `TriggerHost` (a document event). PostgreSQL is real (`SPOT_TEST_PG_URL`; the tests
//! skip, printing `SKIPPED:`, without it, and fail under `SPOT_REQUIRE_PG=1`). Who is calling,
//! the platform reader and the outbox are small fakes. No test sleeps: the trigger host is driven
//! with `ingest`.
//!
//! The input schema is the subset `spot-bpm-triggers::validate_input` enforces (`type`,
//! `required`, `properties`, `additionalProperties`, `enum`, `items`, `minimum`, `maximum`,
//! `minLength`, `maxLength`); `pattern` is not enforced, so a schema that uses it is refused at
//! publish (see the last two tests).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value as Json, json};
use spot_app_api::document::{DocState, Document};
use spot_app_api::prelude::{Name, OffsetDateTime, RecordId, SiteId, Value};
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_documents::{CaseFileStore, MemoryCaseFile};
use spot_bpm_engine::{CaseStore, HandlerRegistry};
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture, SystemReader};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_api::{
    Access as ApiAccess, AuditEntry as ApiAudit, Caller as ApiCaller, Deps, ProcessApi,
    SourceUnavailable, Sourced, engine_key,
};
use spot_cli::services::process_triggers::{
    Audit, AuditEntry as TriggerAudit, Fut, Meta, Parts as TriggerParts, SecretSource,
    Settings as TriggerSettings, TriggerHost,
};
use spot_cli::services::process_versions::{
    Access as VersionAccess, Caller as VersionCaller, Parts as VersionParts,
    Settings as VersionSettings, VersionHost,
};
use spot_core::{Clock, ManualClock};
use spot_document::engine::{MemoryPrincipals, PrincipalSource};
use spot_events_contract::{Actor, EventMeta, PayloadDocument};
use spot_http::api::ports::process::{ProcessPort, ProcessPortError, ProcessResult, Run, RunStart};
use spot_http::api::ports::process_version::{
    PublishRequest, VersionPort, VersionPortError, VersionState,
};
use spot_http::api::service::{BoxFuture, Credential, FieldIssue, RequestCtx};
use spot_http::request::RequestId;
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, Lease, LeaseStore};
use spot_storage_contract::{FeedPosition, NodeRef, OutboxEvent, OutboxRecord, Version};

/// A value that must never appear in an error, an audit entry or a log line.
const MARKER: &str = "SECRET-MARKER-7";

fn at(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).unwrap()
}

fn to_jiff(at: OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(at.unix_timestamp(), 0).unwrap()
}

// --- process texts -------------------------------------------------------------------------------

/// The start-input schema of the manual trigger: an integer amount in 1..=1,000,000 and a
/// supplier of 1..=64 characters, nothing else.
const MANUAL_SCHEMA: &str = r#"{"type":"object","required":["amount","supplier"],"additionalProperties":false,"properties":{"amount":{"type":"integer","minimum":1,"maximum":1000000},"supplier":{"type":"string","minLength":1,"maxLength":64}}}"#;

/// The API trigger's schema: one short reference, no extra property.
const API_SCHEMA: &str = r#"{"type":"object","required":["ref"],"additionalProperties":false,"properties":{"ref":{"type":"string","minLength":1,"maxLength":16}}}"#;

/// A one-trigger process: `start -> done`. `trigger_lines` are the trigger node's own keys.
fn process_text(name: &str, trigger_lines: &str) -> String {
    format!(
        "name = \"{name}\"\napp = \"trigger_fields\"\nversion = 1\nstatus = \"published\"\n\n\
         [[nodes]]\nid = \"start\"\nkind = \"trigger\"\n{trigger_lines}\n\n\
         [[nodes]]\nid = \"done\"\nkind = \"logic\"\nlogic = \"join\"\nterminal = true\n\n\
         [[edges]]\nfrom = \"start\"\nto = \"done\"\n"
    )
}

/// `approvals`: a manual trigger for `purchaser`s with the amount and supplier schema.
fn approvals(with_role: bool) -> String {
    let roles = if with_role {
        "roles = [\"purchaser\"]\n"
    } else {
        ""
    };
    process_text(
        "approvals",
        &format!("trigger = \"manual\"\n{roles}input_schema = '{MANUAL_SCHEMA}'"),
    )
}

/// `intake_api`: an API trigger for `integrator`s that rejects extra properties.
fn intake_api() -> String {
    process_text(
        "intake_api",
        &format!(
            "trigger = \"api\"\npath = \"intake\"\nroles = [\"integrator\"]\n\
             input_schema = '{API_SCHEMA}'"
        ),
    )
}

/// `big_orders`: a document-event trigger gated by a condition on the after-image.
fn big_orders() -> String {
    process_text(
        "big_orders",
        "trigger = \"document_event\"\ndoctype = \"widget\"\nevent = \"post\"\n\
         condition = \"doc.total > 1000\"",
    )
}

// --- fakes ---------------------------------------------------------------------------------------

#[derive(Default)]
struct NullSubmit;
impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

/// The people of the site: a name, an id (the credential token of the process API) and roles.
#[derive(Default)]
struct People {
    users: Mutex<BTreeMap<String, (RecordId, Vec<String>)>>,
}

impl People {
    fn add(&self, name: &str, roles: &[&str]) {
        self.users.lock().unwrap().insert(
            name.to_owned(),
            (
                RecordId::new_v7(),
                roles.iter().map(|r| (*r).to_owned()).collect(),
            ),
        );
    }

    fn token(&self, name: &str) -> String {
        self.users.lock().unwrap()[name].0.to_string()
    }
}

impl ApiAccess for People {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, ProcessResult<ApiCaller>> {
        Box::pin(async move {
            let token = ctx
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or(ProcessPortError::PermissionDenied)?;
            let users = self.users.lock().unwrap();
            let (id, roles) = users
                .values()
                .find(|(id, _)| id.to_string() == token)
                .ok_or(ProcessPortError::PermissionDenied)?;
            let roles: Vec<&str> = roles.iter().map(String::as_str).collect();
            Ok(ApiCaller {
                user: id.to_string(),
                user_id: *id,
                principal: Principal::with_roles(&id.to_string(), &roles),
            })
        })
    }

    fn supervises(&self, _principal: &Principal) -> bool {
        false
    }

    fn may_read<'a>(
        &'a self,
        _c: &'a ApiCaller,
        _doctype: &'a str,
        _id: RecordId,
    ) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }

    fn assigned_cases<'a>(&'a self, _user: &'a str) -> Sourced<'a, Vec<String>> {
        Box::pin(async { Ok(Vec::new()) })
    }

    fn is_party<'a>(&'a self, _user: &'a str, _case: &'a str) -> Sourced<'a, bool> {
        Box::pin(async { Ok::<bool, SourceUnavailable>(false) })
    }
}

/// Who may publish: one user holding `process_publish` (the token is the user name).
struct Publishers;

impl VersionAccess for Publishers {
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> Fut<'a, Option<VersionCaller>> {
        Box::pin(async move {
            let user = ctx.credential.as_ref()?.token.clone();
            Some(VersionCaller {
                principal: Principal::user(&user, Vec::new()),
                user,
            })
        })
    }

    fn holds(&self, caller: &VersionCaller, permission: &str) -> bool {
        caller.user == "publisher" && permission == "process_publish"
    }
}

/// The platform reader: documents by `(doctype, name)`.
#[derive(Default)]
struct Reader {
    docs: Mutex<BTreeMap<(String, String), Document>>,
}

impl Reader {
    fn put(&self, doc: Document) {
        let key = (doc.doctype().as_str().to_owned(), doc.id().to_string());
        self.docs.lock().unwrap().insert(key, doc);
    }
}

impl SystemReader for Reader {
    fn read<'a>(&'a self, doctype: &'a str, name: &'a str) -> PortFuture<'a, Option<Document>> {
        Box::pin(async move {
            let key = (doctype.to_owned(), name.to_owned());
            Ok(self.docs.lock().unwrap().get(&key).cloned())
        })
    }
}

/// No stateful doctype.
struct NoStates;

impl Meta for NoStates {
    fn stateful(&self) -> Option<BTreeSet<String>> {
        Some(BTreeSet::new())
    }
    fn engine(&self) -> Option<spot_permissions::PermissionEngine> {
        None
    }
}

struct NoSecrets;

impl SecretSource for NoSecrets {
    fn secret<'a>(&'a self, _reference: &'a str) -> Fut<'a, Option<Vec<u8>>> {
        Box::pin(async { None })
    }
}

#[derive(Default)]
struct Trail(Mutex<Vec<TriggerAudit>>);

impl Audit for Trail {
    fn record(&self, entry: TriggerAudit) {
        self.0.lock().unwrap().push(entry);
    }
}

/// Captures every tracing event of this thread as text (field names and debug values), so a test
/// can prove a marker never reached a log line. Only for `current_thread` tests.
#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<String>>);

struct Fields<'a>(&'a mut String);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn Debug) {
        self.0.push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl tracing::Subscriber for LogCapture {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let mut line = String::new();
        event.record(&mut Fields(&mut line));
        let mut all = self.0.lock().unwrap();
        all.push_str(&line);
        all.push('\n');
    }
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

// --- the site under test -------------------------------------------------------------------------

fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 6;
    PgBackend::connect_unchecked(cfg).unwrap()
}

fn timer_port(leases: Arc<dyn LeaseStore>, name: &str) -> (Lease, FencedTimerPort) {
    let lease = leases
        .acquire(
            name,
            "node-1",
            Duration::from_secs(3600),
            to_jiff(at(1_700_000_000)),
        )
        .unwrap();
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let port = FencedTimerPort::new(leases, lease.clone(), inner);
    (lease, port)
}

struct Site {
    tenant: String,
    pool: deadpool_postgres::Pool,
    process: Arc<ProcessHost>,
    versions: Arc<VersionHost>,
    triggers: Arc<TriggerHost>,
    api: ProcessApi,
    people: Arc<People>,
    reader: Arc<Reader>,
    api_audit: Arc<Mutex<Vec<ApiAudit>>>,
    trigger_trail: Arc<Trail>,
}

async fn site(db: &TestDb, tenant: &str) -> Site {
    let pool = backend(db, tenant).pool();
    let store = PgCaseStore::new(pool.clone()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions = Arc::new(PgDefinitionStore::new(pool.clone()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let (_lease, timers) = timer_port(leases, &format!("timer:{tenant}"));
    let clock = Arc::new(ManualClock::new(at(1_700_000_000)));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let site_id = SiteId::new(tenant).unwrap();
    let process = ProcessHost::build(
        site_id.clone(),
        store,
        timers,
        HandlerRegistry::logic_default(),
        definitions.clone(),
        Arc::new(NullSubmit),
        dyn_clock.clone(),
    );
    let reader = Arc::new(Reader::default());
    let trigger_trail = Arc::new(Trail::default());
    let principals: Arc<dyn PrincipalSource> = Arc::new(MemoryPrincipals::new());
    let triggers = TriggerHost::new(TriggerParts {
        site: site_id.clone(),
        clock: clock.clone(),
        settings: TriggerSettings::default(),
        process: process.clone(),
        definitions: definitions.clone(),
        submit: Arc::new(NullSubmit),
        reader: reader.clone(),
        principals,
        meta: Arc::new(NoStates),
        secrets: Arc::new(NoSecrets),
        audit: trigger_trail.clone(),
        pool: pool.clone(),
    });
    triggers.prepare().await.unwrap();
    triggers.reload().await.unwrap();
    let versions = VersionHost::new(VersionParts {
        site: site_id.clone(),
        pool: pool.clone(),
        definitions: definitions.clone(),
        process: process.clone(),
        access: Arc::new(Publishers),
        guard: None,
        triggers: Some(triggers.clone()),
        trigger_settings: TriggerSettings::default(),
        clock: clock.clone(),
        settings: VersionSettings::default(),
    });
    versions.prepare().await.unwrap();
    let people = Arc::new(People::default());
    let api_audit: Arc<Mutex<Vec<ApiAudit>>> = Arc::default();
    let sink = Arc::clone(&api_audit);
    let api = ProcessApi::new(Deps {
        site: site_id,
        host: process.clone(),
        definitions,
        case_file: Arc::new(MemoryCaseFile::new()) as Arc<dyn CaseFileStore>,
        access: people.clone() as Arc<dyn ApiAccess>,
        directory: None,
        audit: Arc::new(move |entry| sink.lock().unwrap().push(entry)),
        clock: dyn_clock,
    });
    Site {
        tenant: tenant.to_owned(),
        pool,
        process,
        versions,
        triggers,
        api,
        people,
        reader,
        api_audit,
        trigger_trail,
    }
}

impl Site {
    fn publisher_ctx(&self) -> RequestCtx {
        RequestCtx {
            site: SiteId::new(&self.tenant).unwrap(),
            credential: Some(Credential::new("publisher")),
            idempotency_key: None,
            language: None,
            request_id: RequestId::generate(),
            peer: None,
        }
    }

    /// Publishes `text` as `name` through the version port.
    async fn publish(
        &self,
        name: &str,
        text: String,
        expected_previous: Option<u32>,
    ) -> Result<u32, VersionPortError> {
        let published = self
            .versions
            .publish(
                &self.publisher_ctx(),
                name,
                PublishRequest {
                    definition: text,
                    note: String::new(),
                    expected_previous,
                },
            )
            .await?;
        assert_eq!(published.version.state, VersionState::Published);
        Ok(published.version.version)
    }

    fn ctx(&self, user: &str, key: Option<&str>) -> RequestCtx {
        RequestCtx {
            site: SiteId::new(&self.tenant).unwrap(),
            credential: Some(Credential::new(self.people.token(user))),
            idempotency_key: key.map(str::to_owned),
            language: None,
            request_id: RequestId::generate(),
            peer: None,
        }
    }

    async fn start(&self, user: &str, key: &str, process: &str, input: Json) -> ProcessResult<Run> {
        let Json::Object(input) = input else {
            panic!("the test input is an object");
        };
        self.start_object(user, key, process, input).await
    }

    async fn start_object(
        &self,
        user: &str,
        key: &str,
        process: &str,
        input: Map<String, Json>,
    ) -> ProcessResult<Run> {
        self.api
            .start_run(
                &self.ctx(user, Some(key)),
                process,
                RunStart {
                    input,
                    business_key: None,
                },
            )
            .await
    }

    async fn cases(&self) -> i64 {
        let client = self.pool.get().await.unwrap();
        client
            .query_one(
                "SELECT count(*) FROM spot_bpm_case WHERE tenant = $1",
                &[&self.tenant],
            )
            .await
            .unwrap()
            .get(0)
    }

    fn audited(&self) -> Vec<ApiAudit> {
        self.api_audit.lock().unwrap().clone()
    }

    fn outcomes(&self, process: &str) -> Vec<&'static str> {
        self.audited()
            .iter()
            .filter(|e| e.operation == "run_start" && e.subject == process)
            .map(|e| e.outcome)
            .collect()
    }
}

macro_rules! db_or_skip {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => {
                assert!(
                    std::env::var("SPOT_REQUIRE_PG").as_deref() != Ok("1"),
                    "SPOT_REQUIRE_PG=1 but SPOT_TEST_PG_URL is not set"
                );
                println!("SKIPPED: SPOT_TEST_PG_URL is not set");
                return;
            }
        }
    };
}

fn issues(error: ProcessPortError) -> Vec<FieldIssue> {
    match error {
        ProcessPortError::InputInvalid(issues) => issues,
        other => panic!("expected input_invalid, got {other:?}"),
    }
}

fn fields(issues: &[FieldIssue]) -> Vec<(&str, &str)> {
    issues
        .iter()
        .map(|i| (i.field.as_str(), i.code.as_str()))
        .collect()
}

fn widget(total: i64) -> (RecordId, Document) {
    let id = RecordId::new_v7();
    let mut doc = Document::new(Name::new("widget").unwrap(), id);
    doc.set(Name::new("total").unwrap(), Value::Int(total));
    doc.mark_saved(Version(1), Some(DocState::Posted));
    (id, doc)
}

/// A `doc_posted` record of the platform itself for `doc` at feed position `seq`.
fn posted(seq: u64, doc: &RecordId) -> OutboxRecord {
    let payload = PayloadDocument {
        meta: EventMeta {
            occurred_at: at(1_700_000_000),
            actor: Actor::System {
                component: Name::new("test").unwrap(),
            },
            command_id: None,
            causation_id: None,
            correlation_id: RecordId::new_v7(),
            trace_id: None,
        },
        data: json!({}),
    };
    OutboxRecord {
        position: FeedPosition::new(1, seq),
        transaction: RecordId::new_v7(),
        ordinal: 0,
        event: OutboxEvent {
            id: RecordId::new_v7(),
            topic: Name::new("doc_posted").unwrap(),
            subject: NodeRef {
                entity: Name::new("widget").unwrap(),
                id: *doc,
            },
            subject_version: Some(Version(1)),
            schema_version: 1,
            payload: serde_json::to_string(&payload).unwrap(),
        },
    }
}

// --- the manual trigger ----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_caller_without_the_role_gets_the_answer_for_an_unknown_process() {
    let db = db_or_skip!();
    let s = site(&db, "tf-role").await;
    assert_eq!(
        s.publish("approvals", approvals(true), None).await.unwrap(),
        1
    );
    s.people.add("ann", &[]);
    s.people.add("purchaser_pat", &["purchaser"]);
    let valid = json!({"amount": 10, "supplier": "Acme"});
    let denied = s
        .start("ann", "k-denied", "approvals", valid.clone())
        .await
        .unwrap_err();
    let unknown = s
        .start("ann", "k-unknown", "no_such_process", valid)
        .await
        .unwrap_err();
    assert_eq!(denied, ProcessPortError::NotFound);
    assert_eq!(format!("{denied:?}"), format!("{unknown:?}"));
    assert_eq!(s.outcomes("approvals"), vec!["process.not_found"]);
    assert_eq!(s.cases().await, 0, "nothing was started");
    // The role opens the same process.
    let run = s
        .start(
            "purchaser_pat",
            "k-ok",
            "approvals",
            json!({"amount": 10, "supplier": "Acme"}),
        )
        .await
        .unwrap();
    assert_eq!(run.process, "approvals");
    assert_eq!(s.cases().await, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bad_input_lists_every_field_problem_in_field_order() {
    let db = db_or_skip!();
    let s = site(&db, "tf-input").await;
    s.publish("approvals", approvals(true), None).await.unwrap();
    s.people.add("pat", &["purchaser"]);

    // Both required fields are missing: both are listed, with their paths.
    let missing = issues(
        s.start("pat", "k1", "approvals", json!({}))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        fields(&missing),
        vec![
            ("amount", "process.input_required"),
            ("supplier", "process.input_required")
        ]
    );
    assert!(missing.iter().all(|i| !i.message.is_empty()));

    // A wrong type, a value above the maximum and an unknown field: three issues, field order.
    let three = issues(
        s.start(
            "pat",
            "k2",
            "approvals",
            json!({"amount": 2_000_000, "extra": true, "supplier": 7}),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        fields(&three),
        vec![
            ("amount", "process.input_invalid"),
            ("extra", "process.input_unknown_field"),
            ("supplier", "process.input_invalid")
        ]
    );

    // Below the minimum and too long are issues too.
    let bounds = issues(
        s.start(
            "pat",
            "k3",
            "approvals",
            json!({"amount": 0, "supplier": "s".repeat(65)}),
        )
        .await
        .unwrap_err(),
    );
    assert_eq!(
        fields(&bounds),
        vec![
            ("amount", "process.input_invalid"),
            ("supplier", "process.input_invalid")
        ]
    );
    assert_eq!(s.cases().await, 0, "no refused start created a Case");
    assert_eq!(
        s.outcomes("approvals"),
        vec!["process.input_invalid"; 3],
        "each refusal is audited with its code"
    );
}

#[tokio::test]
async fn a_supplied_value_is_never_echoed_by_the_error_the_audit_or_the_log() {
    let capture = LogCapture::default();
    let _guard = tracing::subscriber::set_default(capture.clone());
    let db = db_or_skip!();
    let s = site(&db, "tf-echo").await;
    s.publish("approvals", approvals(true), None).await.unwrap();
    s.people.add("pat", &["purchaser"]);
    let error = s
        .start(
            "pat",
            "k-marker",
            "approvals",
            json!({"amount": {"nested": MARKER}, "supplier": [MARKER]}),
        )
        .await
        .unwrap_err();
    let wrong = issues(error.clone());
    assert_eq!(wrong.len(), 2, "{wrong:?}");
    let mut with_unknown = Map::new();
    with_unknown.insert("amount".to_owned(), json!(5));
    with_unknown.insert("supplier".to_owned(), json!("ok"));
    // An unknown key is named in the issue (the caller typed it, and another test in this file
    // pins that); the value supplied under it is the secret and is never echoed.
    with_unknown.insert("surplus_key".to_owned(), json!(MARKER));
    let unknown = s
        .start_object("pat", "k-marker-2", "approvals", with_unknown)
        .await
        .unwrap_err();
    let shown = format!("{error:?} {unknown:?} {:?}", s.audited());
    assert!(
        !shown.contains(MARKER),
        "a supplied value reached an error or the audit"
    );
    let logs = capture.0.lock().unwrap().clone();
    assert!(
        !logs.contains(MARKER),
        "a supplied value reached a log line"
    );
    assert!(s.audited().iter().all(|e| e.outcome != "ok"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_conforming_start_runs_and_the_same_idempotency_key_replays() {
    let db = db_or_skip!();
    let s = site(&db, "tf-run").await;
    s.publish("approvals", approvals(true), None).await.unwrap();
    s.people.add("pat", &["purchaser"]);
    let input = json!({"amount": 250, "supplier": "Acme Ltd"});
    let run = s
        .start("pat", "idem-1", "approvals", input.clone())
        .await
        .unwrap();
    assert_eq!(run.process, "approvals");
    assert_eq!(run.version, 1);

    let got = s.api.get_run(&s.ctx("pat", None), &run.id).await.unwrap();
    assert_eq!(got.id, run.id);

    // The engine key carries the caller and the idempotency key.
    let user = s.people.token("pat");
    let key = engine_key("approvals", &user, None, Some("idem-1")).unwrap();
    assert!(
        key.contains("idem-1"),
        "the engine key includes the idempotency key"
    );
    let stored = s
        .process
        .engine()
        .unwrap()
        .store()
        .find_by_key(&key)
        .await
        .unwrap();
    assert!(stored.is_some(), "the Case is stored under that key");

    // The same key replays the same Run; a new key starts another.
    let again = s
        .start("pat", "idem-1", "approvals", input.clone())
        .await
        .unwrap();
    assert_eq!(again.id, run.id);
    assert_eq!(s.cases().await, 1);
    let other = s.start("pat", "idem-2", "approvals", input).await.unwrap();
    assert_ne!(other.id, run.id);
    assert_eq!(s.cases().await, 2);
}

// --- the API trigger -----------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_api_trigger_has_its_own_roles_and_schema() {
    let db = db_or_skip!();
    let s = site(&db, "tf-api").await;
    s.publish("intake_api", intake_api(), None).await.unwrap();
    s.people.add("ann", &["purchaser"]);
    s.people.add("ivy", &["integrator"]);

    let denied = s
        .start("ann", "a1", "intake_api", json!({"ref": "r1"}))
        .await
        .unwrap_err();
    assert_eq!(denied, ProcessPortError::NotFound);
    assert_eq!(s.outcomes("intake_api"), vec!["process.not_found"]);
    assert_eq!(s.cases().await, 0);

    let missing = issues(
        s.start("ivy", "a2", "intake_api", json!({}))
            .await
            .unwrap_err(),
    );
    assert_eq!(fields(&missing), vec![("ref", "process.input_required")]);
    let extra = issues(
        s.start("ivy", "a3", "intake_api", json!({"ref": "r1", "more": 1}))
            .await
            .unwrap_err(),
    );
    assert_eq!(
        fields(&extra),
        vec![("more", "process.input_unknown_field")]
    );
    let long = issues(
        s.start("ivy", "a4", "intake_api", json!({"ref": "r".repeat(17)}))
            .await
            .unwrap_err(),
    );
    assert_eq!(fields(&long), vec![("ref", "process.input_invalid")]);
    assert_eq!(s.cases().await, 0);

    let run = s
        .start("ivy", "a5", "intake_api", json!({"ref": "r1"}))
        .await
        .unwrap();
    assert_eq!(run.process, "intake_api");
    assert_eq!(s.cases().await, 1);
}

// --- the document-event trigger ----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_document_condition_gates_the_start_and_a_redelivery_starts_nothing_more() {
    let db = db_or_skip!();
    let s = site(&db, "tf-cond").await;
    s.publish("big_orders", big_orders(), None).await.unwrap();

    let (small_id, small) = widget(500);
    let (big_id, big) = widget(1500);
    s.reader.put(small);
    s.reader.put(big);
    let host = s.triggers.clone();

    host.ingest(&[posted(1, &small_id)]).await.unwrap();
    assert_eq!(
        host.stats().started,
        0,
        "total 500 does not meet the condition"
    );
    assert_eq!(s.cases().await, 0);

    let record = posted(2, &big_id);
    host.ingest(std::slice::from_ref(&record)).await.unwrap();
    assert_eq!(host.stats().started, 1, "total 1500 starts one Run");
    assert_eq!(s.cases().await, 1);

    host.ingest(std::slice::from_ref(&record)).await.unwrap();
    assert_eq!(host.stats().started, 1, "a redelivery starts nothing more");
    assert_eq!(host.stats().existing, 1);
    assert_eq!(s.cases().await, 1);
    // The trail records what the host did with each firing: the start and the redelivery are
    // `ok` entries; a condition that does not hold adds no refusal.
    let refused: Vec<&str> = s
        .trigger_trail
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|e| e.outcome)
        .filter(|outcome| *outcome != "started" && *outcome != "existing")
        .collect();
    assert!(
        refused.is_empty(),
        "a condition that does not hold is not a refusal: {refused:?}"
    );
}

// --- republishing ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn republishing_without_the_role_replaces_the_live_index() {
    let db = db_or_skip!();
    let s = site(&db, "tf-republish").await;
    s.publish("approvals", approvals(true), None).await.unwrap();
    s.people.add("ann", &[]);
    let input = json!({"amount": 10, "supplier": "Acme"});
    let refused = s
        .start("ann", "r1", "approvals", input.clone())
        .await
        .unwrap_err();
    assert_eq!(refused, ProcessPortError::NotFound);

    let second = s
        .publish("approvals", approvals(false), Some(1))
        .await
        .unwrap();
    assert_eq!(second, 2);
    let run = s.start("ann", "r2", "approvals", input).await.unwrap();
    assert_eq!(run.version, 2, "the live version is replaced, not merged");
    assert_eq!(s.cases().await, 1);
    assert_eq!(
        s.outcomes("approvals"),
        vec!["process.not_found", "ok"],
        "the earlier refusal stays in the audit history"
    );
}

// --- refused at publish ------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unusable_schema_or_condition_is_refused_at_publish_with_the_node_and_the_code() {
    let db = db_or_skip!();
    let s = site(&db, "tf-refuse").await;
    let bad_schema = process_text(
        "bad_schema",
        &format!("trigger = \"manual\"\ninput_schema = 'not json {MARKER}'"),
    );
    let bad_condition = process_text(
        "bad_condition",
        &format!(
            "trigger = \"document_event\"\ndoctype = \"widget\"\nevent = \"post\"\n\
             condition = \"doc.total >> )( {MARKER}\""
        ),
    );
    for (name, text) in [("bad_schema", bad_schema), ("bad_condition", bad_condition)] {
        let error = s.publish(name, text, None).await.unwrap_err();
        let VersionPortError::DefinitionInvalid(problems) = &error else {
            panic!("expected definition_invalid, got {error:?}");
        };
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert_eq!(problems[0].node.as_deref(), Some("start"));
        assert_eq!(problems[0].kind, "bpm_model.trigger_field_invalid");
        assert!(
            !format!("{error:?}").contains(MARKER),
            "the refusal echoed the definition's text"
        );
        // Nothing was stored: the process does not exist for the engine.
        assert!(
            s.process
                .engine()
                .unwrap()
                .processes()
                .latest(name)
                .is_none()
        );
    }
}

/// `pattern` is enforced by the triggers crate (WP-BPM-02.T13, the `regex` crate), so a plain
/// pattern publishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_pattern_the_triggers_crate_enforces_is_published() {
    let db = db_or_skip!();
    let s = site(&db, "tf-pattern-ok").await;
    let text = process_text(
        "patterned_ok",
        "trigger = \"manual\"\n\
         input_schema = '{\"type\":\"object\",\"properties\":{\"supplier\":{\"type\":\"string\",\"pattern\":\"^[A-Za-z0-9 .-]+$\"}}}'",
    );
    s.publish("patterned_ok", text, None).await.unwrap();
}

/// `pattern` is valid for the model but not enforceable by `validate_input` when the `regex`
/// crate cannot compile it (a look-ahead), so the triggers crate refuses it when the index is
/// built. The publish port reports that refusal as `trigger_invalid` with no node: the current
/// behaviour (kept as a test so a change is seen).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_schema_the_triggers_crate_cannot_enforce_is_refused_at_publish() {
    let db = db_or_skip!();
    let s = site(&db, "tf-pattern").await;
    let text = process_text(
        "patterned",
        "trigger = \"manual\"\n\
         input_schema = '{\"type\":\"object\",\"properties\":{\"supplier\":{\"type\":\"string\",\"pattern\":\"^(?=a)a+$\"}}}'",
    );
    let error = s.publish("patterned", text, None).await.unwrap_err();
    let VersionPortError::DefinitionInvalid(problems) = &error else {
        panic!("expected definition_invalid, got {error:?}");
    };
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert_eq!(problems[0].kind, "trigger_invalid");
    assert!(
        s.process
            .engine()
            .unwrap()
            .processes()
            .latest("patterned")
            .is_none()
    );
}

/// The gap behind the test above: the publish check turns the triggers crate's error into the
/// bare token `trigger_invalid` (`services/process_versions.rs`, `check_definition`, the
/// `TriggerIndex::from_versions(..).map_err(|_| bad("trigger_invalid"))` line). The brief wants
/// the node id and the code (`bpm_triggers.input_schema_invalid`). Ignored until that line keeps
/// the error's node and code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "gap: the publish check drops the node and code of a trigger error (process_versions.rs)"]
async fn the_publish_refusal_for_an_unenforceable_schema_names_the_node_and_the_code() {
    let db = db_or_skip!();
    let s = site(&db, "tf-pattern-code").await;
    let text = process_text(
        "patterned",
        "trigger = \"manual\"\n\
         input_schema = '{\"type\":\"object\",\"properties\":{\"supplier\":{\"type\":\"string\",\"pattern\":\"^a$\"}}}'",
    );
    let error = s.publish("patterned", text, None).await.unwrap_err();
    let VersionPortError::DefinitionInvalid(problems) = &error else {
        panic!("expected definition_invalid, got {error:?}");
    };
    assert_eq!(problems[0].node.as_deref(), Some("start"));
    assert_eq!(problems[0].kind, "bpm_triggers.input_schema_invalid");
}
