//! The oracle: for random acyclic document graphs under the fixture's permission rules, the real
//! walk through the storage fake and the naive in-memory oracle agree exactly — the same visible
//! nodes and depths, the same chosen predecessors, and the same redaction marker. A node the
//! caller may not read is neither returned nor counted.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::collections::BTreeSet;

use proptest::prelude::*;
use spot_graph_query::{Direction, Invocation};
use spot_lineage::{LineageQuery, LineageResult, Quantity, compile, oracle, run};
use spot_permissions::Principal;
use spot_storage_contract::{NodeRef, RecordId, Value};
use support::*;

const KINDS: [&str; 4] = ["po", "grn", "invoice", "payment"];

#[derive(Clone, Debug)]
struct Spec {
    /// Index into [`KINDS`] per node.
    kinds: Vec<usize>,
    /// A raw link target per node: an index below its own, taken only when the DocTypes fit.
    links: Vec<usize>,
    /// Whether the creator is alice, per node.
    alice: Vec<bool>,
    /// The start node index.
    start: usize,
    /// The direction: 0 out, 1 in, 2 both.
    direction: u8,
    depth: u16,
}

fn spec() -> impl Strategy<Value = Spec> {
    (1usize..=8).prop_flat_map(|n| {
        (
            proptest::collection::vec(0usize..4, n),
            proptest::collection::vec(0usize..n, n),
            proptest::collection::vec(any::<bool>(), n),
            0usize..n,
            0u8..3,
            1u16..=4,
        )
            .prop_map(|(kinds, links, alice, start, direction, depth)| Spec {
                kinds,
                links,
                alice,
                start,
                direction,
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

/// Builds the records of a random acyclic graph. A link only ever points at a lower index, so
/// the graph is acyclic by construction.
fn build(spec: &Spec) -> Vec<spot_storage_contract::DocumentData> {
    let ids: Vec<RecordId> = (0..spec.kinds.len()).map(|_| RecordId::new_v7()).collect();
    let entities: Vec<&str> = spec.kinds.iter().map(|kind| KINDS[*kind]).collect();
    let mut docs = Vec::new();
    for (i, entity) in entities.iter().enumerate() {
        let owner = if spec.alice[i] { "alice" } else { "bob" };
        let mut values: Vec<(&str, Value)> = vec![("owner", owner.into())];
        let (field, target_doctype) = link_of(entity);
        let target = spec.links[i];
        if target < i && entities[target] == target_doctype {
            values.push((field, Value::Id(ids[target])));
        }
        docs.push(record(entity, ids[i], &values));
    }
    docs
}

fn direction(spec: &Spec) -> Direction {
    [Direction::Out, Direction::In, Direction::Both][usize::from(spec.direction)]
}

/// The node the caller may not read, for the fixture's `Reader`/`Owner` principal: every receipt
/// (no read grant) and every payment bob created (the owner rule).
fn unreadable(docs: &[spot_storage_contract::DocumentData]) -> BTreeSet<NodeRef> {
    docs.iter()
        .filter(|doc| {
            doc.entity == n("grn")
                || (doc.entity == n("payment")
                    && doc.fields.get(&n("owner")) == Some(&Value::Text("bob".into())))
        })
        .map(|doc| NodeRef {
            entity: doc.entity.clone(),
            id: doc.id,
        })
        .collect()
}

fn compare(
    docs: &[spot_storage_contract::DocumentData],
    query: &LineageQuery,
    principal: &Principal,
) -> (LineageResult, LineageResult) {
    let (backend, _) = block_on(store(docs));
    let plan = compile(query, &config()).unwrap();
    let mut executor = block_on(executor(&backend));
    let engine = engine();
    let invocation = Invocation {
        principal,
        engine: engine.as_ref(),
    };
    let real = block_on(run(&mut executor, &plan, &invocation)).unwrap();
    let expected = oracle::run(&plan, &graph_model(), &node_data(docs), &invocation).unwrap();
    (real, expected)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn the_walk_matches_the_oracle_on_random_acyclic_graphs(spec in spec()) {
        let docs = build(&spec);
        let entity = KINDS[spec.kinds[spec.start]];
        let query = LineageQuery::Document {
            doctype: n(entity),
            name: docs[spec.start].id,
            direction: direction(&spec),
            max_depth: spec.depth,
        };
        let principal = who("alice", &["Reader", "Owner"]);
        let (real, expected) = compare(&docs, &query, &principal);
        prop_assert_eq!(&real, &expected);

        // The redaction, directly: no node the caller may not read is returned or counted.
        let hidden = unreadable(&docs);
        for node in &real.nodes {
            prop_assert!(!hidden.contains(&node.node));
        }
        let counted: u32 = real.levels.iter().map(|level| level.nodes).sum();
        prop_assert_eq!(counted, real.nodes.len() as u32);
    }

    #[test]
    fn the_walk_matches_the_oracle_for_an_auditor(spec in spec()) {
        let docs = build(&spec);
        let entity = KINDS[spec.kinds[spec.start]];
        let query = LineageQuery::Document {
            doctype: n(entity),
            name: docs[spec.start].id,
            direction: direction(&spec),
            max_depth: spec.depth,
        };
        let (real, expected) = compare(&docs, &query, &who("auditor", &["Auditor"]));
        prop_assert_eq!(&real, &expected);
    }
}

#[test]
fn the_oracle_matches_a_bill_of_materials_explosion() {
    let (a, b, c, d) = (
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
    );
    let decimal = |text: &str| Value::from(spot_money::parse_decimal(text).unwrap());
    let docs = vec![
        record("item", a, &[("title", "A".into())]),
        record(
            "item",
            b,
            &[
                ("title", "B".into()),
                ("parent_item", Value::Id(a)),
                ("qty", decimal("2.5")),
            ],
        ),
        record(
            "item",
            c,
            &[
                ("title", "C".into()),
                ("parent_item", Value::Id(a)),
                ("qty", decimal("3")),
            ],
        ),
        record(
            "item",
            d,
            &[
                ("title", "D".into()),
                ("parent_item", Value::Id(b)),
                ("qty", decimal("0.5")),
            ],
        ),
    ];
    let query = LineageQuery::BomExplosion {
        item: at("item", a),
        qty_basis: Quantity::from_int(4),
        max_depth: 5,
    };
    let (real, expected) = compare(&docs, &query, &who("auditor", &["Auditor"]));
    assert_eq!(real, expected);
    // 4 x 2.5 = 10, and 10 x 0.5 = 5.
    let quantity = |id| {
        real.nodes
            .iter()
            .find(|node| node.node == at("item", id))
            .and_then(|node| node.quantity)
    };
    assert_eq!(quantity(b), Some(Quantity::parse("10").unwrap()));
    assert_eq!(quantity(d), Some(Quantity::parse("5").unwrap()));
}

#[test]
fn the_oracle_matches_where_used() {
    let (a, b, c) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    let docs = vec![
        record("item", a, &[("title", "A".into())]),
        record(
            "item",
            b,
            &[("title", "B".into()), ("parent_item", Value::Id(a))],
        ),
        record(
            "item",
            c,
            &[("title", "C".into()), ("parent_item", Value::Id(b))],
        ),
    ];
    let query = LineageQuery::WhereUsed {
        item: at("item", c),
    };
    let (real, expected) = compare(&docs, &query, &who("auditor", &["Auditor"]));
    assert_eq!(real, expected);
}
