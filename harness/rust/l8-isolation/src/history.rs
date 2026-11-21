//! The transactional history the checker reads, in a documented, versioned format.
//!
//! # Relationship to the L7 history
//!
//! This format **extends** the one `spot-l7-invariants` (WP-TST-07.T1) documents in its
//! `history` module; it does not fork it. [`write_jsonl`](TxnHistory::write_jsonl) writes JSON
//! lines: line 1 is a [`HistoryHeader`], every further line one [`TxnEntry`]. The header keeps
//! the L7 header's fields (`format`, `version`, `seed`, `workers`, `profile`) and adds the
//! [`workload`](Workload) and the key-space size; every entry keeps the L7 entry's `worker`,
//! `seq`, `start_us`, `end_us`, `isolation`, `outcome` and `attempts` fields — reusing
//! [`Level`] and [`Outcome`] from `spot-l7-invariants` verbatim, so the isolation level of a
//! transaction serialises identically in both formats — and adds the transaction's `ops`, which
//! only an isolation workload has. Times are microseconds on one monotonic clock:
//! `[start_us, end_us]` contains the moment the transaction's effects became visible.
//!
//! # Operations
//!
//! A transaction is a bundle of micro-operations, in order:
//! - `append(key, value)` and `write(key, value)` write a **unique** value to a key, so a
//!   read's result pins down which writes it observed.
//! - `read(key)` records what it observed: the full list of a list-append key, or the value of
//!   an rw-register key.
//!
//! # Versioning
//!
//! A change to a field's meaning, or removing a field, bumps [`HISTORY_VERSION`]; adding an
//! optional field does not (the L7 rule).

use std::collections::BTreeMap;
use std::io::Write;

use serde::{Deserialize, Serialize};
use spot_l7_invariants::history::{Level, Outcome};

/// The `format` value of the header line.
pub const HISTORY_FORMAT: &str = "spot-l8-history";

/// The version of the format described in the module documentation.
pub const HISTORY_VERSION: u32 = 1;

pub use spot_l7_invariants::history::Level as IsolationLevel;

/// Which micro-operation workload a history came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Workload {
    /// Each write appends a unique value to a key's list; a read returns the whole list.
    ListAppend,
    /// Each write sets a key's register to a unique value; a read returns the value.
    RwRegister,
}

impl Workload {
    /// The name used on the command line and in reports.
    pub fn name(self) -> &'static str {
        match self {
            Workload::ListAppend => "list-append",
            Workload::RwRegister => "rw-register",
        }
    }

    /// Parses a workload name.
    pub fn parse(s: &str) -> Option<Workload> {
        match s {
            "list-append" => Some(Workload::ListAppend),
            "rw-register" => Some(Workload::RwRegister),
            _ => None,
        }
    }
}

/// What a read observed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observed {
    /// A list-append read: every value of the key, in order, at the moment of the read.
    List(Vec<String>),
    /// An rw-register read: the value the key held, or `None` if it held no value.
    Register(Option<String>),
}

/// One micro-operation of a transaction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TxnOp {
    /// Append `value` to `key`'s list (list-append workload).
    Append {
        /// The key.
        key: String,
        /// The unique value appended.
        value: String,
    },
    /// Set `key`'s register to `value` (rw-register workload).
    Write {
        /// The key.
        key: String,
        /// The unique value written.
        value: String,
    },
    /// Read `key` and record what was observed.
    Read {
        /// The key.
        key: String,
        /// The observed state.
        observed: Observed,
    },
}

impl TxnOp {
    /// The key this operation touches.
    pub fn key(&self) -> &str {
        match self {
            TxnOp::Append { key, .. } | TxnOp::Write { key, .. } | TxnOp::Read { key, .. } => key,
        }
    }

    /// The value this operation writes, if it writes one.
    pub fn written(&self) -> Option<&str> {
        match self {
            TxnOp::Append { value, .. } | TxnOp::Write { value, .. } => Some(value),
            TxnOp::Read { .. } => None,
        }
    }
}

/// One transaction of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TxnEntry {
    /// A number unique to this transaction within the run, assigned in start order.
    pub txn: u64,
    /// The worker that ran it, from 0.
    pub worker: u32,
    /// The transaction's index within its worker, from 0.
    pub seq: u64,
    /// Microseconds since the run started, before the first attempt opened a transaction.
    pub start_us: u64,
    /// Microseconds since the run started, after the last attempt ended.
    pub end_us: u64,
    /// The declared isolation level.
    pub isolation: Level,
    /// How the transaction ended.
    pub outcome: Outcome,
    /// Transactions attempted (1 plus retries).
    pub attempts: u32,
    /// The micro-operations, in order.
    pub ops: Vec<TxnOp>,
}

impl TxnEntry {
    /// Whether the transaction's effects are part of the history: it committed, or its commit
    /// outcome was unknown and a read-back resolved it as committed.
    pub fn is_committed(&self) -> bool {
        match &self.outcome {
            Outcome::Committed => true,
            Outcome::Indeterminate { resolved } => *resolved == Some(true),
            _ => false,
        }
    }

    /// Whether the history knows the transaction did **not** commit. A clean refusal
    /// ([`Outcome::Rejected`]) is one: the store refused and nothing happened.
    pub fn is_aborted(&self) -> bool {
        match &self.outcome {
            Outcome::RolledBack | Outcome::Failed { .. } | Outcome::Rejected { .. } => true,
            Outcome::Indeterminate { resolved } => *resolved == Some(false),
            _ => false,
        }
    }

    /// Whether the outcome is still unknown (an unresolved indeterminate commit).
    pub fn is_unknown(&self) -> bool {
        matches!(&self.outcome, Outcome::Indeterminate { resolved: None })
    }
}

/// The first line of a history file: the L7 header plus the workload and key count.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryHeader {
    /// Always [`HISTORY_FORMAT`].
    pub format: String,
    /// The format version.
    pub version: u32,
    /// The run's seed.
    pub seed: u64,
    /// Number of workers.
    pub workers: u32,
    /// Profile name.
    pub profile: String,
    /// The workload the history came from.
    pub workload: Workload,
    /// The key-space size.
    pub keys: u32,
}

/// All transactions of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TxnHistory {
    /// The header describing the run.
    pub header: HistoryHeader,
    /// The transactions, ordered by start time.
    pub entries: Vec<TxnEntry>,
}

impl TxnHistory {
    /// An empty history for a run.
    pub fn new(seed: u64, workers: u32, profile: &str, workload: Workload, keys: u32) -> Self {
        TxnHistory {
            header: HistoryHeader {
                format: HISTORY_FORMAT.to_owned(),
                version: HISTORY_VERSION,
                seed,
                workers,
                profile: profile.to_owned(),
                workload,
                keys,
            },
            entries: Vec::new(),
        }
    }

    /// Counts entries by outcome into `(committed, aborted, unknown)`.
    pub fn outcome_counts(&self) -> (u64, u64, u64) {
        let mut committed = 0;
        let mut aborted = 0;
        let mut unknown = 0;
        for e in &self.entries {
            if e.is_committed() {
                committed += 1;
            } else if e.is_aborted() {
                aborted += 1;
            } else {
                unknown += 1;
            }
        }
        (committed, aborted, unknown)
    }

    /// The distinct keys, in sorted order.
    pub fn keys(&self) -> Vec<String> {
        let mut set: BTreeMap<String, ()> = BTreeMap::new();
        for e in &self.entries {
            for op in &e.ops {
                set.insert(op.key().to_owned(), ());
            }
        }
        set.into_keys().collect()
    }

    /// Writes the header and every entry as JSON lines.
    pub fn write_jsonl<W: Write>(&self, out: &mut W) -> std::io::Result<()> {
        let line = |v: Result<String, serde_json::Error>| {
            v.map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        };
        writeln!(out, "{}", line(serde_json::to_string(&self.header))?)?;
        for entry in &self.entries {
            writeln!(out, "{}", line(serde_json::to_string(entry))?)?;
        }
        Ok(())
    }

    /// Reads a history written by [`TxnHistory::write_jsonl`].
    pub fn read_jsonl(text: &str) -> Result<TxnHistory, String> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: HistoryHeader = serde_json::from_str(lines.next().ok_or("empty history")?)
            .map_err(|e| format!("bad header: {e}"))?;
        if header.format != HISTORY_FORMAT || header.version != HISTORY_VERSION {
            return Err(format!(
                "unsupported history format {} v{} (want {HISTORY_FORMAT} v{HISTORY_VERSION})",
                header.format, header.version
            ));
        }
        let entries = lines
            .map(|l| serde_json::from_str(l).map_err(|e| format!("bad entry: {e}")))
            .collect::<Result<Vec<TxnEntry>, String>>()?;
        Ok(TxnHistory { header, entries })
    }
}
