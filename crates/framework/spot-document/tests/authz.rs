//! Permission enforcement in the document engine (WP-PLT-03.T2): the decision matrix, field
//! writes, masking, link scope, list filtering, nested writes and the snapshot swap.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "authz/world.rs"]
mod world;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::context::{DocumentService, HookContext, MethodContext};
use spot_app_api::controller::{Controller, MethodDecl, StepDecl, StepName};
use spot_app_api::prelude::{Decimal, Query, RecordId, Value, Version};
use spot_app_api::{
    AppResult, BoxFuture, ChildRow, Document, DocumentPatch, ErrorClass, HookPoint, Operation,
    Phase, Target,
};
use spot_core::ManualClock;
use spot_document::engine::{Authorizer, Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_permissions::{Action, DocFacts, Principal};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{
    ChangeFeed, Expr, FeedPosition, OutboxEvent, QueryPath, Selection, Value as SValue,
};
use spot_storage_fake::FakeBackend;
use world::{ALLOWED_REGION, DOCTYPES, OPERATIONS, OTHER_REGION, World, n, region};

// -------------------------------------------------------------------------------------------
// Fixture
// -------------------------------------------------------------------------------------------

fn memory(world: &World) -> MemoryPrincipals {
    let mut memory = MemoryPrincipals::new();
    for (name, principal) in &world.principals {
        memory = memory.with(&world.id(name), principal.clone());
    }
    memory
}

fn authorizer(world: &World) -> Arc<PermissionAuthorizer> {
    Arc::new(PermissionAuthorizer::new(
        world.engine.clone(),
        Arc::new(memory(world)),
    ))
}

type StepFn =
    Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;

/// A controller that can carry steps and whitelisted methods.
#[derive(Clone)]
struct Probe {
    doctype: &'static str,
    steps: Vec<(StepDecl, StepFn)>,
    methods: Vec<MethodDecl>,
}

impl Probe {
    fn new(doctype: &'static str) -> Self {
        Self {
            doctype,
            steps: Vec::new(),
            methods: Vec::new(),
        }
    }

    fn step<F>(mut self, name: &str, point: HookPoint, f: F) -> Self
    where
        F: for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>>
            + Send
            + Sync
            + 'static,
    {
        self.steps.push((
            StepDecl {
                name: StepName::new(name).unwrap(),
                point,
                priority: 0,
                budget: Duration::from_secs(5),
            },
            Arc::new(f),
        ));
        self
    }

    fn method(mut self, name: &str, mutates: bool) -> Self {
        self.methods.push(MethodDecl::new(n(name), mutates));
        self
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        self.doctype
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.steps.iter().map(|(d, _)| d.clone()).collect()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        self.methods.clone()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match self.steps.iter().find(|(d, _)| &d.name == step) {
            Some((_, f)) => f(ctx),
            None => Box::pin(async { Ok(()) }),
        }
    }

    fn call_method<'a>(
        &'a self,
        _method: &'a str,
        _args: Json,
        _ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async { Ok(json!({})) })
    }
}

async fn build(authorizer: Arc<dyn Authorizer>, probe: Probe) -> Engine<FakeBackend> {
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", Arc::new(probe.clone()));
    let engine = Engine::new(
        FakeBackend::new(),
        world::meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(world::now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(authorizer)
    .with_controllers(vec![Arc::new(probe)]);
    engine.prepare().await.unwrap();
    engine
}

async fn engine(authorizer: Arc<dyn Authorizer>) -> Engine<FakeBackend> {
    build(authorizer, Probe::new("customer")).await
}

fn at(phase: Phase, op: Operation) -> HookPoint {
    HookPoint::of(phase, Target::Op(op))
}

fn version(doc: &Document) -> Version {
    doc.version().unwrap()
}

fn fields(error: &spot_app_api::AppError) -> Vec<(String, String)> {
    error
        .fields
        .iter()
        .map(|f| (f.field.to_string(), f.code.clone()))
        .collect()
}

async fn outbox(engine: &Engine<FakeBackend>) -> Vec<OutboxEvent> {
    engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|r| r.event)
        .collect()
}

/// Creates the two `region` documents the scope tests link to, as the system (the `region`
/// DocType grants nobody the right, so only the platform can seed it).
async fn seed_regions(engine: &Engine<FakeBackend>, world: &World) {
    for id in [ALLOWED_REGION, OTHER_REGION] {
        let mut doc = Document::new(
            n("region"),
            spot_permissions::ids::parse_record_id(id).unwrap(),
        );
        doc.set(n("name"), id);
        engine.create(&world.system_inv(), doc).await.unwrap();
    }
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

fn invoice(customer: RecordId) -> Document {
    let mut doc = Document::new(n("sales_invoice"), RecordId::new_v7());
    doc.set(n("number"), "SI-1");
    doc.set(n("customer"), Value::Id(customer));
    let fields = BTreeMap::from([
        (n("item"), Value::Text("bolt".into())),
        (n("qty"), Value::Decimal(Decimal::new(2_000, 3))),
    ]);
    doc.rows_mut(n("items")).push(ChildRow {
        id: RecordId::new_v7(),
        fields,
    });
    doc
}

/// A query over `customer` selecting `name`, `salary` and `secret_note`.
fn customer_query() -> Query {
    let mut query = Query::from(&n("customer"), &n("c"), QueryPath::Transactional);
    for field in ["name", "salary", "secret_note"] {
        query.select.push(Selection {
            expr: Expr::field(&n("c"), &n(field)),
            alias: n(field),
        });
    }
    query
}

// -------------------------------------------------------------------------------------------
// The matrix
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_decision_matrix_holds() {
    let world = World::new();
    let authorizer = PermissionAuthorizer::new(world.engine.clone(), Arc::new(memory(&world)));
    let text: toml::Value = include_str!("authz/matrix.toml").parse().unwrap();
    let rows = text.get("rows").and_then(toml::Value::as_array).unwrap();
    assert_eq!(
        rows.len(),
        8 * 6 * 3,
        "8 principals x 6 operations x 3 DocTypes"
    );
    for row in rows {
        let principal_name = row.get("principal").unwrap().as_str().unwrap();
        let doctype = row.get("doctype").unwrap().as_str().unwrap();
        let op = row.get("op").unwrap().as_str().unwrap();
        let allowed = row.get("allowed").unwrap().as_bool().unwrap();
        let reason = row.get("reason").unwrap().as_str().unwrap();

        let principal = world.principal(principal_name).clone();
        let created_by = Some(if principal_name == "owner_reader_own" {
            principal_name.to_owned()
        } else {
            "someone_else".to_owned()
        });
        let facts = DocFacts {
            doctype: doctype.to_owned(),
            id: Some(ALLOWED_REGION.to_owned()),
            created_by,
            state: None,
            values: BTreeMap::from([("region".to_owned(), region(OTHER_REGION))]),
        };
        let action = Action::parse(op).unwrap();
        let decision = authorizer.decide(&principal, action, &facts).await;
        assert_eq!(
            decision.allowed, allowed,
            "{principal_name} {op} {doctype}: {decision:?}"
        );
        assert!(
            decision.has_kind(reason),
            "{principal_name} {op} {doctype}: expected a `{reason}` reason, got {decision:?}"
        );
    }
}

#[tokio::test]
async fn a_principal_with_no_role_is_refused_everywhere() {
    let world = World::new();
    let authorizer = PermissionAuthorizer::new(world.engine.clone(), Arc::new(memory(&world)));
    let principal = world.principal("no_role");
    for doctype in DOCTYPES {
        for op in OPERATIONS {
            let facts = DocFacts {
                doctype: doctype.to_owned(),
                id: Some(ALLOWED_REGION.to_owned()),
                ..DocFacts::default()
            };
            let decision = authorizer
                .decide(principal, Action::parse(op).unwrap(), &facts)
                .await;
            assert!(!decision.allowed, "no_role may not {op} {doctype}");
            assert!(decision.has_kind("no_grant"));
        }
    }
}

// -------------------------------------------------------------------------------------------
// Field writes
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_protected_field_write_is_refused_with_its_path() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let mut doc = customer("Acme");
    doc.set(n("salary"), Value::Decimal(Decimal::new(50_000, 2)));
    let error = engine.create(&world.inv("writer"), doc).await.unwrap_err();
    assert_eq!(error.code, "permission.field_denied");
    assert_eq!(error.class, ErrorClass::PermissionDenied);
    assert_eq!(
        fields(&error),
        [("salary".to_owned(), "permission.field_denied".to_owned())]
    );
}

#[tokio::test]
async fn a_child_row_field_write_is_refused_with_its_row_path() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let saved_customer = engine
        .create(&world.inv("editor"), customer("Acme"))
        .await
        .unwrap()
        .value;
    let mut doc = invoice(saved_customer.id());
    doc.rows_mut(n("items"))[0]
        .fields
        .insert(n("cost"), Value::Decimal(Decimal::new(10_000, 2)));
    let error = engine.create(&world.inv("writer"), doc).await.unwrap_err();
    assert_eq!(error.code, "permission.field_denied");
    assert_eq!(
        fields(&error),
        [(
            "items[1].cost".to_owned(),
            "permission.field_denied".to_owned()
        )]
    );
}

// -------------------------------------------------------------------------------------------
// Masking on every read path
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_read_masks_protected_fields_and_drops_unreadable_ones() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    seed_regions(&engine, &world).await;
    let mut doc = customer("Acme");
    doc.set(n("secret_note"), "the launch code is 1234");
    doc.set(n("salary"), Value::Decimal(Decimal::new(900_000, 2)));
    doc.set(n("region"), region(ALLOWED_REGION));
    let saved = engine
        .create(&world.inv("editor"), doc)
        .await
        .unwrap()
        .value;
    assert_eq!(
        saved.get("secret_note"),
        &Value::Text("the launch code is 1234".into()),
        "the writer sees what it wrote"
    );

    let loaded = engine
        .load(&world.inv("reader"), "customer", saved.id())
        .await
        .unwrap()
        .value;
    assert_eq!(
        loaded.get("secret_note"),
        &Value::Text("***".into()),
        "a masked field comes back masked"
    );
    assert!(
        !loaded.fields().contains_key(&n("salary")),
        "an unreadable field is gone"
    );
    assert!(loaded.fields().contains_key(&n("name")), "readable stays");
}

#[tokio::test]
async fn the_document_a_write_returns_goes_through_the_same_filter() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let mut doc = customer("Beta");
    doc.set(n("secret_note"), "classified");
    let saved = engine
        .create(&world.inv("masked_creator"), doc)
        .await
        .unwrap()
        .value;
    assert_eq!(
        saved.get("secret_note"),
        &Value::Text("***".into()),
        "the write result is a read of the document as written"
    );
}

#[tokio::test]
async fn the_system_principal_is_not_masked() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let mut doc = customer("Gamma");
    doc.set(n("secret_note"), "raw");
    doc.set(n("salary"), Value::Decimal(Decimal::new(100, 2)));
    let saved = engine.create(&world.system_inv(), doc).await.unwrap().value;
    assert_eq!(saved.get("secret_note"), &Value::Text("raw".into()));
    assert_eq!(saved.get("salary"), &Value::Decimal(Decimal::new(100, 2)));
}

// -------------------------------------------------------------------------------------------
// Link scope
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_link_moved_outside_the_users_scope_is_refused() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    seed_regions(&engine, &world).await;
    let mut doc = customer("Acme");
    doc.set(n("region"), region(ALLOWED_REGION));
    let saved = engine
        .create(&world.inv("editor"), doc)
        .await
        .unwrap()
        .value;

    let patch = DocumentPatch {
        fields: BTreeMap::from([(n("region"), region(OTHER_REGION))]),
        ..DocumentPatch::default()
    };
    let error = engine
        .update(
            &world.inv("scoped_writer"),
            "customer",
            saved.id(),
            patch,
            version(&saved),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.field_denied");
    assert_eq!(
        fields(&error),
        [("region".to_owned(), "permission.field_denied".to_owned())]
    );
    assert!(error.details.contains_key("reason"));
}

#[tokio::test]
async fn a_create_with_a_link_outside_the_scope_is_refused() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let mut doc = customer("Acme");
    doc.set(n("region"), region(OTHER_REGION));
    let error = engine
        .create(&world.inv("scoped_writer"), doc)
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert!(
        error
            .details
            .get("reason")
            .unwrap()
            .as_str()
            .unwrap()
            .contains("user_permission_blocked")
    );
}

// -------------------------------------------------------------------------------------------
// Nested writes keep the original actor
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_nested_write_is_checked_against_the_original_actor() {
    let world = World::new();
    let probe = Probe::new("customer").step(
        "probe:make_note",
        at(Phase::After, Operation::Create),
        |ctx| {
            Box::pin(async move {
                let mut note = Document::new(n("note"), RecordId::new_v7());
                note.set(n("text"), "from the customer");
                ctx.create(note).await?;
                Ok(())
            })
        },
    );
    let engine = build(authorizer(&world), probe).await;

    // `customer_creator` may create customers but not notes: the nested create is refused, and
    // the whole operation rolls back (it does not silently become the system principal).
    let doc = customer("Nested");
    let id = doc.id();
    let error = engine
        .create(&world.inv("customer_creator"), doc)
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert!(engine.peek("customer", id).await.unwrap().is_none());
    assert!(outbox(&engine).await.is_empty(), "nothing was enqueued");

    // `editor` holds both rights, so the same step succeeds.
    let ok = engine
        .create(&world.inv("editor"), customer("Nested ok"))
        .await;
    assert!(ok.is_ok());
}

// -------------------------------------------------------------------------------------------
// A permission change swaps the snapshot with no restart
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_snapshot_swap_takes_effect_on_the_next_request() {
    let world = World::new();
    let authorizer = authorizer(&world);
    let engine = engine(authorizer.clone()).await;

    let mut note = Document::new(n("note"), RecordId::new_v7());
    note.set(n("text"), "before");
    let error = engine
        .create(&world.inv("reader"), note.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");

    // The same engine, same authorizer, a new decision core: `reader` may now create notes.
    authorizer.swap(world::decision_engine_with_extra_grant());

    let mut note = Document::new(n("note"), RecordId::new_v7());
    note.set(n("text"), "after");
    let saved = engine
        .create(&world.inv("reader"), note)
        .await
        .unwrap()
        .value;
    assert_eq!(saved.get("text"), &Value::Text("after".into()));
}

// -------------------------------------------------------------------------------------------
// A refusal leaves nothing behind, and is counted
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_operation_leaves_no_storage_write_no_event_and_a_denial_record() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let doc = customer("Nope");
    let id = doc.id();
    let error = engine.create(&world.inv("no_role"), doc).await.unwrap_err();
    assert_eq!(error.code, "permission.denied");
    assert!(engine.peek("customer", id).await.unwrap().is_none());
    assert!(outbox(&engine).await.is_empty());
    assert_eq!(engine.denial_count(), 1);
    let record = engine.denial_log();
    assert_eq!(record[0].action, "create");
    assert_eq!(record[0].doctype, "customer");
    assert!(record[0].reason.contains("no_grant"));
}

// -------------------------------------------------------------------------------------------
// Correct needs both cancel and create
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn correct_needs_both_cancel_and_create() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let customer = engine
        .create(&world.inv("editor"), customer("Acme"))
        .await
        .unwrap()
        .value;
    let draft = engine
        .create(&world.inv("editor"), invoice(customer.id()))
        .await
        .unwrap()
        .value;
    let posted = engine
        .post(
            &world.inv("editor"),
            "sales_invoice",
            draft.id(),
            version(&draft),
        )
        .await
        .unwrap()
        .value;

    for principal in ["cancel_only", "create_only"] {
        let error = engine
            .correct(
                &world.inv(principal),
                "sales_invoice",
                posted.id(),
                version(&posted),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "permission.denied", "{principal}");
    }
    // The editor holds both, so the correction goes through.
    let amended = engine
        .correct(
            &world.inv("editor"),
            "sales_invoice",
            posted.id(),
            version(&posted),
        )
        .await
        .unwrap()
        .value;
    assert_eq!(amended.state(), Some(spot_app_api::DocState::Draft));
}

// -------------------------------------------------------------------------------------------
// Whitelisted methods
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_mutating_method_defaults_to_write_and_a_read_only_one_to_read() {
    let world = World::new();
    let probe = Probe::new("customer")
        .method("touch", true)
        .method("peek", false);
    let engine = build(authorizer(&world), probe).await;
    let saved = engine
        .create(&world.inv("editor"), customer("Acme"))
        .await
        .unwrap()
        .value;
    let id = saved.id();

    // A reader has no write right: the mutating method is refused.
    let error = engine
        .call_method(
            &world.inv("reader"),
            "customer",
            id,
            "touch",
            json!({}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");

    // The read-only method needs only `read`, which the reader holds.
    engine
        .call_method(
            &world.inv("reader"),
            "customer",
            id,
            "peek",
            json!({}),
            None,
        )
        .await
        .unwrap();
}

// -------------------------------------------------------------------------------------------
// Lists
// -------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_list_query_is_filtered_checked_and_column_restricted() {
    let world = World::new();
    let authorizer = PermissionAuthorizer::new(world.engine.clone(), Arc::new(memory(&world)));
    let query = customer_query();

    // A reader may read some rows; the hidden column is dropped and the masked one is planned.
    let plan = authorizer
        .readable_query(world.principal("reader"), &query)
        .await
        .unwrap()
        .expect("a readable plan");
    let aliases: Vec<String> = plan
        .query
        .select
        .iter()
        .map(|s| s.alias.as_str().to_owned())
        .collect();
    assert_eq!(aliases, ["name", "secret_note"], "salary is hidden");
    assert_eq!(plan.masks.len(), 1);
    assert_eq!(plan.masks[0].column.as_str(), "secret_note");

    // No role: no readable rows at all, answered without a storage read.
    assert!(
        authorizer
            .readable_query(world.principal("no_role"), &query)
            .await
            .unwrap()
            .is_none()
    );

    // Filtering on a field the reader may not read unmasked is refused.
    let mut filtered = customer_query();
    filtered.filter =
        Some(Expr::field(&n("c"), &n("salary")).eq(Expr::Value(SValue::Text("1".into()))));
    let error = authorizer
        .readable_query(world.principal("reader"), &filtered)
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.field_denied");
}

#[tokio::test]
async fn a_repository_query_in_a_step_is_masked_for_the_original_actor() {
    let world = World::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let probe = Probe::new("customer").step(
        "probe:peek_note",
        at(Phase::After, Operation::Create),
        move |ctx| {
            let captured = captured.clone();
            Box::pin(async move {
                let rows = ctx.query(&customer_query()).await?;
                let index = rows
                    .columns
                    .iter()
                    .position(|c| c.as_str() == "secret_note")
                    .unwrap();
                let value = rows.rows[0][index].clone();
                captured.lock().unwrap().push(value);
                Ok(())
            })
        },
    );
    let engine = build(authorizer(&world), probe).await;
    let mut doc = customer("Acme");
    doc.set(n("secret_note"), "classified");
    // `masked_creator` may create and only see the secret masked; the query it runs in the step
    // sees the masked value too.
    engine
        .create(&world.inv("masked_creator"), doc)
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert_eq!(seen.as_slice(), [Value::Text("***".into())]);
}

#[tokio::test]
async fn the_denial_counter_and_log_record_what_was_refused() {
    let world = World::new();
    let engine = engine(authorizer(&world)).await;
    let _ = engine
        .create(&world.inv("reader"), customer("A"))
        .await
        .unwrap_err();
    let mut doc = customer("B");
    doc.set(n("salary"), Value::Decimal(Decimal::new(100, 2)));
    let _ = engine.create(&world.inv("writer"), doc).await.unwrap_err();

    assert_eq!(engine.denial_count(), 2);
    let log = engine.denial_log();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0].code, "permission.denied");
    assert_eq!(log[1].code, "permission.field_denied");
    assert_eq!(log[1].at, world::now());
}

#[tokio::test]
async fn the_principal_of_a_user_invocation_is_the_one_the_source_resolves() {
    let world = World::new();
    let authorizer = authorizer(&world);
    let principal = authorizer.principal_for(&world.inv("reader")).await;
    assert_eq!(principal, world.principals["reader"]);
    let unknown = authorizer
        .principal_for(&spot_app_api::context::Invocation {
            actor: spot_app_api::context::Actor::User {
                id: RecordId::new_v7(),
                via: None,
            },
            ..world.inv("reader")
        })
        .await;
    assert!(
        unknown.assignments.is_empty(),
        "an unknown user gets no assignments and is refused, never an error"
    );
    assert_eq!(
        authorizer.principal_for(&world.system_inv()).await,
        Principal::system()
    );
}
