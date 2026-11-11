//! For random graphs, random starts, depths and directions, under the chain's real permission
//! engine: the walk's visible nodes and depths equal a reference BFS over the permitted induced
//! subgraph, and the flag is set exactly when a visible node reaches a node the caller may not read.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::{BTreeMap, BTreeSet};

use proptest::prelude::*;
use spot_graph_query::{Direction, EdgeSelector, GraphExecutor, GraphQuery};
use spot_storage_contract::{NodeRef, RecordId, Value};
use support::*;

const EDGES: [(&str, &str, &str); 4] = [
    ("po.superseded_by", "po", "superseded_by"),
    ("grn.po", "grn", "po"),
    ("invoice.grn", "invoice", "grn"),
    ("payment.invoice", "payment", "invoice"),
];

#[derive(Clone, Debug)]
struct Spec {
    /// Index into DOCTYPES per node.
    kinds: Vec<usize>,
    /// Raw link target per node: an index into the nodes, taken only when its DocType fits.
    links: Vec<usize>,
    /// Whether the creator is alice, per node.
    alice: Vec<bool>,
    /// Start node indices.
    starts: Vec<usize>,
    /// Direction per followed edge type: 0 out, 1 in, 2 both.
    directions: Vec<u8>,
    depth: u16,
}

fn spec() -> impl Strategy<Value = Spec> {
    (1usize..=8).prop_flat_map(|n| {
        (
            proptest::collection::vec(0usize..4, n),
            proptest::collection::vec(0usize..n, n),
            proptest::collection::vec(any::<bool>(), n),
            proptest::collection::vec(0usize..n, 1..=2),
            proptest::collection::vec(0u8..3, 4),
            1u16..=4,
        )
            .prop_map(|(kinds, links, alice, starts, directions, depth)| Spec {
                kinds,
                links,
                alice,
                starts,
                directions,
                depth,
            })
    })
}

/// The link field of each DocType and the DocType it links to.
fn link_of(doctype: &str) -> (&'static str, &'static str) {
    match doctype {
        "po" => ("superseded_by", "po"),
        "grn" => ("po", "po"),
        "invoice" => ("grn", "grn"),
        _ => ("invoice", "invoice"),
    }
}

/// Builds the records, their ids and the node data.
fn build(spec: &Spec) -> (Vec<NodeRef>, BTreeMap<NodeRef, spot_graph_query::NodeData>) {
    let ids: Vec<RecordId> = (0..spec.kinds.len()).map(|_| RecordId::new_v7()).collect();
    let entities: Vec<&str> = spec.kinds.iter().map(|k| DOCTYPES[*k]).collect();
    let mut nodes: Vec<NodeRef> = Vec::new();
    let mut data: BTreeMap<NodeRef, spot_graph_query::NodeData> = BTreeMap::new();
    for i in 0..ids.len() {
        let node = NodeRef {
            entity: n(entities[i]),
            id: ids[i],
        };
        let owner = if spec.alice[i] { "alice" } else { "bob" };
        let mut values = BTreeMap::new();
        values.insert(n("owner"), Value::Text(owner.into()));
        let (field, target_doctype) = link_of(entities[i]);
        let target = spec.links[i];
        if entities[target] == target_doctype {
            values.insert(n(field), Value::Id(ids[target]));
        }
        nodes.push(node.clone());
        data.insert(
            node,
            spot_graph_query::NodeData {
                values,
                state: None,
                created_by: Some(owner.into()),
            },
        );
    }
    (nodes, data)
}

fn permitted(node: &NodeRef, data: &BTreeMap<NodeRef, spot_graph_query::NodeData>) -> bool {
    match node.entity.as_str() {
        "po" | "invoice" => true,
        "payment" => data
            .get(node)
            .and_then(|d| d.created_by.as_deref())
            .is_some_and(|who| who == "alice"),
        _ => false,
    }
}

/// The reference: BFS over the permitted induced subgraph, and whether a visible node reaches a
/// node the caller may not read.
fn reference(
    data: &BTreeMap<NodeRef, spot_graph_query::NodeData>,
    steps: &[spot_graph_query::Step],
    starts: &[NodeRef],
    depth_limit: u16,
) -> (BTreeMap<NodeRef, u16>, bool) {
    let mut depth: BTreeMap<NodeRef, u16> = BTreeMap::new();
    let mut truncated = false;
    let mut level: Vec<NodeRef> = Vec::new();
    let mut starts: Vec<NodeRef> = starts.to_vec();
    starts.sort();
    starts.dedup();
    for s in starts {
        if data.contains_key(&s) && permitted(&s, data) {
            depth.insert(s.clone(), 0);
            level.push(s);
        }
    }
    for d in 1..=depth_limit {
        if level.is_empty() {
            break;
        }
        let mut next: BTreeSet<NodeRef> = BTreeSet::new();
        for p in &level {
            for step in steps {
                for q in neighbours(data, p, step) {
                    if depth.contains_key(&q) {
                        continue;
                    }
                    if !permitted(&q, data) {
                        truncated = true;
                        continue;
                    }
                    next.insert(q);
                }
            }
        }
        for q in &next {
            depth.insert(q.clone(), d);
        }
        level = next.into_iter().collect();
    }
    (depth, truncated)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn the_walk_matches_the_reference(spec in spec()) {
        let (nodes, data) = build(&spec);
        let starts: Vec<NodeRef> = spec.starts.iter().map(|i| nodes[*i].clone()).collect();
        let directions = [Direction::Out, Direction::In, Direction::Both];
        let selectors: Vec<EdgeSelector> = EDGES
            .iter()
            .zip(&spec.directions)
            .map(|((edge, _, _), d)| EdgeSelector::directed(*edge, directions[usize::from(*d)]))
            .collect();
        let query = GraphQuery::builder()
            .from(starts.clone())
            .follow(selectors)
            .max_depth(spec.depth)
            .max_nodes(100_000)
            .build()
            .unwrap();
        let steps = spot_graph_query::compile(&graph_model(), &query).unwrap();
        let engine = engine();
        let principal = who("alice", &["Reader", "Owner"]);
        let invocation = invocation(&principal, &engine);
        let mut executor = MemExecutor { model: graph_model(), nodes: data.clone() };
        let result = block_on(executor.execute(&query, &invocation)).unwrap();

        let (expected, expected_truncated) = reference(&data, &steps, &starts, spec.depth);
        prop_assert_eq!(depths(&result), expected);
        prop_assert_eq!(result.truncated_by_permission, expected_truncated);
        // Every returned node is permitted, and every edge joins two returned nodes.
        for node in &result.nodes {
            prop_assert!(permitted(&node.node, &data));
            if let Some((pred, _)) = &node.via {
                prop_assert!(depths(&result).get(pred) == Some(&(node.depth - 1)));
            }
        }
        let returned: BTreeSet<NodeRef> = result.nodes.iter().map(|x| x.node.clone()).collect();
        for edge in &result.edges {
            prop_assert!(returned.contains(&edge.from) && returned.contains(&edge.to));
        }
    }
}
