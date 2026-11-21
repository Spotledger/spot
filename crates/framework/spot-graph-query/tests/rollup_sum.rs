//! The summed rollup (WP-GRA-03.T3): the sum over every simple path of the product along the
//! path, with its bound, its memoisation and its permission behaviour.
//!
//! The source is an explicit adjacency list, so a diamond (which one link field cannot express)
//! is easy to build; the edge type the query names only has to exist in the graph model.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_graph_model::GraphModel;
use spot_graph_query::{
    Direction, EdgeProps, EdgeSelector, GraphExecutor, GraphQuery, GraphQueryBuilder,
    GraphQueryError, GraphResult, Invocation, NodeData, NodeSource, Step, compile, walk,
};
use spot_storage_contract::{NodeRef, RecordId, Value};
use support::*;

type Key = (NodeRef, NodeRef);

struct Graph {
    nodes: BTreeMap<NodeRef, NodeData>,
    out: BTreeMap<NodeRef, Vec<NodeRef>>,
    edges: BTreeMap<Key, EdgeProps>,
}

impl NodeSource for Graph {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        Ok(self.nodes.get(node).cloned())
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        _step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        Ok(self.out.get(node).cloned().unwrap_or_default())
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

struct Exec {
    model: Arc<GraphModel>,
    source: Graph,
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

fn node(entity: &str) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id: RecordId::new_v7(),
    }
}

fn dec(text: &str) -> Value {
    Value::Decimal(text.parse().unwrap())
}

/// A graph under construction: add nodes, then edges with a `qty`.
struct Builder {
    docs: Vec<spot_storage_contract::DocumentData>,
    out: BTreeMap<NodeRef, Vec<NodeRef>>,
    edges: BTreeMap<Key, EdgeProps>,
}

impl Builder {
    fn new() -> Self {
        Self {
            docs: Vec::new(),
            out: BTreeMap::new(),
            edges: BTreeMap::new(),
        }
    }

    fn add(&mut self, entity: &str) -> NodeRef {
        let node = node(entity);
        let mut values: Vec<(&str, Value)> = vec![("owner", "alice".into())];
        if entity == "po" {
            values.push(("title", "PO".into()));
        }
        self.docs.push(record(entity, node.id, &values));
        node
    }

    fn edge(&mut self, from: &NodeRef, to: &NodeRef, qty: Value) -> &mut Self {
        self.out.entry(from.clone()).or_default().push(to.clone());
        let mut props = EdgeProps::new();
        props.insert(n("qty"), qty);
        self.edges.insert((from.clone(), to.clone()), props);
        self
    }

    fn finish(self) -> Exec {
        Exec {
            model: graph_model(),
            source: Graph {
                nodes: node_data(&self.docs),
                out: self.out,
                edges: self.edges,
            },
        }
    }
}

fn summed(start: &NodeRef, depth: u16) -> GraphQueryBuilder {
    GraphQuery::builder()
        .from(vec![start.clone()])
        .follow(vec![EdgeSelector::new("po.superseded_by")])
        .direction(Direction::Out)
        .max_depth(depth)
        .max_nodes(1_000)
        .rollup_sum(n("qty"))
}

fn auditor() -> spot_permissions::Principal {
    who("alice", &["Auditor"])
}

fn answer(exec: &mut Exec, query: &GraphQuery) -> Result<GraphResult, GraphQueryError> {
    block_on(run(exec, query, &auditor()))
}

fn at<'a>(result: &'a GraphResult, node: &NodeRef) -> &'a spot_graph_query::GraphNode {
    result.nodes.iter().find(|x| x.node == *node).unwrap()
}

#[test]
fn a_diamond_sums_the_two_path_products() {
    let mut b = Builder::new();
    let (p, a, c, t) = (b.add("po"), b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Int(2))
        .edge(&p, &c, Value::Int(3))
        .edge(&a, &t, Value::Int(5))
        .edge(&c, &t, Value::Int(7));
    let mut exec = b.finish();
    let result = answer(&mut exec, &summed(&p, 3).build().unwrap()).unwrap();

    // 2*5 + 3*7.
    let target = at(&result, &t);
    assert_eq!(target.rolled_up, Some(Value::Int(31)));
    assert_eq!(target.path_count, 2);
    assert!(!target.path_count_saturated);
    assert_eq!(at(&result, &a).rolled_up, Some(Value::Int(2)));
    assert_eq!(at(&result, &a).path_count, 1);
    // The start node has no path of its own.
    assert_eq!(at(&result, &p).rolled_up, None);
    assert_eq!(at(&result, &p).path_count, 0);
}

#[test]
fn a_part_used_directly_and_through_a_sub_assembly_adds_up() {
    // Assembly P holds 1 sub-assembly A; A holds 2 screws S; P also holds 1 screw S directly.
    let mut b = Builder::new();
    let (p, a, s) = (b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Int(1))
        .edge(&a, &s, Value::Int(2))
        .edge(&p, &s, Value::Int(1));
    let mut exec = b.finish();
    let result = answer(&mut exec, &summed(&p, 3).build().unwrap()).unwrap();
    assert_eq!(at(&result, &s).rolled_up, Some(Value::Int(3)));
    assert_eq!(at(&result, &s).path_count, 2);
    // The shortest path (the direct use) is the walk's own path; the sum is not that product.
    assert_eq!(at(&result, &s).depth, 1);
}

#[test]
fn the_product_rollup_is_unchanged_and_counts_no_paths() {
    let mut b = Builder::new();
    let (p, a, s) = (b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Int(1))
        .edge(&a, &s, Value::Int(2))
        .edge(&p, &s, Value::Int(4));
    let mut exec = b.finish();
    let query = GraphQuery::builder()
        .from(vec![p.clone()])
        .follow(vec![EdgeSelector::new("po.superseded_by")])
        .max_depth(3)
        .rollup(n("qty"))
        .build()
        .unwrap();
    let result = answer(&mut exec, &query).unwrap();
    assert_eq!(at(&result, &s).rolled_up, Some(Value::Int(4)));
    assert_eq!(at(&result, &s).path_count, 0);
    assert!(!result.max_paths_clamped);
}

#[test]
fn decimal_quantities_with_scrap_factors_stay_exact() {
    let mut b = Builder::new();
    let (p, a, t) = (b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, dec("1.5"))
        .edge(&a, &t, dec("0.25"))
        .edge(&p, &t, dec("2.00"));
    let mut exec = b.finish();
    let result = answer(&mut exec, &summed(&p, 3).build().unwrap()).unwrap();
    // 1.5 * 0.25 = 0.375; plus 2.00 = 2.375.
    assert_eq!(at(&result, &t).rolled_up, Some(dec("2.375")));
    assert_eq!(at(&result, &t).path_count, 2);

    // Mixed integer and decimal factors.
    let mut b = Builder::new();
    let (p, a, t) = (b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Int(3))
        .edge(&a, &t, dec("0.5"))
        .edge(&p, &t, Value::Int(1));
    let mut exec = b.finish();
    let result = answer(&mut exec, &summed(&p, 3).build().unwrap()).unwrap();
    assert_eq!(at(&result, &t).rolled_up, Some(dec("2.5")));
}

#[test]
fn a_sum_that_does_not_fit_is_an_overflow() {
    let mut b = Builder::new();
    let (p, a, c, t) = (b.add("po"), b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Int(i64::MAX))
        .edge(&p, &c, Value::Int(i64::MAX))
        .edge(&a, &t, Value::Int(1))
        .edge(&c, &t, Value::Int(1));
    let mut exec = b.finish();
    let error = answer(&mut exec, &summed(&p, 3).build().unwrap()).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_overflow");
}

#[test]
fn a_text_or_missing_factor_is_not_numeric() {
    let mut b = Builder::new();
    let (p, a) = (b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Text("two".into()));
    let mut exec = b.finish();
    let error = answer(&mut exec, &summed(&p, 2).build().unwrap()).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_not_numeric");

    let mut b = Builder::new();
    let (p, a) = (b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Null);
    let mut exec = b.finish();
    let error = answer(&mut exec, &summed(&p, 2).build().unwrap()).unwrap_err();
    assert_eq!(error.code(), "graphq.rollup_not_numeric");
}

/// `diamonds` stacked diamonds, every factor 1: `2^diamonds` paths to the last join.
fn ladder(diamonds: usize) -> (Exec, NodeRef, NodeRef) {
    let mut b = Builder::new();
    let start = b.add("po");
    let mut join = start.clone();
    for _ in 0..diamonds {
        let (left, right, next) = (b.add("po"), b.add("po"), b.add("po"));
        b.edge(&join, &left, Value::Int(1))
            .edge(&join, &right, Value::Int(1))
            .edge(&left, &next, Value::Int(1))
            .edge(&right, &next, Value::Int(1));
        join = next;
    }
    (b.finish(), start, join)
}

#[test]
fn a_ladder_of_thirty_diamonds_is_counted_without_being_enumerated() {
    let (mut exec, start, last) = ladder(30);
    // Depth 60, the default bound of 10,000: enumerating 2^30 paths would never finish.
    let result = answer(&mut exec, &summed(&start, 60).build().unwrap()).unwrap();
    let target = at(&result, &last);
    assert_eq!(target.rolled_up, Some(Value::Int(1 << 30)));
    assert_eq!(target.path_count, 1 << 30);
    assert!(!target.path_count_saturated);
}

#[test]
fn a_path_count_beyond_u32_saturates_and_says_so() {
    let (mut exec, start, last) = ladder(32);
    let result = answer(&mut exec, &summed(&start, 64).build().unwrap()).unwrap();
    let target = at(&result, &last);
    assert_eq!(target.rolled_up, Some(Value::Int(1 << 32)));
    assert_eq!(target.path_count, u32::MAX);
    assert!(target.path_count_saturated);
}

#[test]
fn the_bound_refuses_instead_of_returning_a_partial_sum() {
    let (mut exec, start, _last) = ladder(30);
    let query = summed(&start, 60).max_paths(50).build().unwrap();
    assert_eq!(query.max_paths, 50);
    let error = answer(&mut exec, &query).unwrap_err();
    assert_eq!(error.code(), "graphq.too_many_paths");
    assert!(matches!(
        error,
        GraphQueryError::TooManyPaths { max_paths: 50 }
    ));
}

#[test]
fn a_larger_bound_is_clamped_and_the_result_says_so() {
    let (mut exec, start, last) = ladder(2);
    let query = summed(&start, 6).max_paths(5_000_000).build().unwrap();
    assert_eq!(query.max_paths, 1_000_000);
    assert!(query.max_paths_clamped);
    let result = answer(&mut exec, &query).unwrap();
    assert!(result.max_paths_clamped);
    assert_eq!(at(&result, &last).path_count, 4);

    let default = summed(&start, 6).build().unwrap();
    assert_eq!(default.max_paths, 10_000);
    assert!(!default.max_paths_clamped);
}

#[test]
fn a_cycle_counts_simple_paths_only() {
    // P -> A -> B -> T, with B -> A closing a cycle and T -> P leading back to the start.
    let mut b = Builder::new();
    let (p, a, c, t) = (b.add("po"), b.add("po"), b.add("po"), b.add("po"));
    b.edge(&p, &a, Value::Int(2))
        .edge(&a, &c, Value::Int(3))
        .edge(&c, &a, Value::Int(5))
        .edge(&c, &t, Value::Int(7))
        .edge(&t, &p, Value::Int(11));
    let mut exec = b.finish();
    let result = answer(&mut exec, &summed(&p, 10).build().unwrap()).unwrap();
    assert_eq!(at(&result, &t).rolled_up, Some(Value::Int(2 * 3 * 7)));
    assert_eq!(at(&result, &t).path_count, 1);
    assert_eq!(at(&result, &a).rolled_up, Some(Value::Int(2)));
    assert_eq!(at(&result, &c).rolled_up, Some(Value::Int(6)));
    // The start node is never entered again, so it has no path.
    assert_eq!(at(&result, &p).path_count, 0);

    // The cyclic case is bounded too.
    let error = answer(&mut exec, &summed(&p, 10).max_paths(2).build().unwrap()).unwrap_err();
    assert_eq!(error.code(), "graphq.too_many_paths");
}

#[test]
fn a_hidden_middle_node_contributes_nothing_and_is_never_revealed() {
    // Two paths P -> A -> T and P -> G -> T; the reader cannot read receipts (G).
    let mut b = Builder::new();
    let (p, a, g, t) = (b.add("po"), b.add("po"), b.add("grn"), b.add("po"));
    b.edge(&p, &a, Value::Int(2))
        .edge(&p, &g, Value::Int(1_000))
        .edge(&a, &t, Value::Int(5))
        .edge(&g, &t, Value::Int(1_000));
    let mut exec = b.finish();
    let query = summed(&p, 3).build().unwrap();

    let all = answer(&mut exec, &query).unwrap();
    assert_eq!(at(&all, &t).rolled_up, Some(Value::Int(10 + 1_000_000)));
    assert_eq!(at(&all, &t).path_count, 2);

    let reader = who("bob", &["Reader"]);
    let seen = block_on(run(&mut exec, &query, &reader)).unwrap();
    // The sum equals the visible path only, and nothing of the hidden quantity is in it.
    assert_eq!(at(&seen, &t).rolled_up, Some(Value::Int(10)));
    assert_eq!(at(&seen, &t).path_count, 1);
    assert!(seen.truncated_by_permission);
    assert!(seen.nodes.iter().all(|x| x.node != g));
}
