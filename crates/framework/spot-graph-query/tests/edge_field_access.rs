//! Field-level permission on edge properties (WP-GRA-03.T4): a property of an edge type backed by
//! a relation DocType is shown only to a caller who may read that field, is absent otherwise, and
//! cannot be probed through a filter or a rollup.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_model::{EdgeProperty, GraphModel, PropertyKind};
use spot_graph_query::{
    Direction, EdgeFieldAccess, EdgeProps, EdgeSelector, GraphExecutor, GraphQuery,
    GraphQueryError, GraphResult, Include, Invocation, NodeData, NodeSource, Step, compile_with,
    walk,
};
use spot_storage_contract::{CmpOp, Expr, FieldRef, Name, NodeRef, RecordId, Value};
use support::*;

const EDGE: &str = "po.superseded_by";

/// Refuses the listed properties of every edge type.
struct Hide(Vec<&'static str>);

impl EdgeFieldAccess for Hide {
    fn edge_field_readable(&self, _edge_type: &str, property: &Name) -> bool {
        !self.0.contains(&property.as_str())
    }
}

/// Records plus the properties of each edge, keyed `(holder, target)`.
struct PropSource {
    nodes: BTreeMap<NodeRef, NodeData>,
    edges: BTreeMap<(NodeRef, NodeRef), EdgeProps>,
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
        _step: &Step,
    ) -> Result<Option<EdgeProps>, GraphQueryError> {
        Ok(self.edges.get(&(from.clone(), to.clone())).cloned())
    }
}

struct Exec<'a> {
    model: Arc<GraphModel>,
    source: PropSource,
    access: &'a dyn EdgeFieldAccess,
}

impl GraphExecutor for Exec<'_> {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = compile_with(&self.model, query, self.access)?;
        walk(&mut self.source, &steps, query, who).await
    }
}

fn node(id: RecordId) -> NodeRef {
    NodeRef {
        entity: n("po"),
        id,
    }
}

/// The model with `declared` as the edge type's properties (empty: a plain link edge).
fn model_with(declared: &[&str]) -> Arc<GraphModel> {
    let mut model = (*graph_model()).clone();
    model.edges.get_mut(EDGE).unwrap().properties = declared
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

/// P1 -> P2 -> P3 with `quantity` 2 then 3 and `unit_cost` 10 then 20 on the edges.
fn bom<'a>(declared: &[&str], access: &'a dyn EdgeFieldAccess) -> (Exec<'a>, RecordId) {
    let ids = [RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7()];
    let mut docs = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let next = ids.get(i + 1).map_or(Value::Null, |x| Value::Id(*x));
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
    for (i, (qty, cost)) in [(2i64, 10i64), (3, 20)].into_iter().enumerate() {
        edges.insert(
            (node(ids[i]), node(ids[i + 1])),
            [
                (n("quantity"), Value::Int(qty)),
                (n("unit_cost"), Value::Int(cost)),
            ]
            .into_iter()
            .collect(),
        );
    }
    let exec = Exec {
        model: model_with(declared),
        source: PropSource {
            nodes: node_data(&docs),
            edges,
        },
        access,
    };
    (exec, ids[0])
}

fn explode(start: RecordId) -> spot_graph_query::GraphQueryBuilder {
    GraphQuery::builder()
        .from(vec![node(start)])
        .follow(vec![EdgeSelector::new(EDGE)])
        .direction(Direction::Out)
        .max_depth(5)
}

fn with_properties() -> Include {
    Include {
        fields: vec![],
        edge_properties: true,
    }
}

fn at_least(property: &str) -> Expr {
    Expr::Cmp(
        Box::new(Expr::Field(FieldRef {
            alias: n("e"),
            field: n(property),
        })),
        CmpOp::Ge,
        Box::new(Expr::Value(Value::Int(0))),
    )
}

fn caller() -> spot_permissions::Principal {
    who("alice", &["Auditor"])
}

/// The nodes below the start node, shallowest first.
fn reached(result: &GraphResult) -> Vec<&spot_graph_query::GraphNode> {
    let mut found: Vec<_> = result.nodes.iter().filter(|x| x.depth >= 1).collect();
    found.sort_by_key(|x| x.depth);
    found
}

const DECLARED: [&str; 2] = ["quantity", "unit_cost"];

#[test]
fn a_readable_field_is_shown_and_a_hidden_one_is_absent_on_the_same_edge_type() {
    let hide = Hide(vec!["unit_cost"]);
    let (mut exec, start) = bom(&DECLARED, &hide);
    let query = explode(start).include(with_properties()).build().unwrap();
    let result = block_on(run(&mut exec, &query, &caller())).unwrap();
    assert_eq!(reached(&result).len(), 2);
    for found in reached(&result) {
        let hop = found.path.last().unwrap();
        assert!(hop.properties.contains_key(&n("quantity")));
        // Absent, not null and not masked.
        assert!(!hop.properties.contains_key(&n("unit_cost")), "{hop:?}");
        assert_eq!(hop.properties.len(), 1);
    }

    // The same data with nothing hidden shows both.
    let nothing = Hide(vec![]);
    let (mut open, start) = bom(&DECLARED, &nothing);
    let query = explode(start).include(with_properties()).build().unwrap();
    let result = block_on(run(&mut open, &query, &caller())).unwrap();
    assert_eq!(reached(&result)[0].path[0].properties.len(), 2);
}

#[test]
fn a_filter_on_a_hidden_property_is_indistinguishable_from_an_unknown_one() {
    let hide = Hide(vec!["unit_cost"]);
    let (exec, start) = bom(&DECLARED, &hide);
    let ask = |property: &str| {
        let query = explode(start)
            .edge_var(n("e"))
            .edge_filter(at_least(property))
            .build()
            .unwrap();
        compile_with(&exec.model, &query, &hide).unwrap_err()
    };
    let hidden = ask("unit_cost");
    let unknown = ask("no_such_property");
    assert_eq!(hidden.code(), "graphq.unknown_edge_property");
    assert_eq!(unknown.code(), "graphq.unknown_edge_property");
    // The text differs only in the name the caller itself typed.
    assert_eq!(
        hidden.to_string().replace("unit_cost", "X"),
        unknown.to_string().replace("no_such_property", "X")
    );
    // And the message never lists a hidden property as declared.
    assert!(hidden.to_string().contains("quantity"), "{hidden}");
    assert!(
        !hidden
            .to_string()
            .replace("`unit_cost`", "")
            .contains("unit_cost"),
        "{hidden}"
    );
}

#[test]
fn a_rollup_over_a_hidden_property_is_refused_not_summed() {
    let hide = Hide(vec!["unit_cost"]);
    let (mut exec, start) = bom(&DECLARED, &hide);
    for query in [
        explode(start).rollup(n("unit_cost")).build().unwrap(),
        explode(start).rollup_sum(n("unit_cost")).build().unwrap(),
    ] {
        let error = block_on(run(&mut exec, &query, &caller())).unwrap_err();
        assert_eq!(error.code(), "graphq.unknown_edge_property");
    }
    // A readable property still rolls up: 2 then 2 * 3.
    let query = explode(start).rollup(n("quantity")).build().unwrap();
    let result = block_on(run(&mut exec, &query, &caller())).unwrap();
    let rolled: Vec<_> = reached(&result)
        .iter()
        .map(|x| x.rolled_up.clone())
        .collect();
    assert_eq!(rolled, vec![Some(Value::Int(2)), Some(Value::Int(6))]);
}

#[test]
fn a_hidden_property_cannot_change_which_nodes_a_readable_filter_keeps() {
    let hide = Hide(vec!["unit_cost"]);
    let (mut exec, start) = bom(&DECLARED, &hide);
    let query = explode(start)
        .edge_var(n("e"))
        .edge_filter(at_least("quantity"))
        .include(with_properties())
        .build()
        .unwrap();
    let result = block_on(run(&mut exec, &query, &caller())).unwrap();
    assert_eq!(reached(&result).len(), 2);
}

#[test]
fn a_hidden_property_of_an_unsupported_type_does_not_make_the_query_fail() {
    // The value type check must not run on a value the caller cannot see.
    let hide = Hide(vec!["unit_cost"]);
    let (mut exec, start) = bom(&DECLARED, &hide);
    for props in exec.source.edges.values_mut() {
        props.insert(n("unit_cost"), Value::Json("{}".to_owned()));
    }
    let query = explode(start).include(with_properties()).build().unwrap();
    let result = block_on(run(&mut exec, &query, &caller())).unwrap();
    assert_eq!(reached(&result).len(), 2);
}

#[test]
fn the_default_policy_allows_every_property() {
    struct Plain;
    impl EdgeFieldAccess for Plain {
        fn edge_field_readable(&self, _edge_type: &str, _property: &Name) -> bool {
            true
        }
    }
    let (mut exec, start) = bom(&DECLARED, &Plain);
    let query = explode(start)
        .edge_var(n("e"))
        .edge_filter(at_least("unit_cost"))
        .rollup(n("unit_cost"))
        .include(with_properties())
        .build()
        .unwrap();
    let result = block_on(run(&mut exec, &query, &caller())).unwrap();
    assert_eq!(reached(&result)[0].path[0].properties.len(), 2);
    assert_eq!(reached(&result)[1].rolled_up, Some(Value::Int(200)));
    // `compile` itself is the allow-all policy.
    let steps = spot_graph_query::compile(&exec.model, &query).unwrap();
    assert_eq!(steps[0].properties.len(), 2);
    assert!(steps[0].hidden.is_empty());
}

#[test]
fn an_edge_type_without_declared_properties_ignores_the_policy() {
    // No relation DocType: nothing to protect, so even a policy that refuses everything changes
    // nothing, as in T2.
    let hide = Hide(vec!["quantity", "unit_cost"]);
    let (mut exec, start) = bom(&[], &hide);
    let query = explode(start)
        .edge_var(n("e"))
        .edge_filter(at_least("quantity"))
        .rollup(n("quantity"))
        .include(with_properties())
        .build()
        .unwrap();
    let result = block_on(run(&mut exec, &query, &caller())).unwrap();
    assert_eq!(reached(&result)[0].path[0].properties.len(), 2);
    assert_eq!(reached(&result)[1].rolled_up, Some(Value::Int(6)));
}
