//! `Engine::query_as` (WP-EXT-06.T11): a public query that runs with a given invocation's
//! rights. A permitted caller reads rows, a caller without the right gets what a step's own
//! query gets, a masked field is masked, a hidden column is dropped, an owner-restricted DocType
//! returns only what the owner rule lets through, and the result equals the repository's.
#![allow(missing_docs, dead_code, clippy::unwrap_used, clippy::expect_used)]

#[path = "authz/world.rs"]
mod world;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_app_api::context::{DocumentService, HookContext, MethodContext};
use spot_app_api::controller::{Controller, MethodDecl, StepDecl, StepName};
use spot_app_api::prelude::{Query, RecordId, Rows, Value};
use spot_app_api::{AppResult, BoxFuture, Document, HookPoint, Operation, Phase, Target};
use spot_core::ManualClock;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{Expr, QueryPath, Selection, Value as SValue};
use spot_storage_fake::FakeBackend;
use world::{World, n};

fn authorizer(world: &World) -> Arc<PermissionAuthorizer> {
    let mut memory = MemoryPrincipals::new();
    for (name, principal) in &world.principals {
        memory = memory.with(&world.id(name), principal.clone());
    }
    Arc::new(PermissionAuthorizer::new(
        world.engine.clone(),
        Arc::new(memory),
    ))
}

type StepFn =
    Arc<dyn for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>> + Send + Sync>;

/// A controller with one step on `customer` that runs `f`.
struct Probe {
    decl: Option<(StepDecl, StepFn)>,
}

impl Probe {
    fn none() -> Self {
        Self { decl: None }
    }

    fn after_create<F>(name: &str, f: F) -> Self
    where
        F: for<'a> Fn(&'a mut dyn HookContext) -> BoxFuture<'a, AppResult<()>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            decl: Some((
                StepDecl {
                    name: StepName::new(name).unwrap(),
                    point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
                    priority: 0,
                    budget: Duration::from_secs(5),
                },
                Arc::new(f),
            )),
        }
    }
}

impl Controller for Probe {
    fn doctype(&self) -> &str {
        "customer"
    }

    fn steps(&self) -> Vec<StepDecl> {
        self.decl.iter().map(|(d, _)| d.clone()).collect()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        Vec::new()
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        match &self.decl {
            Some((d, f)) if &d.name == step => f(ctx),
            _ => Box::pin(async { Ok(()) }),
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

async fn engine(world: &World, probe: Probe) -> Engine<FakeBackend> {
    let probe = Arc::new(probe);
    let mut builder = RegistryBuilder::new();
    builder.add_controller("probe", probe.clone());
    let engine = Engine::new(
        FakeBackend::new(),
        world::meta(),
        Arc::new(builder.build().unwrap()),
        Arc::new(ManualClock::new(world::now())),
        Arc::new(V7Ids),
    )
    .unwrap()
    .with_authorizer(authorizer(world))
    .with_controllers(vec![probe]);
    engine.prepare().await.unwrap();
    engine
}

fn customer(name: &str, secret: &str, salary_cents: i64) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc.set(n("secret_note"), secret);
    doc.set(n("created_by"), "editor");
    doc.set(
        n("salary"),
        Value::Decimal(spot_app_api::prelude::Decimal::new(salary_cents, 2)),
    );
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

fn column(rows: &Rows, name: &str) -> Option<Vec<Value>> {
    let index = rows.columns.iter().position(|c| c.as_str() == name)?;
    Some(rows.rows.iter().map(|r| r[index].clone()).collect())
}

async fn seeded(world: &World, probe: Probe) -> Engine<FakeBackend> {
    let engine = engine(world, probe).await;
    for (name, secret, cents) in [("Acme", "alpha", 1_000), ("Beta", "beta", 2_000)] {
        engine
            .create(&world.inv("editor"), customer(name, secret, cents))
            .await
            .unwrap();
    }
    engine
}

#[tokio::test]
async fn a_permitted_caller_reads_the_rows_unmasked() {
    let world = World::new();
    let engine = seeded(&world, Probe::none()).await;
    let rows = engine
        .query_as(&world.inv("editor"), &customer_query())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    let mut names: Vec<String> = column(&rows, "name")
        .unwrap()
        .into_iter()
        .map(|v| match v {
            Value::Text(t) => t,
            other => panic!("a name is text, got {other:?}"),
        })
        .collect();
    names.sort();
    assert_eq!(names, ["Acme", "Beta"]);
    let mut notes = column(&rows, "secret_note").unwrap();
    notes.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(
        notes,
        [Value::Text("alpha".into()), Value::Text("beta".into())],
        "an editor holds the confidential group: the secret is not masked"
    );
    assert!(column(&rows, "salary").is_some(), "the editor reads salary");
}

#[tokio::test]
async fn a_masked_field_is_masked_and_a_hidden_column_is_dropped() {
    let world = World::new();
    let engine = seeded(&world, Probe::none()).await;
    let rows = engine
        .query_as(&world.inv("reader"), &customer_query())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    assert_eq!(
        column(&rows, "secret_note").unwrap(),
        [Value::Text("***".into()), Value::Text("***".into())],
        "read_masked comes back masked"
    );
    assert!(
        column(&rows, "salary").is_none(),
        "a column the reader may not read is not in the result"
    );
    assert!(column(&rows, "name").is_some());
}

#[tokio::test]
async fn a_caller_without_the_right_gets_what_a_step_query_gets_and_no_bypass() {
    let world = World::new();
    let engine = seeded(&world, Probe::none()).await;
    // No role at all: no readable row, an empty page without a storage read, never rows.
    let rows = engine
        .query_as(&world.inv("no_role"), &customer_query())
        .await
        .unwrap();
    assert!(rows.rows.is_empty(), "{rows:?}");
    assert_eq!(
        rows.columns,
        customer_query()
            .select
            .iter()
            .map(|s| s.alias.clone())
            .collect::<Vec<_>>(),
        "the empty page keeps the query's own column names"
    );

    // A filter on a field the caller may not read unmasked is refused, as it is for a list.
    let mut filtered = customer_query();
    filtered.filter =
        Some(Expr::field(&n("c"), &n("salary")).eq(Expr::Value(SValue::Text("1".into()))));
    let error = engine
        .query_as(&world.inv("reader"), &filtered)
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.field_denied");
    assert!(engine.denial_count() >= 1, "the refusal is counted");
}

#[tokio::test]
async fn an_owner_restricted_role_sees_only_what_the_owner_rule_lets_through() {
    let world = World::new();
    let engine = seeded(&world, Probe::none()).await;
    // The two documents were created by the editor. The owner-only role reads none of them; a
    // plain reader reads both: the owner restriction is in the query, not in the caller.
    for owner_only in ["owner_reader_own", "owner_reader_other"] {
        let rows = engine
            .query_as(&world.inv(owner_only), &customer_query())
            .await
            .unwrap();
        assert!(rows.rows.is_empty(), "{owner_only} owns none of them");
    }
    let everyone = engine
        .query_as(&world.inv("reader"), &customer_query())
        .await
        .unwrap();
    assert_eq!(everyone.rows.len(), 2);
}

#[tokio::test]
async fn the_system_invocation_is_not_masked() {
    let world = World::new();
    let engine = seeded(&world, Probe::none()).await;
    let rows = engine
        .query_as(&world.system_inv(), &customer_query())
        .await
        .unwrap();
    assert_eq!(rows.rows.len(), 2);
    let mut notes = column(&rows, "secret_note").unwrap();
    notes.sort_by_key(|v| format!("{v:?}"));
    assert_eq!(
        notes,
        [Value::Text("alpha".into()), Value::Text("beta".into())]
    );
}

#[tokio::test]
async fn the_result_equals_the_repository_query_of_a_step_for_the_same_caller() {
    let world = World::new();
    let seen: Arc<Mutex<Vec<Rows>>> = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let probe = Probe::after_create("probe:peek", move |ctx| {
        let captured = captured.clone();
        Box::pin(async move {
            let rows = ctx.query(&customer_query()).await?;
            captured.lock().unwrap().push(rows);
            Ok(())
        })
    });
    let engine = engine(&world, probe).await;
    // `masked_creator` creates (the step queries as that caller) and may read masked only.
    // It may not set `salary` (a field it cannot read unmasked), so the document leaves it out.
    let mut gamma = Document::new(n("customer"), RecordId::new_v7());
    gamma.set(n("name"), "Gamma");
    gamma.set(n("secret_note"), "classified");
    gamma.set(n("created_by"), "masked_creator");
    engine
        .create(&world.inv("masked_creator"), gamma)
        .await
        .unwrap();
    let from_step = seen.lock().unwrap().pop().unwrap();
    let from_engine = engine
        .query_as(&world.inv("masked_creator"), &customer_query())
        .await
        .unwrap();
    assert_eq!(from_engine.columns, from_step.columns);
    assert_eq!(from_engine.rows, from_step.rows);
    assert_eq!(
        column(&from_engine, "secret_note").unwrap(),
        [Value::Text("***".into())]
    );
}
