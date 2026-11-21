//! The healing journal: the append-only evidence of what the system tried (ADR-067 section 3,
//! `sys_heal_action`).
//!
//! An entry holds keys only: the node, the scope, the entity and name of the record, the
//! version number and the action. It never holds record content.

use serde::{Deserialize, Serialize};
use spot_conditions::Millis;
use spot_sync_reconcile::Scope;

use crate::{ActionKind, ActionOutcome, Target};

/// The longest failure reason kept, in characters.
const MAX_REASON: usize = 120;

/// How an entry ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum JournalOutcome {
    /// The action returned `Done`.
    Done,
    /// The action returned `Failed`, with its (shortened) reason.
    Failed(String),
    /// The node cannot do this action.
    NotSupported,
    /// An earlier `Done` was not followed by the finding disappearing in time.
    NotVerified,
}

impl JournalOutcome {
    /// The journal form of an action outcome. A failure reason is cut to a short length and
    /// stripped of control characters, so a careless implementation cannot make the journal a
    /// carrier of content.
    #[must_use]
    pub fn of(outcome: &ActionOutcome) -> Self {
        match outcome {
            ActionOutcome::Done => Self::Done,
            ActionOutcome::NotSupported => Self::NotSupported,
            ActionOutcome::Failed(reason) => Self::Failed(
                reason
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_REASON)
                    .collect(),
            ),
        }
    }
}

/// One repair attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalEntry {
    /// The destination node.
    pub node: String,
    /// The scope.
    pub scope: Scope,
    /// The record or scope repaired.
    pub target: Target,
    /// The version the owner holds, when known.
    pub version: Option<u64>,
    /// The action tried.
    pub kind: ActionKind,
    /// How it ended.
    pub outcome: JournalOutcome,
    /// When, on the caller's clock.
    pub at: Millis,
}

/// Where the entries go. Append only: there is no way to change or remove an entry.
pub trait HealingJournal {
    /// Appends one entry.
    fn append(&mut self, entry: JournalEntry);
    /// The entries of one node and scope, oldest first: the evidence for a condition.
    fn entries(&self, node: &str, scope: &Scope) -> Vec<JournalEntry>;
}

/// An in-memory journal, for tests and for a node that has not wired storage yet.
#[derive(Debug, Clone, Default)]
pub struct MemoryJournal {
    entries: Vec<JournalEntry>,
}

impl MemoryJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every entry, oldest first.
    #[must_use]
    pub fn all(&self) -> &[JournalEntry] {
        &self.entries
    }
}

impl HealingJournal for MemoryJournal {
    fn append(&mut self, entry: JournalEntry) {
        self.entries.push(entry);
    }

    fn entries(&self, node: &str, scope: &Scope) -> Vec<JournalEntry> {
        self.entries
            .iter()
            .filter(|e| e.node == node && &e.scope == scope)
            .cloned()
            .collect()
    }
}
