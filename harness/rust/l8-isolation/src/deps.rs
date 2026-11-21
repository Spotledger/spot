//! The direct serialization graph: the `ww`, `wr` and `rw` edges between committed
//! transactions.
//!
//! The definitions are Adya's (Adya, Liskov and O'Neil, *Generalized Isolation Level
//! Definitions*, ICDE 2000; the words are ours):
//!
//! - **`ww` (write dependency)** — `T1 -> T2` if `T1` installed a version of a key and `T2`
//!   installed the next version in that key's version order.
//! - **`wr` (read dependency)** — `T1 -> T2` if `T2` read a version `T1` installed.
//! - **`rw` (anti-dependency)** — `T1 -> T2` if `T1` read a version of a key and `T2` installed
//!   a later version of that key.
//!
//! Only committed transactions are nodes: the effects of an aborted transaction never
//! happened, so it cannot be part of a dependency cycle (its reads are still checked directly
//! for G1a). A `wr` edge comes straight from a read; a `ww` or `rw` edge comes from an edge in a
//! key's version order (see [`crate::versions`]), which is why the version order has to be
//! inferred first.
//!
//! Real time (`rt`) is not built as a graph here: the only real-time anomaly this checker
//! reports is a read that missed a committed write, and that is recorded directly during
//! inference as a [`crate::versions::StaleRead`]. A general `rt`-cycle search would need an
//! edge per ordered pair of transactions, which a 100,000-transaction history cannot afford;
//! the limitation is stated in the crate docs.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::graph::DiGraph;
use crate::versions::VersionModel;

/// The kind of a dependency edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    /// Write-write: one transaction installed the version the other overwrote.
    Ww,
    /// Write-read: one transaction read a version the other installed.
    Wr,
    /// Read-write (anti-dependency): one transaction read the version the other overwrote.
    Rw,
}

impl EdgeKind {
    /// The short symbol used in witnesses.
    pub fn symbol(self) -> &'static str {
        match self {
            EdgeKind::Ww => "ww",
            EdgeKind::Wr => "wr",
            EdgeKind::Rw => "rw",
        }
    }

    /// The long name.
    pub fn name(self) -> &'static str {
        match self {
            EdgeKind::Ww => "write-write",
            EdgeKind::Wr => "write-read",
            EdgeKind::Rw => "read-write",
        }
    }
}

/// One dependency edge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepEdge {
    /// The transaction depended upon (the edge's tail).
    pub from: u64,
    /// The dependent transaction (the edge's head).
    pub to: u64,
    /// The kind.
    pub kind: EdgeKind,
    /// The key the dependency is over.
    pub key: String,
    /// The version value at the tail, when the edge is about one.
    pub from_value: Option<String>,
    /// The version value at the head, when the edge is about one.
    pub to_value: Option<String>,
}

impl DepEdge {
    /// A one-line explanation of the edge.
    pub fn detail(&self) -> String {
        match (self.kind, &self.from_value, &self.to_value) {
            (EdgeKind::Ww, Some(a), Some(b)) => {
                format!("key={} wrote {} then {}", self.key, a, b)
            }
            (EdgeKind::Wr, Some(a), _) => format!("key={} read {}", self.key, a),
            (EdgeKind::Rw, Some(a), Some(b)) => {
                format!("key={} read {} then wrote {}", self.key, a, b)
            }
            (EdgeKind::Rw, Some(a), None) => format!("key={} read {}", self.key, a),
            _ => format!("key={}", self.key),
        }
    }
}

/// The direct serialization graph over committed transactions.
#[derive(Clone, Debug, Default)]
pub struct DepGraph {
    /// The transaction ids, sorted.
    pub nodes: Vec<u64>,
    /// Transaction id to node index.
    pub index: HashMap<u64, usize>,
    /// The edges.
    pub edges: Vec<DepEdge>,
}

impl DepGraph {
    /// Builds the graph from an inferred version model.
    pub fn build(model: &VersionModel) -> DepGraph {
        let mut nodes: Vec<u64> = model.committed.iter().copied().collect();
        nodes.sort_unstable();
        nodes.dedup();
        let mut index: HashMap<u64, usize> =
            nodes.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        let mut edges: Vec<DepEdge> = Vec::new();

        // ww: every version-order edge between different writers.
        for order in model.keys.values() {
            for (a, b) in order_edges(&order.order) {
                let (Some(va), Some(vb)) = (order.version(a), order.version(b)) else {
                    continue;
                };
                if va.initial || vb.initial || va.writer == vb.writer {
                    continue;
                }
                edges.push(DepEdge {
                    from: va.writer,
                    to: vb.writer,
                    kind: EdgeKind::Ww,
                    key: order.key.clone(),
                    from_value: Some(va.value.clone()),
                    to_value: Some(vb.value.clone()),
                });
            }
        }

        // wr and rw: one pass over the committed reads.
        for r in &model.reads {
            let Some(order) = model.order(&r.key) else {
                continue;
            };
            // wr: each observed version was installed by its writer.
            for value in &r.observed {
                if let Some(&vi) = order.by_value.get(value) {
                    let writer = order.versions[vi].writer;
                    if writer != 0 && writer != r.reader {
                        edges.push(DepEdge {
                            from: writer,
                            to: r.reader,
                            kind: EdgeKind::Wr,
                            key: r.key.clone(),
                            from_value: Some(value.clone()),
                            to_value: None,
                        });
                    }
                }
            }
            // rw: the versions that come strictly after what the read observed.
            //
            // For a register that is a version that started after the observed version ended, or
            // after the read ended. Both are "started after a threshold", so they are a suffix of
            // the key's versions (which are in start order) and a binary search finds it. For a
            // list it is every unobserved version: a read returns the whole list, so a version it
            // did not report must have been appended after it saw the list (a version committed
            // before the read would have been in it, a stale read).
            //
            // Only the *minimal* such versions get an edge. Once one after-version ended before a
            // later one started, every later one is non-minimal and the scan stops; the write
            // dependencies chain the rest. This makes the graph linear instead of quadratic and
            // is an under-approximation, which is safe for an anomaly finder: it can miss an
            // anomaly, never invent one.
            let observed: std::collections::BTreeSet<&str> =
                r.observed.iter().map(String::as_str).collect();
            let frontier = r
                .observed
                .iter()
                .rev()
                .find_map(|v| order.by_value.get(v).copied());
            let frontier_value =
                frontier.and_then(|f| order.versions.get(f).map(|v| v.value.clone()));
            let n = order.versions.len();
            let lo = if r.is_list {
                0
            } else {
                match frontier.and_then(|f| order.versions.get(f)) {
                    Some(fv) => {
                        let threshold = fv.end_us.min(r.reader_end);
                        order.versions.partition_point(|u| u.start_us <= threshold)
                    }
                    // A value no committed write produced: no order constraint.
                    None => n,
                }
            };
            let mut min_end: Option<u64> = None;
            for x in lo..n {
                let vx = &order.versions[x];
                if Some(x) == frontier || vx.initial || vx.writer == 0 || vx.writer == r.reader {
                    continue;
                }
                if observed.contains(vx.value.as_str()) {
                    continue;
                }
                if let Some(me) = min_end
                    && me < vx.start_us
                {
                    // Non-minimal, and every later version too: stop.
                    break;
                }
                min_end = Some(min_end.map_or(vx.end_us, |m| m.min(vx.end_us)));
                edges.push(DepEdge {
                    from: r.reader,
                    to: vx.writer,
                    kind: EdgeKind::Rw,
                    key: r.key.clone(),
                    from_value: frontier_value.clone(),
                    to_value: Some(vx.value.clone()),
                });
            }
        }

        // Make sure every edge endpoint has a node (a wr edge from the synthetic initial
        // version is dropped above; nothing can point at transaction 0).
        for e in &edges {
            for t in [e.from, e.to] {
                index.entry(t).or_insert_with(|| {
                    let i = nodes.len();
                    nodes.push(t);
                    i
                });
            }
        }
        // Keep the index dense after any late insertions.
        index = nodes.iter().enumerate().map(|(i, t)| (*t, i)).collect();
        DepGraph {
            nodes,
            index,
            edges,
        }
    }

    /// The node index of a transaction.
    pub fn node(&self, txn: u64) -> Option<usize> {
        self.index.get(&txn).copied()
    }

    /// The edges of the given kinds, as a graph over node indices.
    pub fn subgraph(&self, kinds: &[EdgeKind]) -> DiGraph {
        let mut g = DiGraph::new(self.nodes.len());
        for e in &self.edges {
            if kinds.contains(&e.kind)
                && let (Some(&u), Some(&v)) = (self.index.get(&e.from), self.index.get(&e.to))
            {
                g.add_edge(u, v);
            }
        }
        g
    }

    /// Edges whose tail is `txn`.
    pub fn edges_from(&self, txn: u64, kinds: &[EdgeKind]) -> impl Iterator<Item = &DepEdge> {
        self.edges
            .iter()
            .filter(move |e| e.from == txn && kinds.contains(&e.kind))
    }
}

/// Every `(tail, head)` index pair of a graph's edges.
pub fn order_edges(g: &DiGraph) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for u in 0..g.len() {
        for &v in g.successors(u) {
            out.push((u, v));
        }
    }
    out
}
