//! Version-order inference: putting the writes of every key in the order they were applied.
//!
//! # Why this is the hard part
//!
//! A history records *what* each transaction read and wrote, not the order the backend applied
//! the writes. Elle's insight is that a workload whose values are unique lets the reads recover
//! that order. This module turns one [`TxnHistory`] into, per key, an explicit **version order**
//! (a directed graph over the versions that must come before one another), following Elle
//! (Kingsbury and Alvaro, *Elle: Inferring Isolation Anomalies from Experimental
//! Observations*, VLDB 2021) and Adya's treatment of version order (Adya, Liskov and O'Neil,
//! *Generalized Isolation Level Definitions*, ICDE 2000).
//!
//! # The rules, stated in our own words
//!
//! A **version** is one committed write of a key. A read observes the versions whose values it
//! returned. Two facts constrain the order between two versions `a` and `b` of the same key:
//!
//! - **List-append**, from the list prefix. A read returns the whole current list, so if it
//!   returned `[v1, v2, ...]` then `v1` was appended before `v2`, and so on: we add `v1 -> v2`
//!   between consecutive values of one read. Because every read returns a whole list, two reads
//!   that share a value also pin the values between and around them.
//! - **A committed write before a read.** If `a` finished (committed) before the reader's
//!   transaction started and the read did not observe `a`, then for a register `b -> a` where
//!   `b` is the version the read did observe: the read saw the value that overwrote `a`. For a
//!   list this is impossible — a committed append before the read *must* be in the returned
//!   list — so we record it as a [`StaleRead`] instead.
//! - **A write after a read.** If `b` started after the reader's transaction ended, then `b`
//!   is later than any version the read observed, so `a -> b` for `a` the last observed version.
//! - **Real time between writes.** If `a` committed before `b` started, then `a -> b`.
//!
//! A read whose value matches no committed write is either a read of an aborted write (an
//! anomaly the anomaly module names) or an unknown value; both are left out of the order here
//! and reported later.
//!
//! The version order must be acyclic. If it is not, the reads contradict one another and the
//! cycle itself is an anomaly ([`OrderCycle`]); we still keep the edges so the other checks can
//! run.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::graph::DiGraph;
use crate::history::{TxnHistory, Workload};

/// One committed write of a key: a version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    /// The unique value written.
    pub value: String,
    /// The transaction that wrote it, or `0` for the synthetic initial version of a register.
    pub writer: u64,
    /// The write's position within its transaction (`u32::MAX` for the initial version).
    pub op_index: u32,
    /// The writer's transaction start, microseconds (0 for the initial version).
    pub start_us: u64,
    /// The writer's transaction end (0 for the initial version).
    pub end_us: u64,
    /// Whether this is the synthetic initial version of a register.
    pub initial: bool,
}

/// The versions of one key and the forced order between them.
#[derive(Clone, Debug)]
pub struct KeyOrder {
    /// The key.
    pub key: String,
    /// The key's workload.
    pub workload: Workload,
    /// The versions: index `0` is the initial version for a register, absent for a list.
    pub versions: Vec<Version>,
    /// Version value to index.
    pub by_value: HashMap<String, usize>,
    /// Edges `a -> b` meaning "version `a` precedes version `b`".
    pub order: DiGraph,
}

impl KeyOrder {
    /// The index of a version by value.
    pub fn index_of(&self, value: &str) -> Option<usize> {
        self.by_value.get(value).copied()
    }

    /// A version by index.
    pub fn version(&self, index: usize) -> Option<&Version> {
        self.versions.get(index)
    }
}

/// A committed read that observed a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadEvent {
    /// The reading transaction.
    pub reader: u64,
    /// The reader's transaction start.
    pub reader_start: u64,
    /// The reader's transaction end.
    pub reader_end: u64,
    /// The key read.
    pub key: String,
    /// The values observed, in order (a register has at most one).
    pub observed: Vec<String>,
    /// Whether this was a list-append read.
    pub is_list: bool,
}

/// A read that missed a write it should have seen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StaleRead {
    /// The committed reader.
    pub reader: u64,
    /// The key.
    pub key: String,
    /// The writer whose committed version the read missed.
    pub writer: u64,
    /// The value it missed.
    pub value: String,
    /// What the read observed instead.
    pub observed: Vec<String>,
}

/// A key whose version order is contradictory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderCycle {
    /// The key.
    pub key: String,
    /// The version values in a cycle (in order).
    pub values: Vec<String>,
}

/// A write, committed or not, so a read of an aborted write can be recognised.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteRecord {
    /// The writing transaction.
    pub writer: u64,
    /// Whether the write is committed.
    pub committed: bool,
    /// The write's position within its transaction.
    pub op_index: u32,
    /// The writer's transaction end.
    pub end_us: u64,
}

/// Everything the checker learns from a history before it builds dependencies.
#[derive(Clone, Debug, Default)]
pub struct VersionModel {
    /// Per-key version order.
    pub keys: BTreeMap<String, KeyOrder>,
    /// Committed reads, in history order.
    pub reads: Vec<ReadEvent>,
    /// Every write, committed or not, by `(key, value)`.
    pub writes: HashMap<(String, String), WriteRecord>,
    /// Reads that missed a committed write.
    pub stale_reads: Vec<StaleRead>,
    /// Contradictory version orders.
    pub order_cycles: Vec<OrderCycle>,
    /// Committed transaction ids.
    pub committed: BTreeSet<u64>,
    /// Aborted transaction ids.
    pub aborted: BTreeSet<u64>,
}

impl VersionModel {
    /// The versions of a key.
    pub fn order(&self, key: &str) -> Option<&KeyOrder> {
        self.keys.get(key)
    }

    /// The initial register value every `l8_item` starts with.
    pub const INITIAL_REGISTER: &'static str = "0";
}

/// Builds the version model from a history whose workload is known.
pub fn infer(history: &TxnHistory) -> VersionModel {
    let mut model = VersionModel::default();
    let workload = history.header.workload;

    // Pass 1: transaction outcomes.
    for e in &history.entries {
        if e.is_committed() {
            model.committed.insert(e.txn);
        } else if e.is_aborted() {
            model.aborted.insert(e.txn);
        }
        // Unknown-outcome transactions contribute nothing at all.
        if !e.is_committed() && !e.is_aborted() {
            continue;
        }
        // Every write, committed or not, is remembered.
        for (i, op) in e.ops.iter().enumerate() {
            if let Some(value) = op.written() {
                model.writes.insert(
                    (op.key().to_owned(), value.to_owned()),
                    WriteRecord {
                        writer: e.txn,
                        committed: e.is_committed(),
                        op_index: u32::try_from(i).unwrap_or(u32::MAX),
                        end_us: e.end_us,
                    },
                );
            }
        }
        // Only a committed read is part of the dependency graph.
        if e.is_committed() {
            for op in &e.ops {
                if let crate::history::TxnOp::Read { key, observed } = op {
                    let (values, is_list) = match observed {
                        crate::history::Observed::List(v) => (v.clone(), true),
                        crate::history::Observed::Register(v) => {
                            (v.iter().cloned().collect::<Vec<_>>(), false)
                        }
                    };
                    model.reads.push(ReadEvent {
                        reader: e.txn,
                        reader_start: e.start_us,
                        reader_end: e.end_us,
                        key: key.clone(),
                        observed: values,
                        is_list,
                    });
                }
            }
        }
    }

    // Pass 2: build the versions per key from committed writes, in history order.
    let mut key_versions: BTreeMap<String, Vec<Version>> = BTreeMap::new();
    for e in &history.entries {
        if !e.is_committed() {
            continue;
        }
        for (i, op) in e.ops.iter().enumerate() {
            if let Some(value) = op.written() {
                let entry = key_versions.entry(op.key().to_owned()).or_default();
                if entry.iter().any(|v| v.value == value) {
                    // The same value written twice: not unique, so the inference cannot place
                    // it. Keep the first; the anomaly module reports the duplicate value.
                    continue;
                }
                entry.push(Version {
                    value: value.to_owned(),
                    writer: e.txn,
                    op_index: u32::try_from(i).unwrap_or(u32::MAX),
                    start_us: e.start_us,
                    end_us: e.end_us,
                    initial: false,
                });
            }
        }
    }
    if workload == Workload::RwRegister {
        for versions in key_versions.values_mut() {
            versions.insert(
                0,
                Version {
                    value: VersionModel::INITIAL_REGISTER.to_owned(),
                    writer: 0,
                    op_index: u32::MAX,
                    start_us: 0,
                    end_us: 0,
                    initial: true,
                },
            );
        }
    }

    // Reads grouped by key once, so each key's pass does not scan every read in the history.
    let mut reads_by_key: HashMap<&str, Vec<&ReadEvent>> = HashMap::new();
    for r in &model.reads {
        reads_by_key.entry(r.key.as_str()).or_default().push(r);
    }

    // Pass 3: order edges.
    for (key, versions) in key_versions.iter_mut() {
        // Versions in start order (the synthetic initial first), so a read's neighbours can be
        // found by binary search rather than by scanning the key.
        versions.sort_by_key(|v| (!v.initial, v.start_us));
        let mut by_value: HashMap<String, usize> = HashMap::new();
        for (i, v) in versions.iter().enumerate() {
            by_value.insert(v.value.clone(), i);
        }
        // Version indices sorted by their writer's end, for the register predecessor search.
        let mut by_end: Vec<usize> = (0..versions.len()).collect();
        by_end.sort_by_key(|&i| (versions[i].end_us, i));
        let mut order = DiGraph::new(versions.len());
        // Real time between writes: a version committed strictly before the next version in
        // start order began must precede it. (We add only these adjacent edges; longer real-time
        // chains follow from them transitively, and this keeps a key with many versions linear
        // in memory rather than quadratic.)
        for i in 0..versions.len().saturating_sub(1) {
            if versions[i].end_us < versions[i + 1].start_us {
                order.add_edge(i, i + 1);
            }
        }
        // Reads of this key.
        for &r in reads_by_key
            .get(key.as_str())
            .map_or(&[][..], Vec::as_slice)
        {
            let resolved: Vec<usize> = r
                .observed
                .iter()
                .filter_map(|v| by_value.get(v).copied())
                .collect();
            if r.is_list {
                for pair in resolved.windows(2) {
                    order.add_edge(pair[0], pair[1]);
                }
                let frontier = resolved.last().copied();
                let mut earliest_future: Option<usize> = None;
                for (i, u) in versions.iter().enumerate() {
                    if u.initial || resolved.contains(&i) {
                        continue;
                    }
                    if u.end_us < r.reader_start {
                        model.stale_reads.push(StaleRead {
                            reader: r.reader,
                            key: key.clone(),
                            writer: u.writer,
                            value: u.value.clone(),
                            observed: r.observed.clone(),
                        });
                    } else if u.start_us > r.reader_end {
                        earliest_future = Some(match earliest_future {
                            Some(x) if versions[x].start_us <= u.start_us => x,
                            _ => i,
                        });
                    }
                }
                if let (Some(f), Some(i)) = (frontier, earliest_future) {
                    order.add_edge(f, i);
                }
            } else {
                // Register: the read observed one value, the version current at the read. The
                // nearest predecessor and successor get an order edge (found by binary search);
                // longer chains follow from the real-time edges, so the graph stays linear.
                let Some(&v) = resolved.last() else {
                    // A value no committed write produced: either an aborted write or unknown,
                    // named by the anomaly module. It constrains nothing here.
                    continue;
                };
                // The latest version committed before the read started, if any. `by_end` is
                // sorted by end, so the ones with end < reader_start are a prefix.
                let before = by_end.partition_point(|&x| versions[x].end_us < r.reader_start);
                let latest_past = by_end[..before]
                    .iter()
                    .rev()
                    .copied()
                    .find(|&x| x != v && !versions[x].initial);
                // The first version to start after the read ended, if any. `versions` is in start
                // order.
                let after = versions.partition_point(|u| u.start_us <= r.reader_end);
                let earliest_future =
                    (after..versions.len()).find(|&x| x != v && !versions[x].initial);
                if versions[v].initial {
                    if let Some(i) = latest_past {
                        // The read saw the initial value although a committed write preceded it:
                        // a stale read.
                        model.stale_reads.push(StaleRead {
                            reader: r.reader,
                            key: key.clone(),
                            writer: versions[i].writer,
                            value: versions[i].value.clone(),
                            observed: r.observed.clone(),
                        });
                    }
                } else if let Some(i) = latest_past {
                    order.add_edge(i, v);
                }
                if let Some(i) = earliest_future {
                    order.add_edge(v, i);
                }
            }
        }
        // Contradictory orders.
        let scc = order.sccs();
        for (_, members) in scc.nontrivial() {
            let values = members
                .iter()
                .filter_map(|i| versions.get(*i).map(|v| v.value.clone()))
                .collect();
            model.order_cycles.push(OrderCycle {
                key: key.clone(),
                values,
            });
        }
        model.keys.insert(
            key.clone(),
            KeyOrder {
                key: key.clone(),
                workload,
                versions: versions.clone(),
                by_value,
                order,
            },
        );
    }
    model
}
