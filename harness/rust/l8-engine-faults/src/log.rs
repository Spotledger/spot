//! The attempt log: every call the workload made, with its command id and how it ended.

use std::sync::{Mutex, MutexGuard, PoisonError};

use spot_app_api::DocState;
use spot_app_api::prelude::RecordId;

/// The operations the workload runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// Creates a Task (a worker's own, for the method call).
    CreateTask,
    /// Creates a Time Entry draft.
    CreateTe,
    /// Updates a draft.
    UpdateTe,
    /// Posts a draft.
    PostTe,
    /// Cancels a posted entry.
    CancelTe,
    /// Cancels a posted entry and creates its amended draft in one transaction.
    CorrectTe,
    /// Deletes a draft.
    DeleteTe,
    /// Calls the whitelisted `touch` method on the worker's own Task.
    TouchTask,
    /// A post by a principal that may not (a refusal).
    DeniedPost,
}

impl OpKind {
    /// The engine method the operation calls, as the findings and the call ledger name it.
    pub fn method(self) -> &'static str {
        match self {
            OpKind::CreateTask | OpKind::CreateTe => "create",
            OpKind::UpdateTe => "update",
            OpKind::PostTe => "post",
            OpKind::CancelTe => "cancel",
            OpKind::CorrectTe => "correct",
            OpKind::DeleteTe => "delete",
            OpKind::TouchTask => "call_method:touch",
            OpKind::DeniedPost => "post",
        }
    }

    /// Whether the command ledger covers the operation (a method call is not covered).
    pub fn ledger_covered(self) -> bool {
        self != OpKind::TouchTask
    }
}

/// The document state a result shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seen {
    /// The version.
    pub version: u64,
    /// The lifecycle state, for a Time Entry.
    pub state: Option<DocState>,
}

/// What a committed call produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Done {
    /// The document the call returned (the amended draft for a correct).
    pub doc: RecordId,
    /// Its version and state.
    pub seen: Seen,
}

/// How an attempt ended as the engine first answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Committed.
    Committed(Done),
    /// Refused cleanly.
    Refused(String),
    /// Refused by permission.
    Denied(String),
    /// The commit state was unknown, with the code.
    Indeterminate(String),
}

/// How an indeterminate attempt was settled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Settled {
    /// The re-send committed or replayed: this is the result.
    Committed(Done),
    /// The re-send was refused cleanly.
    Refused(String),
    /// Never settled within the deadline, or never re-sent.
    Unresolved,
}

/// The settlement of one indeterminate attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// What a read-back showed **before** the re-send: `None` when the target is absent.
    pub before: Option<Seen>,
    /// How the re-send ended.
    pub settled: Settled,
    /// How many times the same command was sent again.
    pub resends: u32,
}

/// One call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// The command id sent with every send of this call.
    pub command_id: String,
    /// The worker.
    pub worker: u32,
    /// The index within the worker.
    pub seq: u64,
    /// The operation.
    pub op: OpKind,
    /// The principal's name in the world (`manager`, `viewer`).
    pub actor: &'static str,
    /// The DocType.
    pub doctype: &'static str,
    /// The target document (the id a create uses).
    pub target: RecordId,
    /// The version the call expected, for a call on an existing document.
    pub expected: Option<u64>,
    /// How the engine first answered.
    pub outcome: Outcome,
    /// The settlement, when the first answer was indeterminate.
    pub resolution: Option<Resolution>,
}

impl Attempt {
    /// The result that stands after resolution: `Some(done)` when the call committed.
    pub fn committed(&self) -> Option<&Done> {
        match (&self.outcome, &self.resolution) {
            (Outcome::Committed(d), _) => Some(d),
            (
                Outcome::Indeterminate(_),
                Some(Resolution {
                    settled: Settled::Committed(d),
                    ..
                }),
            ) => Some(d),
            _ => None,
        }
    }

    /// Whether the call is still unsettled.
    pub fn unresolved(&self) -> bool {
        matches!(
            (&self.outcome, &self.resolution),
            (Outcome::Indeterminate(_), None)
                | (
                    Outcome::Indeterminate(_),
                    Some(Resolution {
                        settled: Settled::Unresolved,
                        ..
                    })
                )
        )
    }
}

/// The command id of call `seq` of `worker` in run `seed`: deterministic and unique.
pub fn command_id(seed: u64, worker: u32, seq: u64) -> String {
    format!("{seed}-{worker}-{seq}")
}

/// A thread-safe log.
#[derive(Default)]
pub struct AttemptLog {
    attempts: Mutex<Vec<Attempt>>,
}

impl AttemptLog {
    /// An empty log.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Attempt>> {
        self.attempts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Records an attempt.
    pub fn push(&self, attempt: Attempt) {
        self.lock().push(attempt);
    }

    /// A copy of the attempts, sorted by worker and sequence.
    pub fn snapshot(&self) -> Vec<Attempt> {
        let mut all = self.lock().clone();
        all.sort_by_key(|a| (a.worker, a.seq));
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_are_deterministic_and_unique_across_workers() {
        assert_eq!(command_id(7, 1, 2), command_id(7, 1, 2));
        let mut seen = std::collections::BTreeSet::new();
        for w in 0..8 {
            for s in 0..50 {
                assert!(seen.insert(command_id(7, w, s)));
            }
        }
        assert_ne!(command_id(7, 1, 2), command_id(8, 1, 2));
    }
}
