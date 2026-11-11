//! Edge properties in the query API (WP-GRA-03.T2): filter on them through the edge variable,
//! return them per hop, roll one up along a path, and show them only between visible nodes.
//!
//! The source is explicit: a three-level bill of materials over the `po.superseded_by` link
//! (each purchase order "contains" the next, with a quantity on the edge), and a receipt edge
//! (`grn.po`) that the `Reader` role cannot see the holder of.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_model::{EdgeProperty, GraphModel, PropertyKind};
use spot_graph_query::{
    Direction, EdgeProps, EdgeSelector, GraphExecutor, GraphQuery, GraphQueryError, GraphResult,
    Include, Invocation, NodeData, NodeSource, Step, compile, walk,
};
use spot_storage_contract::{CmpOp, Expr, FieldRef, NodeRef, RecordId, Value};
use support::*;

type Key = (NodeRef, NodeRef);

/// Records plus the properties of each edge, keyed `(holder, target)` as storage orients links.
struct PropSource {
    nodes: BTreeMap<NodeRef, NodeData>,
    edges: BTreeMap<Key, EdgeProps>,
}

impl NodeSource for PropSource {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        Ok(self.nodes.get(node).cloned())
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        Ok(neighbours(&self.nodes, node, step))
    }

    async fn edge_properties(
        &mut self,
        from: &NodeRef,
        to: &NodeRef,
        step: &Step,
    ) -> Result<Option<EdgeProps>, GraphQueryError> {
        use spot_storage_contract::Direction as D;
        let forward = (from.clone(), to.clone());
        let backward = (to.clone(), from.clone());
        let found = match step.direction {
            D::Outgoing => self.edges.get(&forward),
            D::Incoming => self.edges.get(&backward),
            D::Both => self
                .edges
                .get(&forward)
                .or_else(|| self.edges.get(&backward)),
        };
        Ok(found.cloned())
    }
}

struct Exec {
    model: Arc<GraphModel>,
    source: PropSource,
}

impl GraphExecutor for Exec {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = compile(&self.model, query)?;
        walk(&mut self.source, &steps, query, who).await
    }
}

fn node(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}

fn props(pairs: &[(&str, Value)]) -> EdgeProps {
    pairs.iter().map(|(k, v)| (n(k), v.clone())).collect()
}

fn dec(text: &str) -> Value {
    Value::Decimal(text.parse().unwrap())
}

struct Bom {
    ids: [RecordId; 4],
    exec: Exec,
}

/// P1 -> P2 -> P3 -> P4 through `superseded_by`, with `qty` on each edge.
fn bom(quantities: [Value; 3]) -> Bom {
    let ids = [
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
    ];
    let mut docs = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let next = ids.get(i + 1).map_or(Value::Null, |n| Value::Id(*n));
        docs.push(record(
            "po",
            *id,
            &[
                ("title", format!("PO-{}", i + 1).into()),
                ("owner", "alice".into()),
                ("superseded_by", next),
            ],
        ));
    }
    let mut edges = BTreeMap::new();
    for (i, qty) in quantities.into_iter().enumerate() {
        edges.insert(
            (node("po", ids[i]), node("po", ids[i + 1])),
            props(&[("qty", qty), ("scrap", Value::Int(i as i64))]),
        );
    }
    Bom {
        ids,
        exec: Exec {
            model: graph_model(),
            source: PropSource {
                nodes: node_data(&docs),
                edges,
            },
        },
    }
}

fn qty_ref(var: &str, property: &str) -> Box<Expr> {
    Box::new(Expr::Field(FieldRef {
        alias: n(var),
        field: n(property),
    }))
}

fn qty_at_least(var: &str, property: &str, min: i64) -> Expr {
    Expr::Cmp(
        qty_ref(var, property),
        CmpOp::Ge,
        Box::new(Expr::Value(Value::Int(min))),
    )
}

fn explode(start: RecordId) -> spot_graph_query::GraphQueryBuilder {
    GraphQuery::builder()
        .from(vec![node("po", start)])
        .follow(vec![EdgeSelector::new("po.superseded_by")])
        .direction(Direction::Out)
        .max_depth(5)
}

fn auditor() -> spot_permissions::Principal {
    who("alice", &["Auditor"])
}

#[test]
fn a_three_level_bom_returns_per_level_and_rolled_up_quantities() {
    let mut b = bom([Value::Int(2), Value::Int(3), Value::Int(4)]);
    let query = explode(b.ids[0])
        .edge_var(n("e"))
        .rollup(n("qty"))
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
        .build()
        .unwrap();
    let result = block_on(run(&mut b.exec, &query, &auditor())).unwrap();

    // Hand-computed oracle: per-level quantity 2, 3, 4; rolled up 2, 2*3 = 6, 2*3*4 = 24.
    let oracle = [(1u16, 2i64, 2i64), (2, 3, 6), (3, 4, 24)];
    for (depth, per_level, rolled) in oracle {
        let found = result.nodes.iter().find(|x| x.depth == depth).unwrap();
        assert_eq!(found.rolled_up, Some(Value::Int(rolled)), "depth {depth}");
        assert_eq!(found.path.len(), usize::from(depth));
        let last = found.path.last().unwrap();
        assert_eq!(last.node, found.node);
        assert_eq!(last.edge_type, "po.superseded_by");
        assert_eq!(last.properties.get(&n("qty")), Some(&Value::Int(per_level)));
        assert_eq!(
            last.properties.get(&n("scrap")),
            Some(&Value::Int(i64::from(depth) - 1))
        );
    }
    // Each path starts at the first hop reached from the start node.
    let deepest = result.nodes.iter().find(|x| x.depth == 3).unwrap();
    let hop_nodes: Vec<_> = deepest.path.iter().map(|h| h.node.id).collect();
    assert_eq!(hop_nodes, vec![b.ids[1], b.ids[2], b.ids[3]]);
    // The start node has no path and no product.
    let start = result.nodes.iter().find(|x| x.depth == 0).unwrap();
    assert!(start.path.is_empty() && start.rolled_up.is_none());
}

#[test]
fn a_query_without_edge_properties_is_unchanged() {
    let mut b = bom([Value::Int(2), Value::Int(3), Value::Int(4)]);
    let query = explode(b.ids[0]).build().unwrap();
    let result = block_on(run(&mut b.exec, &query, &auditor())).unwrap();
    assert_eq!(result.nodes.len(), 4);
    assert!(
        result
            .nodes
            .iter()
            .all(|x| x.path.is_empty() && x.rolled_up.is_none())
    );
}

#[test]
fn the_edge_filter_reads_the_edge_variable() {
    // qty 5, 1, 7: only the first edge passes `e.qty >= 2`, so the walk stops there.
    let mut b = bom([Value::Int(5), Value::Int(1), Value::Int(7)]);
    let query = explode(b.ids[0])
        .edge_var(n("e"))
        .edge_filter(qty_at_least("e", "qty", 2))
        .build()
        .unwrap();
    let result = block_on(run(&mut b.exec, &query, &auditor())).unwrap();
    let ids: Vec<RecordId> = result.nodes.iter().map(|x| x.node.id).collect();
    assert_eq!(ids, vec![b.ids[0], b.ids[1]]);
}

#[test]
fn the_edge_filter_mixes_the_variable_with_the_holding_record() {
    let mut b = bom([Value::Int(5), Value::Int(5), Value::Int(5)]);
    // `e.qty >= 2 and title = 'PO-1'`: `title` is the record holding the link (the predecessor).
    let filter = Expr::And(vec![
        qty_at_least("e", "qty", 2),
        Expr::Cmp(
            qty_ref("po", "title"),
            CmpOp::Eq,
            Box::new(Expr::Value(Value::Text("PO-1".to_owned()))),
        ),
    ]);
    let query = explode(b.ids[0])
        .edge_var(n("e"))
        .edge_filter(filter)
        .build()
        .unwrap();
    let result = block_on(run(&mut b.exec, &query, &auditor())).unwrap();
    assert_eq!(result.nodes.len(), 2, "only the edge held by PO-1 passes");
}

#[test]
fn decimal_quantities_roll_up_exactly_with_the_summed_scale() {
    let mut b = bom([dec("1.5"), dec("2.25"), Value::Int(2)]);
    let query = explode(b.ids[0]).rollup(n("qty")).build().unwrap();
    let result = block_on(run(&mut b.exec, &query, &auditor())).unwrap();
    let rolled = |depth: u16| {
        result
            .nodes
            .iter()
            .find(|x| x.depth == depth)
            .unwrap()
            .rolled_up
            .clone()
    };
    assert_eq!(rolled(1), Some(dec("1.5")));
    // 1.5 * 2.25 = 3.375 (scale 1 + 2 = 3), times the integer 2 = 6.750.
    assert_eq!(rolled(2), Some(dec("3.375")));
    assert_eq!(rolled(3), Some(dec("6.750")));
    let Some(Value::Decimal(last)) = rolled(3) else {
        panic!("a decimal");
    };
    assert_eq!(last.scale(), 3);
}

#[test]
fn an_integer_overflow_is_an_error_not_a_wrap() {
    let mut b = bom([Value::Int(i64::MAX / 2 + 1), Value::Int(2), Value::Int(1)]);
    let query = explode(b.ids[0]).rollup(n("qty")).build().unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_overflow");
}

#[test]
fn a_decimal_that_does_not_fit_or_would_round_is_an_error() {
    // 79228162514264337593543950335 is the largest decimal; doubling it does not fit.
    let big = "79228162514264337593543950335";
    let mut b = bom([dec(big), Value::Int(2), Value::Int(1)]);
    let query = explode(b.ids[0]).rollup(n("qty")).build().unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_overflow");

    // Scales 15 and 14 sum to 29, past the 28 digits a decimal holds: refused, not rounded.
    let mut b = bom([
        dec("0.000000000000001"),
        dec("0.00000000000001"),
        Value::Int(1),
    ]);
    let query = explode(b.ids[0]).rollup(n("qty")).build().unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_overflow");
}

#[test]
fn a_rollup_over_text_or_null_is_an_error() {
    let mut b = bom([
        Value::Int(2),
        Value::Text("three".to_owned()),
        Value::Int(1),
    ]);
    let query = explode(b.ids[0]).rollup(n("qty")).build().unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_not_numeric");

    let mut b = bom([Value::Null, Value::Int(1), Value::Int(1)]);
    let query = explode(b.ids[0]).rollup(n("qty")).build().unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_not_numeric");
}

#[test]
fn a_property_the_edge_does_not_have_is_a_typed_runtime_error() {
    let mut b = bom([Value::Int(2), Value::Int(3), Value::Int(4)]);
    let query = explode(b.ids[0])
        .edge_var(n("e"))
        .edge_filter(qty_at_least("e", "nope", 1))
        .build()
        .unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert!(matches!(
        error,
        GraphQueryError::UnknownEdgeProperty { ref property, .. } if property == "nope"
    ));
    assert_eq!(error.code(), "graphq.unknown_edge_property");

    // The same through the rollup.
    let query = explode(b.ids[0]).rollup(n("nope")).build().unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.unknown_edge_property");
}

#[test]
fn a_value_type_outside_the_supported_set_is_refused() {
    let mut b = bom([Value::Int(2), Value::Int(3), Value::Int(4)]);
    let first = (node("po", b.ids[0]), node("po", b.ids[1]));
    b.exec
        .source
        .edges
        .insert(first, props(&[("qty", Value::Json("{}".to_owned()))]));
    let query = explode(b.ids[0])
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
        .build()
        .unwrap();
    let error = block_on(run(&mut b.exec, &query, &auditor())).unwrap_err();
    assert_eq!(error.code(), "graphq.unsupported_edge_value");
}

#[test]
fn an_edge_type_that_declares_its_properties_is_checked_at_compile_time() {
    let mut model = (*graph_model()).clone();
    model.edges.get_mut("po.superseded_by").unwrap().properties = vec![EdgeProperty {
        name: "qty".to_owned(),
        kind: PropertyKind::Scalar,
        required: true,
        options: None,
    }];
    let model = Arc::new(model);
    let id = RecordId::new_v7();

    let good = explode(id)
        .edge_var(n("e"))
        .edge_filter(qty_at_least("e", "qty", 1))
        .rollup(n("qty"))
        .build()
        .unwrap();
    let steps = compile(&model, &good).unwrap();
    assert_eq!(steps[0].properties, vec![n("qty")]);

    let bad = explode(id)
        .edge_var(n("e"))
        .edge_filter(qty_at_least("e", "scrap", 1))
        .build()
        .unwrap();
    let error = compile(&model, &bad).unwrap_err();
    assert_eq!(error.code(), "graphq.unknown_edge_property");
    assert!(error.to_string().contains("qty"), "{error}");

    let bad_rollup = explode(id).rollup(n("scrap")).build().unwrap();
    assert_eq!(
        compile(&model, &bad_rollup).unwrap_err().code(),
        "graphq.unknown_edge_property"
    );
}

/// P1 <- G1 <- I1: the receipt edge (`grn.po`, G1 holds the link) and the invoice edge
/// (`invoice.grn`), with a quantity on each. `Reader` cannot read receipts.
fn receipts() -> (Exec, RecordId, RecordId, RecordId) {
    let (p1, g1, i1) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    let docs = vec![
        record(
            "po",
            p1,
            &[("title", "PO-1".into()), ("owner", "alice".into())],
        ),
        record(
            "grn",
            g1,
            &[("owner", "alice".into()), ("po", Value::Id(p1))],
        ),
        record(
            "invoice",
            i1,
            &[("owner", "alice".into()), ("grn", Value::Id(g1))],
        ),
    ];
    let mut edges = BTreeMap::new();
    edges.insert(
        (node("grn", g1), node("po", p1)),
        props(&[("qty", Value::Int(10))]),
    );
    edges.insert(
        (node("invoice", i1), node("grn", g1)),
        props(&[("qty", Value::Int(10))]),
    );
    (
        Exec {
            model: graph_model(),
            source: PropSource {
                nodes: node_data(&docs),
                edges,
            },
        },
        p1,
        g1,
        i1,
    )
}

fn lineage(p1: RecordId) -> GraphQuery {
    GraphQuery::builder()
        .from(vec![node("po", p1)])
        .follow(vec![
            EdgeSelector::new("grn.po"),
            EdgeSelector::new("invoice.grn"),
        ])
        .direction(Direction::In)
        .max_depth(3)
        .edge_var(n("e"))
        .rollup(n("qty"))
        .include(Include {
            fields: vec![],
            edge_properties: true,
        })
        .build()
        .unwrap()
}

#[test]
fn a_hidden_end_hides_the_edge_and_its_properties() {
    let (mut exec, p1, g1, i1) = receipts();
    let query = lineage(p1);

    // The auditor reads everything and sees both edges with their properties.
    let seen = block_on(run(&mut exec, &query, &auditor())).unwrap();
    assert_eq!(seen.nodes.len(), 3);
    assert!(!seen.truncated_by_permission);
    let last = seen.nodes.iter().find(|x| x.node.id == i1).unwrap();
    assert_eq!(last.path.len(), 2);
    assert_eq!(last.path[0].node.id, g1);
    assert_eq!(
        last.path[0].properties.get(&n("qty")),
        Some(&Value::Int(10))
    );
    assert_eq!(last.rolled_up, Some(Value::Int(100)));

    // The reader cannot read the receipt: the edge to it, its properties, and everything behind
    // it disappear, and the result says only that something was removed.
    let reader = who("bob", &["Reader"]);
    let seen = block_on(run(&mut exec, &query, &reader)).unwrap();
    assert_eq!(seen.nodes.len(), 1);
    assert!(seen.truncated_by_permission);
    assert!(seen.edges.is_empty());
    assert!(seen.nodes.iter().all(|x| x.path.is_empty()));
}

#[test]
fn a_hidden_end_gives_the_same_answer_as_no_edge_properties_at_all() {
    // The edge filter never sees the properties of an edge to a record the caller cannot read:
    // even a filter that would accept it cannot make the hidden node appear or change the flag.
    let (mut exec, p1, _g1, _i1) = receipts();
    let reader = who("bob", &["Reader"]);
    let mut query = lineage(p1);
    query.edge_filter = Some(qty_at_least("e", "qty", 0));
    let with = block_on(run(&mut exec, &query, &reader)).unwrap();
    query.edge_filter = None;
    let without = block_on(run(&mut exec, &query, &reader)).unwrap();
    assert_eq!(with, without);
}
