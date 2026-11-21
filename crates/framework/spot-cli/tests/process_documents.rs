//! The Document node hosting module, over a real PostgreSQL and the real document engine
//! (WP-BPM-04.T6; ADR-49, ADR-080).
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset — the convention `tests/process.rs` and `spot-bpm-pg`'s own
//! tests use. A Case is a real row in a real `ProcessHost`'s Case store (one trivial
//! trigger-to-join process is published and started per test, exactly as `tests/process.rs`
//! does), so a Document node's audit, Case-file and idempotency pre-check all read and write
//! the same durable state a hosted site would. The node itself is run directly against a
//! [`NodeCtx`] (the style `spot-bpm-documents`' own `tests/scenario.rs` uses), rather than
//! through a full compiled process DAG: this crate's Document node kind is not yet threaded
//! through `spot-bpm-model`'s TOML compiler in a way a test file outside that crate can drive
//! without duplicating its node-config grammar, and the handler's own contract (`NodeHandler::
//! run`) does not care which caller built its `NodeCtx`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use spot_app_api::context::DocumentService as _;
use spot_app_api::prelude::{OffsetDateTime, RecordId, SiteId};
use spot_bpm_definitions::DefinitionStore;
use spot_bpm_defs_pg::PgDefinitionStore;
use spot_bpm_documents::{
    CaseFileStore, DocumentNodeHandler, ServicePrincipalPolicy, ServicePrincipals,
};
use spot_bpm_engine::{
    CaseId, HandlerRegistry, IdempotencyKey, NodeCtx, NodeHandler, NodeKind, NodeOutcome,
    OutgoingEdge,
};
use spot_bpm_model::from_text;
use spot_bpm_pg::{FencedTimerPort, PgCaseStore};
use spot_cli::services::ports::{JobSpec, JobSubmit, PortFuture};
use spot_cli::services::process::ProcessHost;
use spot_cli::services::process_documents::{
    ConfiguredAuthorPrincipals, ConfiguredPrincipals, EngineCaseAuthors, HostDocumentOps,
    PgAfterCaseFile, PgPrincipalAudit, PrincipalConfig,
};
use spot_core::{Clock, SystemClock};
use spot_dag::NodeId;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::{
    FieldFacts, GroupName, Model, ModelTexts, PermissionEngine, Principal, Snapshot, load,
};
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_scheduler::{InMemoryLeaseStore, Lease, LeaseStore};
use spot_seams::RegistryBuilder;

fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A backend with its own pool, named for diagnostics.
fn backend(db: &TestDb, app: &str) -> PgBackend {
    let mut cfg = db.config();
    cfg.application_name = app.to_owned();
    cfg.pool_max_size = 4;
    PgBackend::connect_unchecked(cfg).unwrap()
}

fn to_jiff(at: OffsetDateTime) -> jiff::Timestamp {
    jiff::Timestamp::new(
        at.unix_timestamp(),
        i32::try_from(at.nanosecond()).unwrap_or(0),
    )
    .unwrap()
}

fn timer_port(leases: Arc<dyn LeaseStore>, name: &str) -> (Lease, FencedTimerPort) {
    let lease = leases
        .acquire(name, "node-1", Duration::from_secs(3600), to_jiff(now()))
        .unwrap();
    let inner: Arc<dyn spot_bpm_engine::TimerPort> =
        Arc::new(spot_bpm_engine::InMemoryTimerPort::new());
    let port = FencedTimerPort::new(leases, lease.clone(), inner);
    (lease, port)
}

#[derive(Default)]
struct NullSubmit;
impl JobSubmit for NullSubmit {
    fn submit<'a>(&'a self, _job: JobSpec) -> PortFuture<'a, bool> {
        Box::pin(async { Ok(true) })
    }
}

/// A trivial trigger-to-join process: just enough for a real Case row to exist in the store.
const ONE_NODE: &str = r#"
name = "doc_host_test"
app = "process_documents_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "done"
"#;

/// A `ProcessHost` over its own tenant-scoped tables on `db`, with one published process, so a
/// real Case exists for the Document node tests to run against.
async fn host(db: &TestDb, tenant: &str) -> Arc<ProcessHost> {
    let store = PgCaseStore::new(backend(db, tenant).pool()).with_tenant(tenant);
    store.prepare().await.unwrap();
    let definitions =
        Arc::new(PgDefinitionStore::new(backend(db, tenant).pool()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let leases: Arc<dyn LeaseStore> = Arc::new(InMemoryLeaseStore::new());
    let (_lease, timers) = timer_port(leases, &format!("timer:{tenant}"));
    let site = SiteId::new(tenant).unwrap();
    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    ProcessHost::build(
        site,
        store,
        timers,
        HandlerRegistry::logic_default(),
        definitions,
        Arc::new(NullSubmit),
        clock,
    )
}

/// Publishes [`ONE_NODE`] under `publisher` and starts one Case; returns the Case id.
async fn one_case(
    host: &ProcessHost,
    definitions: &PgDefinitionStore,
    key: &str,
    publisher: &str,
) -> CaseId {
    definitions
        .publish("doc_host_test", ONE_NODE, None, publisher, None, now())
        .await
        .unwrap();
    let version = from_text(ONE_NODE).unwrap();
    host.engine().unwrap().publish(&version).unwrap();
    host.start_run("doc_host_test", None, json!({}), None, key, now())
        .await
        .unwrap()
}

// --- the widget DocType and its permission model -------------------------------------------

fn definitions_meta() -> Vec<Definition> {
    let mut widget = Definition::new("widget");
    widget.is_posted = true;
    widget.fields = vec![
        FieldSpec::new("name", FieldType::Text),
        FieldSpec::new("restricted", FieldType::Text),
    ];
    vec![widget]
}

fn meta() -> Arc<EffectiveMeta> {
    Registry::new(definitions_meta()).unwrap().effective()
}

const GRANTS: &str = r#"
[[entries]]
doctype = "widget"
role = "widget_editor"
rights = ["read", "write", "create", "submit", "cancel"]

[[entries]]
doctype = "widget"
role = "widget_limited"
rights = ["read", "write", "create"]
"#;

const FIELD_GRANTS: &str = r#"
[[entries]]
doctype = "widget"
role = "widget_editor"
target = { group = "secret" }
access = ["read", "create", "update"]
"#;

const GROUPS: &str = r#"
[[entries]]
name = "secret"
"#;

fn model() -> Model {
    load(&ModelTexts {
        grants: GRANTS.to_owned(),
        field_grants: FIELD_GRANTS.to_owned(),
        groups: GROUPS.to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("the test model loads: {e}"))
}

fn decision_engine() -> Arc<PermissionEngine> {
    let mut snapshot = Snapshot::build(&meta(), &model()).unwrap();
    snapshot = snapshot
        .with_field_facts(
            "widget",
            vec![FieldFacts {
                path: "restricted".to_owned(),
                groups: vec![GroupName("secret".to_owned())],
                mask: None,
                ..FieldFacts::default()
            }],
        )
        .unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// A document engine over real PostgreSQL, authorised for `editor` (full rights, incl. the
/// `secret` field group) and `limited` (widget rights, but not the `secret` field group).
async fn document_engine(
    db: &TestDb,
    tenant: &str,
    editor: RecordId,
    limited: RecordId,
) -> Arc<Engine<PgBackend>> {
    let memory = MemoryPrincipals::new()
        .with(&editor, Principal::with_roles("editor", &["widget_editor"]))
        .with(
            &limited,
            Principal::with_roles("limited", &["widget_limited"]),
        );
    let authorizer: Arc<PermissionAuthorizer> = Arc::new(PermissionAuthorizer::new(
        decision_engine(),
        Arc::new(memory),
    ));
    let seams = Arc::new(RegistryBuilder::new().build().unwrap());
    let backend = backend(db, tenant);
    let engine = Engine::new(
        backend,
        meta(),
        seams,
        Arc::new(SystemClock::new()) as Arc<dyn Clock>,
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(authorizer);
    engine.prepare().await.unwrap();
    Arc::new(engine)
}

/// Builds `(handler, principals, host, definitions)` for `tenant`: a Document node handler
/// over the real document engine, a Case-file writer and an idempotency pre-check both over
/// the same database, and a `ProcessHost` for the Case the tests run against.
async fn fixture(
    db: &TestDb,
    tenant: &str,
    editor: RecordId,
    limited: RecordId,
    configs: &[PrincipalConfig],
) -> (
    DocumentNodeHandler,
    Arc<ProcessHost>,
    Arc<PgDefinitionStore>,
    SiteId,
) {
    let site = SiteId::new(tenant).unwrap();
    let doc_engine = document_engine(db, tenant, editor, limited).await;
    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let backend = backend(db, tenant);
    {
        let mut client = db.raw_client().await;
        spot_edge_pg::schema::prepare(&mut client).await.unwrap();
    }
    let principals = Arc::new(ConfiguredPrincipals::parse(configs).unwrap());
    let ops = Arc::new(HostDocumentOps::new(
        doc_engine,
        site.clone(),
        Arc::clone(&principals),
        PgAfterCaseFile::new(backend.clone(), Arc::clone(&catalog)),
        spot_bpm_documents::PgCaseFileEdgeStore::new(
            backend.clone(),
            Arc::clone(&catalog),
            site.clone(),
        ),
    ));
    let audit = Arc::new(PgPrincipalAudit::new(backend.clone(), Arc::clone(&catalog)));
    let host = host(db, tenant).await;
    let definitions = Arc::new(PgDefinitionStore::new(backend.pool()).with_tenant(tenant));
    definitions.prepare().await.unwrap();
    let authors = Arc::new(EngineCaseAuthors::new(
        Arc::clone(&host),
        Arc::clone(&definitions),
    ));
    let policy = ServicePrincipalPolicy::new(
        site.clone(),
        principals as Arc<dyn ServicePrincipals>,
        authors as Arc<dyn spot_bpm_documents::CaseAuthors>,
        audit as Arc<dyn spot_bpm_documents::ServicePrincipalAudit>,
    );
    let handler = DocumentNodeHandler::new(ops as Arc<dyn spot_bpm_documents::DocumentOps>)
        .with_service_principals(policy);
    (handler, host, definitions, site)
}

fn ctx<'a>(
    case: &'a CaseId,
    node: &'a NodeId,
    attempt: u32,
    config: &'a serde_json::Value,
    principal: Option<&'a str>,
    variables: &'a mut serde_json::Value,
    outgoing: &'a [OutgoingEdge],
) -> NodeCtx<'a> {
    NodeCtx {
        case,
        node,
        kind: NodeKind::Document,
        attempt,
        key: IdempotencyKey::new(case.clone(), node.clone(), attempt),
        is_retry: attempt > 1,
        now: now(),
        deadline: None,
        principal,
        config,
        outgoing,
        variables,
        cancelled: false,
    }
}

fn create_config(output: &str, principal: Option<&str>) -> serde_json::Value {
    let mut object = serde_json::json!({
        "operation": "create_from",
        "doctype": "widget",
        "mapping": { "name": "doc.name" },
        "output": output,
    });
    if let Some(principal) = principal {
        object["principal"] = serde_json::Value::String(principal.to_owned());
    }
    object
}

fn submit_config(output: &str, document_path: &str, principal: Option<&str>) -> serde_json::Value {
    let mut object = serde_json::json!({
        "operation": "submit",
        "doctype": "widget",
        "document": document_path,
        "output": output,
    });
    if let Some(principal) = principal {
        object["principal"] = serde_json::Value::String(principal.to_owned());
    }
    object
}

fn document_id_of(outcome: &NodeOutcome) -> RecordId {
    match outcome {
        NodeOutcome::Done {
            output: Some(output),
            ..
        } => spot_bpm_documents::ids::parse_record_id(output["id"].as_str().unwrap()).unwrap(),
        other => panic!("expected a successful outcome, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_principal_creates_then_posts_a_document_and_the_audit_names_it() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "billing_bot".to_owned(),
        user: editor.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-success", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-1", "platform").await;
    let node1 = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "First Widget" });
    let config1 = create_config("created", Some("billing_bot"));
    let outcome1 = handler.run(&mut ctx(
        &case,
        &node1,
        1,
        &config1,
        Some("billing_bot"),
        &mut vars,
        &[],
    ));
    let document = document_id_of(&outcome1);

    let node2 = NodeId::new("post_widget").unwrap();
    // The created document's id lives in the Case's own variables under `created.id` (the
    // create node's output), so the submit node names it as an expression, not a literal.
    let config2 = submit_config("posted", "doc.created.id", Some("billing_bot"));
    let outcome2 = handler.run(&mut ctx(
        &case,
        &node2,
        1,
        &config2,
        Some("billing_bot"),
        &mut vars,
        &[],
    ));
    assert!(matches!(outcome2, NodeOutcome::Done { .. }), "{outcome2:?}");
    let _ = document;

    // The Case file names both touches, and the audit trail names the principal.
    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let backend = backend(&db, "tenant-success");
    let case_file = PgAfterCaseFile::new(backend.clone(), Arc::clone(&catalog));
    // The edge is an upsert keyed `(case, document)` (see `spot_bpm_documents::case_file`), so
    // the create then the submit of the SAME document leave one edge, now naming the submit.
    let entries = case_file.entries(&case).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "one Case-file entry for the one document touched: {entries:?}"
    );
    assert_eq!(entries[0].operation, spot_bpm_documents::Operation::Submit);
    let audit = PgPrincipalAudit::new(backend, catalog);
    let used = audit.principals_used(&case).unwrap();
    assert!(used.contains(&"billing_bot".to_owned()), "{used:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_principal_without_an_allow_list_entry_fails_the_node() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "billing_bot".to_owned(),
        user: editor.to_string(),
        // Granted to a different author only: this Case's author (`author-x`, a non-platform
        // process author) may not use it.
        grants: vec!["process:other-author".to_owned()],
    }];
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-unlisted", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-2", "author-x").await;
    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "Nope" });
    let config = create_config("created", Some("billing_bot"));
    let outcome = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("billing_bot"),
        &mut vars,
        &[],
    ));
    match outcome {
        NodeOutcome::Fail { code, .. } => {
            assert_eq!(code, spot_bpm_documents::codes::PRINCIPAL_NOT_PERMITTED);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_validation_failure_reports_a_stable_code_with_no_field_value() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-validation", editor, limited, &[]).await;
    let case = one_case(&host, &definitions, "run-3", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    // No `principal`: the node runs as the Case's own actor, a plain user id.
    let actor = editor.to_string();
    let secret_marker = "do-not-leak-SENTINEL-42";
    let mut vars = json!({ "name": secret_marker });
    // `restricted` is a field the Case's own actor (no role grants at all beyond what the
    // document engine's own validation reaches) cannot set without the `secret` field group;
    // omit a role for `editor`'s own plain user actor by asking for a DocType that does not
    // exist instead, which is the sure-fire validation failure (`meta.unknown_doctype`) with a
    // stable code and no document text.
    let mut config = create_config("created", None);
    config["doctype"] = serde_json::Value::String("no_such_doctype".to_owned());
    let outcome = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some(actor.as_str()),
        &mut vars,
        &[],
    ));
    match outcome {
        NodeOutcome::Fail { code, reason } => {
            assert!(!code.is_empty());
            assert!(
                !reason.contains(secret_marker),
                "the failure must not carry the document's field values: {reason}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_field_the_principals_user_may_not_write_refuses_the_create() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "limited_bot".to_owned(),
        user: limited.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-fieldguard", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-4", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "ok", "secret": "classified" });
    let mut config = create_config("created", Some("limited_bot"));
    config["mapping"] = json!({ "name": "doc.name", "restricted": "doc.secret" });
    let outcome = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("limited_bot"),
        &mut vars,
        &[],
    ));
    assert!(
        matches!(outcome, NodeOutcome::Fail { .. }),
        "`limited_bot`'s user may not write the `secret` field group: {outcome:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_repeated_attempt_after_a_crash_settles_from_the_committed_edge() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "billing_bot".to_owned(),
        user: editor.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-restart", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-5", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "Once" });
    let config = create_config("created", Some("billing_bot"));
    let outcome1 = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("billing_bot"),
        &mut vars,
        &[],
    ));
    let first_id = document_id_of(&outcome1);

    // The simulated crash: a *fresh* handler (and so a fresh, empty in-process replay cache)
    // over the same database replays the same attempt.
    let (handler2, _host2, _definitions2, _site2) =
        fixture(&db, "tenant-restart", editor, limited, &configs).await;
    let mut vars2 = json!({ "name": "Once" });
    let outcome2 = handler2.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("billing_bot"),
        &mut vars2,
        &[],
    ));
    let second_id = document_id_of(&outcome2);
    assert_eq!(
        first_id, second_id,
        "a replay settles on the same document, never a second one"
    );

    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let backend = backend(&db, "tenant-restart");
    let case_file = PgAfterCaseFile::new(backend, catalog);
    let entries = case_file.entries(&case).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "exactly one Case-file entry, not two: {entries:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_sites_do_not_share_a_cases_edges() {
    let Some(db_a) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let Some(db_b) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "billing_bot".to_owned(),
        user: editor.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler_a, host_a, definitions_a, _site_a) =
        fixture(&db_a, "tenant-a", editor, limited, &configs).await;
    let (_handler_b, host_b, definitions_b, _site_b) =
        fixture(&db_b, "tenant-b", editor, limited, &configs).await;
    // Both tenants start a Case from the same business key, so the derived Case id is
    // identical: isolation must come from the separate databases, never from the id.
    let case_a = one_case(&host_a, &definitions_a, "same-key", "platform").await;
    let case_b = one_case(&host_b, &definitions_b, "same-key", "platform").await;
    assert_eq!(
        case_a, case_b,
        "the derived Case id is a pure function of the business key"
    );

    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "A-only" });
    let config = create_config("created", Some("billing_bot"));
    handler_a.run(&mut ctx(
        &case_a,
        &node,
        1,
        &config,
        Some("billing_bot"),
        &mut vars,
        &[],
    ));

    let catalog_a = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let catalog_b = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let case_file_a = PgAfterCaseFile::new(backend(&db_a, "tenant-a"), catalog_a);
    let case_file_b = PgAfterCaseFile::new(backend(&db_b, "tenant-b"), catalog_b);
    assert_eq!(
        case_file_a.entries(&case_b).unwrap().len(),
        1,
        "tenant a recorded its touch"
    );
    assert_eq!(
        case_file_b.entries(&case_b).unwrap().len(),
        0,
        "tenant b's database holds no edge tenant a wrote"
    );
}

// --- fail-closed, audit of refusals, the crash window, the module scan ----------------------

/// A [`HostDocumentOps`] port and the document engine behind it, for tests that drive the port
/// directly (below the node handler, so a request the handler would never build can be sent).
async fn ops_and_engine(
    db: &TestDb,
    tenant: &str,
    editor: RecordId,
    limited: RecordId,
    configs: &[PrincipalConfig],
    edge_dbname: Option<&str>,
) -> (HostDocumentOps, Arc<Engine<PgBackend>>, SiteId) {
    let site = SiteId::new(tenant).unwrap();
    let doc_engine = document_engine(db, tenant, editor, limited).await;
    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let backend = backend(db, tenant);
    {
        let mut client = db.raw_client().await;
        spot_edge_pg::schema::prepare(&mut client).await.unwrap();
    }
    let principals = Arc::new(ConfiguredPrincipals::parse(configs).unwrap());
    // The edge store reads through its own backend; `edge_dbname` points it at a database that
    // does not exist, so every read of the Case-file edge fails.
    let edge_backend = match edge_dbname {
        Some(name) => {
            let mut cfg = db.config();
            cfg.application_name = tenant.to_owned();
            cfg.pool_max_size = 1;
            PgBackend::connect_unchecked(cfg.with_dbname(name)).unwrap()
        }
        None => backend.clone(),
    };
    let ops = HostDocumentOps::new(
        Arc::clone(&doc_engine),
        site.clone(),
        principals,
        PgAfterCaseFile::new(backend.clone(), Arc::clone(&catalog)),
        spot_bpm_documents::PgCaseFileEdgeStore::new(edge_backend, catalog, site.clone()),
    );
    (ops, doc_engine, site)
}

fn request_for(
    case: &CaseId,
    node: &NodeId,
    principal: Option<&str>,
    operation: spot_bpm_documents::Operation,
) -> spot_bpm_documents::DocumentRequest {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        spot_app_api::prelude::Name::new("name").unwrap(),
        spot_app_api::prelude::Value::from("direct"),
    );
    spot_bpm_documents::DocumentRequest {
        operation,
        doctype: spot_app_api::prelude::Name::new("widget").unwrap(),
        document: None,
        fields,
        expected: None,
        compensation: None,
        principal: principal.map(str::to_owned),
        case: case.clone(),
        node: node.clone(),
        key: IdempotencyKey::new(case.clone(), node.clone(), 1).to_string(),
        now: now(),
    }
}

fn editor_invocation(site: &SiteId, editor: RecordId) -> spot_app_api::context::Invocation {
    spot_app_api::context::Invocation {
        site: site.clone(),
        actor: spot_app_api::context::Actor::User {
            id: editor,
            via: None,
        },
        command_id: None,
        language: None,
        now: now(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_with_no_acting_principal_fails_closed_and_creates_no_document() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-nosystem", editor, limited, &[]).await;
    // A trigger- or timer-started Run has no acting user: the node's context carries none and
    // the node names no service principal. It must be refused, never run as the platform.
    let case = one_case(&host, &definitions, "run-nosys", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "Escalated" });
    let config = create_config("created", None);
    let outcome = handler.run(&mut ctx(&case, &node, 1, &config, None, &mut vars, &[]));
    match outcome {
        NodeOutcome::Fail { code, .. } => {
            assert_eq!(code, spot_bpm_documents::codes::PRINCIPAL_INVALID);
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let case_file = PgAfterCaseFile::new(backend(&db, "tenant-nosystem"), catalog);
    assert!(
        case_file.entries(&case).unwrap().is_empty(),
        "a refused node touches no document"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_port_refuses_a_missing_or_unrecognised_principal_without_calling_the_engine() {
    use spot_bpm_documents::{DocumentOps, Operation};
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let (ops, engine, site) =
        ops_and_engine(&db, "tenant-portguard", editor, limited, &[], None).await;
    let host_db = host(&db, "tenant-portguard").await;
    let definitions = Arc::new(
        PgDefinitionStore::new(backend(&db, "tenant-portguard").pool())
            .with_tenant("tenant-portguard"),
    );
    definitions.prepare().await.unwrap();
    let case = one_case(&host_db, &definitions, "run-port", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    for principal in [None, Some("not a registered name"), Some("system")] {
        let request = request_for(&case, &node, principal, Operation::CreateFrom);
        let failure = ops.execute(&request).unwrap_err();
        assert_eq!(
            failure.code(),
            spot_bpm_documents::codes::PRINCIPAL_INVALID,
            "{principal:?}"
        );
        let derived = spot_bpm_documents::ids::record_id_for(&format!("{}:document", request.key));
        let loaded = engine
            .load(&editor_invocation(&site, editor), "widget", derived)
            .await;
        assert!(loaded.is_err(), "no document was created for {principal:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_permission_source_that_does_not_know_the_user_denies() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    // The backing user is in no role table of the permission source: it cannot be resolved.
    let unknown = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "ghost_bot".to_owned(),
        user: unknown.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-ghost", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-ghost", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "Ghost" });
    let config = create_config("created", Some("ghost_bot"));
    let outcome = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("ghost_bot"),
        &mut vars,
        &[],
    ));
    assert!(matches!(outcome, NodeOutcome::Fail { .. }), "{outcome:?}");
    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let case_file = PgAfterCaseFile::new(backend(&db, "tenant-ghost"), catalog);
    assert!(case_file.entries(&case).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unreadable_case_file_denies_the_node_and_creates_no_document() {
    use spot_bpm_documents::{DocumentOps, Operation};
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    // The edge source fails (its database does not exist): the idempotency pre-check cannot
    // run, so the node must not act.
    let (ops, engine, site) = ops_and_engine(
        &db,
        "tenant-unreadable",
        editor,
        limited,
        &[],
        Some("spot_no_such_database"),
    )
    .await;
    let host_db = host(&db, "tenant-unreadable").await;
    let definitions = Arc::new(
        PgDefinitionStore::new(backend(&db, "tenant-unreadable").pool())
            .with_tenant("tenant-unreadable"),
    );
    definitions.prepare().await.unwrap();
    let case = one_case(&host_db, &definitions, "run-unreadable", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    let actor = editor.to_string();
    let request = request_for(&case, &node, Some(actor.as_str()), Operation::CreateFrom);
    let failure = ops.execute(&request).unwrap_err();
    assert_eq!(failure.code(), spot_bpm_documents::codes::CASE_FILE);
    let derived = spot_bpm_documents::ids::record_id_for(&format!("{}:document", request.key));
    let loaded = engine
        .load(&editor_invocation(&site, editor), "widget", derived)
        .await;
    assert!(loaded.is_err(), "the node did not act");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_operation_is_audited_with_the_principal_that_attempted_it() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "limited_bot".to_owned(),
        user: limited.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler, host, definitions, _site) =
        fixture(&db, "tenant-refusal-audit", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-audit", "platform").await;
    let node = NodeId::new("create_widget").unwrap();
    let mut vars = json!({ "name": "ok", "secret": "classified" });
    let mut config = create_config("created", Some("limited_bot"));
    config["mapping"] = json!({ "name": "doc.name", "restricted": "doc.secret" });
    let outcome = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("limited_bot"),
        &mut vars,
        &[],
    ));
    assert!(matches!(outcome, NodeOutcome::Fail { .. }), "{outcome:?}");
    // The engine refused (field guard), yet the attempt is on the audit trail: the principal
    // use is recorded before the node acts, so a refusal leaves who tried and on which node.
    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let audit = PgPrincipalAudit::new(backend(&db, "tenant-refusal-audit"), catalog);
    let used = audit.principals_used(&case).unwrap();
    assert!(used.contains(&"limited_bot".to_owned()), "{used:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_between_the_document_commit_and_its_edge_settles_on_the_one_document() {
    let Some(db) = TestDb::create().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let editor = RecordId::new_v7();
    let limited = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "billing_bot".to_owned(),
        user: editor.to_string(),
        grants: vec!["platform".to_owned()],
    }];
    let (handler, host, definitions, site) =
        fixture(&db, "tenant-edgecrash", editor, limited, &configs).await;
    let case = one_case(&host, &definitions, "run-edgecrash", "platform").await;
    let node = NodeId::new("create_widget").unwrap();

    // The other crash window: the lifecycle commit happened (the document exists under the id
    // derived from the node attempt's key) but the process died before the Case-file edge was
    // written. Reproduce it by creating that document straight through the engine.
    let key = IdempotencyKey::new(case.clone(), node.clone(), 1).to_string();
    let derived = spot_bpm_documents::ids::record_id_for(&format!("{key}:document"));
    let engine = document_engine(&db, "tenant-edgecrash", editor, limited).await;
    let mut document =
        spot_app_api::Document::new(spot_app_api::prelude::Name::new("widget").unwrap(), derived);
    document.set(
        spot_app_api::prelude::Name::new("name").unwrap(),
        spot_app_api::prelude::Value::from("Once"),
    );
    engine
        .create(&editor_invocation(&site, editor), document)
        .await
        .unwrap();

    let catalog = Arc::new(spot_cli::schema::catalog(&meta()).unwrap());
    let case_file = PgAfterCaseFile::new(backend(&db, "tenant-edgecrash"), catalog);
    assert!(case_file.entries(&case).unwrap().is_empty(), "no edge yet");

    let mut vars = json!({ "name": "Once" });
    let config = create_config("created", Some("billing_bot"));
    let outcome = handler.run(&mut ctx(
        &case,
        &node,
        1,
        &config,
        Some("billing_bot"),
        &mut vars,
        &[],
    ));
    assert_eq!(
        document_id_of(&outcome),
        derived,
        "the replay settles on the document the crashed attempt created"
    );
    let entries = case_file.entries(&case).unwrap();
    assert_eq!(
        entries.len(),
        1,
        "the missing edge is now written: {entries:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The publish guard resolves a real user's role assignments (WP-BPM-04.T8).
//
// These use a real `Snapshot::build_with_apps` permission engine and a fake `PrincipalSource`
// that behaves like `StoragePrincipals::resolve` (active, unexpired assignments only; a storage
// error resolves to a principal with no assignments), so they need no PostgreSQL. The
// real-database variants (assignment rows in a site's storage) run in the compile bundle.
// ---------------------------------------------------------------------------------------------

const PUBLISH: &str = "process_publish_service_principal";

/// One assignment a fake source holds: its role names and an optional expiry (unix seconds).
#[derive(Clone)]
struct FakeAssignment {
    id: String,
    roles: Vec<String>,
    expires_at: Option<i64>,
}

/// A principal source over an in-memory assignment table, with the clock fixed at `now()`.
#[derive(Default)]
struct FakeSource {
    rows: std::sync::Mutex<std::collections::BTreeMap<String, Vec<FakeAssignment>>>,
    broken: std::sync::atomic::AtomicBool,
}

impl FakeSource {
    fn assign(&self, user: &RecordId, roles: &[&str], expires_at: Option<i64>) {
        let mut rows = self.rows.lock().unwrap();
        let list = rows.entry(user.to_string()).or_default();
        let id = format!("a{}", list.len() + 1);
        list.push(FakeAssignment {
            id,
            roles: roles.iter().map(|r| (*r).to_owned()).collect(),
            expires_at,
        });
    }

    fn revoke_all(&self, user: &RecordId) {
        self.rows.lock().unwrap().remove(&user.to_string());
    }

    fn break_storage(&self) {
        self.broken.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl spot_document::engine::PrincipalSource for FakeSource {
    fn resolve<'a>(
        &'a self,
        user: &'a RecordId,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Principal> + Send + 'a>> {
        Box::pin(async move {
            // A storage error is swallowed into an empty assignment list, as `StoragePrincipals`
            // does: the principal exists but holds nothing.
            let assignments = if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
                Vec::new()
            } else {
                self.rows
                    .lock()
                    .unwrap()
                    .get(&user.to_string())
                    .map(|list| {
                        list.iter()
                            .filter(|a| a.expires_at.is_none_or(|at| at > now().unix_timestamp()))
                            .map(|a| spot_permissions::AssignmentContext {
                                id: a.id.clone(),
                                roles: a
                                    .roles
                                    .iter()
                                    .map(|r| spot_permissions::Role(r.clone()))
                                    .collect(),
                                scope: Vec::new(),
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            };
            Principal::user(&user.to_string(), assignments)
        })
    }
}

/// A permission engine where role `publisher` holds `process_publish_service_principal`.
fn publish_engine() -> Arc<PermissionEngine> {
    let name = spot_permissions::AppPermissionName::parse(PUBLISH).unwrap();
    let mut catalogue = spot_permissions::AppPermissionCatalogue::new();
    catalogue
        .insert(
            name.clone(),
            spot_permissions::AppPermissionDecl {
                app: "bpm".to_owned(),
                title: "Publish".to_owned(),
                description: "Publish a service-principal process".to_owned(),
            },
        )
        .unwrap();
    let grants = [spot_permissions::AppPermissionGrant {
        role: spot_permissions::Role("publisher".to_owned()),
        permission: name,
    }];
    let snapshot =
        Snapshot::build_with_apps(&meta(), &Model::default(), &catalogue, &grants).unwrap();
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

fn publish_site() -> SiteId {
    SiteId::new("bpm.test").unwrap()
}

/// The guard a site builds in `register`, with `svc_ledger` allow-listed for `users`.
fn publish_guard(
    site: &SiteId,
    source: &Arc<FakeSource>,
    users: &[&RecordId],
) -> spot_bpm_documents::PublishGuard {
    let robot = RecordId::new_v7();
    let configs = vec![PrincipalConfig {
        name: "svc_ledger".to_owned(),
        user: robot.to_string(),
        grants: users.iter().map(|u| format!("user:{u}")).collect(),
    }];
    let registry = Arc::new(ConfiguredPrincipals::parse(&configs).unwrap());
    let authors = Arc::new(ConfiguredAuthorPrincipals::new(
        site.clone(),
        Arc::clone(source) as Arc<dyn spot_document::engine::PrincipalSource>,
    ));
    let permission = Arc::new(spot_bpm_documents::IamPublishPermission::new(
        site.clone(),
        publish_engine(),
        authors,
    ));
    spot_bpm_documents::PublishGuard::new(registry, permission)
}

fn check(
    guard: &spot_bpm_documents::PublishGuard,
    site: &SiteId,
    author: &spot_bpm_documents::ProcessAuthor,
) -> Vec<spot_bpm_documents::PublishViolation> {
    let node = NodeId::new("pay").unwrap();
    let kind = NodeKind::Document;
    let config = json!({ "principal": "svc_ledger" });
    spot_cli::services::process_documents::check_publish(
        guard,
        site,
        author,
        &[spot_bpm_documents::GuardedNode {
            node: &node,
            kind: &kind,
            config: &config,
        }],
    )
}

fn user_author(user: &RecordId) -> spot_bpm_documents::ProcessAuthor {
    spot_bpm_documents::ProcessAuthor::User(user.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_holding_the_publish_role_passes_the_guard() {
    let site = publish_site();
    let user = RecordId::new_v7();
    let source = Arc::new(FakeSource::default());
    source.assign(&user, &["publisher"], None);
    let guard = publish_guard(&site, &source, &[&user]);
    let violations = check(&guard, &site, &user_author(&user));
    assert!(violations.is_empty(), "{violations:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_user_without_the_role_is_refused_with_the_node_and_rule_only() {
    let site = publish_site();
    let user = RecordId::new_v7();
    let source = Arc::new(FakeSource::default());
    source.assign(&user, &["clerk"], None);
    let guard = publish_guard(&site, &source, &[&user]);
    let violations = check(&guard, &site, &user_author(&user));
    assert_eq!(violations.len(), 1, "{violations:?}");
    assert_eq!(violations[0].node.as_str(), "pay");
    assert_eq!(
        violations[0].code,
        spot_bpm_documents::codes::PRINCIPAL_NOT_PERMITTED
    );
    // The reason names no user id, no role and no permission value.
    assert!(!violations[0].reason.contains(&user.to_string()));
    assert!(!violations[0].reason.contains("clerk"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_assignment_is_refused() {
    let site = publish_site();
    let user = RecordId::new_v7();
    let source = Arc::new(FakeSource::default());
    source.assign(&user, &["publisher"], Some(now().unix_timestamp() - 1));
    let guard = publish_guard(&site, &source, &[&user]);
    let violations = check(&guard, &site, &user_author(&user));
    // The node is refused for both reasons: the author lacks the permission and may not use the
    // principal.
    assert!(!violations.is_empty(), "{violations:?}");
    assert!(
        violations
            .iter()
            .all(|v| v.code == "bpm_documents.principal_not_permitted"),
        "{violations:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_revoked_assignment_is_seen_on_the_next_check_without_a_restart() {
    let site = publish_site();
    let user = RecordId::new_v7();
    let source = Arc::new(FakeSource::default());
    source.assign(&user, &["publisher"], None);
    let guard = publish_guard(&site, &source, &[&user]);
    assert!(check(&guard, &site, &user_author(&user)).is_empty());
    source.revoke_all(&user);
    assert_eq!(check(&guard, &site, &user_author(&user)).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn another_sites_roles_do_not_grant_here() {
    let site_a = publish_site();
    let site_b = SiteId::new("other.site").unwrap();
    let user = RecordId::new_v7();
    let source = Arc::new(FakeSource::default());
    source.assign(&user, &["publisher"], None);
    let guard = publish_guard(&site_a, &source, &[&user]);
    // Site A's guard asked about site B refuses, and the resolver does not even look the user up.
    assert_eq!(check(&guard, &site_b, &user_author(&user)).len(), 1);
    let authors = ConfiguredAuthorPrincipals::new(
        site_a.clone(),
        Arc::clone(&source) as Arc<dyn spot_document::engine::PrincipalSource>,
    );
    use spot_bpm_documents::AuthorPrincipals as _;
    assert_eq!(authors.principal_of(&site_b, &user_author(&user)), Ok(None));
    // Site B's own source holds no assignment for this user: its guard refuses too.
    let source_b = Arc::new(FakeSource::default());
    let guard_b = publish_guard(&site_b, &source_b, &[&user]);
    assert_eq!(check(&guard_b, &site_b, &user_author(&user)).len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn platform_process_and_malformed_authors_resolve_to_nothing() {
    use spot_bpm_documents::{AuthorPrincipals as _, ProcessAuthor};
    let site = publish_site();
    let source = Arc::new(FakeSource::default());
    let authors = ConfiguredAuthorPrincipals::new(
        site.clone(),
        Arc::clone(&source) as Arc<dyn spot_document::engine::PrincipalSource>,
    );
    assert_eq!(
        authors.principal_of(&site, &ProcessAuthor::Platform),
        Ok(None)
    );
    assert_eq!(
        authors.principal_of(&site, &ProcessAuthor::Process("p".to_owned())),
        Ok(None)
    );
    assert_eq!(
        authors.principal_of(&site, &ProcessAuthor::User("not-a-record-id".to_owned())),
        Ok(None)
    );
    // A process author is refused for a version carrying a service principal.
    let user = RecordId::new_v7();
    let guard = publish_guard(&site, &source, &[&user]);
    let violations = check(&guard, &site, &ProcessAuthor::Process("p".to_owned()));
    // Both checks refuse it: it lacks the publish permission and may not use the principal.
    assert_eq!(violations.len(), 2, "{violations:?}");
    assert!(
        violations
            .iter()
            .all(|v| v.code == "bpm_documents.principal_not_permitted"),
        "{violations:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_storage_failure_resolves_to_no_assignments_and_the_guard_denies() {
    let site = publish_site();
    let user = RecordId::new_v7();
    let source = Arc::new(FakeSource::default());
    source.assign(&user, &["publisher"], None);
    let guard = publish_guard(&site, &source, &[&user]);
    assert!(check(&guard, &site, &user_author(&user)).is_empty());
    source.break_storage();
    assert_eq!(check(&guard, &site, &user_author(&user)).len(), 1);
}

/// The module acts only through the engine's lifecycle methods: its non-test source names no
/// storage write primitive (no SQL write, no raw record or transaction write on the backend).
#[test]
fn the_module_imports_no_storage_write_api() {
    let source = include_str!("../src/services/process_documents.rs");
    let code = source
        .split("#[cfg(test)]")
        .next()
        .unwrap()
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for banned in [
        concat!("INSERT", " INTO"),
        concat!("UPDATE", " "),
        concat!("DELETE", " FROM"),
        "tokio_postgres",
        "sqlx",
        "put_record",
        "write_record",
        "upsert_record",
        "delete_record",
        "Mutation",
        "Actor::System",
    ] {
        assert!(
            !code.contains(banned),
            "the module must not use `{banned}`: documents change only through the lifecycle"
        );
    }
}
