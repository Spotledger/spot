//! Shared scaffolding for the crate's tests: a dependency-free executor, a small compiled DAG
//! and a storage fake with one `probe` document entity.
//!
//! Compiled only under `cfg(test)`, so it may use `unwrap` and the dev-dependency backends.
#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::sync::Arc;

use serde_json::Value as Json;
use spot_app_api::prelude::OffsetDateTime;
use spot_dag::{CompiledDag, DagDefinition, Edge, JoinRule, Limits, Node, NodeId, Published};
use spot_storage_contract::{
    Catalog, Collation, DocumentData, EntityDef, EntityKind, FieldDef, FieldType, Fields, Name,
    RecordId, StorageBackend, Value,
};
use spot_storage_fake::FakeBackend;

/// The fixed instant every test passes in (2023-11-14T22:13:20Z); tests never read a clock.
pub(crate) const NOW_SECONDS: i64 = 1_700_000_000;

/// The fixed instant every test passes in.
pub(crate) fn now() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(NOW_SECONDS).unwrap()
}

/// Runs a future to completion on the current thread.
///
/// Every future this crate and the in-memory fake produce is immediately ready, so a no-op
/// waker suffices; there is no runtime to start.
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = std::task::Context::from_waker(waker);
    loop {
        match future.as_mut().poll(&mut context) {
            std::task::Poll::Ready(value) => return value,
            std::task::Poll::Pending => std::thread::yield_now(),
        }
    }
}

/// A node with the default join rule.
pub(crate) fn node(id: &str) -> Node {
    Node::new(NodeId::new(id).unwrap(), "step", JoinRule::All, Json::Null).unwrap()
}

/// An unconditional edge.
pub(crate) fn edge(from: &str, to: &str) -> Edge {
    Edge::new(NodeId::new(from).unwrap(), NodeId::new(to).unwrap(), None)
}

/// Compiles a definition with the default limits.
pub(crate) fn dag(nodes: Vec<Node>, edges: Vec<Edge>) -> CompiledDag {
    let def = DagDefinition::new("workflow", 1, nodes, edges);
    CompiledDag::compile(&def, &Limits::new()).unwrap()
}

/// The `Published` pin matching a compiled DAG.
pub(crate) fn published(dag: &CompiledDag) -> Published {
    Published {
        version: dag.version(),
        hash: dag.hash().clone(),
    }
}

fn name(raw: &str) -> Name {
    Name::new(raw).unwrap()
}

/// A fresh backend with one `probe` document entity, prepared and ready to write.
pub(crate) fn store() -> (FakeBackend, Arc<Catalog>) {
    let entity = EntityDef {
        name: name("probe"),
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: name("label"),
            ty: FieldType::Text { max_chars: None },
            nullable: true,
        }],
        children: Vec::new(),
        indexes: Vec::new(),
        exclusions: vec![],
        search: None,
    };
    let catalog = Catalog::new(1, Collation::new("en").unwrap(), vec![entity]).unwrap();
    let backend = FakeBackend::new();
    block_on(backend.prepare(&catalog)).unwrap();
    (backend, Arc::new(catalog))
}

/// A `probe` document with the given id.
pub(crate) fn probe_doc(id: RecordId) -> DocumentData {
    let mut fields = Fields::new();
    fields.insert(name("label"), Value::Text("probe".to_owned()));
    DocumentData {
        entity: name("probe"),
        id,
        fields,
        children: Default::default(),
    }
}

/// A document of an entity no catalog holds, for a failing write.
pub(crate) fn unknown_doc(id: RecordId) -> DocumentData {
    DocumentData {
        entity: name("missing"),
        id,
        fields: Fields::new(),
        children: Default::default(),
    }
}
