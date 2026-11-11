//! The per-step audit trail.
//!
//! The journal is the resumable state of a deployment; the audit is the evidence of who ran
//! what. Every step writes one entry **before** it starts and one when it ends, through an
//! [`AuditSink`] the management layer implements over its tamper-evident audit chain. If the
//! start entry cannot be written the step does not run.
//!
//! An entry names the plan, the step, the operation, the actor, the outcome and the time. It
//! never carries a parameter, a command, an executor answer or a key: the point of the audit
//! is to say *that* a step ran, not to copy what it did.

use serde::{Deserialize, Serialize};

/// Where a step stands in an audit entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditOutcome {
    /// Written before the step does anything.
    Started,
    /// The node accepted the step and the journal recorded it.
    Succeeded,
    /// The node or the executor refused the step; the journal holds `Failed`.
    Failed,
    /// The step could not be carried out (it could not be signed, or the journal could not
    /// record it); it counts as failed.
    Errored,
}

/// One audit record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// The plan's id (hex).
    pub plan_id: String,
    /// The step's index in the plan.
    pub step: usize,
    /// The operation id the step runs, for example `deploy.install`.
    pub step_id: String,
    /// The administrator who authorised the run.
    pub actor: String,
    /// Where the step stands.
    pub outcome: AuditOutcome,
    /// When this entry was written, in seconds.
    pub at: i64,
}

/// The audit port could not take an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditUnavailable;

/// The audit port, implemented by the management layer.
pub trait AuditSink {
    /// Records `entry` durably.
    ///
    /// # Errors
    /// [`AuditUnavailable`] when the entry is not safely recorded. The wizard then stops: a
    /// step with no start entry does not run.
    fn record(&mut self, entry: &AuditEntry) -> Result<(), AuditUnavailable>;
}

/// An in-memory sink for tests; it can be told to fail.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemoryAudit {
    entries: Vec<AuditEntry>,
    fail_on: Option<(usize, AuditOutcome)>,
    fail_all: bool,
}

impl MemoryAudit {
    /// A sink that records everything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A sink that refuses every entry.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            fail_all: true,
            ..Self::default()
        }
    }

    /// A sink that refuses the entry for `step` with `outcome` (and records the rest).
    #[must_use]
    pub fn failing_on(step: usize, outcome: AuditOutcome) -> Self {
        Self {
            fail_on: Some((step, outcome)),
            ..Self::default()
        }
    }

    /// The entries recorded so far, in order.
    #[must_use]
    pub fn entries(&self) -> &[AuditEntry] {
        &self.entries
    }
}

impl AuditSink for MemoryAudit {
    fn record(&mut self, entry: &AuditEntry) -> Result<(), AuditUnavailable> {
        if self.fail_all || self.fail_on == Some((entry.step, entry.outcome)) {
            return Err(AuditUnavailable);
        }
        self.entries.push(entry.clone());
        Ok(())
    }
}
