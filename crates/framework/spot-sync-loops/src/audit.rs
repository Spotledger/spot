//! The audit of automatic responses: `sys_sync_decision` (WP-RTB-02.T11).
//!
//! A loop step that **does** something on its own (re-sends a bundle, asks for a missing record,
//! stamps a class stale, suspends a node) leaves one [`Decision`]: which step, what it did, to
//! which subject (an identity, never content) and when. A step that only raises a condition
//! records nothing here: a condition is its own record. Nothing in a decision is a record's
//! content, a key, a code or a payload.

use std::sync::{Arc, Mutex, PoisonError};

/// The event topic a decision is stored under.
pub const SYS_SYNC_DECISION: &str = "sys_sync_decision";

/// One automatic response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The step that responded.
    pub step: &'static str,
    /// What it did (`stamp_stale`, `heal`, `repair_pull`, ...): a fixed word.
    pub action: &'static str,
    /// The subject: a class, a node, an obligation, a bundle first position.
    pub subject: String,
    /// When, Unix seconds.
    pub at: i64,
    /// Whether the response worked.
    pub ok: bool,
}

/// Where decisions are written (the events outbox of the site).
pub trait DecisionSink: Send {
    /// Records one decision. A sink that cannot store it must not panic.
    fn record(&mut self, decision: &Decision);
}

/// A decision sink shared by the steps of one runner.
#[derive(Clone)]
pub struct SharedDecisions(Arc<Mutex<Box<dyn DecisionSink>>>);

impl SharedDecisions {
    /// Shares `sink`.
    #[must_use]
    pub fn new(sink: Box<dyn DecisionSink>) -> Self {
        Self(Arc::new(Mutex::new(sink)))
    }

    /// Records `decision`.
    pub fn record(&self, decision: &Decision) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(decision);
    }
}

impl std::fmt::Debug for SharedDecisions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedDecisions").finish_non_exhaustive()
    }
}

/// A sink that remembers what it was given, for tests and for the health view.
#[derive(Clone, Default)]
pub struct MemoryDecisions(Arc<Mutex<Vec<Decision>>>);

impl MemoryDecisions {
    /// An empty memory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// What was recorded, oldest first.
    #[must_use]
    pub fn all(&self) -> Vec<Decision> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl DecisionSink for MemoryDecisions {
    fn record(&mut self, decision: &Decision) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(decision.clone());
    }
}
