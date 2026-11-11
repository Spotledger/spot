//! The checker: from a history alone, which anomalies does it exhibit?
//!
//! [`check`] infers the version order ([`crate::versions`]), builds the direct serialization
//! graph ([`crate::deps`]) and searches it for the phenomena [`crate::anomaly`] names. Every
//! cycle is found with Tarjan's strongly-connected components and a shortest-path search, so a
//! history with no anomaly costs one linear pass and a history with an anomaly costs one search
//! inside the offending component. A finding is a [`crate::anomaly::Witness`]: the transactions,
//! keys and edges of a concrete, minimal cycle, or the bad read itself.
//!
//! # Detection rules
//!
//! For each committed transaction the graph is over its node indices; a cycle length is counted
//! in edges.
//!
//! - **G0** uses only `ww` edges: a nontrivial strongly-connected component of the `ww` graph.
//! - **G1c** uses `ww` and `wr` edges: a nontrivial component of that subgraph.
//! - **G2** is any cycle with at least one `rw` edge: a nontrivial component of the full graph
//!   that contains an `rw` edge (any edge in a strongly-connected component lies on a cycle).
//! - **G-single** is a cycle with exactly one `rw` edge: for an `rw` edge `u -> v` inside a
//!   component, a path `v -> u` in the `ww`/`wr` subgraph closes it.
//! - **Lost update** is a single-key cycle: the same search restricted to one key's `ww` and
//!   `rw` edges.
//! - **G1a**, **G1b**, **stale read** and the **internal inconsistencies** are read directly
//!   from the history and the inferred order; they are not cycles.
//!
//! The witnesses are capped ([`MAX_WITNESSES_PER_CLASS`]) so a badly broken backend cannot make
//! the report unbounded; the counts still count every finding the search reached.

use std::collections::{BTreeMap, BTreeSet};

use spot_l7_invariants::history::Level;

use crate::anomaly::{Anomaly, AnomalyClass, Witness, WitnessEdge};
use crate::deps::{DepEdge, DepGraph, EdgeKind};
use crate::graph::DiGraph;
use crate::history::{Observed, TxnHistory, TxnOp};
use crate::levels::anomalies_forbidden;
use crate::report::{ClassReport, IsolationReport, Verdict};
use crate::versions::{self, VersionModel};

/// The most witnesses kept per class in a report.
pub const MAX_WITNESSES_PER_CLASS: usize = 25;

/// What one class's search found.
#[derive(Clone, Debug, Default)]
struct ClassFindings {
    total: u64,
    witnesses: Vec<Witness>,
}

/// Findings for every class.
#[derive(Clone, Debug, Default)]
struct Findings {
    per_class: BTreeMap<AnomalyClass, ClassFindings>,
}

impl Findings {
    fn push(&mut self, anomaly: Anomaly) {
        let entry = self.per_class.entry(anomaly.class).or_default();
        entry.total += 1;
        if entry.witnesses.len() < MAX_WITNESSES_PER_CLASS {
            entry.witnesses.push(anomaly.witness);
        }
    }

    fn count(&self, class: AnomalyClass) -> u64 {
        self.per_class.get(&class).map_or(0, |f| f.total)
    }

    /// Counts one more finding of a class without keeping a witness.
    fn count_only(&mut self, class: AnomalyClass) {
        self.per_class.entry(class).or_default().total += 1;
    }
}

/// Checks a history against a claimed isolation level.
pub fn check(history: &TxnHistory, claimed: Level) -> IsolationReport {
    let model = versions::infer(history);
    let deps = DepGraph::build(&model);
    let mut found = Findings::default();

    detect_write_cycle(&deps, &mut found); // G0
    detect_circular_information(&deps, &mut found); // G1c
    detect_g2(&deps, &mut found); // G2
    detect_g_single(&deps, &mut found); // G-single
    detect_lost_update(&deps, &model, &mut found); // lost update
    detect_g1a(&model, &mut found); // G1a
    detect_g1b(&model, &mut found); // G1b
    detect_stale(&model, &mut found); // stale read
    detect_internal_inconsistency(history, &model, &mut found);
    detect_non_repeatable(history, &mut found);

    // Classify.
    let forbidden = anomalies_forbidden(claimed);
    let mut violations: Vec<AnomalyClass> = Vec::new();
    let mut observed_allowed: Vec<AnomalyClass> = Vec::new();
    let mut classes: Vec<ClassReport> = Vec::new();
    let mut all_found: BTreeSet<AnomalyClass> = BTreeSet::new();
    for class in AnomalyClass::all() {
        let count = found.count(class);
        if count == 0 {
            continue;
        }
        all_found.insert(class);
        let is_forbidden = forbidden.contains(&class);
        if is_forbidden {
            violations.push(class);
        } else {
            observed_allowed.push(class);
        }
        classes.push(ClassReport {
            class,
            forbidden: is_forbidden,
            total: count,
            witnesses: found
                .per_class
                .get(&class)
                .map(|f| f.witnesses.clone())
                .unwrap_or_default(),
        });
    }

    let strongest = crate::levels::strongest_consistent(&all_found);
    let verdict = if violations.is_empty() {
        Verdict::Consistent
    } else {
        Verdict::Violation {
            claimed,
            observed: violations.clone(),
        }
    };
    let (committed, aborted, unknown) = history.outcome_counts();
    IsolationReport {
        schema: crate::report::REPORT_SCHEMA.to_owned(),
        claimed,
        workload: history.header.workload,
        seed: history.header.seed,
        workers: history.header.workers,
        transactions: history.entries.len() as u64,
        committed,
        aborted,
        unknown,
        verdict,
        violations,
        observed_allowed,
        strongest_consistent: strongest,
        classes,
    }
}

// -- helpers ---------------------------------------------------------------------------------

fn txn_of(deps: &DepGraph, node: usize) -> u64 {
    deps.nodes.get(node).copied().unwrap_or(0)
}

/// Finds one real edge of the given kinds between two transactions (optionally on `key`).
fn find_edge(
    deps: &DepGraph,
    kinds: &[EdgeKind],
    from: u64,
    to: u64,
    key: Option<&str>,
) -> Option<DepEdge> {
    deps.edges
        .iter()
        .find(|e| {
            e.from == from
                && e.to == to
                && kinds.contains(&e.kind)
                && key.is_none_or(|k| e.key == k)
        })
        .cloned()
}

/// Builds a cycle witness from a node path whose first and last node are equal.
fn cycle_witness(
    deps: &DepGraph,
    class: AnomalyClass,
    path: &[usize],
    kinds: &[EdgeKind],
    key: Option<&str>,
) -> Witness {
    let mut edges = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    let mut txns: Vec<u64> = Vec::new();
    for window in path.windows(2) {
        let from = txn_of(deps, window[0]);
        let to = txn_of(deps, window[1]);
        let edge = find_edge(deps, kinds, from, to, key);
        if let Some(e) = &edge {
            if !keys.contains(&e.key) {
                keys.push(e.key.clone());
            }
            edges.push(WitnessEdge {
                from,
                to,
                kind: e.kind,
                key: e.key.clone(),
                detail: e.detail(),
            });
        } else {
            edges.push(WitnessEdge {
                from,
                to,
                kind: kinds.first().copied().unwrap_or(EdgeKind::Rw),
                key: key.unwrap_or("").to_owned(),
                detail: String::new(),
            });
        }
        if !txns.contains(&from) {
            txns.push(from);
        }
    }
    let story = story_for(class, &txns, &keys, &edges);
    Witness {
        txns,
        keys,
        edges,
        story,
    }
}

fn story_for(class: AnomalyClass, txns: &[u64], keys: &[String], edges: &[WitnessEdge]) -> String {
    let chain = edges
        .iter()
        .map(|e| format!("T{} -{}-> T{}", e.from, e.kind.symbol(), e.to))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{}: transactions {:?} over key(s) {:?}: {}",
        class.name(),
        txns,
        keys,
        chain
    )
}

/// A cycle in a subgraph, as a node path `[u, ..., u]`, if the component has one.
fn component_cycle(graph: &DiGraph, members: &[usize]) -> Option<Vec<usize>> {
    // Any edge inside the component closes a cycle. Choose the first and find a shortest path
    // back to its tail.
    for &u in members {
        for &v in graph.successors(u) {
            if members.contains(&v)
                && let Some(mut path) = graph.shortest_path(v, u)
            {
                let mut cycle = vec![u];
                cycle.append(&mut path);
                return Some(cycle);
            }
        }
    }
    None
}

// -- cycle detectors -------------------------------------------------------------------------

fn detect_write_cycle(deps: &DepGraph, found: &mut Findings) {
    let g = deps.subgraph(&[EdgeKind::Ww]);
    let scc = g.sccs();
    for (_, members) in scc.nontrivial() {
        if let Some(cycle) = component_cycle(&g, members) {
            found.push(Anomaly {
                class: AnomalyClass::G0,
                witness: cycle_witness(deps, AnomalyClass::G0, &cycle, &[EdgeKind::Ww], None),
            });
        }
    }
}

fn detect_circular_information(deps: &DepGraph, found: &mut Findings) {
    let kinds = [EdgeKind::Ww, EdgeKind::Wr];
    let g = deps.subgraph(&kinds);
    let scc = g.sccs();
    for (_, members) in scc.nontrivial() {
        if let Some(cycle) = component_cycle(&g, members) {
            found.push(Anomaly {
                class: AnomalyClass::G1c,
                witness: cycle_witness(deps, AnomalyClass::G1c, &cycle, &kinds, None),
            });
        }
    }
}

fn detect_g2(deps: &DepGraph, found: &mut Findings) {
    let kinds = [EdgeKind::Ww, EdgeKind::Wr, EdgeKind::Rw];
    let g = deps.subgraph(&kinds);
    let scc = g.sccs();
    // Components that contain an rw edge.
    let mut has_rw: Vec<bool> = vec![false; scc.count()];
    for e in &deps.edges {
        if e.kind != EdgeKind::Rw {
            continue;
        }
        if let (Some(&u), Some(&v)) = (deps.index.get(&e.from), deps.index.get(&e.to))
            && scc.component_of(u) == scc.component_of(v)
            && scc.members(scc.component_of(u)).len() > 1
        {
            has_rw[scc.component_of(u)] = true;
        }
    }
    for (c, members) in scc.nontrivial() {
        if !has_rw[c] {
            continue;
        }
        // Prefer a witness whose closing edge is the rw edge, so the story names it.
        if let Some(cycle) = component_cycle_with_kind(deps, &g, members, EdgeKind::Rw) {
            found.push(Anomaly {
                class: AnomalyClass::G2,
                witness: cycle_witness(deps, AnomalyClass::G2, &cycle, &kinds, None),
            });
        }
    }
}

/// Like [`component_cycle`], but prefers a cycle that contains an edge of `want`.
fn component_cycle_with_kind(
    deps: &DepGraph,
    graph: &DiGraph,
    members: &[usize],
    want: EdgeKind,
) -> Option<Vec<usize>> {
    let member_set: BTreeSet<usize> = members.iter().copied().collect();
    // Try each node whose outgoing edge of `want` stays inside the component.
    for &u in members {
        let from_txn = txn_of(deps, u);
        let crossing = deps
            .edges_from(from_txn, &[want])
            .filter_map(|e| deps.node(e.to))
            .find(|v| member_set.contains(v));
        if let Some(v) = crossing
            && let Some(mut path) = graph.shortest_path(v, u)
        {
            let mut cycle = vec![u];
            cycle.append(&mut path);
            return Some(cycle);
        }
    }
    component_cycle(graph, members)
}

fn detect_g_single(deps: &DepGraph, found: &mut Findings) {
    let full = deps.subgraph(&[EdgeKind::Ww, EdgeKind::Wr, EdgeKind::Rw]);
    let scc = full.sccs();
    let wr_kinds = [EdgeKind::Ww, EdgeKind::Wr];
    let ww_wr = deps.subgraph(&wr_kinds);
    let ww_wr_scc = ww_wr.sccs();
    // The condensation of the ww/wr graph, for reachability between different ww/wr components.
    let mut cond = DiGraph::new(ww_wr_scc.count());
    for e in &deps.edges {
        if !wr_kinds.contains(&e.kind) {
            continue;
        }
        if let (Some(&u), Some(&v)) = (deps.index.get(&e.from), deps.index.get(&e.to)) {
            let (cu, cv) = (ww_wr_scc.component_of(u), ww_wr_scc.component_of(v));
            if cu != cv {
                cond.add_edge(cu, cv);
            }
        }
    }
    let mut reach: BTreeMap<(usize, usize), bool> = BTreeMap::new();
    let mut kept = 0usize;
    for e in &deps.edges {
        if e.kind != EdgeKind::Rw {
            continue;
        }
        let (Some(&u), Some(&v)) = (deps.index.get(&e.from), deps.index.get(&e.to)) else {
            continue;
        };
        if scc.component_of(u) != scc.component_of(v) {
            continue;
        }
        if scc.members(scc.component_of(u)).len() <= 1 {
            continue;
        }
        let (cu, cv) = (ww_wr_scc.component_of(u), ww_wr_scc.component_of(v));
        let closes = if cu == cv {
            // Both ends of the rw edge are in one nontrivial ww/wr component: a ww/wr path
            // always closes the cycle.
            ww_wr_scc.members(cu).len() > 1
        } else {
            *reach
                .entry((cv, cu))
                .or_insert_with(|| cond_reaches(&cond, cv, cu))
        };
        if !closes {
            continue;
        }
        if kept < MAX_WITNESSES_PER_CLASS {
            if let Some(mut back) = ww_wr.shortest_path(v, u) {
                let mut cycle = vec![u];
                cycle.append(&mut back);
                found.push(Anomaly {
                    class: AnomalyClass::GSingle,
                    witness: cycle_witness(
                        deps,
                        AnomalyClass::GSingle,
                        &cycle,
                        &[EdgeKind::Ww, EdgeKind::Wr, EdgeKind::Rw],
                        None,
                    ),
                });
                kept += 1;
            }
        } else {
            found.count_only(AnomalyClass::GSingle);
        }
    }
}

/// Whether component `from` reaches component `to` in a condensation graph.
fn cond_reaches(cond: &DiGraph, from: usize, to: usize) -> bool {
    if from == to {
        return true;
    }
    let mut seen = vec![false; cond.len()];
    let mut stack = vec![from];
    seen[from] = true;
    while let Some(u) = stack.pop() {
        for &v in cond.successors(u) {
            if v == to {
                return true;
            }
            if !seen[v] {
                seen[v] = true;
                stack.push(v);
            }
        }
    }
    false
}

fn detect_lost_update(deps: &DepGraph, model: &VersionModel, found: &mut Findings) {
    // Group the ww/rw edges by key once, so the per-key scan does not walk every edge per key.
    let mut by_key: BTreeMap<&str, Vec<&DepEdge>> = BTreeMap::new();
    for e in &deps.edges {
        if matches!(e.kind, EdgeKind::Ww | EdgeKind::Rw) && e.from != e.to {
            by_key.entry(e.key.as_str()).or_default().push(e);
        }
    }
    let kinds = [EdgeKind::Ww, EdgeKind::Rw];
    for key in model.keys.keys() {
        let Some(edges) = by_key.get(key.as_str()) else {
            continue;
        };
        if !edges.iter().any(|e| e.kind == EdgeKind::Rw) {
            continue;
        }
        for cycle in small_graph_cycles(edges, deps) {
            found.push(Anomaly {
                class: AnomalyClass::LostUpdate,
                witness: cycle_witness(deps, AnomalyClass::LostUpdate, &cycle, &kinds, Some(key)),
            });
        }
    }
}

/// Finds one cycle per strongly-connected component of the graph made of `edges`, over only the
/// nodes those edges touch. This keeps a per-key scan cheap: a key with a few versions never
/// allocates a graph over the whole history.
fn small_graph_cycles(edges: &[&DepEdge], deps: &DepGraph) -> Vec<Vec<usize>> {
    let mut set: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
    for e in edges {
        if let (Some(&u), Some(&v)) = (deps.index.get(&e.from), deps.index.get(&e.to)) {
            set.insert(u);
            set.insert(v);
        }
    }
    let nodes: Vec<usize> = set.into_iter().collect();
    let mut map: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
    for (i, &n) in nodes.iter().enumerate() {
        map.insert(n, i);
    }
    let mut g = DiGraph::new(nodes.len());
    for e in edges {
        if let (Some(&u), Some(&v)) = (deps.index.get(&e.from), deps.index.get(&e.to)) {
            g.add_edge(map[&u], map[&v]);
        }
    }
    let scc = g.sccs();
    let mut cycles = Vec::new();
    for (_, members) in scc.nontrivial() {
        if let Some(local) = component_cycle(&g, members) {
            cycles.push(local.iter().map(|i| nodes[*i]).collect());
        }
    }
    cycles
}

// -- read detectors --------------------------------------------------------------------------

fn detect_g1a(model: &VersionModel, found: &mut Findings) {
    for r in &model.reads {
        for value in &r.observed {
            if let Some(w) = model.writes.get(&(r.key.clone(), value.clone()))
                && !w.committed
            {
                found.push(Anomaly {
                    class: AnomalyClass::G1a,
                    witness: Witness::read(
                        r.reader,
                        &r.key,
                        format!(
                            "G1a: T{} read {}={} written by aborted T{}",
                            r.reader, r.key, value, w.writer
                        ),
                    ),
                });
            }
        }
    }
}

fn detect_g1b(model: &VersionModel, found: &mut Findings) {
    // Writes of a key by one writer, in operation order, committed only.
    let mut by_writer: BTreeMap<(String, u64), Vec<(u32, String)>> = BTreeMap::new();
    for ((key, value), w) in &model.writes {
        if w.committed {
            by_writer
                .entry((key.clone(), w.writer))
                .or_default()
                .push((w.op_index, value.clone()));
        }
    }
    for r in &model.reads {
        for value in &r.observed {
            let Some(w) = model.writes.get(&(r.key.clone(), value.clone())) else {
                continue;
            };
            if !w.committed {
                continue; // G1a's territory.
            }
            if w.writer == r.reader {
                continue; // reading your own earlier write is read-your-writes, not G1b.
            }
            if let Some(writes) = by_writer.get(&(r.key.clone(), w.writer)) {
                // A later write of the same key by the same transaction that the read did not
                // observe: an intermediate read.
                if let Some((_, later)) = writes
                    .iter()
                    .find(|(i, v)| *i > w.op_index && !r.observed.contains(v))
                {
                    found.push(Anomaly {
                        class: AnomalyClass::G1b,
                        witness: Witness::read(
                            r.reader,
                            &r.key,
                            format!(
                                "G1b: T{} read intermediate {}={} of T{}'s writes to {} (later {})",
                                r.reader, r.key, value, w.writer, r.key, later
                            ),
                        ),
                    });
                }
            }
        }
    }
}

fn detect_stale(model: &VersionModel, found: &mut Findings) {
    for s in &model.stale_reads {
        found.push(Anomaly {
            class: AnomalyClass::StaleRead,
            witness: Witness::read(
                s.reader,
                &s.key,
                format!(
                    "stale-read: T{} read {}={:?} but T{} had committed {} before it started",
                    s.reader, s.key, s.observed, s.writer, s.value
                ),
            ),
        });
    }
}

// -- inner detectors -------------------------------------------------------------------------

fn detect_internal_inconsistency(history: &TxnHistory, model: &VersionModel, found: &mut Findings) {
    for cycle in &model.order_cycles {
        found.push(Anomaly {
            class: AnomalyClass::InternalInconsistency,
            witness: Witness::read(
                0,
                &cycle.key,
                format!(
                    "internal-inconsistency: key {} reads demand a contradictory version order {:?}",
                    cycle.key, cycle.values
                ),
            ),
        });
    }
    for e in &history.entries {
        if !e.is_committed() {
            continue;
        }
        // Own writes so far, per key.
        let mut list_writes: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut reg_writes: BTreeMap<String, String> = BTreeMap::new();
        for op in &e.ops {
            match op {
                TxnOp::Append { key, value } => {
                    list_writes
                        .entry(key.clone())
                        .or_default()
                        .push(value.clone());
                }
                TxnOp::Write { key, value } => {
                    reg_writes.insert(key.clone(), value.clone());
                }
                TxnOp::Read { key, observed } => {
                    if let Some(own) = reg_writes.get(key) {
                        let seen = match observed {
                            Observed::Register(v) => v.as_deref(),
                            Observed::List(v) => v.last().map(String::as_str),
                        };
                        if seen != Some(own.as_str()) {
                            found.push(Anomaly {
                                class: AnomalyClass::InternalInconsistency,
                                witness: Witness::read(
                                    e.txn,
                                    key,
                                    format!(
                                        "internal-inconsistency: T{} wrote {}={} but then read {}={:?}",
                                        e.txn, key, own, key, observed
                                    ),
                                ),
                            });
                        }
                    }
                    if let Some(own) = list_writes.get(key) {
                        let Observed::List(v) = observed else {
                            continue;
                        };
                        if !own.iter().all(|w| v.contains(w)) {
                            found.push(Anomaly {
                                class: AnomalyClass::InternalInconsistency,
                                witness: Witness::read(
                                    e.txn,
                                    key,
                                    format!(
                                        "internal-inconsistency: T{} appended {:?} to {} but read {:?}",
                                        e.txn, own, key, v
                                    ),
                                ),
                            });
                        }
                    }
                }
            }
        }
    }
}

fn detect_non_repeatable(history: &TxnHistory, found: &mut Findings) {
    for e in &history.entries {
        if !e.is_committed() {
            continue;
        }
        // The last read observed per key, and whether the transaction wrote the key since.
        let mut last_read: BTreeMap<String, Observed> = BTreeMap::new();
        let mut wrote_since: BTreeSet<String> = BTreeSet::new();
        for op in &e.ops {
            match op {
                TxnOp::Append { key, .. } | TxnOp::Write { key, .. } => {
                    wrote_since.insert(key.clone());
                }
                TxnOp::Read { key, observed } => {
                    if !wrote_since.contains(key)
                        && let Some(prev) = last_read.get(key)
                    {
                        let changed = match (prev, observed) {
                            (Observed::Register(a), Observed::Register(b)) => a != b,
                            (Observed::List(a), Observed::List(b)) => {
                                // A later list read must extend the earlier one.
                                !a.iter().all(|v| b.contains(v)) || b.len() < a.len()
                            }
                            _ => true,
                        };
                        if changed {
                            found.push(Anomaly {
                                class: AnomalyClass::NonRepeatableRead,
                                witness: Witness::read(
                                    e.txn,
                                    key,
                                    format!(
                                        "non-repeatable-read: T{} read {}={:?} then {}={:?} with no write between",
                                        e.txn, key, prev, key, observed
                                    ),
                                ),
                            });
                        }
                    }
                    last_read.insert(key.clone(), observed.clone());
                }
            }
        }
    }
}
