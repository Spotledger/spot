//! The closed set of repair actions the driver may ask for, behind a trait.
//!
//! The driver never performs a repair itself: it calls a [`RepairActions`] implementation (the
//! transport, owned by another task) and journals what came back. Every action is idempotent
//! (ADR-067 section 3) and may only **overwrite a mirror from its owner**: an implementation
//! never deletes a record, never writes to an owner and never overwrites a mirror that is newer
//! than its owner.

/// What a repair is aimed at.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Target {
    /// One record of the scope, by entity and name.
    Record {
        /// The record's entity.
        entity: String,
        /// The record's name (its key, not its content).
        name: String,
    },
    /// The whole scope.
    Scope,
}

/// The four actions, from the cheapest to the widest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ActionKind {
    /// The mirror's derived digests and aggregates are rebuilt from its own records.
    RebuildDigest,
    /// The mirror pulls one record again from its owner.
    RepullRecord,
    /// The owner resends everything it holds in the scope to the mirror.
    ResendScope,
    /// The mirror asks its owner for a snapshot of the scope (or the record's range).
    RequestSnapshot,
}

impl ActionKind {
    /// A stable name, for the journal and for metrics labels.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RebuildDigest => "rebuild_digest",
            Self::RepullRecord => "repull_record",
            Self::ResendScope => "resend_scope",
            Self::RequestSnapshot => "request_snapshot",
        }
    }
}

/// What an action returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActionOutcome {
    /// The action completed. The finding must still disappear in a later comparison.
    Done,
    /// The action failed. The reason is a short code or phrase and never carries record
    /// content: it is written to the journal, which is evidence and may be exported.
    Failed(String),
    /// This node cannot do this action; the driver tries the next, wider one.
    NotSupported,
}

/// One request to an action: who, which scope, aimed at what.
#[derive(Debug, Clone, Copy)]
pub struct ActionRequest<'a> {
    /// The destination node: the mirror being repaired.
    pub node: &'a str,
    /// The scope.
    pub scope: &'a spot_sync_reconcile::Scope,
    /// What is repaired.
    pub target: &'a Target,
    /// The version the owner holds, when the finding named one.
    pub version: Option<u64>,
}

/// The transport side of healing. Implemented by whoever can reach the nodes.
pub trait RepairActions {
    /// Rebuild the mirror's derived digests.
    fn rebuild_digest(&mut self, request: &ActionRequest<'_>) -> ActionOutcome;
    /// Pull one record again from the owner.
    fn repull_record(&mut self, request: &ActionRequest<'_>) -> ActionOutcome;
    /// Resend the scope from the owner.
    fn resend_scope(&mut self, request: &ActionRequest<'_>) -> ActionOutcome;
    /// Request a snapshot from the owner.
    fn request_snapshot(&mut self, request: &ActionRequest<'_>) -> ActionOutcome;
}

/// Runs one action by kind.
pub(crate) fn run<A: RepairActions>(
    actions: &mut A,
    kind: ActionKind,
    request: &ActionRequest<'_>,
) -> ActionOutcome {
    match kind {
        ActionKind::RebuildDigest => actions.rebuild_digest(request),
        ActionKind::RepullRecord => actions.repull_record(request),
        ActionKind::ResendScope => actions.resend_scope(request),
        ActionKind::RequestSnapshot => actions.request_snapshot(request),
    }
}
