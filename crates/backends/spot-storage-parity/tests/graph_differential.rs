//! Seeded random graphs, run on the fake and on PostgreSQL: the answers must be identical, the
//! results or the error kind (SEM-GRA, WP-STO-07 "traversal results match reference answers").
//!
//! 60 seeds by default, 200 with `SPOT_PARITY_SEEDS=200`. Without `SPOT_TEST_PG_URL` the tests
//! print `SKIPPED`; under `SPOT_REQUIRE_PG=1` a missing server is a failure.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_l7_invariants::schedule::Rng;
use spot_storage_contract::{
    Catalog, ChildRow, Direction, DocumentData, EdgeSpec, ErrorKind, Name, NodeRef, RecordId,
    StorageResult, Traversal, TraversalResult, Value,
};
use spot_storage_parity::catalog::{GRAPH_FIELDS, graph_catalog};
use spot_storage_parity::{BackendFactory, CaseNeeds, Driver, FakeFactory, PgFactory};

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn seeds() -> u64 {
    std::env::var("SPOT_PARITY_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(60)
}

/// A generated graph: the documents and the text of the case-DSL lines that rebuild it.
struct Graph {
    docs: Vec<DocumentData>,
    ids: Vec<RecordId>,
    dsl: Vec<String>,
}

fn id_of(i: u64) -> RecordId {
    let mut bytes = [0_u8; 16];
    bytes[..6].copy_from_slice(&(1_700_000_000_000_u64 + i).to_be_bytes()[2..]);
    bytes[6] = 0x70;
    bytes[8] = 0x80;
    bytes[14] = (i >> 8) as u8;
    bytes[15] = (i & 0xff) as u8;
    RecordId::from_bytes(bytes).unwrap()
}

fn generate(rng: &mut Rng, count: usize) -> Graph {
    let ids: Vec<RecordId> = (0..count as u64).map(|i| id_of(i + 1)).collect();
    let mut docs = Vec::new();
    let mut dsl = Vec::new();
    for i in 0..count {
        let mut fields: BTreeMap<Name, Value> =
            GRAPH_FIELDS.iter().map(|f| (n(f), Value::Null)).collect();
        fields.insert(n("code"), Value::Text(format!("n{i}")));
        let mut line = format!("n{i} code=\"t:n{i}\"");
        for field in ["next", "ref"] {
            // 0 to 3 outgoing links over the two item edge types, duplicates and cycles included.
            if rng.below(3) != 0 {
                let target = rng.below(count as u64) as usize;
                fields.insert(n(field), Value::Id(ids[target]));
                line.push_str(&format!(" {field}=\"id:n{target}\""));
            }
        }
        let mut children = BTreeMap::new();
        let lines = rng.below(3);
        if lines > 0 {
            let rows = (0..lines)
                .map(|k| ChildRow {
                    id: id_of(100_000 + (i as u64) * 4 + k),
                    fields: BTreeMap::from([
                        (n("label"), Value::Text(format!("l{k}"))),
                        (n("amount"), Value::Null),
                    ]),
                })
                .collect();
            children.insert(n("lines"), rows);
        } else {
            children.insert(n("lines"), Vec::new());
        }
        docs.push(DocumentData {
            entity: n("item"),
            id: ids[i],
            fields,
            children,
        });
        dsl.push(line);
    }
    Graph { docs, ids, dsl }
}

fn traversal(rng: &mut Rng, graph: &Graph) -> (Traversal, String) {
    let count = graph.ids.len();
    let mut start = Vec::new();
    for _ in 0..(1 + rng.below(3)) {
        start.push(NodeRef {
            entity: n("item"),
            id: graph.ids[rng.below(count as u64) as usize],
        });
    }
    if rng.below(4) == 0 {
        // A start node that does not exist.
        start.push(NodeRef {
            entity: n("item"),
            id: id_of(999_999),
        });
    }
    let mut edges = Vec::new();
    for field in ["next", "ref"] {
        if rng.below(3) != 0 {
            edges.push(EdgeSpec {
                entity: n("item"),
                field: n(field),
            });
        }
    }
    if rng.below(3) == 0 {
        edges.push(EdgeSpec {
            entity: n("item_line"),
            field: n("parent"),
        });
    }
    if edges.is_empty() {
        edges.push(EdgeSpec {
            entity: n("item"),
            field: n("next"),
        });
    }
    let direction = match rng.below(3) {
        0 => Direction::Outgoing,
        1 => Direction::Incoming,
        _ => Direction::Both,
    };
    let max_depth = 1 + rng.below(10) as u16;
    let max_nodes = if rng.below(3) == 0 {
        1 + rng.below(count as u64 + 5) as u32
    } else {
        10_000
    };
    let text = format!(
        "start={:?} edges={:?} direction={direction:?} max_depth={max_depth} max_nodes={max_nodes}",
        start.iter().map(|s| s.id.to_string()).collect::<Vec<_>>(),
        edges
            .iter()
            .map(|e| format!("{}.{}", e.entity, e.field))
            .collect::<Vec<_>>()
    );
    (
        Traversal {
            start,
            edges,
            direction,
            max_depth,
            max_nodes,
        },
        text,
    )
}

async fn answer<F: BackendFactory>(
    factory: &F,
    catalog: &Arc<Catalog>,
    docs: &[DocumentData],
    traversal: &Traversal,
) -> StorageResult<Option<Result<TraversalResult, ErrorKind>>> {
    let Some(driver) = factory
        .fresh(catalog.clone(), &CaseNeeds::default())
        .await?
    else {
        return Ok(None);
    };
    // One transaction: the links of a generated graph point forward as well as back.
    driver.insert(docs, Some(10_000)).await?;
    Ok(Some(driver.traverse(traversal).await.map_err(|e| e.kind)))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn random_graphs_give_the_same_traversal_on_the_fake_and_on_postgres() {
    let catalog = Arc::new(graph_catalog("en").unwrap());
    for seed in 0..seeds() {
        let mut rng = Rng::new(0x6772_6170_6800_0000 ^ seed);
        let count = 3 + rng.below(78) as usize;
        let graph = generate(&mut rng, count);
        let (t, text) = traversal(&mut rng, &graph);
        let fake = answer(&FakeFactory, &catalog, &graph.docs, &t)
            .await
            .unwrap()
            .unwrap();
        let Some(pg) = answer(&PgFactory, &catalog, &graph.docs, &t).await.unwrap() else {
            println!("SKIPPED: set SPOT_TEST_PG_URL to compare with PostgreSQL");
            return;
        };
        if fake != pg {
            panic!(
                "seed {seed}: the fake and PostgreSQL differ\nfake: {fake:?}\npg:   {pg:?}\n{text}\nsetup lines (paste into cases/graph.toml):\n{}",
                graph.dsl.join("\n")
            );
        }
    }
}

/// A line of 5,000 nodes walked to depth 64 returns 65 nodes on both backends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_line_of_five_thousand_nodes_is_cut_at_depth_64() {
    let catalog = Arc::new(graph_catalog("en").unwrap());
    let count = 5_000_usize;
    let ids: Vec<RecordId> = (0..count as u64).map(|i| id_of(i + 1)).collect();
    let docs: Vec<DocumentData> = (0..count)
        .map(|i| {
            let mut fields: BTreeMap<Name, Value> =
                GRAPH_FIELDS.iter().map(|f| (n(f), Value::Null)).collect();
            fields.insert(n("code"), Value::Text(format!("n{i}")));
            if i + 1 < count {
                fields.insert(n("next"), Value::Id(ids[i + 1]));
            }
            DocumentData {
                entity: n("item"),
                id: ids[i],
                fields,
                children: BTreeMap::from([(n("lines"), Vec::new())]),
            }
        })
        .collect();
    let t = Traversal {
        start: vec![NodeRef {
            entity: n("item"),
            id: ids[0],
        }],
        edges: vec![EdgeSpec {
            entity: n("item"),
            field: n("next"),
        }],
        direction: Direction::Outgoing,
        max_depth: 64,
        max_nodes: 100,
    };
    let fake = answer(&FakeFactory, &catalog, &docs, &t)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        fake.as_ref().map(|r| r.nodes.len()).ok(),
        Some(65),
        "{fake:?}"
    );
    if let Some(pg) = answer(&PgFactory, &catalog, &docs, &t).await.unwrap() {
        assert_eq!(fake, pg);
    } else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to compare with PostgreSQL");
    }
}
