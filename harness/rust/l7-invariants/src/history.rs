//! The client-side history of a run: one entry per operation, in a documented, versioned
//! format that later suites (the isolation checker, WP-TST-08) read.
//!
//! # Format
//!
//! [`History::write_jsonl`] writes JSON lines. Line 1 is a [`HistoryHeader`]
//! (`{"format":"spot-l7-history","version":1,"seed":..,"workers":..,"profile":..}`), every further
//! line one [`HistoryEntry`]. Times are microseconds since the start of the run, taken on one
//! monotonic clock in the driver: `start_us` before the transaction was opened, `end_us` after
//! `commit` or `rollback` returned, so `[start_us, end_us]` contains the moment the effects became
//! visible. Within one worker, `seq` counts operations from 0. A change to a field's meaning, or
//! removing a field, bumps [`HISTORY_VERSION`]; adding an optional field does not.

use std::collections::BTreeMap;
use std::io::Write;

use serde::{Deserialize, Serialize};
use spot_storage_contract::Isolation;

/// The `format` value of the header line.
pub const HISTORY_FORMAT: &str = "spot-l7-history";

/// The version of the format described in the module documentation.
pub const HISTORY_VERSION: u32 = 1;

/// An isolation level, in a serializable form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    /// `ReadCommitted`.
    ReadCommitted,
    /// `RepeatableRead`.
    RepeatableRead,
    /// `Serializable`.
    Serializable,
}

impl Level {
    /// The contract's isolation level.
    pub fn to_contract(self) -> Isolation {
        match self {
            Level::ReadCommitted => Isolation::ReadCommitted,
            Level::RepeatableRead => Isolation::RepeatableRead,
            Level::Serializable => Isolation::Serializable,
        }
    }
}

/// How an increment avoids losing an update.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncrementMode {
    /// Read without a lock, write with the version read; a concurrent change makes the write
    /// fail with `StaleVersion` or `SerializationFailure`, and the whole increment is retried.
    VersionCheck,
    /// Read with `RowLock::Update(Wait::Block)`, so a concurrent increment waits.
    RowLock,
}

/// What an operation does. The planned parameters are part of the deterministic schedule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OpKind {
    /// Create a party.
    CreateParty,
    /// Create an order with `lines` child rows.
    CreateOrder {
        /// Child rows to create.
        lines: u32,
    },
    /// Create an order with `lines` child rows that links to a known party (`watch`: also sets
    /// the engine-only link).
    CreateLinkedOrder {
        /// Child rows to create.
        lines: u32,
        /// Whether to set the engine-only `watch` link as well.
        watch: bool,
    },
    /// Update a known order with an optimistic version check.
    UpdateOrder,
    /// Delete a known order.
    DeleteOrder,
    /// Try to delete a known party (refused while a database-checked link points at it).
    DeleteParty,
    /// Increment a hot counter document.
    IncrementCounter {
        /// How the increment avoids a lost update.
        mode: IncrementMode,
    },
    /// Allocate one number of a series inside a larger transaction, which `rollback` abandons.
    AllocateSequence {
        /// The series, 0-based.
        series: u32,
        /// Whether the transaction is rolled back after the allocation.
        rollback: bool,
    },
    /// Enqueue `events` outbox events about a known document.
    Emit {
        /// Extra events (besides the transaction's marker event).
        events: u32,
    },
    /// A read-only snapshot check of one known order.
    Read,
}

/// What an operation did to a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Created.
    Created,
    /// Updated.
    Updated,
    /// Deleted.
    Deleted,
    /// Read.
    Read,
}

/// A record an operation touched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Touched {
    /// The entity.
    pub entity: String,
    /// The record id.
    pub id: String,
    /// What was done.
    pub role: Role,
    /// The version written (or read); for a delete, the version deleted.
    pub version: Option<u64>,
}

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// Committed.
    Committed,
    /// Rolled back on purpose.
    RolledBack,
    /// Refused cleanly by the store (stale version, missing record, reference or unique
    /// violation): nothing happened.
    Rejected {
        /// The error code.
        code: String,
    },
    /// Failed. Never counted as a success. `exhausted` is true when the retry budget ran out.
    Failed {
        /// The last error's code.
        code: String,
        /// Whether the retry budget ran out on a retryable error.
        exhausted: bool,
    },
    /// The commit outcome was unknown. `resolved` is filled in by the driver's read-back before
    /// the final check: `Some(true)` it committed, `Some(false)` it did not.
    Indeterminate {
        /// Resolution by read-back.
        resolved: Option<bool>,
    },
}

/// One operation of a run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// The worker, from 0.
    pub worker: u32,
    /// The operation's index within its worker, from 0.
    pub seq: u64,
    /// Microseconds since the run started, before the first attempt opened a transaction.
    pub start_us: u64,
    /// Microseconds since the run started, after the last attempt ended.
    pub end_us: u64,
    /// What it did.
    pub kind: OpKind,
    /// The declared isolation level.
    pub isolation: Level,
    /// How it ended.
    pub outcome: Outcome,
    /// Transactions attempted (1 plus retries).
    pub attempts: u32,
    /// Records touched by the final attempt.
    pub touched: Vec<Touched>,
    /// Numeric values written or read by the final attempt, by name (for example `number` for an
    /// allocation, `value` for a counter, `total` for an order read).
    pub values: BTreeMap<String, i64>,
}

/// The first line of a history file.
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
}

/// Counts of operations by outcome.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeCounts {
    /// Operations in the history.
    pub total: u64,
    /// Committed.
    pub committed: u64,
    /// Rolled back on purpose.
    pub rolled_back: u64,
    /// Refused cleanly.
    pub rejected: u64,
    /// Failed (never counted as success).
    pub failed: u64,
    /// Failed because the retry budget ran out.
    pub exhausted: u64,
    /// Commit outcome unknown.
    pub indeterminate: u64,
    /// Indeterminate and resolved by read-back.
    pub resolved: u64,
}

/// All operations of a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct History {
    /// The header describing the run.
    pub header: HistoryHeader,
    /// The operations, ordered by start time.
    pub entries: Vec<HistoryEntry>,
}

impl History {
    /// An empty history for a run.
    pub fn new(seed: u64, workers: u32, profile: &str) -> History {
        History {
            header: HistoryHeader {
                format: HISTORY_FORMAT.to_owned(),
                version: HISTORY_VERSION,
                seed,
                workers,
                profile: profile.to_owned(),
            },
            entries: Vec::new(),
        }
    }

    /// Counts the entries by outcome.
    pub fn counts(&self) -> OutcomeCounts {
        let mut c = OutcomeCounts::default();
        for e in &self.entries {
            c.total += 1;
            match &e.outcome {
                Outcome::Committed => c.committed += 1,
                Outcome::RolledBack => c.rolled_back += 1,
                Outcome::Rejected { .. } => c.rejected += 1,
                Outcome::Failed { exhausted, .. } => {
                    c.failed += 1;
                    if *exhausted {
                        c.exhausted += 1;
                    }
                }
                Outcome::Indeterminate { resolved } => {
                    c.indeterminate += 1;
                    if resolved.is_some() {
                        c.resolved += 1;
                    }
                }
            }
        }
        c
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

    /// Reads a history written by [`History::write_jsonl`].
    pub fn read_jsonl(text: &str) -> Result<History, String> {
        let mut lines = text.lines().filter(|l| !l.trim().is_empty());
        let header: HistoryHeader = serde_json::from_str(lines.next().ok_or("empty history")?)
            .map_err(|e| format!("bad header: {e}"))?;
        if header.format != HISTORY_FORMAT || header.version != HISTORY_VERSION {
            return Err(format!(
                "unsupported history format {} v{}",
                header.format, header.version
            ));
        }
        let entries = lines
            .map(|l| serde_json::from_str(l).map_err(|e| format!("bad entry: {e}")))
            .collect::<Result<Vec<HistoryEntry>, String>>()?;
        Ok(History { header, entries })
    }
}
