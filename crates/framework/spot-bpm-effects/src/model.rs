//! The record file's data: entries, reversals, and the queries over them.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::prelude::{OffsetDateTime, Version};
use spot_app_api::{
    ActionRef, EffectClassName, EffectKind, EffectReceipt, Operation, OperationReceipt,
};
use spot_dag::NodeId;

/// Most entries one request's file holds.
pub const MAX_ENTRIES: usize = 4096;
/// Most records one entry may link to.
pub const MAX_LINKS: usize = 64;
/// Most dependants a blocker or a graph answer keeps.
pub const MAX_DEPENDANTS: usize = 64;
/// Most alternatives a refusal may offer.
pub const MAX_ALTERNATIVES: usize = 8;
/// Longest text an opaque principal may have.
pub const MAX_PRINCIPAL_LEN: usize = 128;

/// A record, as the file names it: the DocType and the record's name.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordRef {
    /// The DocType.
    pub doctype: String,
    /// The record's name.
    pub name: String,
}

impl RecordRef {
    /// A record reference.
    #[must_use]
    pub fn new(doctype: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            doctype: doctype.into(),
            name: name.into(),
        }
    }
}

/// One declared way to undo what an entry did.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryReversal {
    /// Run this lifecycle operation on the same record (`ReversibleBy`).
    Lifecycle(Operation),
    /// Run this named compensating action (`CompensableBy`).
    Action(ActionRef),
    /// Nothing can undo it (`Irreversible`, or an effect no declaration covers).
    PointOfNoReturn,
}

/// What a caller hands the file when a step has run an operation.
#[derive(Clone, Debug)]
pub struct NewEntry {
    /// The node that ran the operation.
    pub node: NodeId,
    /// The receipt the lifecycle returned.
    pub receipt: OperationReceipt,
    /// Records this one depends on (a payment links the invoice it pays).
    pub links: BTreeSet<RecordRef>,
    /// The instant the step ran at; time is an argument.
    pub at: OffsetDateTime,
}

/// Which earlier entry's reversal an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reverses {
    /// The sequence number of the entry that was reversed.
    pub entry: u64,
    /// The index of the reversal within that entry.
    pub index: u32,
}

/// One entry of the request's file.
#[derive(Clone, Debug, PartialEq)]
pub struct FileEntry {
    /// The position in the file: 0, 1, 2 ..., never reused.
    pub seq: u64,
    /// The node that ran the operation.
    pub node: NodeId,
    /// The operation.
    pub operation: Operation,
    /// The record it touched.
    pub record: RecordRef,
    /// The record's version after the operation.
    pub version: Version,
    /// The receipt the lifecycle returned.
    pub receipt: OperationReceipt,
    /// The instant it ran at.
    pub at: OffsetDateTime,
    /// Records this entry depends on.
    pub links: BTreeSet<RecordRef>,
    /// The declared ways to undo it, derived once when the entry was recorded.
    pub reversals: Vec<EntryReversal>,
    /// Set when this entry is itself the reversal of an earlier one.
    pub reverses: Option<Reverses>,
    /// Set when the entry revises a record an earlier pass of the same node created.
    pub revise_of: Option<RecordRef>,
}

impl FileEntry {
    /// Whether the entry is a forward step (not the reversal of another).
    #[must_use]
    pub fn is_forward(&self) -> bool {
        self.reverses.is_none()
    }
}

/// How a rewound node revises the record its earlier pass created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviseHow {
    /// Through the `update` lifecycle (a draft).
    Update,
    /// Through the `correct` lifecycle (a posted record: cancel plus a new draft).
    Correct,
}

/// A revision a rewind left for the next pass of a node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Revision {
    /// The record to revise instead of creating a new one.
    pub record: RecordRef,
    /// How.
    pub how: ReviseHow,
}

/// What a request waits on: a message from a partner, and how to cancel the wait.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PartnerWait {
    /// The conversation (an opaque id the conversations crate owns).
    pub conversation: String,
    /// The contract's cancellation message name.
    pub cancellation: String,
    /// Whether the contract lets the cancellation go before the local reversals succeed.
    pub before_local: bool,
}

/// One audited choice a person made about a refused reversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// The entry whose reversal was refused.
    pub entry: u64,
    /// Who chose.
    pub by: String,
    /// When.
    pub at: OffsetDateTime,
    /// What they chose: an alternative's id, or `None` for abandoning.
    pub alternative: Option<String>,
}

/// The per-request lock and journal an undo in progress keeps in the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoRun {
    /// The plan this run executes; a different plan is refused while it is held.
    pub plan_id: String,
    /// The node the undo goes back to; `None` is the start.
    pub to: Option<NodeId>,
    /// The file length at the target: entries from here on are being undone.
    pub boundary: usize,
    /// What is done: the journal, keyed `(entry, reversal)` or the cancellation.
    pub journal: BTreeSet<JournalKey>,
    /// Who started the undo (the acting principal of the first call).
    pub started_by: String,
    /// True while one caller is executing the plan: a second `execute_undo` or `choose` is
    /// refused with `request_locked`. It is never trusted from a stored file (see
    /// `RequestEffects::restore`).
    pub running: bool,
}

/// A journal key: one reversal of one entry, or the partner cancellation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum JournalKey {
    /// Reversal `index` of entry `entry`.
    Reversal {
        /// The entry.
        entry: u64,
        /// The reversal's index.
        index: u32,
    },
    /// The partner cancellation.
    Cancellation,
}

/// The request's record file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordFile {
    pub(crate) entries: Vec<FileEntry>,
    /// `(node, entries.len() after it finished)`, oldest first.
    pub(crate) visits: Vec<(NodeId, usize)>,
    pub(crate) position: Option<NodeId>,
    pub(crate) revisions: BTreeMap<NodeId, Revision>,
    pub(crate) waiting: Option<PartnerWait>,
    pub(crate) undo: Option<UndoRun>,
    pub(crate) pending: Option<crate::undo::Decision>,
    pub(crate) audit: Vec<AuditRecord>,
}

impl RecordFile {
    /// Every entry, in order.
    #[must_use]
    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// The node the request stands at.
    #[must_use]
    pub fn position(&self) -> Option<&NodeId> {
        self.position.as_ref()
    }

    /// The revision pending for a node's next pass, when a rewind left one.
    #[must_use]
    pub fn revision_for(&self, node: &NodeId) -> Option<&Revision> {
        self.revisions.get(node)
    }

    /// What the request waits on, when it waits on a partner.
    #[must_use]
    pub fn waiting(&self) -> Option<&PartnerWait> {
        self.waiting.as_ref()
    }

    /// The decision an undo is stopped at, when it is.
    #[must_use]
    pub fn pending_decision(&self) -> Option<&crate::undo::Decision> {
        self.pending.as_ref()
    }

    /// Every audited choice, in order.
    #[must_use]
    pub fn audit(&self) -> &[AuditRecord] {
        &self.audit
    }

    /// The undo holding the request, when there is one.
    #[must_use]
    pub fn undo(&self) -> Option<&UndoRun> {
        self.undo.as_ref()
    }

    /// Whether an undo holds the request.
    #[must_use]
    pub fn is_locked(&self) -> bool {
        self.undo.is_some()
    }

    /// The entries that touched `record`, in order.
    #[must_use]
    pub fn entries_by_record(&self, record: &RecordRef) -> Vec<&FileEntry> {
        self.entries
            .iter()
            .filter(|e| &e.record == record)
            .collect()
    }

    /// The effects of the given class and kind, with the entry each came from. `None` matches
    /// any.
    #[must_use]
    pub fn effects_by(
        &self,
        class: Option<&EffectClassName>,
        kind: Option<EffectKind>,
    ) -> Vec<(u64, &EffectReceipt)> {
        self.entries
            .iter()
            .flat_map(|e| e.receipt.effects.iter().map(move |f| (e.seq, f)))
            .filter(|(_, f)| {
                class.is_none_or(|c| &f.class == c) && kind.is_none_or(|k| f.kind == k)
            })
            .collect()
    }

    /// The records in the file that depend on `record`: the links inside the file, plus what
    /// the business graph reports. Sorted and bounded to [`MAX_DEPENDANTS`].
    ///
    /// # Errors
    /// The graph's failure code, when it cannot answer.
    pub fn dependants(
        &self,
        record: &RecordRef,
        graph: &dyn crate::graph::GraphDependants,
    ) -> Result<Vec<RecordRef>, String> {
        let mut out: BTreeSet<RecordRef> = self
            .entries
            .iter()
            .filter(|e| e.is_forward() && e.links.contains(record) && &e.record != record)
            .map(|e| e.record.clone())
            .collect();
        out.extend(graph.dependants(record)?);
        out.remove(record);
        Ok(out.into_iter().take(MAX_DEPENDANTS).collect())
    }

    /// Whether reversal `index` of `entry` has a reversing entry in the file.
    pub(crate) fn is_reversed(&self, entry: u64, index: u32) -> bool {
        self.entries
            .iter()
            .any(|e| e.reverses == Some(Reverses { entry, index }))
    }
}
