//! The named lineage questions over the storage fake: the document chain, a batch trace, a bill
//! of materials exploded with exact quantities, where-used, the permission redaction, the depth
//! and node truncation reasons, and the configuration-driven shape.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use spot_graph_query::{Direction, Invocation};
use spot_lineage::{
    ChainConfig, LineageError, LineageQuery, LineageResult, Quantity, Truncation, compile, run,
};
use spot_permissions::model::Principal;
use spot_storage_contract::{RecordId, Value};
use support::*;

struct Chain {
    p1: RecordId,
    g1: RecordId,
    i1: RecordId,
    y1: RecordId,
    y2: RecordId,
    docs: Vec<spot_storage_contract::DocumentData>,
}

fn chain() -> Chain {
    let (p1, g1, i1, y1, y2) = (
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
    );
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
        record(
            "payment",
            y1,
            &[
                ("owner", "alice".into()),
                ("invoice", Value::Id(i1)),
                ("kind", "bank".into()),
            ],
        ),
        record(
            "payment",
            y2,
            &[
                ("owner", "bob".into()),
                ("invoice", Value::Id(i1)),
                ("kind", "cash".into()),
            ],
        ),
    ];
    Chain {
        p1,
        g1,
        i1,
        y1,
        y2,
        docs,
    }
}

/// The bill of materials: item `a` has components `b` (qty 2) and `c` (qty 3), and `b` has
/// component `d` (qty 5).
struct Bom {
    a: RecordId,
    b: RecordId,
    c: RecordId,
    d: RecordId,
    docs: Vec<spot_storage_contract::DocumentData>,
}

fn bom() -> Bom {
    let (a, b, c, d) = (
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
        RecordId::new_v7(),
    );
    let docs = vec![
        record("item", a, &[("title", "A".into())]),
        record(
            "item",
            b,
            &[
                ("title", "B".into()),
                ("parent_item", Value::Id(a)),
                ("qty", decimal("2")),
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
                ("qty", decimal("5")),
            ],
        ),
    ];
    Bom { a, b, c, d, docs }
}

/// A decimal value from its text, through the money layer's exact parser.
fn decimal(text: &str) -> Value {
    Value::from(spot_money::parse_decimal(text).expect("a decimal"))
}

/// Compiles `query` and runs it through the storage executor for `principal`.
fn lineage(
    backend: &spot_storage_fake::FakeBackend,
    query: &LineageQuery,
    principal: &Principal,
) -> Result<LineageResult, LineageError> {
    lineage_with(backend, query, principal, config(), |plan| plan)
}

/// As [`lineage`], with a custom configuration and a plan adjustment.
fn lineage_with(
    backend: &spot_storage_fake::FakeBackend,
    query: &LineageQuery,
    principal: &Principal,
    config: ChainConfig,
    adjust: impl FnOnce(spot_lineage::Plan) -> spot_lineage::Plan,
) -> Result<LineageResult, LineageError> {
    let plan = adjust(compile(query, &config)?);
    let mut executor = block_on(executor(backend));
    let engine = engine();
    let invocation = Invocation {
        principal,
        engine: engine.as_ref(),
    };
    block_on(run(&mut executor, &plan, &invocation))
}

fn depths(
    result: &LineageResult,
) -> std::collections::BTreeMap<spot_storage_contract::NodeRef, u16> {
    result
        .nodes
        .iter()
        .map(|node| (node.node.clone(), node.depth))
        .collect()
}

fn document(doctype: &str, id: RecordId, direction: Direction, depth: u16) -> LineageQuery {
    LineageQuery::Document {
        doctype: n(doctype),
        name: id,
        direction,
        max_depth: depth,
    }
}

#[test]
fn the_document_chain_forward_from_the_purchase_order() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let result = lineage(
        &backend,
        &document("po", c.p1, Direction::In, 5),
        &who("auditor", &["Auditor"]),
    )
    .unwrap();
    let got = depths(&result);
    assert_eq!(got.get(&at("po", c.p1)), Some(&0));
    assert_eq!(got.get(&at("grn", c.g1)), Some(&1));
    assert_eq!(got.get(&at("invoice", c.i1)), Some(&2));
    assert_eq!(got.get(&at("payment", c.y1)), Some(&3));
    assert_eq!(got.get(&at("payment", c.y2)), Some(&3));
    assert_eq!(result.truncated, None);
    assert_eq!(result.edges.len(), result.nodes.len() - 1);
    assert_eq!(
        result.levels,
        vec![
            spot_lineage::LevelTotal {
                depth: 0,
                nodes: 1,
                quantity: None
            },
            spot_lineage::LevelTotal {
                depth: 1,
                nodes: 1,
                quantity: None
            },
            spot_lineage::LevelTotal {
                depth: 2,
                nodes: 1,
                quantity: None
            },
            spot_lineage::LevelTotal {
                depth: 3,
                nodes: 2,
                quantity: None
            },
        ]
    );
    let grn = result
        .nodes
        .iter()
        .find(|node| node.node == at("grn", c.g1))
        .unwrap();
    assert_eq!(grn.via, Some((at("po", c.p1), "grn.po".to_owned())));
}

#[test]
fn the_document_chain_back_from_the_payment() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let result = lineage(
        &backend,
        &document("payment", c.y1, Direction::Out, 5),
        &who("auditor", &["Auditor"]),
    )
    .unwrap();
    let got = depths(&result);
    assert_eq!(got.get(&at("payment", c.y1)), Some(&0));
    assert_eq!(got.get(&at("invoice", c.i1)), Some(&1));
    assert_eq!(got.get(&at("grn", c.g1)), Some(&2));
    assert_eq!(got.get(&at("po", c.p1)), Some(&3));
    assert_eq!(got.len(), 4);
}

#[test]
fn a_batch_traces_forward_and_back() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let auditor = who("auditor", &["Auditor"]);
    let forward = lineage(
        &backend,
        &LineageQuery::BatchTrace {
            batch: at("grn", c.g1),
            direction: Direction::In,
        },
        &auditor,
    )
    .unwrap();
    let got = depths(&forward);
    assert_eq!(got.get(&at("grn", c.g1)), Some(&0));
    assert_eq!(got.get(&at("invoice", c.i1)), Some(&1));
    assert_eq!(got.get(&at("payment", c.y1)), Some(&2));
    assert_eq!(got.get(&at("payment", c.y2)), Some(&2));
    let back = lineage(
        &backend,
        &LineageQuery::BatchTrace {
            batch: at("grn", c.g1),
            direction: Direction::Out,
        },
        &auditor,
    )
    .unwrap();
    let got = depths(&back);
    assert_eq!(got.get(&at("po", c.p1)), Some(&1));
    assert_eq!(got.len(), 2);
}

#[test]
fn a_bill_of_materials_explodes_with_exact_quantities() {
    let b = bom();
    let (backend, _) = block_on(store(&b.docs));
    let result = lineage(
        &backend,
        &LineageQuery::BomExplosion {
            item: at("item", b.a),
            qty_basis: Quantity::from_int(10),
            max_depth: 5,
        },
        &who("auditor", &["Auditor"]),
    )
    .unwrap();
    let quantity = |id| {
        result
            .nodes
            .iter()
            .find(|node| node.node == at("item", id))
            .and_then(|node| node.quantity)
    };
    assert_eq!(quantity(b.a), Some(Quantity::from_int(10)));
    assert_eq!(quantity(b.b), Some(Quantity::from_int(20)));
    assert_eq!(quantity(b.c), Some(Quantity::from_int(30)));
    assert_eq!(quantity(b.d), Some(Quantity::from_int(100)));
    assert_eq!(
        result.levels,
        vec![
            spot_lineage::LevelTotal {
                depth: 0,
                nodes: 1,
                quantity: Some(Quantity::from_int(10))
            },
            spot_lineage::LevelTotal {
                depth: 1,
                nodes: 2,
                quantity: Some(Quantity::from_int(50))
            },
            spot_lineage::LevelTotal {
                depth: 2,
                nodes: 1,
                quantity: Some(Quantity::from_int(100))
            },
        ]
    );
    assert_eq!(result.truncated, None);
}

#[test]
fn a_fractional_bill_of_materials_stays_exact() {
    let b = bom();
    let mut docs = b.docs.clone();
    for doc in &mut docs {
        if doc.id == b.b {
            doc.fields.insert(n("qty"), decimal("0.5"));
        }
    }
    let (backend, _) = block_on(store(&docs));
    let result = lineage(
        &backend,
        &LineageQuery::BomExplosion {
            item: at("item", b.a),
            qty_basis: Quantity::parse("1.25").unwrap(),
            max_depth: 5,
        },
        &who("auditor", &["Auditor"]),
    )
    .unwrap();
    let quantity = |id| {
        result
            .nodes
            .iter()
            .find(|node| node.node == at("item", id))
            .and_then(|node| node.quantity)
    };
    // 1.25 x 0.5 = 0.625, and d is 0.625 x 5 = 3.125.
    assert_eq!(quantity(b.b), Some(Quantity::parse("0.625").unwrap()));
    assert_eq!(quantity(b.d), Some(Quantity::parse("3.125").unwrap()));
}

#[test]
fn where_used_walks_up_to_the_top_item() {
    let b = bom();
    let (backend, _) = block_on(store(&b.docs));
    let result = lineage(
        &backend,
        &LineageQuery::WhereUsed {
            item: at("item", b.d),
        },
        &who("auditor", &["Auditor"]),
    )
    .unwrap();
    let got = depths(&result);
    assert_eq!(got.get(&at("item", b.d)), Some(&0));
    assert_eq!(got.get(&at("item", b.b)), Some(&1));
    assert_eq!(got.get(&at("item", b.a)), Some(&2));
    assert!(result.nodes.iter().all(|node| node.quantity.is_none()));
}

#[test]
fn a_hidden_node_is_neither_returned_nor_counted() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    // The reader may not read receipts, so the receipt is hidden and the invoice and payments
    // behind it are unreachable.
    let result = lineage(
        &backend,
        &document("po", c.p1, Direction::In, 5),
        &who("clerk", &["Reader"]),
    )
    .unwrap();
    assert_eq!(depths(&result).len(), 1);
    assert_eq!(result.truncated, Some(Truncation::Permission));
    assert_eq!(result.levels.len(), 1);
    assert_eq!(result.levels[0].nodes, 1);
    assert!(result.edges.is_empty());
    assert!(result.nodes.iter().all(|node| node.node == at("po", c.p1)));
}

#[test]
fn a_depth_limit_is_reported() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let result = lineage(
        &backend,
        &document("po", c.p1, Direction::In, 1),
        &who("auditor", &["Auditor"]),
    )
    .unwrap();
    assert_eq!(depths(&result).len(), 2);
    assert_eq!(result.truncated, Some(Truncation::Depth { max_depth: 1 }));
}

#[test]
fn a_node_limit_is_reported() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let result = lineage_with(
        &backend,
        &document("po", c.p1, Direction::In, 5),
        &who("auditor", &["Auditor"]),
        config(),
        |plan| plan.with_max_nodes(1).unwrap(),
    )
    .unwrap();
    assert_eq!(depths(&result).len(), 1);
    assert_eq!(result.truncated, Some(Truncation::Nodes { limit: 1 }));
}

#[test]
fn the_chain_configuration_drives_the_walk() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    // A configuration that follows only the receipt link reaches only the receipt.
    let config = ChainConfig::parse("[[chain]]\ndoctype = \"po\"\nedges = [\"grn.po\"]\n").unwrap();
    let result = lineage_with(
        &backend,
        &document("po", c.p1, Direction::In, 5),
        &who("auditor", &["Auditor"]),
        config,
        |plan| plan,
    )
    .unwrap();
    let got = depths(&result);
    assert_eq!(got.len(), 2);
    assert_eq!(got.get(&at("grn", c.g1)), Some(&1));
    assert!(!got.contains_key(&at("invoice", c.i1)));
}

#[test]
fn an_unknown_chain_is_reported() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let error = lineage(
        &backend,
        &document("widget", c.p1, Direction::In, 5),
        &who("auditor", &["Auditor"]),
    )
    .unwrap_err();
    assert_eq!(error.code(), "lineage.unknown_chain");
}

#[test]
fn an_unknown_bill_of_materials_is_reported() {
    let c = chain();
    let (backend, _) = block_on(store(&c.docs));
    let error = lineage(
        &backend,
        &LineageQuery::WhereUsed {
            item: at("po", c.p1),
        },
        &who("auditor", &["Auditor"]),
    )
    .unwrap_err();
    assert_eq!(error.code(), "lineage.unknown_bom");
}

#[test]
fn a_depth_out_of_range_is_reported() {
    let error = compile(
        &document("po", RecordId::new_v7(), Direction::In, 0),
        &config(),
    )
    .unwrap_err();
    assert_eq!(error.code(), "lineage.depth_out_of_range");
}

#[test]
fn the_built_in_configuration_carries_the_fixture_chains() {
    let config = config();
    assert_eq!(config.chained_doctypes().count(), 4);
    assert_eq!(config.bom_doctypes().count(), 1);
    assert_eq!(
        config.chain(&n("po")).unwrap().edges,
        vec![
            "grn.po".to_owned(),
            "invoice.grn".to_owned(),
            "payment.invoice".to_owned()
        ]
    );
    let bom = config.bom(&n("item")).unwrap();
    assert_eq!(bom.edge, "item.parent_item");
    assert_eq!(bom.qty_field, n("qty"));
}

#[test]
fn a_bad_configuration_reports_its_path() {
    let error = ChainConfig::parse("[[chain]]\nedges = [\"grn.po\"]\n").unwrap_err();
    assert_eq!(error.code(), "lineage.config");
    assert!(error.to_string().contains("chain[0].doctype"), "{error}");

    let error = ChainConfig::parse("[[chain]]\ndoctype = \"po\"\nedges = []\n").unwrap_err();
    assert!(error.to_string().contains("chain[0].edges"), "{error}");

    let error = ChainConfig::parse("this is not toml").unwrap_err();
    assert_eq!(error.code(), "lineage.config");
}
