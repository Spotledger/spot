//! What a run ends as, and the typed refusal that stops one before it changes anything.
//!
//! A run that finishes its plan is [`Outcome::Applied`]; a run whose step failed and whose
//! rollback finished is [`Outcome::RolledBack`]; a run whose rollback itself failed is
//! [`Outcome::NeedsAttention`], and the site stays flagged `installing`. A refusal — an
//! unapproved plan, a plan still waiting for capabilities, or a lost lease — is an
//! [`ExecError`], never a silent half-run.

use crate::journal::StepId;

/// The step that failed, and why, as the outcome reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The step that failed.
    pub step: StepId,
    /// The stable code of the failure (an `installer.*` or adapter code).
    pub code: &'static str,
    /// What went wrong, phrased for the administrator.
    pub message: String,
}

impl Failure {
    /// A failure from its step, code and message.
    #[must_use]
    pub fn new(step: StepId, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            step,
            code,
            message: message.into(),
        }
    }
}

/// What a run ended as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Every step ran; `steps` are the step identities in order.
    Applied {
        /// The steps that ran, in order.
        steps: Vec<StepId>,
    },
    /// A step failed and the site was put back to its state before the run.
    RolledBack {
        /// The step that failed.
        failed_step: StepId,
        /// Why it failed.
        reason: Failure,
        /// The steps that were undone, in the order they were undone.
        undone: Vec<StepId>,
    },
    /// A step failed and undoing an earlier step failed too. Everything stays journalled and the
    /// site stays flagged `installing`; a person must look.
    NeedsAttention {
        /// The step whose rollback failed.
        failed_step: StepId,
        /// Why the rollback failed.
        reason: Failure,
        /// The steps that were undone before the rollback failure, in order.
        undone: Vec<StepId>,
    },
    /// `resume` found no journal for the site: nothing was interrupted.
    Nothing,
}

impl Outcome {
    /// Whether the run left the site in the new state.
    #[must_use]
    pub fn is_applied(&self) -> bool {
        matches!(self, Outcome::Applied { .. })
    }

    /// Whether the run left the site in its old state.
    #[must_use]
    pub fn is_rolled_back(&self) -> bool {
        matches!(self, Outcome::RolledBack { .. })
    }

    /// Whether the run needs a person.
    #[must_use]
    pub fn needs_attention(&self) -> bool {
        matches!(self, Outcome::NeedsAttention { .. })
    }
}

/// A typed refusal: a stable code and a message.
///
/// The executor raises these before a run changes anything (an unapproved plan, a plan waiting
/// for capabilities, a lease it cannot take) and for a lost lease mid-run; trait methods return
/// them too, with the adapter's own code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ExecError {
    /// The stable machine-readable code, for the API error envelope.
    pub code: &'static str,
    /// What went wrong, phrased for the administrator.
    pub message: String,
}

impl ExecError {
    /// A refusal from its code and message.
    #[must_use]
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
