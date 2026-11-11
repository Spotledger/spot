//! The shared result shape every module returns: [`Preview`] from `plan` (what would
//! change, no side effects), [`Outcome`] from `apply`, and [`Check`] from `verify`.
//!
//! The console renders a preview directly: the current value, the desired value and the
//! risk level. An idempotent module returns a preview with no [`Change`] entries when the
//! machine already matches, which is what "apply twice, the second plan is empty" means.

use spot_mgmt_ops::Risk;

/// One field the operation would move from its current value to a desired one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    /// The field's name, as the console labels it (a sysctl key, an interface, a mode).
    pub field: String,
    /// The value read from the machine, or `None` when it is absent.
    pub current: Option<String>,
    /// The value the operation would set, or `None` when it is being removed.
    pub desired: Option<String>,
}

impl Change {
    /// A change of `field` from `current` to `desired`.
    #[must_use]
    pub fn new(field: impl Into<String>, current: Option<String>, desired: Option<String>) -> Self {
        Self {
            field: field.into(),
            current,
            desired,
        }
    }

    /// Whether the machine already matches (the two sides are equal).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.current == self.desired
    }
}

/// What an operation would change, without changing it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// The operation id.
    pub op: String,
    /// The operation's risk level, taken from the catalogue.
    pub risk: Risk,
    /// The changes the operation would make, in a deterministic order.
    pub changes: Vec<Change>,
    /// Whether the operation destroys data or is otherwise hard to undo.
    pub destructive: bool,
    /// Free-form, secret-free notes for the console.
    pub notes: Vec<String>,
}

impl Preview {
    /// An empty preview for `op` at `risk`.
    #[must_use]
    pub fn new(op: impl Into<String>, risk: Risk) -> Self {
        Self {
            op: op.into(),
            risk,
            changes: Vec::new(),
            destructive: false,
            notes: Vec::new(),
        }
    }

    /// Adds one change.
    #[must_use]
    pub fn change(mut self, change: Change) -> Self {
        self.changes.push(change);
        self
    }

    /// Adds one note.
    #[must_use]
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// Marks the operation destructive.
    #[must_use]
    pub fn destructive(mut self, destructive: bool) -> Self {
        self.destructive = destructive;
        self
    }

    /// Whether the machine already matches (no change would be made).
    #[must_use]
    pub fn is_noop(&self) -> bool {
        self.changes.iter().all(Change::is_noop)
    }
}

/// What running an operation did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The operation id.
    pub op: String,
    /// Whether the machine changed (`false` for an idempotent no-op).
    pub changed: bool,
    /// A short, secret-free description of what happened.
    pub detail: String,
}

impl Outcome {
    /// An outcome for `op`.
    #[must_use]
    pub fn new(op: impl Into<String>, changed: bool, detail: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            changed,
            detail: detail.into(),
        }
    }
}

/// What re-reading the machine found after a change.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    /// The operation id.
    pub op: String,
    /// Whether the machine matches the desired state.
    pub ok: bool,
    /// A short, secret-free description of the observed state.
    pub detail: String,
}

impl Check {
    /// A passing check.
    #[must_use]
    pub fn ok(op: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            ok: true,
            detail: detail.into(),
        }
    }

    /// A failing check.
    #[must_use]
    pub fn failed(op: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            op: op.into(),
            ok: false,
            detail: detail.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_preview_with_no_real_change_is_a_noop() {
        let preview = Preview::new("os.sysctl.set", Risk::Medium).change(Change::new(
            "vm.swappiness",
            Some("60".to_owned()),
            Some("60".to_owned()),
        ));
        assert!(preview.is_noop());
        let preview = preview.change(Change::new(
            "vm.swappiness",
            Some("60".to_owned()),
            Some("10".to_owned()),
        ));
        assert!(!preview.is_noop());
        assert_eq!(preview.risk, Risk::Medium);
    }

    #[test]
    fn a_change_from_absent_is_not_a_noop() {
        assert!(!Change::new("x", None, Some("1".to_owned())).is_noop());
        assert!(Change::new("x", None, None).is_noop());
    }
}
