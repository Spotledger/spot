//! The rescue run's write-ahead journal, identity and checkpoint fold.
//!
//! The journal is what makes a rescue resumable. Every record is written before the action it
//! describes, and per-item outcomes are written in the same transaction as the item's effect
//! (ADR-071 section 3). A restart folds the records and finds out what was restored, which items
//! are done, what the last checkpoint was, and whether the run halted.
//!
//! The checkpoint is only an optimisation: correctness comes from the outcome rows, so a kill
//! between an item and the next checkpoint loses no work and repeats none.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::invariants::InvariantReport;
use crate::reentry::OperationKey;

/// The journal's sequence number; strictly increasing.
pub type Seq = u64;

/// The key of one re-entry item (its operation id).
pub type ItemKey = OperationKey;

/// The identity of a rescue run: the ADR-071 `(kind, instance_key)` pair.
///
/// The same snapshot scope must not start twice; a resume reads the identity back out of the
/// journal and refuses a different one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunIdentity {
    /// The run kind, qualified by app or platform (`platform:migration_rescue`).
    pub kind: String,
    /// A hash or canonical text of the identifying parameters (site, snapshot time).
    pub instance_key: String,
    /// The kind's declared version at the start.
    pub kind_version: u32,
}

impl RunIdentity {
    /// Validates and builds an identity.
    ///
    /// # Errors
    /// [`Error::InvalidInput`] when the kind or instance key is empty.
    pub fn new(
        kind: impl Into<String>,
        instance_key: impl Into<String>,
        kind_version: u32,
    ) -> Result<Self> {
        let kind = kind.into();
        let instance_key = instance_key.into();
        if kind.trim().is_empty() {
            return Err(Error::InvalidInput {
                what: "run kind".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        if instance_key.trim().is_empty() {
            return Err(Error::InvalidInput {
                what: "instance key".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        Ok(Self {
            kind,
            instance_key,
            kind_version,
        })
    }
}

/// What happened to one item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum ItemOutcome {
    /// The old engine accepted and posted it.
    Done,
    /// The old engine deliberately skipped it, with a stable reason.
    Skipped {
        /// The stable reason code.
        code: String,
        /// A secret-free explanation.
        detail: String,
    },
    /// The old engine rejected it; the run stops and a person decides.
    Failed {
        /// The stable reason code.
        code: String,
        /// A secret-free explanation.
        detail: String,
    },
}

impl ItemOutcome {
    /// A skipped outcome.
    #[must_use]
    pub fn skipped(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Skipped {
            code: code.into(),
            detail: detail.into(),
        }
    }

    /// A failed outcome.
    #[must_use]
    pub fn failed(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::Failed {
            code: code.into(),
            detail: detail.into(),
        }
    }

    /// A short, stable label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Skipped { .. } => "skipped",
            Self::Failed { .. } => "failed",
        }
    }

    /// Whether the item is finished successfully (done or skipped).
    #[must_use]
    pub fn is_success(&self) -> bool {
        !matches!(self, Self::Failed { .. })
    }
}

/// One journal record with its sequence number and time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The sequence number, assigned by the caller and strictly increasing.
    pub seq: Seq,
    /// Unix microseconds; the engine reads no clock.
    pub at: i64,
    /// What was being attempted.
    pub record: JournalRecord,
}

/// One write-ahead record of a rescue run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "record")]
pub enum JournalRecord {
    /// The run started for this identity, snapshot and time.
    Started {
        /// The run id.
        run_id: String,
        /// The site the run belongs to.
        site: String,
        /// The ADR-071 identity.
        identity: RunIdentity,
        /// The snapshot being restored.
        snapshot: String,
        /// The snapshot time.
        snapshot_at: i64,
        /// The attempt counter (starts at 1).
        attempt: u32,
        /// The id of the two-person restore ticket that authorised the run.
        #[serde(default)]
        ticket_id: String,
        /// The two approvers the ticket names.
        #[serde(default)]
        approvers: Vec<String>,
    },
    /// A restart took the unfinished run over as a new attempt.
    Resumed {
        /// The attempt counter, one more than the last attempt.
        attempt: u32,
    },
    /// The maintenance gate is about to close.
    GateClosing,
    /// The maintenance gate is closed.
    GateClosed,
    /// The snapshot restore is about to run.
    RestoreStarted {
        /// The snapshot being restored.
        snapshot: String,
    },
    /// The snapshot was restored.
    Restored {
        /// The snapshot.
        snapshot: String,
        /// Where the data landed (never a secret).
        location: String,
        /// Unix microseconds.
        at: i64,
    },
    /// One item committed its outcome.
    ItemOutcome {
        /// The item key.
        key: ItemKey,
        /// What happened.
        outcome: ItemOutcome,
    },
    /// A bounded batch finished and its cursor was recorded.
    Checkpoint {
        /// How many items have outcomes.
        processed: u64,
        /// How many are done.
        done: u64,
        /// How many were skipped.
        skipped: u64,
        /// How many failed.
        failed: u64,
        /// The last item key in the batch.
        last_key: Option<ItemKey>,
    },
    /// An invariant pass finished.
    InvariantsChecked {
        /// The pass result.
        report: InvariantReport,
    },
    /// The gate opened; users may return.
    GateOpened,
    /// The run stopped and needs a person.
    Halted {
        /// The stable reason code.
        code: String,
        /// A secret-free reason.
        reason: String,
    },
}

/// The write-ahead journal of one site's rescue runs.
pub trait RescueJournal: Send + Sync {
    /// Appends `record` under `seq`.
    ///
    /// # Errors
    /// A backend error when the journal cannot be written.
    fn append(&self, seq: Seq, record: JournalRecord, at: i64) -> BoxFuture<'_, Result<()>>;

    /// Every record, in sequence order.
    ///
    /// # Errors
    /// A backend error when the journal cannot be read.
    fn entries(&self) -> BoxFuture<'_, Result<Vec<JournalEntry>>>;
}

/// The reference in-memory journal: one `Vec`, one counter, no I/O.
#[derive(Debug, Default)]
pub struct InMemoryRescueJournal {
    entries: Mutex<Vec<JournalEntry>>,
    next: AtomicU64,
}

impl InMemoryRescueJournal {
    /// An empty journal whose next sequence number is `1`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        }
    }

    /// The entries written so far, for a test to inspect.
    #[must_use]
    pub fn written(&self) -> Vec<JournalEntry> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl RescueJournal for InMemoryRescueJournal {
    fn append(&self, seq: Seq, record: JournalRecord, at: i64) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let expected = self.next.load(Ordering::SeqCst);
            if seq != expected {
                return Err(Error::JournalFailed {
                    message: format!("sequence {seq} is not the next one ({expected})"),
                });
            }
            self.entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(JournalEntry { seq, at, record });
            self.next.store(seq + 1, Ordering::SeqCst);
            Ok(())
        })
    }

    fn entries(&self) -> BoxFuture<'_, Result<Vec<JournalEntry>>> {
        Box::pin(async move {
            Ok(self
                .entries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone())
        })
    }
}

/// The fold of a rescue journal: everything a restart needs to know.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunProgress {
    /// Whether a run ever started.
    pub started: bool,
    /// The run id.
    pub run_id: Option<String>,
    /// The ADR-071 identity.
    pub identity: Option<RunIdentity>,
    /// The snapshot.
    pub snapshot: Option<String>,
    /// The snapshot time.
    pub snapshot_at: Option<i64>,
    /// The attempt counter.
    pub attempt: u32,
    /// The gate was closed.
    pub gate_closed: bool,
    /// The snapshot was restored.
    pub restored: Option<(String, String, i64)>,
    /// Item outcomes by key.
    pub outcomes: BTreeMap<ItemKey, ItemOutcome>,
    /// The last checkpoint.
    pub checkpoint: Option<JournalRecord>,
    /// Every completed invariant pass.
    pub invariants: Vec<InvariantReport>,
    /// The gate opened.
    pub gate_open: bool,
    /// Why the run halted.
    pub halted: Option<(String, String)>,
    /// The time of the last record.
    pub last_at: i64,
    /// The sequence number of the last record (`0` when empty).
    pub last_seq: Seq,
}

impl RunProgress {
    /// Folds `entries` into a [`RunProgress`].
    #[must_use]
    pub fn fold(entries: Vec<JournalEntry>) -> Self {
        let mut progress = Self::default();
        for entry in entries {
            progress.last_at = entry.at;
            progress.last_seq = entry.seq;
            match entry.record {
                JournalRecord::Started {
                    run_id,
                    site: _,
                    identity,
                    snapshot,
                    snapshot_at,
                    attempt,
                    ticket_id: _,
                    approvers: _,
                } => {
                    // A new run resets every run-scoped fact, so one journal can hold several
                    // attempts and a restart reads only the last one.
                    progress = Self {
                        started: true,
                        run_id: Some(run_id),
                        identity: Some(identity),
                        snapshot: Some(snapshot),
                        snapshot_at: Some(snapshot_at),
                        attempt,
                        last_at: entry.at,
                        last_seq: entry.seq,
                        ..Self::default()
                    };
                    continue;
                }
                JournalRecord::Resumed { attempt } => {
                    progress.attempt = attempt;
                }
                JournalRecord::GateClosing | JournalRecord::GateClosed => {
                    progress.gate_closed = true;
                }
                JournalRecord::RestoreStarted { .. } => {}
                JournalRecord::Restored {
                    snapshot,
                    location,
                    at,
                } => progress.restored = Some((snapshot, location, at)),
                JournalRecord::ItemOutcome { key, outcome } => {
                    progress.outcomes.insert(key, outcome);
                }
                JournalRecord::Checkpoint { .. } => progress.checkpoint = Some(entry.record),
                JournalRecord::InvariantsChecked { report } => {
                    progress.invariants.push(report);
                }
                JournalRecord::GateOpened => progress.gate_open = true,
                JournalRecord::Halted { code, reason } => progress.halted = Some((code, reason)),
            }
        }
        progress
    }

    /// Whether a run is unfinished: it started and neither opened the site nor halted.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.started && !self.gate_open && self.halted.is_none()
    }

    /// The outcome of `key`, if it has one.
    #[must_use]
    pub fn outcome(&self, key: &ItemKey) -> Option<&ItemOutcome> {
        self.outcomes.get(key)
    }

    /// How many outcomes are done.
    #[must_use]
    pub fn done(&self) -> u64 {
        self.outcomes
            .values()
            .filter(|outcome| matches!(outcome, ItemOutcome::Done))
            .count() as u64
    }

    /// How many outcomes were skipped.
    #[must_use]
    pub fn skipped(&self) -> u64 {
        self.outcomes
            .values()
            .filter(|outcome| matches!(outcome, ItemOutcome::Skipped { .. }))
            .count() as u64
    }

    /// How many outcomes failed.
    #[must_use]
    pub fn failed(&self) -> u64 {
        self.outcomes
            .values()
            .filter(|outcome| matches!(outcome, ItemOutcome::Failed { .. }))
            .count() as u64
    }

    /// The next sequence number to write.
    #[must_use]
    pub fn next_seq(&self) -> Seq {
        self.last_seq + 1
    }
}
