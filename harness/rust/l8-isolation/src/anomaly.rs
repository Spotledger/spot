//! The anomaly classes the checker names, and their witnesses.
//!
//! The classes are Adya's phenomena plus two the brief calls for. Every witness is a small,
//! explainable story: the transactions, the keys, and the dependency edges (or the bad read)
//! that form the cycle, so a finding can be checked by hand. Nothing here carries document
//! content beyond the unique values the workload wrote.

use serde::{Deserialize, Serialize};

use crate::deps::EdgeKind;

/// One anomaly class.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AnomalyClass {
    /// G0: a cycle of `ww` edges — dirty writes.
    G0,
    /// G1a: a committed transaction read a value written by an aborted transaction.
    G1a,
    /// G1b: a committed transaction read a non-final version of one writer's multi-write to a
    /// key — an intermediate read.
    G1b,
    /// G1c: a cycle of `ww` and `wr` edges — circular information flow.
    G1c,
    /// G-single: a cycle with exactly one `rw` edge — read skew.
    GSingle,
    /// G2: a cycle with one or more `rw` edges — write skew (an item anti-dependency cycle;
    /// this workload has no predicate reads, so G2 and G2-item coincide).
    G2,
    /// A cycle over a single key with an `rw` edge — lost update / read-modify-write conflict.
    LostUpdate,
    /// One transaction read two different states of a key with no write of its own between.
    NonRepeatableRead,
    /// A read missed a write that had committed before it.
    StaleRead,
    /// A transaction's own reads contradict its own writes, or the reads cannot be ordered.
    InternalInconsistency,
}

impl AnomalyClass {
    /// Every class, in the order reports list them.
    pub fn all() -> [AnomalyClass; 10] {
        [
            AnomalyClass::G0,
            AnomalyClass::G1a,
            AnomalyClass::G1b,
            AnomalyClass::G1c,
            AnomalyClass::GSingle,
            AnomalyClass::G2,
            AnomalyClass::LostUpdate,
            AnomalyClass::NonRepeatableRead,
            AnomalyClass::StaleRead,
            AnomalyClass::InternalInconsistency,
        ]
    }

    /// The name used in reports.
    pub fn name(self) -> &'static str {
        match self {
            AnomalyClass::G0 => "G0",
            AnomalyClass::G1a => "G1a",
            AnomalyClass::G1b => "G1b",
            AnomalyClass::G1c => "G1c",
            AnomalyClass::GSingle => "G-single",
            AnomalyClass::G2 => "G2",
            AnomalyClass::LostUpdate => "lost-update",
            AnomalyClass::NonRepeatableRead => "non-repeatable-read",
            AnomalyClass::StaleRead => "stale-read",
            AnomalyClass::InternalInconsistency => "internal-inconsistency",
        }
    }

    /// A one-line description.
    pub fn describe(self) -> &'static str {
        match self {
            AnomalyClass::G0 => "a write cycle (dirty write)",
            AnomalyClass::G1a => "a read of an aborted write",
            AnomalyClass::G1b => "a read of an intermediate version",
            AnomalyClass::G1c => "a cycle of write and read dependencies",
            AnomalyClass::GSingle => "a cycle with exactly one anti-dependency (read skew)",
            AnomalyClass::G2 => "a cycle with an anti-dependency (write skew)",
            AnomalyClass::LostUpdate => "a single-key anti-dependency cycle (lost update)",
            AnomalyClass::NonRepeatableRead => {
                "one transaction read two different states of a key with no write between"
            }
            AnomalyClass::StaleRead => "a read missed a write committed before it",
            AnomalyClass::InternalInconsistency => {
                "a transaction contradicts its own reads or writes"
            }
        }
    }
}

/// One edge of a witness cycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WitnessEdge {
    /// The transaction depended upon.
    pub from: u64,
    /// The dependent transaction.
    pub to: u64,
    /// The edge kind.
    pub kind: EdgeKind,
    /// The key.
    pub key: String,
    /// A one-line explanation.
    pub detail: String,
}

/// A minimal, explainable witness of one anomaly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Witness {
    /// The transactions involved, in the order the story presents them.
    pub txns: Vec<u64>,
    /// The keys involved, distinct.
    pub keys: Vec<String>,
    /// The edges of a cycle, when the anomaly is a cycle.
    pub edges: Vec<WitnessEdge>,
    /// A human-readable story.
    pub story: String,
}

impl Witness {
    /// A witness for a read that went wrong (no cycle).
    pub fn read(read: u64, key: &str, story: String) -> Witness {
        Witness {
            txns: vec![read],
            keys: vec![key.to_owned()],
            edges: Vec::new(),
            story,
        }
    }
}

/// One detected anomaly and its witness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anomaly {
    /// The class.
    pub class: AnomalyClass,
    /// The witness.
    pub witness: Witness,
}
