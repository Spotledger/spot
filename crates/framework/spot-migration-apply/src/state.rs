//! The apply state machine, its end states and the rollback vocabulary.
//!
//! The happy path is `Idle -> Gated -> Applying(step n) -> Verifying -> Open`. The failure edges
//! are `-> Restoring -> Open(old)` (the snapshot was restored) and `-> Halted(needs person)` (the
//! site stays closed). Every transition is written to the journal **before** it happens, so a
//! restart can read what was being attempted.

use serde::{Deserialize, Serialize};

/// Which version of the site is live.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Version {
    /// The version the site ran before the apply.
    Old,
    /// The version the plan moved the site to.
    New,
}

/// The state of one apply run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ApplyState {
    /// Nothing is running and the site is open.
    Idle,
    /// The maintenance gate is closed and the run is about to start.
    Gated {
        /// Why the site is closed.
        reason: String,
        /// Unix microseconds the gate closed at.
        since: i64,
    },
    /// A step is running.
    Applying {
        /// The zero-based index of the step in progress.
        step: usize,
        /// How many steps the plan has.
        total: usize,
    },
    /// Every step finished and verification is running.
    Verifying,
    /// The run is rolling back.
    Restoring {
        /// Why the run is rolling back.
        cause: RestoreCause,
    },
    /// Users are admitted.
    Open {
        /// Which version is live.
        version: Version,
        /// Whether verification passed. `New` is never open unverified.
        verified: bool,
    },
    /// The run stopped and needs a person; the site stays closed.
    Halted {
        /// Why the run halted.
        reason: String,
    },
}

impl ApplyState {
    /// Whether the site admits users: open, or idle with no run in progress.
    #[must_use]
    pub fn admits_users(&self) -> bool {
        matches!(self, Self::Idle | Self::Open { .. })
    }

    /// Whether the run is finished (open, halted or never started).
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self, Self::Idle | Self::Open { .. } | Self::Halted { .. })
    }

    /// A short, stable label for a log line.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Gated { .. } => "gated",
            Self::Applying { .. } => "applying",
            Self::Verifying => "verifying",
            Self::Restoring { .. } => "restoring",
            Self::Open { .. } => "open",
            Self::Halted { .. } => "halted",
        }
    }
}

/// Why a run is rolling back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "cause")]
pub enum RestoreCause {
    /// A step failed; the run rolls back to the old version.
    StepFailed {
        /// The step's index in the plan.
        index: usize,
    },
    /// Verification failed; the run rolls back to the old version.
    VerifyFailed,
    /// A restart found an unfinished run.
    Recovery,
}

/// How a rollback was achieved.
///
/// This is the rollback table of ADR-22: during apply a transactional step is undone by its own
/// transaction aborting, and a non-transactional step is undone from its recorded compensating
/// restore point; after apply, before users return, the snapshot is restored.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreMechanism {
    /// The step's transaction was aborted; nothing was committed.
    TransactionAbort,
    /// The step was undone from its recorded compensating restore point.
    Checkpoint,
    /// The whole site was restored from the snapshot.
    Snapshot,
}

impl RestoreMechanism {
    /// A short, stable label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::TransactionAbort => "transaction_abort",
            Self::Checkpoint => "checkpoint",
            Self::Snapshot => "snapshot",
        }
    }
}

/// What a restart did when it found the journal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "action")]
pub enum RecoveryAction {
    /// The journal was empty; nothing had started.
    NotStarted,
    /// The unfinished run was resumed from `from_step`.
    Resumed {
        /// The first step that still had to run.
        from_step: usize,
    },
    /// The unfinished run was rolled back and the old version is live.
    Restored,
    /// The journal said the site was already open.
    AlreadyOpen {
        /// Which version is live.
        version: Version,
        /// Whether verification passed.
        verified: bool,
    },
    /// The journal said the run had halted.
    Halted,
}

/// The outcome of [`crate::ApplyEngine::recover`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryOutcome {
    /// What the restart did.
    pub action: RecoveryAction,
    /// The state the engine is in afterwards.
    pub state: ApplyState,
}
