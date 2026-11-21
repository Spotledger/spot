//! The plan/apply/verify shape every Linux module follows.
//!
//! Each operation module offers three calls over the same validated arguments:
//!
//! - [`LinuxOperation::plan`] reads the current state and returns a [`Preview`]: the exact
//!   commands that *would* run, a one-line summary and any guard notes. It has no side effects,
//!   so the console can show a human what a change would do before anyone signs it.
//! - [`LinuxOperation::apply`] runs the planned commands and returns an [`Outcome`].
//! - [`LinuxOperation::verify`] reads the state again and returns a [`Check`].
//!
//! The host agent's [`OperationHandler`](spot_agent_core::OperationHandler) is a thin adapter
//! over `apply` and `verify` (see [`crate::handlers`]); the console and the CLI use `plan` to
//! render a preview and `verify` to confirm a change landed.

use spot_mgmt_ops::{OpId, Validated};

use crate::command::CommandRequest;
use crate::error::LinuxError;

/// One command a plan would run, with a human description of why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    /// What this step changes, in one sentence.
    pub description: String,
    /// The command, as a request (never a shell string).
    pub command: CommandRequest,
}

impl Step {
    /// A step with a description and a command.
    #[must_use]
    pub fn new(description: impl Into<String>, command: CommandRequest) -> Self {
        Self {
            description: description.into(),
            command,
        }
    }
}

/// What an operation would change: the commands that would run, a summary, and guard notes.
///
/// An empty preview (no steps) means the desired state already holds: applying it would change
/// nothing. Idempotency is exactly this — a second plan after a successful apply is empty.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The operation this preview is for.
    pub op: OpId,
    /// One sentence describing the overall change (or that there is none).
    pub summary: String,
    /// The commands that would run, in order.
    pub steps: Vec<Step>,
    /// Warnings a human must read before approving, e.g. a lock-out guard or a reboot notice.
    pub guards: Vec<String>,
}

impl Preview {
    /// A preview from its parts.
    #[must_use]
    pub fn new(
        op: OpId,
        summary: impl Into<String>,
        steps: Vec<Step>,
        guards: Vec<String>,
    ) -> Self {
        Self {
            op,
            summary: summary.into(),
            steps,
            guards,
        }
    }

    /// Whether applying this preview would change nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }

    /// How many commands the preview would run.
    #[must_use]
    pub fn len(&self) -> usize {
        self.steps.len()
    }

    /// Adds a guard note.
    #[must_use]
    pub fn with_guard(mut self, guard: impl Into<String>) -> Self {
        self.guards.push(guard.into());
        self
    }
}

/// What running an operation's commands produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The operation that ran.
    pub op: OpId,
    /// How many commands ran.
    pub applied: usize,
    /// Human notes about what happened (a reboot notice, the packages that changed).
    pub notes: Vec<String>,
}

impl Outcome {
    /// An outcome from its parts.
    #[must_use]
    pub fn new(op: &OpId, applied: usize, notes: Vec<String>) -> Self {
        Self {
            op: op.clone(),
            applied,
            notes,
        }
    }
}

/// The result of re-reading the state after an apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The operation that was checked.
    pub op: OpId,
    /// Whether the desired state holds.
    pub ok: bool,
    /// What the check found, in one sentence.
    pub detail: String,
}

impl Check {
    /// A passing check.
    #[must_use]
    pub fn ok(op: &OpId, detail: impl Into<String>) -> Self {
        Self {
            op: op.clone(),
            ok: true,
            detail: detail.into(),
        }
    }

    /// A failing check.
    #[must_use]
    pub fn failed(op: &OpId, detail: impl Into<String>) -> Self {
        Self {
            op: op.clone(),
            ok: false,
            detail: detail.into(),
        }
    }
}

/// One Linux operation: a plan, an apply and a verify over the same validated arguments.
pub trait LinuxOperation: Send + Sync {
    /// The catalogue operation id this module implements.
    fn op(&self) -> &OpId;

    /// What applying the operation would change, with no side effects.
    ///
    /// # Errors
    /// Any [`LinuxError`] the current-state reads produce, and the module's own validation.
    fn plan(&self, args: &Validated) -> Result<Preview, LinuxError>;

    /// Runs the operation.
    ///
    /// # Errors
    /// Any [`LinuxError`] a planned command or a guard produces.
    fn apply(&self, args: &Validated) -> Result<Outcome, LinuxError>;

    /// Re-reads the state and reports whether the desired state holds.
    ///
    /// # Errors
    /// Any [`LinuxError`] the current-state reads produce.
    fn verify(&self, args: &Validated) -> Result<Check, LinuxError>;

    /// The preview as a debug name, for diagnostics.
    fn describe(&self) -> String {
        self.op().as_str().to_owned()
    }
}

/// Runs every step in `preview`, returning how many ran and their commands.
///
/// # Errors
/// The first step's [`LinuxError`]; a non-zero exit is [`LinuxError::CommandFailed`].
pub fn run_steps(
    runner: &dyn crate::command::CommandRunner,
    preview: &Preview,
) -> Result<usize, LinuxError> {
    for step in &preview.steps {
        let output = runner.run(&step.command)?;
        if !output.success() {
            return Err(LinuxError::CommandFailed {
                program: step.command.program.clone(),
                status: output.status,
                detail: first_line(&output.stderr),
            });
        }
    }
    Ok(preview.steps.len())
}

/// The first non-empty line of `text`, trimmed and capped, for a short failure detail.
#[must_use]
pub fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    let mut detail: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        detail.push('…');
    }
    detail
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn op() -> OpId {
        OpId::new("os.service.start").unwrap()
    }

    #[test]
    fn an_empty_preview_means_no_change() {
        let preview = Preview::new(op(), "nothing to do", Vec::new(), Vec::new());
        assert!(preview.is_empty());
        assert_eq!(preview.len(), 0);
    }

    #[test]
    fn a_preview_with_steps_is_not_empty() {
        let step = Step::new(
            "start sshd",
            CommandRequest::new("systemctl").args(["--no-pager", "start", "sshd.service"]),
        );
        let preview = Preview::new(op(), "start sshd", vec![step], Vec::new())
            .with_guard("restarts sessions");
        assert!(!preview.is_empty());
        assert_eq!(preview.len(), 1);
        assert_eq!(preview.guards.len(), 1);
    }

    #[test]
    fn first_line_shortens_long_output() {
        assert_eq!(first_line("\n  boom  \nmore"), "boom");
        assert_eq!(first_line(""), "no output");
    }
}
