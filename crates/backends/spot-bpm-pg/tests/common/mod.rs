//! Shared setup: a scratch database, a connection pool, the process every end-to-end test runs,
//! and the effect ledger's handler registry.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use deadpool_postgres::{Pool, PoolConfig, Runtime};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{EffectLedger, HandlerRegistry, NodeKind, RecordingHandler};
use spot_bpm_model::{ProcessVersion, from_text};
use spot_dag::NodeId;
use spot_postgres::testing::TestDb;
use tokio_postgres::NoTls;

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

/// The fixed instant every test passes in (2023-11-14T22:13:20Z); tests never read a clock.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

/// A node id, unwrapped for tests.
pub fn nid(raw: &str) -> NodeId {
    NodeId::new(raw).unwrap()
}

/// The three-node process the end-to-end tests run: a trigger, a document node that performs one
/// side effect, and a terminal join.
pub const LINEAR: &str = r#"
name = "linear"
app = "bpm_pg_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "work"
kind = "document"
action = "create"
doctype = "Sales Order"

[[nodes]]
id = "done"
kind = "logic"
logic = "join"
terminal = true

[[edges]]
from = "start"
to = "work"

[[edges]]
from = "work"
to = "done"
"#;

/// The process text above, parsed.
pub fn process() -> ProcessVersion {
    from_text(LINEAR).expect("the process text must parse")
}

/// A two-node process: a trigger and one terminal document node, for the scale run.
pub const TWO_NODE: &str = r#"
name = "two"
app = "bpm_pg_test"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "work"
kind = "document"
action = "create"
doctype = "Sales Order"
terminal = true

[[edges]]
from = "start"
to = "work"
"#;

/// The two-node process above, parsed.
pub fn two_node() -> ProcessVersion {
    from_text(TWO_NODE).expect("the two-node process text must parse")
}

/// The default logic handlers plus recording handlers for documents and integrations.
pub fn handlers(ledger: &Arc<EffectLedger>) -> HandlerRegistry {
    let mut registry = HandlerRegistry::logic_default();
    registry.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
    registry.register(RecordingHandler::new(NodeKind::Integration, ledger.clone()));
    registry
}

/// A chain `n0 -> n1 -> ... -> n{len-1}` with the default join rules.
pub fn chain(len: usize) -> spot_dag::CompiledDag {
    use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node};

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
pub fn published(dag: &spot_dag::CompiledDag) -> spot_dag::Published {
    spot_dag::Published {
        version: dag.version(),
        hash: dag.hash().clone(),
    }
}

/// A fresh Case record over a chain, not yet stored.
pub fn new_case(dag: &spot_dag::CompiledDag, id: &str, key: &str) -> spot_bpm_engine::CaseRecord {
    let plan = spot_dag_exec::start(
        dag,
        &published(dag),
        spot_dag_exec::RunId::new(id).unwrap(),
        key,
        now(),
    )
    .unwrap();
    spot_bpm_engine::CaseRecord::new(plan.run, None)
}
