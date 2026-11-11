//! Shared fixtures for the `spot-graph-index` tests: a four-DocType link chain, the graph model
//! and permission engine over it, a database-path [`NodeSource`] and [`GraphExecutor`] over
//! explicit records, and a [`GraphSource`] that streams the same records' edges.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_graph_index::{Edge, EdgeKey, GraphKey, GraphSource};
use spot_graph_model::{EdgeSource as ModelEdgeSource, GraphModel};
use spot_graph_query::{
    GraphExecutor, GraphQuery, GraphQueryError, GraphResult, Invocation, NodeData, NodeSource,
    Step, compile, walk,
};
use spot_meta::{Definition, EffectiveMeta, FieldSpec, FieldType, Registry};
use spot_permissions::model::{ModelTexts, Principal};
use spot_permissions::{PermissionEngine, Snapshot, load};
use spot_storage_contract::schema::system;
use spot_storage_contract::{Direction, EdgeSpec, Name, NodeRef, RecordId, Value};

/// The chain's DocTypes.
pub const DOCTYPES: [&str; 4] = ["po", "grn", "invoice", "payment"];

/// The DocType grants: `Auditor` reads everything, `Reader` reads purchase orders and invoices,
/// `Owner` reads the payments its user created.
pub const GRANTS: &str = r#"
[[entries]]
role = "Auditor"
doctype = "po"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "grn"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Auditor"
doctype = "payment"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "po"
rights = ["read"]
[[entries]]
role = "Reader"
doctype = "invoice"
rights = ["read"]
[[entries]]
role = "Owner"
doctype = "payment"
rights = ["read"]
if_owner = true
"#;

/// A storage name, for fixtures.
pub fn n(text: &str) -> Name {
    Name::new(text).expect("a valid name")
}

/// A record key.
pub fn node(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}

/// The graph key every fixture uses.
pub fn graph_key() -> GraphKey {
    GraphKey::new("acme", "hierarchies")
}

fn definition(name: &str, fields: Vec<FieldSpec>) -> Definition {
    let mut definition = Definition::new(name);
    definition.fields = fields;
    definition
}

fn link(name: &str, to: &str) -> FieldSpec {
    let mut field = FieldSpec::new(name, FieldType::Link);
    field.options = Some(to.to_owned());
    field
}

fn text(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

/// The effective metadata of the chain.
pub fn meta() -> Arc<EffectiveMeta> {
    Registry::new(vec![
        definition(
            "po",
            vec![text("title"), text("owner"), link("superseded_by", "po")],
        ),
        definition("grn", vec![text("owner"), link("po", "po")]),
        definition("invoice", vec![text("owner"), link("grn", "grn")]),
        definition(
            "payment",
            vec![text("owner"), link("invoice", "invoice"), text("kind")],
        ),
    ])
    .expect("definitions")
    .effective()
}

/// The graph model over the chain.
pub fn graph_model() -> Arc<GraphModel> {
    Arc::new(GraphModel::derive(&meta()))
}

/// The permission engine over the chain's grants.
pub fn engine() -> Arc<PermissionEngine> {
    let meta = meta();
    let model = load(&ModelTexts {
        grants: GRANTS.to_owned(),
        ..Default::default()
    })
    .expect("grants load");
    let snapshot = Snapshot::build(&meta, &model).expect("snapshot");
    Arc::new(PermissionEngine::new(Arc::new(snapshot)))
}

/// A principal with the given roles.
pub fn who(user: &str, roles: &[&str]) -> Principal {
    Principal::with_roles(user, roles)
}

/// Runs a future that completes without waiting on anything else.
pub fn block_on<F: Future>(future: F) -> F::Output {
    let mut cx = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
    }
}

/// The five records of the chain: a purchase order, its receipt, its invoice and two payments.
pub struct Chain {
    pub p1: RecordId,
    pub g1: RecordId,
    pub i1: RecordId,
    pub y1: RecordId,
    pub y2: RecordId,
}

impl Chain {
    /// New random record ids.
    #[must_use]
    pub fn new() -> Self {
        Self {
            p1: RecordId::new_v7(),
            g1: RecordId::new_v7(),
            i1: RecordId::new_v7(),
            y1: RecordId::new_v7(),
            y2: RecordId::new_v7(),
        }
    }

    /// The records as a database-path source would read them: each link field carries its target.
    #[must_use]
    pub fn records(&self) -> BTreeMap<NodeRef, NodeData> {
        let mut records: BTreeMap<NodeRef, NodeData> = BTreeMap::new();
        records.insert(
            node("po", self.p1),
            data(
                &[("title", "PO-1".into()), ("owner", "alice".into())],
                "alice",
            ),
        );
        records.insert(
            node("grn", self.g1),
            data(
                &[("owner", "alice".into()), ("po", Value::Id(self.p1))],
                "alice",
            ),
        );
        records.insert(
            node("invoice", self.i1),
            data(
                &[("owner", "alice".into()), ("grn", Value::Id(self.g1))],
                "alice",
            ),
        );
        records.insert(
            node("payment", self.y1),
            data(
                &[
                    ("owner", "alice".into()),
                    ("invoice", Value::Id(self.i1)),
                    ("kind", "bank".into()),
                ],
                "alice",
            ),
        );
        records.insert(
            node("payment", self.y2),
            data(
                &[
                    ("owner", "bob".into()),
                    ("invoice", Value::Id(self.i1)),
                    ("kind", "cash".into()),
                ],
                "bob",
            ),
        );
        records
    }
}

impl Default for Chain {
    fn default() -> Self {
        Self::new()
    }
}

fn data(values: &[(&str, Value)], created_by: &str) -> NodeData {
    NodeData {
        values: values
            .iter()
            .map(|(name, value)| (n(name), value.clone()))
            .collect(),
        state: None,
        created_by: Some(created_by.to_owned()),
    }
}

/// The records' edges, in storage orientation, as the database would stream them. Link,
/// dynamic-link and table edges are derived from the records; multi-select and relation edges are
/// not in this fixture.
#[must_use]
pub fn edges_of(model: &GraphModel, records: &BTreeMap<NodeRef, NodeData>) -> Vec<Edge> {
    let mut out: Vec<Edge> = Vec::new();
    for edge_type in model.edges.values() {
        match edge_type.source {
            ModelEdgeSource::LinkField | ModelEdgeSource::DynamicLink => {
                let Some(field) = edge_type.name.strip_prefix(&format!("{}.", edge_type.from))
                else {
                    continue;
                };
                for (holder, holder_data) in records {
                    if holder.entity.as_str() != edge_type.from {
                        continue;
                    }
                    if let Some(Value::Id(target)) = holder_data.values.get(&n(field))
                        && let Some(found) = find_by_id(records, *target)
                    {
                        out.push(Edge::bare(EdgeKey::new(
                            edge_type.name.clone(),
                            holder.clone(),
                            found,
                        )));
                    }
                }
            }
            ModelEdgeSource::Table => {
                let Some(child) = edge_type.to_one() else {
                    continue;
                };
                for (holder, holder_data) in records {
                    if holder.entity.as_str() != child {
                        continue;
                    }
                    if let Some(Value::Id(parent)) = holder_data.values.get(&n(system::PARENT))
                        && let Some(found) = find_by_id(records, *parent)
                    {
                        out.push(Edge::bare(EdgeKey::new(
                            edge_type.name.clone(),
                            holder.clone(),
                            found,
                        )));
                    }
                }
            }
            ModelEdgeSource::TableMultiselect | ModelEdgeSource::Relation => {}
        }
    }
    out
}

fn find_by_id(records: &BTreeMap<NodeRef, NodeData>, id: RecordId) -> Option<NodeRef> {
    records.keys().find(|candidate| candidate.id == id).cloned()
}

/// A [`NodeSource`] over explicit records: the database path's values and its one-step
/// neighbours, by the storage edge spec exactly as storage computes them.
pub struct MapSource {
    pub records: BTreeMap<NodeRef, NodeData>,
}

impl NodeSource for MapSource {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        Ok(self.records.get(node).cloned())
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        Ok(neighbours(&self.records, node, &step.spec, step.direction))
    }
}

/// The records one step from `node` along `spec`, in `direction`: the same rule storage applies,
/// computed from the records' link values.
#[must_use]
pub fn neighbours(
    records: &BTreeMap<NodeRef, NodeData>,
    node: &NodeRef,
    spec: &EdgeSpec,
    direction: Direction,
) -> Vec<NodeRef> {
    let mut out: BTreeSet<NodeRef> = BTreeSet::new();
    let outgoing = matches!(direction, Direction::Outgoing | Direction::Both);
    let incoming = matches!(direction, Direction::Incoming | Direction::Both);
    if outgoing
        && node.entity == spec.entity
        && let Some(Value::Id(target)) = records.get(node).and_then(|d| d.values.get(&spec.field))
        && let Some(found) = find_by_id(records, *target)
    {
        out.insert(found);
    }
    if incoming {
        for (holder, holder_data) in records {
            if holder.entity == spec.entity
                && holder_data.values.get(&spec.field) == Some(&Value::Id(node.id))
            {
                out.insert(holder.clone());
            }
        }
    }
    out.remove(node);
    out.into_iter().collect()
}

/// An executor that answers from explicit records, with the query API's own compile and walk.
pub struct MapExecutor {
    pub model: Arc<GraphModel>,
    pub records: BTreeMap<NodeRef, NodeData>,
}

impl GraphExecutor for MapExecutor {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = compile(&self.model, query)?;
        let mut source = MapSource {
            records: self.records.clone(),
        };
        walk(&mut source, &steps, query, who).await
    }
}

/// A [`GraphSource`] over explicit records: the edges and watermark the database would report.
pub struct RecordSource {
    pub model: Arc<GraphModel>,
    pub records: BTreeMap<NodeRef, NodeData>,
    pub watermark: u64,
}

impl GraphSource for RecordSource {
    fn stream_edges(
        &self,
        _key: &GraphKey,
    ) -> Result<Vec<Edge>, spot_graph_index::GraphIndexError> {
        Ok(edges_of(&self.model, &self.records))
    }

    fn watermark(&self, _key: &GraphKey) -> Result<u64, spot_graph_index::GraphIndexError> {
        Ok(self.watermark)
    }
}

/// A [`GraphSource`] over an explicit edge map and watermark, for the property tests.
pub struct MapGraphSource {
    pub key: GraphKey,
    pub edges: Vec<Edge>,
    pub watermark: u64,
}

impl GraphSource for MapGraphSource {
    fn stream_edges(
        &self,
        _key: &GraphKey,
    ) -> Result<Vec<Edge>, spot_graph_index::GraphIndexError> {
        Ok(self.edges.clone())
    }

    fn watermark(&self, _key: &GraphKey) -> Result<u64, spot_graph_index::GraphIndexError> {
        Ok(self.watermark)
    }
}

/// Runs `query` for `principal` through `executor`, with the chain's engine.
pub async fn run<E: GraphExecutor>(
    executor: &mut E,
    query: &GraphQuery,
    principal: &Principal,
) -> Result<GraphResult, GraphQueryError> {
    let engine = engine();
    executor
        .execute(
            query,
            &Invocation {
                principal,
                engine: &engine,
            },
        )
        .await
}
