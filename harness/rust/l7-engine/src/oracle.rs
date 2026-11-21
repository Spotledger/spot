//! The engine run's oracle: the exact expected state a run leaves (WP-TST-07.T2).
//!
//! Built only from what the driver observed committing: the documents, their versions and
//! states, the lifecycle event each committed write must have produced, the refusals, and the
//! links the documents hold. The engine invariants read it next to the store.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::DocState;
use spot_app_api::prelude::RecordId;

use crate::driver::Op;

/// How an operation ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpOutcome {
    /// Committed.
    Committed,
    /// Refused because the principal may not do it.
    Denied(String),
    /// Refused cleanly (stale version, wrong state, missing record, validation).
    Refused(String),
    /// Failed unexpectedly; never counted as a success.
    Failed(String),
}

impl OpOutcome {
    /// The error code, if any.
    pub fn code(&self) -> &str {
        match self {
            OpOutcome::Committed => "",
            OpOutcome::Denied(c) | OpOutcome::Refused(c) | OpOutcome::Failed(c) => c,
        }
    }
}

/// One document the oracle expects.
#[derive(Clone, Debug)]
pub struct ExpectedDoc {
    /// The DocType.
    pub entity: String,
    /// The document.
    pub id: RecordId,
    /// The committed version.
    pub version: u64,
    /// Every version a committed operation produced for this document.
    pub versions: BTreeSet<u64>,
    /// The lifecycle state, for a posted DocType.
    pub state: Option<DocState>,
    /// Whether a committed create has not been deleted.
    pub live: bool,
    /// The human-readable name at commit time.
    pub name: String,
    /// The committed lifecycle operations, in order.
    pub transitions: Vec<Op>,
}

/// One lifecycle event a committed operation must have produced.
#[derive(Clone, Debug)]
pub struct ExpectedEvent {
    /// The operation.
    pub op: Op,
    /// The topic.
    pub topic: &'static str,
    /// The subject DocType.
    pub entity: String,
    /// The subject document.
    pub id: RecordId,
    /// The subject version.
    pub version: u64,
    /// The actor (a principal name).
    pub actor: String,
}

/// One refused operation.
#[derive(Clone, Debug)]
pub struct DeniedOp {
    /// The operation.
    pub op: Op,
    /// The subject DocType.
    pub entity: String,
    /// The subject document (the id a create would have used).
    pub id: RecordId,
    /// The actor.
    pub actor: String,
    /// The refusal code.
    pub code: String,
}

/// A link a committed document holds, recorded so `links-checked` can prove the target existed.
#[derive(Clone, Debug)]
pub struct RecordedLink {
    /// The holder DocType.
    pub holder_entity: String,
    /// The holder document.
    pub holder_id: RecordId,
    /// The link field.
    pub field: String,
    /// The target document.
    pub target_id: RecordId,
}

/// One operation of a run.
#[derive(Clone, Debug)]
pub struct OpRecord {
    /// The worker.
    pub worker: u32,
    /// The index within the worker.
    pub seq: u64,
    /// The operation.
    pub op: Op,
    /// The actor.
    pub actor: String,
    /// The subject DocType.
    pub entity: String,
    /// The subject document.
    pub id: RecordId,
    /// How it ended.
    pub outcome: OpOutcome,
}

/// The exact expected state of an engine-level run.
#[derive(Clone, Debug)]
pub struct EngineRun {
    /// The seed.
    pub seed: u64,
    /// The workers.
    pub workers: u32,
    /// The profile.
    pub profile: String,
    /// The backend.
    pub backend: String,
    /// The documents a committed operation left, by `(entity, id)`.
    pub docs: BTreeMap<(String, RecordId), ExpectedDoc>,
    /// The lifecycle event each committed write must have produced, in commit order.
    pub events: Vec<ExpectedEvent>,
    /// The operations refused by permissions.
    pub denials: Vec<DeniedOp>,
    /// Every operation, in completion order.
    pub ops: Vec<OpRecord>,
    /// The secret values planted in protected fields; none may reach an event.
    pub secrets: Vec<String>,
    /// The links committed documents hold, for `links-checked`.
    pub links: Vec<RecordedLink>,
    /// The engine's own denial count, read after the workers stopped.
    pub denial_count: usize,
}

impl EngineRun {
    pub(crate) fn new(seed: u64, workers: u32, profile: &str, backend: &str) -> EngineRun {
        EngineRun {
            seed,
            workers,
            profile: profile.to_owned(),
            backend: backend.to_owned(),
            docs: BTreeMap::new(),
            events: Vec::new(),
            denials: Vec::new(),
            ops: Vec::new(),
            secrets: Vec::new(),
            links: Vec::new(),
            denial_count: 0,
        }
    }

    /// The names used by live committed documents.
    pub fn names(&self) -> BTreeSet<(String, String)> {
        self.docs
            .values()
            .filter(|d| d.live)
            .map(|d| (d.entity.clone(), d.name.clone()))
            .collect()
    }

    /// The number of committed operations.
    pub fn committed(&self) -> usize {
        self.ops
            .iter()
            .filter(|o| o.outcome == OpOutcome::Committed)
            .count()
    }
}
