//! The dashboard draft's edits: the [`DashboardOp`] command log, the pure [`apply_op`] and the
//! [`DashboardEditor`] that makes undo and redo exact.
//!
//! This module mirrors [`crate::report_edit`] for [`crate::dashboard::DashboardDraft`]. Every op
//! is one logical step a designer takes, and an op that fails changes nothing.

use serde::{Deserialize, Serialize};

use crate::dashboard::{DashboardDraft, Place, RefreshPolicy, SharedFilter, Tile};
use crate::error::OpError;

/// One edit of a dashboard draft, applied by [`apply_op`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum DashboardOp {
    /// Adds a tile.
    AddTile {
        /// The tile.
        tile: Tile,
    },
    /// Removes a tile.
    RemoveTile {
        /// The tile's id.
        id: String,
    },
    /// Moves or resizes a tile.
    MoveTile {
        /// The tile's id.
        id: String,
        /// The new place.
        place: Place,
    },
    /// Changes a tile's title and whether it follows the shared filters.
    SetTile {
        /// The tile's id.
        id: String,
        /// The new title.
        title: String,
        /// Whether the shared filters apply to it.
        uses_filters: bool,
    },
    /// Adds a filter shared by the tiles.
    AddFilter {
        /// The filter.
        filter: SharedFilter,
    },
    /// Removes a shared filter.
    RemoveFilter {
        /// The filter's name.
        name: String,
    },
    /// Sets the refresh policy.
    SetRefresh {
        /// The policy.
        refresh: RefreshPolicy,
    },
    /// Sets the roles that may open the dashboard.
    SetRoles {
        /// The roles.
        roles: Vec<String>,
    },
}

impl DashboardOp {
    /// The op's stable name, for logs and audit.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::AddTile { .. } => "add_tile",
            Self::RemoveTile { .. } => "remove_tile",
            Self::MoveTile { .. } => "move_tile",
            Self::SetTile { .. } => "set_tile",
            Self::AddFilter { .. } => "add_filter",
            Self::RemoveFilter { .. } => "remove_filter",
            Self::SetRefresh { .. } => "set_refresh",
            Self::SetRoles { .. } => "set_roles",
        }
    }
}

/// Applies one op to `draft`, returning the changed draft. Pure: `draft` is untouched, and an
/// error changes nothing.
///
/// # Errors
///
/// An [`OpError`] naming the precondition that failed.
pub fn apply_op(draft: &DashboardDraft, op: &DashboardOp) -> Result<DashboardDraft, OpError> {
    let mut next = draft.clone();
    match op {
        DashboardOp::AddTile { tile } => {
            next.add_tile(tile.clone())?;
        }
        DashboardOp::RemoveTile { id } => {
            next.remove_tile(id)?;
        }
        DashboardOp::MoveTile { id, place } => {
            next.move_tile(id, *place)?;
        }
        DashboardOp::SetTile {
            id,
            title,
            uses_filters,
        } => {
            if title.trim().is_empty() {
                return Err(OpError::BadValue {
                    message: "A tile needs a title.".to_owned(),
                });
            }
            let Some(tile) = next.tiles.iter_mut().find(|t| t.id == *id) else {
                return Err(OpError::UnknownTile { tile: id.clone() });
            };
            tile.title = title.clone();
            tile.uses_filters = *uses_filters;
        }
        DashboardOp::AddFilter { filter } => {
            next.add_filter(filter.clone())?;
        }
        DashboardOp::RemoveFilter { name } => {
            let before = next.filters.len();
            next.filters.retain(|f| f.name != *name);
            if next.filters.len() == before {
                return Err(OpError::BadValue {
                    message: "That filter is not in the dashboard any more.".to_owned(),
                });
            }
        }
        DashboardOp::SetRefresh { refresh } => {
            if let RefreshPolicy::Every { seconds } = refresh
                && *seconds == 0
            {
                return Err(OpError::BadRefresh {
                    message: "A refresh interval must be above zero.".to_owned(),
                });
            }
            next.refresh = *refresh;
        }
        DashboardOp::SetRoles { roles } => {
            // An empty list is allowed (nobody yet; publishing warns), a blank name is not.
            if roles.iter().any(|role| role.trim().is_empty()) {
                return Err(OpError::BadValue {
                    message: "A role name cannot be blank.".to_owned(),
                });
            }
            next.roles = roles.clone();
        }
    }
    Ok(next)
}

/// A draft and its op log: undo and redo move a cursor over the states the log produced.
#[derive(Clone, Debug)]
pub struct DashboardEditor {
    initial: DashboardDraft,
    states: Vec<DashboardDraft>,
    ops: Vec<DashboardOp>,
    cursor: usize,
}

impl DashboardEditor {
    /// An editor over `draft`, with no edits.
    #[must_use]
    pub fn new(draft: DashboardDraft) -> Self {
        Self {
            states: vec![draft.clone()],
            initial: draft,
            ops: Vec::new(),
            cursor: 0,
        }
    }

    /// The current draft.
    #[must_use]
    pub fn current(&self) -> &DashboardDraft {
        self.states.get(self.cursor).unwrap_or(&self.initial)
    }

    /// Applies an op, dropping any redoable future.
    ///
    /// # Errors
    ///
    /// The [`OpError`] the op raised; nothing changes.
    pub fn apply(&mut self, op: DashboardOp) -> Result<&DashboardDraft, OpError> {
        let next = apply_op(self.current(), &op)?;
        self.states.truncate(self.cursor + 1);
        self.ops.truncate(self.cursor);
        self.states.push(next);
        self.ops.push(op);
        self.cursor += 1;
        Ok(self.current())
    }

    /// Whether undo would move back.
    #[must_use]
    pub fn can_undo(&self) -> bool {
        self.cursor > 0
    }

    /// Whether redo would move forward.
    #[must_use]
    pub fn can_redo(&self) -> bool {
        self.cursor < self.ops.len()
    }

    /// Moves back one op. Returns whether it moved.
    pub fn undo(&mut self) -> bool {
        if !self.can_undo() {
            return false;
        }
        self.cursor -= 1;
        true
    }

    /// Moves forward one op. Returns whether it moved.
    pub fn redo(&mut self) -> bool {
        if !self.can_redo() {
            return false;
        }
        self.cursor += 1;
        true
    }

    /// The ops applied so far, in order (the command log).
    #[must_use]
    pub fn log(&self) -> &[DashboardOp] {
        &self.ops[..self.cursor]
    }

    /// Every op the editor ever applied, including redoable ones.
    #[must_use]
    pub fn ops(&self) -> &[DashboardOp] {
        &self.ops
    }

    /// How many ops the log holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cursor
    }

    /// Whether the log is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cursor == 0
    }

    /// The current draft recomputed from the initial draft and the log.
    ///
    /// # Errors
    ///
    /// The [`OpError`] a logged op raises on replay.
    pub fn replayed(&self) -> Result<DashboardDraft, OpError> {
        let mut draft = self.initial.clone();
        for op in self.log() {
            draft = apply_op(&draft, op)?;
        }
        Ok(draft)
    }
}
