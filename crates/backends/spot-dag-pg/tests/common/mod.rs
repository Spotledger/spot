//! Shared setup for the tests: a scratch database, a connection pool, and small DAG helpers.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_app_api::prelude::OffsetDateTime;
use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId, Published};
use spot_dag_exec::{NodeOutcome, Run, RunId, claim, complete, start};
use spot_dag_pg::PgExecStore;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::{Catalog, Collation, EntityDef, EntityKind, FieldDef, FieldType, Name};
use tokio_postgres::NoTls;

/// The trait whose methods the tests call on `PgExecStore`; re-exported so `use common::*`
/// brings it into scope.
pub use spot_dag_exec::ExecStore;

/// A pool of `size` connections whose server-side `application_name` is `app`.
pub fn pool(db: &TestDb, size: usize, app: &str) -> Pool {
    let c = db.config();
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some(c.host.clone());
    cfg.port = Some(c.port);
    cfg.dbname = Some(c.dbname.clone());
    cfg.user = Some(c.user.clone());
    cfg.password = Some(c.password.clone());
    cfg.application_name = Some(app.to_owned());
    cfg.pool = Some(PoolConfig::new(size));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

/// A backend on the scratch database, for the transaction the `Unit` port commits through.
pub fn backend(db: &TestDb) -> PgBackend {
    PgBackend::connect_unchecked(db.config()).expect("backend")
}

/// The tenant catalog the tests declare their transactions with: one document entity with one
/// non-null text field, which is all the atomicity tests need to write a real document.
pub fn catalog() -> Arc<Catalog> {
    let entity = EntityDef {
        name: Name::new("probe").unwrap(),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: Name::new("label").unwrap(),
            ty: FieldType::Text {
                max_chars: Some(64),
            },
            nullable: false,
        }],
        children: Vec::new(),
        indexes: Vec::new(),
        exclusions: vec![],
        search: None,
    };
    Arc::new(
        Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).expect("catalog compiles"),
    )
}

/// A store on a new pool, with the backend and catalog its transactions need.
pub fn store(db: &TestDb, size: usize, app: &str) -> PgExecStore {
    PgExecStore::new(pool(db, size, app), backend(db), catalog())
}

/// A storage name, unwrapped for tests.
pub fn n(raw: &str) -> Name {
    Name::new(raw).unwrap()
}

/// The fixed instant every test passes in (2023-11-14T22:13:20Z); tests never read a clock.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A node id, unwrapped for tests.
pub fn nid(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// A chain `n0 -> n1 -> ... -> n{len-1}` with the default join rules.
pub fn chain(len: usize) -> CompiledDag {
    let nodes = (0..len)
        .map(|i| {
            Node::new(
                nid(&format!("n{i}")),
                "step",
                JoinRule::All,
                serde_json::Value::Null,
            )
            .unwrap()
        })
        .collect::<Vec<_>>();
    let edges = (0..len.saturating_sub(1))
        .map(|i| Edge::new(nid(&format!("n{i}")), nid(&format!("n{}", i + 1)), None))
        .collect::<Vec<_>>();
    let def = DagDefinition::new("chain", 1, nodes, edges);
    CompiledDag::compile(&def, &Limits::new()).unwrap()
}

/// The published pin of a compiled DAG.
pub fn published(dag: &CompiledDag) -> Published {
    Published {
        version: dag.version(),
        hash: dag.hash().clone(),
    }
}

/// Stores a fresh run on `store`.
pub async fn create(store: &PgExecStore, dag: &CompiledDag, id: &str, key: &str) -> Run {
    let plan = start(dag, &published(dag), RunId::new(id).unwrap(), key, now()).unwrap();
    store.create_run(plan).await.unwrap()
}

/// Claims and completes one ready node, returning the stored run.
pub async fn run_node(store: &PgExecStore, dag: &CompiledDag, run: &Run, node: &NodeId) -> Run {
    let begin = claim(run, node, now()).unwrap();
    let running = store.apply(&begin, run.version).await.unwrap();
    let done = complete(
        dag,
        &running,
        node,
        NodeOutcome::Done {
            output: None,
            branches: Vec::new(),
        },
        now(),
    )
    .unwrap();
    store.apply(&done, running.version).await.unwrap()
}
