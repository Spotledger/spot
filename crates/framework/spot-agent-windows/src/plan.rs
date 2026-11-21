//! The plan / apply / verify vocabulary every module shares.
//!
//! A module never changes the machine in `plan`: it reads the current state and returns a
//! [`Preview`] describing what *would* change. `apply` performs the change and returns an
//! [`Outcome`]; `verify` re-reads and returns a [`Check`]. Because `plan` is a pure read,
//! a caller (the console, a drift detector) can preview any operation safely, and because
//! `apply` skips a preview with no changes, applying twice changes nothing the second time.
//!
//! Every [`Change`] is a resource, an action and a short, secret-free detail — safe to log
//! and to render in the console.

use spot_mgmt_ops::OpId;

/// One change a plan would make, or one an apply made.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// What is changed, for example `service:wuauserv`.
    pub resource: String,
    /// What is done, for example `start`, `set_start_type`, `remove`.
    pub action: String,
    /// A short, secret-free description for the console.
    pub detail: String,
}

impl Change {
    /// A change to `resource`, doing `action`, described by `detail`.
    #[must_use]
    pub fn new(
        resource: impl Into<String>,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            resource: resource.into(),
            action: action.into(),
            detail: detail.into(),
        }
    }
}

/// What a plan found: the changes it would make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The operation planned.
    pub op: OpId,
    /// The changes that would be made; empty means the machine is already in the desired
    /// state.
    pub changes: Vec<Change>,
}

impl Preview {
    /// A plan that would change nothing.
    #[must_use]
    pub fn no_change(op: OpId) -> Self {
        Self {
            op,
            changes: Vec::new(),
        }
    }

    /// A plan with the given changes.
    #[must_use]
    pub fn with_changes(op: OpId, changes: Vec<Change>) -> Self {
        Self { op, changes }
    }

    /// Whether the plan would change nothing (the operation is already satisfied).
    #[must_use]
    pub fn is_no_change(&self) -> bool {
        self.changes.is_empty()
    }
}

/// What applying a plan did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The operation applied.
    pub op: OpId,
    /// The changes that were made; empty when the machine was already in the desired
    /// state.
    pub applied: Vec<Change>,
    /// Whether the machine was already in the desired state, so `apply` did nothing.
    pub already_current: bool,
    /// Whether the machine is now waiting for a reboot.
    pub reboot_required: bool,
}

impl Outcome {
    /// An apply that changed nothing because the state was already current.
    #[must_use]
    pub fn already_current(op: OpId) -> Self {
        Self {
            op,
            applied: Vec::new(),
            already_current: true,
            reboot_required: false,
        }
    }

    /// An apply that made the given changes.
    #[must_use]
    pub fn applied(op: OpId, applied: Vec<Change>, reboot_required: bool) -> Self {
        Self {
            op,
            applied,
            already_current: false,
            reboot_required,
        }
    }
}

/// What verification saw after an apply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The operation verified.
    pub op: OpId,
    /// Whether the desired state holds.
    pub satisfied: bool,
    /// What verification saw, in words (never a secret).
    pub detail: String,
}

impl Check {
    /// A satisfied check.
    #[must_use]
    pub fn satisfied(op: OpId, detail: impl Into<String>) -> Self {
        Self {
            op,
            satisfied: true,
            detail: detail.into(),
        }
    }

    /// An unsatisfied check.
    #[must_use]
    pub fn unsatisfied(op: OpId, detail: impl Into<String>) -> Self {
        Self {
            op,
            satisfied: false,
            detail: detail.into(),
        }
    }
}
