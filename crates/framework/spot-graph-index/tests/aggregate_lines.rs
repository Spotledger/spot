//! Edge types that merge repeated lines (WP-GRA-03.T9): the hot index never answers a step of
//! such an edge type, because the sum is the sum of the lines the caller may read and the index
//! holds one edge per pair. Hot and cold executors therefore agree, through one `merge_lines`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_graph_index::{EdgeEvent, EdgeKey, EdgeOp, GraphIndex, IndexExecutor};
use spot_graph_model::{AggregateHow, AggregateSpec, GraphModel, RelationRef};
use spot_graph_query::{
    Direction, EdgeFieldAccess, EdgeProps, EdgeSelector, GraphExecutor, GraphQuery,
    GraphQueryError, GraphResult, Include, NodeData, NodeSource, Step, merge_lines,
};
use spot_storage_contract::{Name, NodeRef, RecordId, Value};
use support::{MapSource, block_on, engine, graph_key, graph_model, n, node, who};

const EDGE: &str = "po.superseded_by";

struct AllowAll;

impl EdgeFieldAccess for AllowAll {
    fn edge_field_readable(&self, _edge_type: &str, _property: &Name) -> bool {
        true
    }
}

fn model(aggregate: bool) -> Arc<GraphModel> {
    let mut model = (*graph_model()).clone();
    let edge = model.edges.get_mut(EDGE).unwrap();
    edge.source = spot_graph_model::EdgeSource::Relation;
    edge.properties = vec![spot_graph_model::EdgeProperty {
        name: "quantity".to_owned(),
        kind: spot_graph_model::PropertyKind::Scalar,
        required: false,
        options: None,
    }];
    edge.relation = Some(RelationRef {
        doctype: "bom_line".into(),
        from_field: "assembly".into(),
        to_field: "component".into(),
        aggregate: aggregate.then(|| AggregateSpec {
            property: "quantity".into(),
            how: AggregateHow::Sum,
        }),
    });
    Arc::new(model)
}

/// The database path: the records, and for the one pair `from -> to` the lines the caller can
/// read, merged through the shared function.
struct Lines {
    records: MapSource,
    from: NodeRef,
    to: NodeRef,
    lines: Vec<EdgeProps>,
}

impl NodeSource for Lines {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        self.records.load(node).await
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        _step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        Ok(if *node == self.from {
            vec![self.to.clone()]
        } else {
            vec![]
        })
    }

    async fn edge_properties(
        &mut self,
        from: &NodeRef,
        to: &NodeRef,
        step: &Step,
    ) -> Result<Option<EdgeProps>, GraphQueryError> {
        if *from != self.from || *to != self.to {
            return Ok(None);
        }
        let aggregate = step
            .relation
            .as_ref()
            .and_then(|r| r.aggregate_sum.as_ref());
        merge_lines(&step.name, aggregate, &self.lines).map(Some)
    }
}

fn records(ids: &[RecordId]) -> MapSource {
    MapSource {
        records: ids
            .iter()
            .map(|id| {
                (
                    node("po", *id),
                    NodeData {
                        values: [
                            (n("title"), Value::Text("PO".into())),
                            (n("owner"), Value::Text("alice".into())),
                        ]
                        .into_iter()
                        .collect(),
                        state: None,
                        created_by: Some("alice".into()),
                    },
                )
            })
            .collect(),
    }
}

fn line(quantity: i64) -> EdgeProps {
    [(n("quantity"), Value::Int(quantity))]
        .into_iter()
        .collect()
}

fn ask(
    model: Arc<GraphModel>,
    index: Option<Arc<GraphIndex>>,
    lines: Vec<EdgeProps>,
    a: RecordId,
    b: RecordId,
) -> Result<GraphResult, GraphQueryError> {
    let source = Lines {
        records: records(&[a, b]),
        from: node("po", a),
        to: node("po", b),
        lines,
    };
    let mut executor = IndexExecutor::new(source, model, index, Arc::new(AllowAll));
    let query = GraphQuery::builder()
        .from(vec![node("po", a)])
        .follow(vec![EdgeSelector::new(EDGE)])
        .direction(Direction::Out)
        .max_depth(2)
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
        .build()
        .unwrap();
    let principal = who("alice", &["Auditor"]);
    let engine = engine();
    block_on(executor.execute(
        &query,
        &spot_graph_query::Invocation {
            principal: &principal,
            engine: &engine,
        },
    ))
}

fn hot_index(a: RecordId, b: RecordId, stored_quantity: i64) -> Arc<GraphIndex> {
    let mut graph = GraphIndex::new(graph_key());
    let mut event = EdgeEvent::bare(
        1,
        graph_key(),
        EdgeOp::Add,
        EdgeKey::new(EDGE, node("po", a), node("po", b)),
    );
    event.properties = [("quantity".to_owned(), Value::Int(stored_quantity))]
        .into_iter()
        .collect();
    graph.apply(&event).unwrap();
    Arc::new(graph)
}

fn quantity(result: &GraphResult, id: RecordId) -> Option<Value> {
    result
        .nodes
        .iter()
        .find(|x| x.node.id == id)
        .and_then(|x| x.path.last())
        .and_then(|hop| hop.properties.get(&n("quantity")).cloned())
}

#[test]
fn hot_and_cold_agree_on_a_merged_edge_and_the_index_is_not_consulted() {
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    let lines = vec![line(5), line(3)];
    // The index deliberately stores a different quantity: if it answered, the sums would differ.
    let hot = ask(model(true), Some(hot_index(a, b, 999)), lines.clone(), a, b).unwrap();
    let cold = ask(model(true), None, lines, a, b).unwrap();
    assert_eq!(quantity(&hot, b), Some(Value::Int(8)));
    assert_eq!(hot, cold);
}

#[test]
fn hot_and_cold_refuse_alike_on_overflow_and_the_cold_path_refuses_undeclared_pairs() {
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    let big = vec![line(i64::MAX), line(1)];
    for index in [Some(hot_index(a, b, 1)), None] {
        let err = ask(model(true), index, big.clone(), a, b).unwrap_err();
        assert_eq!(err.code(), "graphq.edge_value_overflow");
    }
    // Without the declaration the database path refuses the pair. (A hot index holds one edge
    // per pair by construction, so it never sees repeated lines; see the next test.)
    let err = ask(model(false), None, vec![line(5), line(3)], a, b).unwrap_err();
    assert_eq!(err.code(), "graphq.ambiguous_relation_edge");
}

#[test]
fn an_edge_type_without_the_declaration_is_still_served_hot() {
    let (a, b) = (RecordId::new_v7(), RecordId::new_v7());
    // One line: the database path would say 5, the index says 42, and the hot answer is the index's.
    let hot = ask(model(false), Some(hot_index(a, b, 42)), vec![line(5)], a, b).unwrap();
    assert_eq!(quantity(&hot, b), Some(Value::Int(42)));
}
