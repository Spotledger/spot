//! A criterion-free micro benchmark, run in the nightly job only (`#[ignore]`): a six-hop trace
//! over a graph of 100,000 edges must stay under the crate's budget constant.
//!
//! The graph is a seven-way tree of 100,001 items (100,000 `item.parent_item` edges); the trace
//! starts at the root and follows the components, so a six-hop trace reaches 100,000 nodes. The
//! executor answers neighbours from a precomputed adjacency, so the measured time is the lineage
//! layer's own cost (the graph walk, the permission check and the assembly), not the storage
//! backend's.
//!
//! `Instant` reads the monotonic clock, not the wall clock; the library itself never reads a
//! clock.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use spot_graph_model::GraphModel;
use spot_graph_query::{
    Direction, GraphExecutor, GraphQuery, GraphQueryError, GraphResult, Invocation, NodeData,
    NodeSource, Step, compile, walk,
};
use spot_lineage::{ChainConfig, LineageQuery, TRACE_BUDGET, compile as compile_lineage, run};
use spot_storage_contract::{Direction as StorageDirection, NodeRef, RecordId};
use support::*;

/// The graph: 100,001 items and 100,000 parent links.
const ITEMS: usize = 100_001;
/// The branching of the six-hop tree (four-way).
const BRANCH: usize = 4;
/// The nodes of the six-hop tree: `1 + 4 + ... + 4^6`, so a six-hop trace from the root reaches
/// every one of them.
const TREE_NODES: usize = 5_461;
/// The traced depth.
const HOPS: u16 = 6;
/// The node budget, the graph query API's ceiling.
const BUDGET: u32 = 100_000;

struct Graph {
    nodes: BTreeMap<NodeRef, NodeData>,
    children: BTreeMap<NodeRef, Vec<NodeRef>>,
    parent_of: BTreeMap<NodeRef, NodeRef>,
    root: NodeRef,
}

fn build_graph() -> Graph {
    let ids: Vec<RecordId> = (0..ITEMS).map(|_| RecordId::new_v7()).collect();
    let node = |index: usize| NodeRef {
        entity: n("item"),
        id: ids[index],
    };
    let mut nodes = BTreeMap::new();
    let mut children: BTreeMap<NodeRef, Vec<NodeRef>> = BTreeMap::new();
    let mut parent_of = BTreeMap::new();
    for index in 0..ITEMS {
        nodes.insert(
            node(index),
            NodeData {
                values: BTreeMap::new(),
                state: None,
                created_by: None,
            },
        );
    }
    {
        let mut link = |child: NodeRef, parent: NodeRef| {
            children
                .entry(parent.clone())
                .or_default()
                .push(child.clone());
            parent_of.insert(child, parent);
        };
        // A four-way tree of 5,461 nodes: a six-hop trace from the root reaches every node.
        for index in 1..TREE_NODES {
            link(node(index), node((index - 1) / BRANCH));
        }
        // A chain of the remaining nodes, hung off a depth-six leaf so it lies beyond the
        // six-hop trace: the graph still carries 100,000 edges.
        link(node(TREE_NODES), node(TREE_NODES - 1));
        for index in TREE_NODES + 1..ITEMS {
            link(node(index), node(index - 1));
        }
    }
    Graph {
        nodes,
        children,
        parent_of,
        root: node(0),
    }
}

struct FastSource<'a> {
    nodes: &'a BTreeMap<NodeRef, NodeData>,
    children: &'a BTreeMap<NodeRef, Vec<NodeRef>>,
    parent_of: &'a BTreeMap<NodeRef, NodeRef>,
}

impl NodeSource for FastSource<'_> {
    async fn load(&mut self, node: &NodeRef) -> Result<Option<NodeData>, GraphQueryError> {
        Ok(self.nodes.get(node).cloned())
    }

    async fn step(
        &mut self,
        node: &NodeRef,
        step: &Step,
        _max_nodes: u32,
    ) -> Result<Vec<NodeRef>, GraphQueryError> {
        let mut out: Vec<NodeRef> = Vec::new();
        if matches!(
            step.direction,
            StorageDirection::Outgoing | StorageDirection::Both
        ) && let Some(parent) = self.parent_of.get(node)
        {
            out.push(parent.clone());
        }
        if matches!(
            step.direction,
            StorageDirection::Incoming | StorageDirection::Both
        ) && let Some(children) = self.children.get(node)
        {
            out.extend(children.iter().cloned());
        }
        out.retain(|candidate| candidate != node);
        Ok(out)
    }
}

struct FastExecutor {
    model: Arc<GraphModel>,
    graph: Graph,
}

impl GraphExecutor for FastExecutor {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        who: &Invocation<'_>,
    ) -> Result<GraphResult, GraphQueryError> {
        let steps = compile(&self.model, query)?;
        let mut source = FastSource {
            nodes: &self.graph.nodes,
            children: &self.graph.children,
            parent_of: &self.graph.parent_of,
        };
        walk(&mut source, &steps, query, who).await
    }
}

#[test]
#[ignore = "nightly micro benchmark; run with --ignored --nocapture"]
fn a_six_hop_trace_over_a_hundred_thousand_edges() {
    let graph = build_graph();
    assert_eq!(graph.parent_of.len(), 100_000);
    let config =
        ChainConfig::parse("[[chain]]\ndoctype = \"item\"\nedges = [\"item.parent_item\"]\n")
            .unwrap();
    let plan = compile_lineage(
        &LineageQuery::Document {
            doctype: n("item"),
            name: graph.root.id,
            direction: Direction::In,
            max_depth: HOPS,
        },
        &config,
    )
    .unwrap()
    .with_max_nodes(BUDGET)
    .unwrap();
    let mut executor = FastExecutor {
        model: graph_model(),
        graph,
    };
    let engine = engine();
    let principal = who("auditor", &["Auditor"]);
    let invocation = Invocation {
        principal: &principal,
        engine: engine.as_ref(),
    };

    let started = Instant::now();
    let result = block_on(run(&mut executor, &plan, &invocation)).unwrap();
    let elapsed = started.elapsed();

    assert_eq!(result.nodes.len(), TREE_NODES);
    assert!(elapsed < TRACE_BUDGET, "trace took {elapsed:?}");
    println!(
        "six-hop trace over 100,000 edges: {} nodes, {} edges in {} ms (budget {} ms)",
        result.nodes.len(),
        result.edges.len(),
        elapsed.as_millis(),
        TRACE_BUDGET.as_millis(),
    );
}
