//! Relation-edge properties served by the hot graph (WP-GRA-03.T5): an edge event carries the
//! relation record's properties, an update changes them, a remove deletes the edge, and nothing
//! leaves the graph without a bound field-level access.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;

use spot_graph_index::{EdgeEvent, EdgeKey, EdgeOp, GraphIndex, IndexExecutor};
use spot_graph_model::{EdgeProperty, GraphModel, PropertyKind};
use spot_graph_query::{
    Direction, EdgeFieldAccess, EdgeSelector, GraphQuery, GraphResult, Include, NodeData,
};
use spot_storage_contract::{CmpOp, Expr, FieldRef, Name, RecordId, Value};
use support::{MapSource, block_on, engine, graph_key, graph_model, n, node, who};

const EDGE: &str = "po.superseded_by";

struct Hide(Vec<&'static str>);

impl EdgeFieldAccess for Hide {
    fn edge_field_readable(&self, _edge_type: &str, property: &Name) -> bool {
        !self.0.contains(&property.as_str())
    }
}

fn model() -> Arc<GraphModel> {
    let mut model = (*graph_model()).clone();
    model.edges.get_mut(EDGE).unwrap().properties = ["quantity", "unit_cost"]
        .iter()
        .map(|name| EdgeProperty {
            name: (*name).to_owned(),
            kind: PropertyKind::Scalar,
            required: false,
            options: None,
        })
        .collect();
    Arc::new(model)
}

fn event(seq: u64, op: EdgeOp, from: RecordId, to: RecordId, quantity: i64) -> EdgeEvent {
    let mut e = EdgeEvent::bare(
        seq,
        graph_key(),
        op,
        EdgeKey::new(EDGE, node("po", from), node("po", to)),
    );
    e.properties = [
        ("quantity".to_owned(), Value::Int(quantity)),
        ("unit_cost".to_owned(), Value::Int(10)),
    ]
    .into_iter()
    .collect();
    e
}

fn records(ids: &[RecordId]) -> MapSource {
    let records = ids
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
        .collect();
    MapSource { records }
}

fn ask(
    index: &Arc<GraphIndex>,
    ids: &[RecordId],
    start: RecordId,
    direction: Direction,
    access: Option<Arc<dyn EdgeFieldAccess>>,
) -> GraphResult {
    let access = access.unwrap_or_else(|| Arc::new(Hide(vec![])));
    let mut executor = IndexExecutor::hot(records(ids), model(), index.clone(), access);
    let query = GraphQuery::builder()
        .from(vec![node("po", start)])
        .follow(vec![EdgeSelector::new(EDGE)])
        .direction(direction)
        .max_depth(3)
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
        .build()
        .unwrap();
    let principal = who("alice", &["Auditor"]);
    let engine = engine();
    block_on(spot_graph_query::GraphExecutor::execute(
        &mut executor,
        &query,
        &spot_graph_query::Invocation {
            principal: &principal,
            engine: &engine,
        },
    ))
    .unwrap()
}

fn hop_quantity(result: &GraphResult, id: RecordId) -> Option<Value> {
    result
        .nodes
        .iter()
        .find(|x| x.node.id == id)
        .and_then(|x| x.path.last())
        .and_then(|hop| hop.properties.get(&n("quantity")).cloned())
}

#[test]
fn the_hot_graph_serves_declared_properties_and_follows_updates_and_removals() {
    let ids = [RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7()];
    let [p1, p2, p3] = ids;
    let mut graph = GraphIndex::new(graph_key());
    graph.apply(&event(1, EdgeOp::Add, p1, p2, 2)).unwrap();
    graph.apply(&event(2, EdgeOp::Add, p2, p3, 3)).unwrap();
    let index = Arc::new(graph.clone());
    let all: Arc<dyn EdgeFieldAccess> = Arc::new(Hide(vec![]));

    // A policy that hides every declared property: the graph hands out none.
    let hide_all: Arc<dyn EdgeFieldAccess> = Arc::new(Hide(vec!["quantity", "unit_cost"]));
    let none = ask(&index, &ids, p1, Direction::Out, Some(hide_all));
    assert_eq!(none.nodes.len(), 3);
    assert!(
        none.nodes
            .iter()
            .all(|x| x.path.iter().all(|hop| hop.properties.is_empty()))
    );

    // Bound access: the properties of each line, in both directions.
    let out = ask(&index, &ids, p1, Direction::Out, Some(all.clone()));
    assert_eq!(hop_quantity(&out, p2), Some(Value::Int(2)));
    assert_eq!(hop_quantity(&out, p3), Some(Value::Int(3)));
    let back = ask(&index, &ids, p3, Direction::In, Some(all.clone()));
    assert_eq!(hop_quantity(&back, p2), Some(Value::Int(3)));
    assert_eq!(hop_quantity(&back, p1), Some(Value::Int(2)));

    // A hidden property never leaves the graph.
    let hidden: Arc<dyn EdgeFieldAccess> = Arc::new(Hide(vec!["unit_cost"]));
    let out = ask(&index, &ids, p1, Direction::Out, Some(hidden));
    let hop = out
        .nodes
        .iter()
        .find(|x| x.node.id == p2)
        .unwrap()
        .path
        .last()
        .unwrap();
    assert!(hop.properties.contains_key(&n("quantity")));
    assert!(!hop.properties.contains_key(&n("unit_cost")));

    // An update changes the properties; a removal removes the edge.
    graph.apply(&event(3, EdgeOp::Update, p1, p2, 5)).unwrap();
    let index = Arc::new(graph.clone());
    let out = ask(&index, &ids, p1, Direction::Out, Some(all.clone()));
    assert_eq!(hop_quantity(&out, p2), Some(Value::Int(5)));
    graph.apply(&event(4, EdgeOp::Remove, p1, p2, 0)).unwrap();
    let index = Arc::new(graph);
    let out = ask(&index, &ids, p1, Direction::Out, Some(all));
    assert_eq!(out.nodes.len(), 1);
}

/// The hot and the cold executor of one policy give identical errors for a filter and a rollup
/// that name a hidden property, and neither lets the property reach a hop.
#[test]
fn hot_and_cold_paths_agree_on_a_hidden_property() {
    let ids = [RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7()];
    let [p1, p2, p3] = ids;
    let mut graph = GraphIndex::new(graph_key());
    graph.apply(&event(1, EdgeOp::Add, p1, p2, 2)).unwrap();
    graph.apply(&event(2, EdgeOp::Add, p2, p3, 3)).unwrap();
    let index = Arc::new(graph);
    let hide: Arc<dyn EdgeFieldAccess> = Arc::new(Hide(vec!["unit_cost"]));
    let principal = who("alice", &["Auditor"]);
    let engine = engine();
    let run = |hot: bool, query: &GraphQuery| {
        let invocation = spot_graph_query::Invocation {
            principal: &principal,
            engine: &engine,
        };
        if hot {
            let mut executor =
                IndexExecutor::hot(records(&ids), model(), index.clone(), hide.clone());
            block_on(spot_graph_query::GraphExecutor::execute(
                &mut executor,
                query,
                &invocation,
            ))
        } else {
            let mut executor = IndexExecutor::cold(records(&ids), model(), hide.clone());
            block_on(spot_graph_query::GraphExecutor::execute(
                &mut executor,
                query,
                &invocation,
            ))
        }
    };
    let base = || {
        GraphQuery::builder()
            .from(vec![node("po", p1)])
            .follow(vec![EdgeSelector::new(EDGE)])
            .direction(Direction::Out)
            .max_depth(3)
    };
    let filter = base()
        .edge_var(n("e"))
        .edge_filter(Expr::Cmp(
            Box::new(Expr::Field(FieldRef {
                alias: n("e"),
                field: n("unit_cost"),
            })),
            CmpOp::Ge,
            Box::new(Expr::Value(Value::Int(0))),
        ))
        .build()
        .unwrap();
    let rollup = base().rollup(n("unit_cost")).build().unwrap();
    for query in [&filter, &rollup] {
        let hot = run(true, query).unwrap_err();
        let cold = run(false, query).unwrap_err();
        assert_eq!(hot.code(), "graphq.unknown_edge_property");
        assert_eq!(hot.code(), cold.code());
        assert_eq!(hot.to_string(), cold.to_string());
    }
    let shown = base()
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
        .build()
        .unwrap();
    for hot in [true, false] {
        let result = run(hot, &shown).unwrap();
        assert!(result.nodes.iter().all(|x| {
            x.path
                .iter()
                .all(|hop| !hop.properties.contains_key(&n("unit_cost")))
        }));
    }
}
