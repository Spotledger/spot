//! Traversals on random graphs against a reference breadth-first search written in this file,
//! plus the `max_nodes` boundary, the lattice that defeats per-path enumeration and a fan-out
//! that must fail fast.
//!
//! The reference follows the contract in the plainest way (levels, a visited set, the minimum
//! `(predecessor, edge type)` per new node) over the same edge lists that are bulk-loaded into
//! PostgreSQL; the graphs have cycles, self-links, missing and dynamic link targets, and are
//! walked in all three directions.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use spot_storage_contract::contract_tests::catalog;
use spot_storage_contract::{
    Catalog, Direction, EdgeSpec, ErrorKind, GraphTraversal, Isolation, Name, NodeRef, Reached,
    RecordId, StorageBackend, Transaction, Traversal, TraversalResult,
};
use tokio_postgres::Client;
use uuid::Uuid;

fn n(s: &str) -> Name {
    Name::new(s).unwrap()
}

fn cat() -> Arc<Catalog> {
    Arc::new(catalog().unwrap())
}

fn id() -> RecordId {
    RecordId::new_v7()
}

fn uuid(id: RecordId) -> Uuid {
    Uuid::from_bytes(*id.as_bytes())
}

fn node(entity: &str, id: RecordId) -> NodeRef {
    NodeRef {
        entity: n(entity),
        id,
    }
}

fn spec(entity: &str, field: &str) -> EdgeSpec {
    EdgeSpec {
        entity: n(entity),
        field: n(field),
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }
}

// --- the model ------------------------------------------------------------------------------------

/// A dynamic link: the entity name, as text, and the id it names.
type Dynamic = Option<(String, RecordId)>;

/// A random graph in the contract catalog: parties linking to parties, invoices linking to a party
/// and (dynamically) to anything, and invoice lines hanging off invoices.
struct Model {
    parties: Vec<(RecordId, Option<usize>)>,
    /// `(id, party, dynamic target)`.
    invoices: Vec<(RecordId, usize, Dynamic)>,
    lines: Vec<(RecordId, usize)>,
}

fn random_model(rng: &mut Rng, max_parties: usize) -> Model {
    let parties_n = 1 + rng.below(max_parties);
    let ids: Vec<RecordId> = (0..parties_n).map(|_| id()).collect();
    let parties: Vec<_> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let parent = match rng.below(10) {
                0..=1 => None,
                2 => Some(i), // a self-link
                _ => Some(rng.below(parties_n)),
            };
            (*id, parent)
        })
        .collect();
    let invoices_n = rng.below(1 + parties_n);
    let invoice_ids: Vec<RecordId> = (0..invoices_n).map(|_| id()).collect();
    let lines_n = if invoices_n == 0 {
        0
    } else {
        rng.below(1 + 2 * invoices_n)
    };
    let lines: Vec<_> = (0..lines_n)
        .map(|_| (id(), rng.below(invoices_n)))
        .collect();
    let invoices = invoice_ids
        .iter()
        .map(|id| {
            let dynamic = match rng.below(8) {
                0 | 1 => Some(("party".to_owned(), ids[rng.below(parties_n)])),
                2 | 3 => Some(("invoice".to_owned(), invoice_ids[rng.below(invoices_n)])),
                4 if !lines.is_empty() => {
                    Some(("invoice_line".to_owned(), lines[rng.below(lines.len())].0))
                }
                5 => Some(("party".to_owned(), RecordId::new_v7())), // names nothing
                6 => Some(("not_an_entity".to_owned(), ids[0])),
                _ => None,
            };
            (*id, rng.below(parties_n), dynamic)
        })
        .collect();
    Model {
        parties,
        invoices,
        lines,
    }
}

impl Model {
    fn nodes(&self) -> BTreeSet<NodeRef> {
        let mut all = BTreeSet::new();
        all.extend(self.parties.iter().map(|(i, _)| node("party", *i)));
        all.extend(self.invoices.iter().map(|(i, _, _)| node("invoice", *i)));
        all.extend(self.lines.iter().map(|(i, _)| node("invoice_line", *i)));
        all
    }

    /// Every link record as `(edge type, from, to)`; `to` need not exist.
    fn links(&self) -> Vec<(EdgeSpec, NodeRef, NodeRef)> {
        let mut out = Vec::new();
        for (i, parent) in &self.parties {
            if let Some(p) = parent {
                out.push((
                    spec("party", "parent_party"),
                    node("party", *i),
                    node("party", self.parties[*p].0),
                ));
            }
        }
        for (i, party, dynamic) in &self.invoices {
            out.push((
                spec("invoice", "party"),
                node("invoice", *i),
                node("party", self.parties[*party].0),
            ));
            // A dynamic link follows only when it names an entity of the catalog.
            if let Some((entity, to)) = dynamic
                && ["party", "invoice", "invoice_line"].contains(&entity.as_str())
            {
                out.push((
                    spec("invoice", "ref_id"),
                    node("invoice", *i),
                    node(entity, *to),
                ));
            }
        }
        for (i, invoice) in &self.lines {
            out.push((
                spec("invoice_line", "parent"),
                node("invoice_line", *i),
                node("invoice", self.invoices[*invoice].0),
            ));
        }
        out
    }

    /// Bulk-loads the model with plain SQL (fast; the document store is tested elsewhere).
    async fn load(&self, client: &mut Client) {
        let t = client.transaction().await.unwrap();
        t.batch_execute("TRUNCATE invoice_line, invoice, party")
            .await
            .unwrap();
        let ids: Vec<Uuid> = self.parties.iter().map(|(i, _)| uuid(*i)).collect();
        let parents: Vec<Option<Uuid>> = self
            .parties
            .iter()
            .map(|(_, p)| p.map(|p| ids[p]))
            .collect();
        let names: Vec<String> = (0..ids.len()).map(|i| format!("p{i}")).collect();
        t.execute(
            "INSERT INTO party (id, version, name, credit_limit, parent_party) \
             SELECT i, 1, nm, 0, p FROM unnest($1::uuid[], $2::text[], $3::uuid[]) AS x(i, nm, p)",
            &[&ids, &names, &parents],
        )
        .await
        .unwrap();
        let inv_ids: Vec<Uuid> = self.invoices.iter().map(|(i, _, _)| uuid(*i)).collect();
        let inv_party: Vec<Uuid> = self.invoices.iter().map(|(_, p, _)| ids[*p]).collect();
        let numbers: Vec<String> = (0..inv_ids.len()).map(|i| format!("INV-{i}")).collect();
        let ref_entity: Vec<Option<String>> = self
            .invoices
            .iter()
            .map(|(_, _, d)| d.as_ref().map(|d| d.0.clone()))
            .collect();
        let ref_id: Vec<Option<Uuid>> = self
            .invoices
            .iter()
            .map(|(_, _, d)| d.as_ref().map(|d| uuid(d.1)))
            .collect();
        t.execute(
            "INSERT INTO invoice (id, version, number, party, total, ref_entity, ref_id) \
             SELECT i, 1, nm, p, 0, e, r FROM unnest($1::uuid[], $2::text[], $3::uuid[], $4::text[], $5::uuid[]) AS x(i, nm, p, e, r)",
            &[&inv_ids, &numbers, &inv_party, &ref_entity, &ref_id],
        )
        .await
        .unwrap();
        let line_ids: Vec<Uuid> = self.lines.iter().map(|(i, _)| uuid(*i)).collect();
        let line_parent: Vec<Uuid> = self.lines.iter().map(|(_, p)| inv_ids[*p]).collect();
        t.execute(
            "INSERT INTO invoice_line (id, parent, parent_field, idx, item, qty, amount) \
             SELECT i, p, 'lines', row_number() OVER (PARTITION BY p ORDER BY i) - 1, 'x', 1, 1 \
             FROM unnest($1::uuid[], $2::uuid[]) AS x(i, p)",
            &[&line_ids, &line_parent],
        )
        .await
        .unwrap();
        t.commit().await.unwrap();
    }
}

/// The reference: levels, a visited set, the minimum `(predecessor, edge)` per new node.
fn reference(model: &Model, t: &Traversal) -> Result<TraversalResult, ErrorKind> {
    let existing = model.nodes();
    let links = model.links();
    let mut reached: BTreeMap<NodeRef, Reached> = BTreeMap::new();
    for s in &t.start {
        if existing.contains(s) {
            reached.entry(s.clone()).or_insert(Reached {
                node: s.clone(),
                depth: 0,
                via: None,
            });
        }
    }
    let max = t.max_nodes as usize;
    if reached.len() > max {
        return Err(ErrorKind::TraversalTooLarge);
    }
    let (out, inc) = match t.direction {
        Direction::Outgoing => (true, false),
        Direction::Incoming => (false, true),
        Direction::Both => (true, true),
    };
    let mut frontier: BTreeSet<NodeRef> = reached.keys().cloned().collect();
    for depth in 1..=t.max_depth {
        let mut next: BTreeMap<NodeRef, (NodeRef, EdgeSpec)> = BTreeMap::new();
        for (edge, from, to) in &links {
            if !t.edges.contains(edge) {
                continue;
            }
            let mut found = Vec::new();
            if out && frontier.contains(from) && existing.contains(to) {
                found.push((from.clone(), to.clone()));
            }
            if inc && frontier.contains(to) {
                found.push((to.clone(), from.clone()));
            }
            for (pred, new) in found {
                if reached.contains_key(&new) {
                    continue;
                }
                let via = (pred, edge.clone());
                next.entry(new)
                    .and_modify(|b| *b = (*b).clone().min(via.clone()))
                    .or_insert(via);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next.keys().cloned().collect();
        for (new, via) in next {
            reached.insert(
                new.clone(),
                Reached {
                    node: new,
                    depth,
                    via: Some(via),
                },
            );
        }
        if reached.len() > max {
            return Err(ErrorKind::TraversalTooLarge);
        }
    }
    let mut nodes: Vec<Reached> = reached.into_values().collect();
    nodes.sort_by(|a, b| (a.depth, &a.node).cmp(&(b.depth, &b.node)));
    Ok(TraversalResult { nodes })
}

fn random_traversal(rng: &mut Rng, model: &Model, max_nodes: u32) -> Traversal {
    let all: Vec<NodeRef> = model.nodes().into_iter().collect();
    let mut start: Vec<NodeRef> = (0..1 + rng.below(3))
        .map(|_| all[rng.below(all.len())].clone())
        .collect();
    if rng.chance(10) {
        start.push(node("party", RecordId::new_v7())); // missing starts are left out
    }
    let edge_types = [
        spec("party", "parent_party"),
        spec("invoice", "party"),
        spec("invoice", "ref_id"),
        spec("invoice_line", "parent"),
    ];
    let mut edges: Vec<EdgeSpec> = edge_types
        .iter()
        .filter(|_| rng.chance(60))
        .cloned()
        .collect();
    if edges.is_empty() {
        edges.push(edge_types[rng.below(4)].clone());
    }
    if rng.chance(10) {
        edges.push(edges[0].clone()); // repeats change nothing
    }
    let direction = [Direction::Outgoing, Direction::Incoming, Direction::Both][rng.below(3)];
    let max_depth = if rng.chance(15) {
        64
    } else {
        1 + rng.below(8) as u16
    };
    Traversal {
        start,
        edges,
        direction,
        max_depth,
        max_nodes,
    }
}

async fn traverse(
    b: &PgBackend,
    t: &Traversal,
) -> Result<TraversalResult, spot_storage_contract::StorageError> {
    let mut tx = b
        .begin(cat(), common::options("graph", Isolation::ReadCommitted))
        .await?;
    let result = tx.traverse(t).await;
    tx.rollback().await.unwrap();
    result
}

async fn graph_env() -> Option<(TestDb, PgBackend)> {
    let db = TestDb::create().await?;
    let backend = PgBackend::connect(db.config()).await.unwrap();
    backend.prepare(&cat()).await.unwrap();
    Some((db, backend))
}

#[tokio::test]
async fn a_hundred_random_graphs_match_the_reference() {
    let Some((db, backend)) = graph_env().await else {
        return;
    };
    let mut client = db.raw_client().await;
    let mut rng = Rng(0x6A09_E667_F3BC_C908);
    let (mut walks, mut too_large, mut deep) = (0, 0, 0);
    for round in 0..100 {
        // Mostly small graphs, a few of the full 2,000 parties.
        let model = random_model(&mut rng, if round % 10 == 9 { 2_000 } else { 120 });
        model.load(&mut client).await;
        for _ in 0..4 {
            let probe = random_traversal(&mut rng, &model, u32::MAX);
            let full = reference(&model, &probe).unwrap();
            let size = full.nodes.len() as u32;
            // At the size, one below it (too large), and well above.
            for max_nodes in [size.max(1), size.saturating_sub(1).max(1), size + 50] {
                let t = Traversal {
                    max_nodes,
                    ..probe.clone()
                };
                let want = reference(&model, &t);
                let got = traverse(&backend, &t).await;
                match (&want, &got) {
                    (Ok(w), Ok(g)) => {
                        assert_eq!(w, g, "round {round}: {t:#?}");
                        deep += usize::from(w.nodes.iter().any(|r| r.depth >= 3));
                    }
                    (Err(w), Err(g)) => {
                        assert_eq!(*w, g.kind, "round {round}");
                        too_large += 1;
                    }
                    _ => panic!("round {round}: reference {want:?}, postgres {got:?}\n{t:#?}"),
                }
                walks += 1;
            }
        }
    }
    // The generator must reach beyond one hop and hit the node limit.
    assert_eq!(walks, 1_200);
    assert!(deep >= 100, "only {deep} walks went three levels deep");
    assert!(
        too_large >= 100,
        "only {too_large} walks hit the node limit"
    );
}

/// A chain of `len` parties linked by `parent_party`, id order = chain order; returns the ids.
async fn chain(db: &TestDb, len: usize) -> Vec<RecordId> {
    let ids: Vec<RecordId> = (0..len).map(|_| id()).collect();
    let model = Model {
        parties: ids
            .iter()
            .enumerate()
            .map(|(i, id)| (*id, (i + 1 < len).then_some(i + 1)))
            .collect(),
        invoices: vec![],
        lines: vec![],
    };
    model.load(&mut db.raw_client().await).await;
    ids
}

#[tokio::test]
async fn max_nodes_is_exact_at_the_boundary() {
    let Some((db, backend)) = graph_env().await else {
        return;
    };
    let ids = chain(&db, 10).await;
    let t = |max_nodes, max_depth| Traversal {
        start: vec![node("party", ids[0])],
        edges: vec![spec("party", "parent_party")],
        direction: Direction::Outgoing,
        max_depth,
        max_nodes,
    };
    // Ten nodes exist: exactly ten is fine, nine is too many (nothing is truncated).
    assert_eq!(
        traverse(&backend, &t(10, 64)).await.unwrap().nodes.len(),
        10
    );
    assert_eq!(
        traverse(&backend, &t(9, 64)).await.unwrap_err().kind,
        ErrorKind::TraversalTooLarge
    );
    // The depth limit applies first: nine hops reach all ten, five hops reach six.
    assert_eq!(traverse(&backend, &t(6, 5)).await.unwrap().nodes.len(), 6);
    assert_eq!(
        traverse(&backend, &t(5, 5)).await.unwrap_err().kind,
        ErrorKind::TraversalTooLarge
    );
    // More start nodes than the limit is too large before any edge is followed.
    let many = Traversal {
        start: ids.iter().map(|i| node("party", *i)).collect(),
        ..t(9, 1)
    };
    assert_eq!(
        traverse(&backend, &many).await.unwrap_err().kind,
        ErrorKind::TraversalTooLarge
    );
    // Missing starts do not count.
    let some = Traversal {
        start: vec![node("party", id()), node("party", ids[9])],
        ..t(1, 3)
    };
    assert_eq!(traverse(&backend, &some).await.unwrap().nodes.len(), 1);
}

#[tokio::test]
async fn invalid_traversals_are_refused_like_every_backend() {
    let Some((_db, backend)) = graph_env().await else {
        return;
    };
    let ok = Traversal {
        start: vec![node("party", id())],
        edges: vec![spec("party", "parent_party")],
        direction: Direction::Both,
        max_depth: 3,
        max_nodes: 10,
    };
    for (bad, kind) in [
        (
            Traversal {
                edges: vec![],
                ..ok.clone()
            },
            ErrorKind::InvalidQuery,
        ),
        (
            Traversal {
                max_depth: 0,
                ..ok.clone()
            },
            ErrorKind::InvalidQuery,
        ),
        (
            Traversal {
                max_depth: 65,
                ..ok.clone()
            },
            ErrorKind::InvalidQuery,
        ),
        (
            Traversal {
                edges: vec![spec("party", "name")],
                ..ok.clone()
            },
            ErrorKind::InvalidQuery,
        ),
        (
            Traversal {
                edges: vec![spec("nope", "x")],
                ..ok.clone()
            },
            ErrorKind::InvalidQuery,
        ),
        (
            Traversal {
                edges: vec![spec("party", "parent")],
                ..ok.clone()
            },
            ErrorKind::InvalidQuery,
        ),
        (
            Traversal {
                start: vec![node("nope", id())],
                ..ok.clone()
            },
            ErrorKind::UnknownEntity,
        ),
    ] {
        assert_eq!(
            traverse(&backend, &bad).await.unwrap_err().kind,
            kind,
            "{bad:?}"
        );
    }
    // No starts at all is an empty result, not an error.
    let none = Traversal {
        start: vec![],
        ..ok
    };
    assert!(traverse(&backend, &none).await.unwrap().nodes.is_empty());
}

#[tokio::test]
async fn a_lattice_with_two_to_the_forty_paths_is_walked_in_linear_work() {
    let Some((db, backend)) = graph_env().await else {
        return;
    };
    // A ladder: party k has parent k+1; invoice k links (dynamically) to invoice k+1 and to
    // party k. Walking both ways, each rung can be crossed in two ways, so a per-path walk would
    // enumerate 2^40 paths; one row per level visits each node once.
    const RUNGS: usize = 40;
    let parties: Vec<RecordId> = (0..RUNGS).map(|_| id()).collect();
    let invoices: Vec<RecordId> = (0..RUNGS).map(|_| id()).collect();
    let model = Model {
        parties: parties
            .iter()
            .enumerate()
            .map(|(i, p)| (*p, (i + 1 < RUNGS).then_some(i + 1)))
            .collect(),
        invoices: invoices
            .iter()
            .enumerate()
            .map(|(i, v)| {
                (
                    *v,
                    i,
                    (i + 1 < RUNGS).then(|| ("invoice".to_owned(), invoices[i + 1])),
                )
            })
            .collect(),
        lines: vec![],
    };
    model.load(&mut db.raw_client().await).await;
    let t = Traversal {
        start: vec![node("invoice", invoices[0])],
        edges: vec![
            spec("party", "parent_party"),
            spec("invoice", "party"),
            spec("invoice", "ref_id"),
        ],
        direction: Direction::Both,
        max_depth: 64,
        max_nodes: 1_000,
    };
    let started = std::time::Instant::now();
    let got = traverse(&backend, &t).await.unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "took {:?}",
        started.elapsed()
    );
    assert_eq!(got, reference(&model, &t).unwrap());
    assert_eq!(got.nodes.len(), 2 * RUNGS);
}

#[tokio::test]
async fn a_huge_fan_out_fails_fast_without_collecting_it() {
    let Some((db, backend)) = graph_env().await else {
        return;
    };
    let mut client = db.raw_client().await;
    // A hub with 50,000 parties linking to it.
    let hub = id();
    let t = client.transaction().await.unwrap();
    t.execute(
        "INSERT INTO party (id, version, name, credit_limit, parent_party) \
         SELECT CASE WHEN g = 0 THEN $1::uuid ELSE uuidv7() END, 1, 'p' || g, 0, \
                CASE WHEN g = 0 THEN NULL ELSE $1::uuid END FROM generate_series(0, 50000) g",
        &[&uuid(hub)],
    )
    .await
    .unwrap();
    t.commit().await.unwrap();
    // Incoming edges are only cheap with an index on the link column, as documented.
    client
        .batch_execute("CREATE INDEX party_parent_fan ON party (parent_party); ANALYZE party")
        .await
        .unwrap();
    let q = Traversal {
        start: vec![node("party", hub)],
        edges: vec![spec("party", "parent_party")],
        direction: Direction::Incoming,
        max_depth: 3,
        max_nodes: 100,
    };
    let started = std::time::Instant::now();
    assert_eq!(
        traverse(&backend, &q).await.unwrap_err().kind,
        ErrorKind::TraversalTooLarge
    );
    println!(
        "50,000-leaf hub with max_nodes 100 refused in {:?}",
        started.elapsed()
    );
    // And exactly at the size it is returned whole.
    let whole = Traversal {
        max_nodes: 50_001,
        ..q
    };
    assert_eq!(
        traverse(&backend, &whole).await.unwrap().nodes.len(),
        50_001
    );
}

/// Depth-3 fan-out timings at one million parent links (run with `--ignored --nocapture`).
#[tokio::test]
#[ignore = "a benchmark: one million edges, about a minute to load"]
async fn depth_three_fan_out_at_one_million_edges() {
    let Some((db, backend)) = graph_env().await else {
        return;
    };
    let client = db.raw_client().await;
    let load = std::time::Instant::now();
    client
        .batch_execute(
            "INSERT INTO party (id, version, name, credit_limit, parent_party) \
             SELECT ids[g], 1, 'p' || g, 0, CASE WHEN g = 1 THEN NULL ELSE ids[1 + ((g::bigint * 7919) % (g - 1))::int] END \
             FROM (SELECT array_agg(uuidv7()) AS ids FROM generate_series(1, 1000000)) s, \
                  generate_series(1, 1000000) g; \
             ",
        )
        .await
        .unwrap();
    client
        .batch_execute("CREATE INDEX party_parent_bench ON party (parent_party); ANALYZE party")
        .await
        .unwrap();
    println!(
        "loaded 1,000,000 parties and parent links in {:?}",
        load.elapsed()
    );
    let root: Uuid = client
        .query_one("SELECT id FROM party WHERE name = 'p1'", &[])
        .await
        .unwrap()
        .get(0);
    let root = RecordId::from_bytes(*root.as_bytes()).unwrap();
    for (label, direction) in [
        ("incoming (children)", Direction::Incoming),
        ("outgoing (ancestors)", Direction::Outgoing),
        ("both ways", Direction::Both),
    ] {
        for depth in [1, 2, 3] {
            let t = Traversal {
                start: vec![node("party", root)],
                edges: vec![spec("party", "parent_party")],
                direction,
                max_depth: depth,
                max_nodes: 2_000_000,
            };
            let started = std::time::Instant::now();
            let got = traverse(&backend, &t).await.unwrap();
            println!(
                "{label:22} depth {depth}: {:>8} nodes in {:?}",
                got.nodes.len(),
                started.elapsed()
            );
        }
    }
}
