//! The write-ahead journal and the fold that reads a restart's intent.
//!
//! The engine writes one [`JournalRecord`] **before** it performs each transition or step, so a
//! restart can always tell what was being attempted. [`ApplyJournal`] is the seam; the
//! in-memory [`InMemoryJournal`] is the reference and a database-backed journal (T2) implements
//! the same trait. [`Progress`] is the fold of the records: it answers what completed, what was
//! in flight, whether the site was opened and whether the run halted.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::error::{Error, Result};
use crate::state::{RestoreCause, RestoreMechanism, Version};
use crate::step::{Idempotency, RestorePoint};

/// The journal's sequence number; strictly increasing.
pub type Seq = u64;

/// One journal record with its sequence number and the time it was written.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The sequence number, assigned by the caller and strictly increasing.
    pub seq: Seq,
    /// Unix microseconds; the engine reads no clock.
    pub at: i64,
    /// What was being attempted.
    pub record: JournalRecord,
}

/// One write-ahead record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "record")]
pub enum JournalRecord {
    /// The run started for this plan, snapshot and version pair.
    Started {
        /// The plan digest.
        plan_digest: String,
        /// The old metadata version.
        from_version: u64,
        /// The new metadata version.
        to_version: u64,
        /// The snapshot the run restores from.
        snapshot: String,
    },
    /// The maintenance gate is about to close.
    GateClosing {
        /// Why the site is closed.
        reason: String,
    },
    /// The maintenance gate is closed.
    GateClosed,
    /// The side-effect hold is about to begin.
    HoldBeginning {
        /// The migration id.
        migration_id: String,
    },
    /// The side-effect hold has begun.
    HoldBegun,
    /// A step is about to run.
    StepStarted {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// Whether the step is transactional.
        transactional: bool,
        /// The declared idempotency (`None` means undeclared).
        idempotency: Option<Idempotency>,
        /// The compensating restore point for a non-transactional step.
        restore_point: Option<RestorePoint>,
    },
    /// A step completed.
    StepCompleted {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// The checkpoint the step recorded, if any.
        checkpoint: Option<String>,
    },
    /// A step failed.
    StepFailed {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// What the runner reported.
        reason: String,
    },
    /// Verification is about to run.
    VerifyStarted,
    /// Verification passed.
    Verified,
    /// Verification failed.
    VerifyFailed {
        /// How many residual differences the plan re-diff found.
        mismatches: u64,
        /// How many invariant violations were found.
        invariants: u64,
    },
    /// A rollback is about to run.
    RestoreStarted {
        /// Why.
        cause: RestoreCause,
        /// How the rollback will be achieved.
        mechanism: RestoreMechanism,
        /// The compensating restore point, for a checkpoint rollback.
        restore_point: Option<RestorePoint>,
    },
    /// A rollback finished.
    Restored {
        /// How it was achieved.
        mechanism: RestoreMechanism,
    },
    /// The held effects were released.
    HoldReleased,
    /// The held effects were discarded.
    HoldDiscarded {
        /// Why.
        reason: String,
    },
    /// The maintenance gate opened.
    GateOpened {
        /// Which version is live.
        version: Version,
        /// Whether verification passed.
        verified: bool,
    },
    /// The run halted and needs a person.
    Halted {
        /// Why.
        reason: String,
    },
}

/// The write-ahead journal of one site's apply runs.
///
/// `append` is given the sequence number so the engine's fault injection can name a boundary
/// deterministically; a backend rejects a number that is not the next one.
pub trait ApplyJournal: Send + Sync {
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
pub struct InMemoryJournal {
    entries: Mutex<Vec<JournalEntry>>,
    next: AtomicU64,
}

impl InMemoryJournal {
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

impl ApplyJournal for InMemoryJournal {
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

/// The step that was in flight when the journal ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepStart {
    /// The step's index.
    pub index: usize,
    /// The step's id.
    pub id: String,
    /// Whether the step is transactional.
    pub transactional: bool,
    /// The declared idempotency.
    pub idempotency: Option<Idempotency>,
    /// The compensating restore point.
    pub restore_point: Option<RestorePoint>,
}

/// The fold of a journal: what the last run intended and how far it got.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    /// Whether a run ever started.
    pub started: bool,
    /// The plan digest the run is for.
    pub plan_digest: Option<String>,
    /// The snapshot the run restores from.
    pub snapshot: Option<String>,
    /// The old metadata version.
    pub from_version: u64,
    /// The new metadata version.
    pub to_version: u64,
    /// Completed steps, by index, with the checkpoint each recorded.
    pub completed: BTreeMap<usize, Option<String>>,
    /// The step in flight when the journal ended.
    pub started_step: Option<StepStart>,
    /// Whether verification started.
    pub verify_started: bool,
    /// Whether verification passed.
    pub verified: bool,
    /// Whether verification failed.
    pub verify_failed: bool,
    /// Whether the gate was closed.
    pub gate_closed: bool,
    /// Whether the side-effect hold began.
    pub hold_begun: bool,
    /// Whether the hold was released or discarded.
    pub hold_finished: bool,
    /// The rollback cause, when one started.
    pub restore_cause: Option<RestoreCause>,
    /// The rollback mechanism and restore point, when one started.
    pub restore_started: Option<(RestoreMechanism, Option<RestorePoint>)>,
    /// Whether a rollback finished.
    pub restore_done: bool,
    /// The version the gate was opened with, when it was.
    pub gate_open: Option<(Version, bool)>,
    /// Why the run halted, when it did.
    pub halted: Option<String>,
    /// The time of the last record.
    pub last_at: i64,
    /// The sequence number of the last record (`0` when empty).
    pub last_seq: Seq,
}

impl Progress {
    /// Folds `entries` into a [`Progress`].
    #[must_use]
    pub fn fold(entries: Vec<JournalEntry>) -> Self {
        let mut progress = Progress::default();
        for entry in entries {
            progress.last_at = entry.at;
            progress.last_seq = entry.seq;
            match entry.record {
                JournalRecord::Started {
                    plan_digest,
                    from_version,
                    to_version,
                    snapshot,
                } => {
                    // A new run resets every run-scoped fact, so one journal can hold several
                    // finished runs and a restart reads only the last one.
                    progress = Progress {
                        started: true,
                        plan_digest: Some(plan_digest),
                        snapshot: Some(snapshot),
                        from_version,
                        to_version,
                        last_at: entry.at,
                        last_seq: entry.seq,
                        ..Progress::default()
                    };
                    continue;
                }
                JournalRecord::GateClosing { .. } | JournalRecord::GateClosed => {
                    progress.gate_closed = true;
                }
                JournalRecord::HoldBeginning { .. } | JournalRecord::HoldBegun => {
                    progress.hold_begun = true;
                }
                JournalRecord::StepStarted {
                    index,
                    id,
                    transactional,
                    idempotency,
                    restore_point,
                } => {
                    progress.started_step = Some(StepStart {
                        index,
                        id,
                        transactional,
                        idempotency,
                        restore_point,
                    });
                }
                JournalRecord::StepCompleted {
                    index, checkpoint, ..
                } => {
                    progress.completed.insert(index, checkpoint);
                    if progress
                        .started_step
                        .as_ref()
                        .is_some_and(|start| start.index == index)
                    {
                        progress.started_step = None;
                    }
                }
                JournalRecord::StepFailed { index, .. } => {
                    if progress
                        .started_step
                        .as_ref()
                        .is_some_and(|start| start.index == index)
                    {
                        progress.started_step = None;
                    }
                }
                JournalRecord::VerifyStarted => progress.verify_started = true,
                JournalRecord::Verified => progress.verified = true,
                JournalRecord::VerifyFailed { .. } => progress.verify_failed = true,
                JournalRecord::RestoreStarted {
                    cause,
                    mechanism,
                    restore_point,
                } => {
                    progress.restore_cause = Some(cause);
                    progress.restore_started = Some((mechanism, restore_point));
                    progress.restore_done = false;
                }
                JournalRecord::Restored { .. } => progress.restore_done = true,
                JournalRecord::HoldReleased | JournalRecord::HoldDiscarded { .. } => {
                    progress.hold_finished = true;
                }
                JournalRecord::GateOpened { version, verified } => {
                    progress.gate_open = Some((version, verified));
                }
                JournalRecord::Halted { reason } => progress.halted = Some(reason),
            }
        }
        progress
    }

    /// Whether a run is unfinished: it started and neither opened the site nor halted.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.started && self.gate_open.is_none() && self.halted.is_none()
    }
}
