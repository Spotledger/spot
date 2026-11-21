//! Live progress, as a plain trait.
//!
//! The engine reports every transition through a [`ProgressSink`]. The trait is deliberately
//! small and synchronous; wiring it to `spot-realtime` rooms is T2, and tests use
//! [`RecordingProgress`] to assert the sequence.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::state::{RestoreCause, RestoreMechanism, Version};

/// One progress event. Times are unix microseconds the caller supplied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "event")]
pub enum ProgressEvent {
    /// The run started.
    Started {
        /// The plan digest.
        plan_digest: String,
        /// Unix microseconds.
        at: i64,
    },
    /// The maintenance gate is about to close.
    GateClosing {
        /// Why.
        reason: String,
        /// Unix microseconds.
        at: i64,
    },
    /// The maintenance gate is closed.
    GateClosed {
        /// Unix microseconds.
        at: i64,
    },
    /// The side-effect hold began.
    HoldBegun {
        /// The migration id.
        migration_id: String,
        /// Unix microseconds.
        at: i64,
    },
    /// A step started.
    StepStarted {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// Unix microseconds.
        at: i64,
    },
    /// A step completed.
    StepCompleted {
        /// The step's index.
        index: usize,
        /// The step's id.
        id: String,
        /// Unix microseconds.
        at: i64,
    },
    /// Verification started.
    VerifyStarted {
        /// Unix microseconds.
        at: i64,
    },
    /// Verification passed.
    VerifyPassed {
        /// Unix microseconds.
        at: i64,
    },
    /// Verification failed.
    VerifyFailed {
        /// How many residual differences were found.
        mismatches: u64,
        /// How many invariant violations were found.
        invariants: u64,
        /// Unix microseconds.
        at: i64,
    },
    /// A rollback started.
    RestoreStarted {
        /// Why.
        cause: RestoreCause,
        /// Unix microseconds.
        at: i64,
    },
    /// A rollback finished.
    Restored {
        /// How it was achieved.
        mechanism: RestoreMechanism,
        /// Unix microseconds.
        at: i64,
    },
    /// The held effects were released.
    HoldReleased {
        /// Unix microseconds.
        at: i64,
    },
    /// The held effects were discarded.
    HoldDiscarded {
        /// Why.
        reason: String,
        /// Unix microseconds.
        at: i64,
    },
    /// The maintenance gate opened.
    Opened {
        /// Which version is live.
        version: Version,
        /// Whether verification passed.
        verified: bool,
        /// Unix microseconds.
        at: i64,
    },
    /// The run halted and needs a person.
    Halted {
        /// Why.
        reason: String,
        /// Unix microseconds.
        at: i64,
    },
    /// A restart resumed an unfinished run.
    Resumed {
        /// The first step that still had to run.
        from_step: usize,
        /// Unix microseconds.
        at: i64,
    },
}

/// The host's view of progress. Implemented by the caller; the engine only calls it.
pub trait ProgressSink: Send + Sync {
    /// Called for every event. Must be quick; long work belongs in the host.
    fn on(&self, event: ProgressEvent);
}

/// A sink that drops every event.
#[derive(Debug, Default)]
pub struct SilentProgress;

impl ProgressSink for SilentProgress {
    fn on(&self, _event: ProgressEvent) {}
}

/// A sink that records every event, for tests and for a host that wants a transcript.
#[derive(Debug, Default)]
pub struct RecordingProgress {
    events: Mutex<Vec<ProgressEvent>>,
}

impl RecordingProgress {
    /// An empty transcript.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The events recorded so far.
    #[must_use]
    pub fn events(&self) -> Vec<ProgressEvent> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// The event tags recorded, in order (for concise assertions).
    #[must_use]
    pub fn tags(&self) -> Vec<&'static str> {
        self.events()
            .iter()
            .map(|event| match event {
                ProgressEvent::Started { .. } => "started",
                ProgressEvent::GateClosing { .. } => "gate_closing",
                ProgressEvent::GateClosed { .. } => "gate_closed",
                ProgressEvent::HoldBegun { .. } => "hold_begun",
                ProgressEvent::StepStarted { .. } => "step_started",
                ProgressEvent::StepCompleted { .. } => "step_completed",
                ProgressEvent::VerifyStarted { .. } => "verify_started",
                ProgressEvent::VerifyPassed { .. } => "verify_passed",
                ProgressEvent::VerifyFailed { .. } => "verify_failed",
                ProgressEvent::RestoreStarted { .. } => "restore_started",
                ProgressEvent::Restored { .. } => "restored",
                ProgressEvent::HoldReleased { .. } => "hold_released",
                ProgressEvent::HoldDiscarded { .. } => "hold_discarded",
                ProgressEvent::Opened { .. } => "opened",
                ProgressEvent::Halted { .. } => "halted",
                ProgressEvent::Resumed { .. } => "resumed",
            })
            .collect()
    }
}

impl ProgressSink for RecordingProgress {
    fn on(&self, event: ProgressEvent) {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(event);
    }
}
