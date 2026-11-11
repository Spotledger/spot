//! The typed operations a remedy may use — and the only way it may touch the world.
//!
//! A [`RemedyCtx`](crate::ctx::RemedyCtx) offers exactly these ports and nothing else: no raw
//! storage handle, no SQL, no direct field write. Every port is a typed operation over one
//! subsystem — jobs, the cache, a retrieval index, edge sync, quarantine, BPM cases and the
//! document lifecycle — and every operation returns a typed change whose before and after
//! fingerprints the framework records and checks on undo.
//!
//! The ports are traits so the framework stays independent of the backend crates: the host wires
//! `spot-jobs`, `spot-cache`, `spot-retrieval`, `spot-edge-sync`, `spot-quarantine`, the BPM
//! engine and the document engine behind them, and the self-test harness wires the in-memory
//! fake.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::Change;

/// A typed operation failed underneath the remedy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpError {
    /// The operation that failed (`job.retry`, `document.repost`).
    pub operation: String,
    /// Why it failed.
    pub detail: String,
}

impl OpError {
    /// An operation failure.
    #[must_use]
    pub fn new(operation: &str, detail: &str) -> Self {
        Self {
            operation: operation.to_owned(),
            detail: detail.to_owned(),
        }
    }
}

impl fmt::Display for OpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.operation, self.detail)
    }
}

/// Defines a typed reference: a newtype over the reference text with a `new`, `as_str` and
/// `Display`, plus serde so it can ride records.
macro_rules! typed_ref {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Wraps the reference text.
            #[must_use]
            pub fn new(value: &str) -> Self {
                Self(value.to_owned())
            }

            /// The reference text.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

typed_ref! {
    /// A background job, by its stable id.
    JobRef
}
typed_ref! {
    /// A cache namespace.
    CacheNs
}
typed_ref! {
    /// A retrieval index or projection, by name.
    IndexRef
}
typed_ref! {
    /// An edge-sync edge or node, by reference.
    EdgeRef
}
typed_ref! {
    /// A quarantined item, by its stable id.
    QuarantineRef
}
typed_ref! {
    /// A BPM case, by its stable id.
    CaseRef
}
typed_ref! {
    /// A document, by its reference.
    DocRef
}

/// Where a background job stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobLife {
    /// The job failed and has attempts left, or is waiting to retry.
    Failed,
    /// The job is claimed by a worker whose lease may have expired.
    Running,
    /// The job is queued for a worker.
    Queued,
    /// The job finished.
    Done,
}

impl JobLife {
    /// The stable snake_case text form, which is also the fingerprint form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            JobLife::Failed => "failed",
            JobLife::Running => "running",
            JobLife::Queued => "queued",
            JobLife::Done => "done",
        }
    }

    /// Parses the stable text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "failed" => Some(JobLife::Failed),
            "running" => Some(JobLife::Running),
            "queued" => Some(JobLife::Queued),
            "done" => Some(JobLife::Done),
            _ => None,
        }
    }
}

/// A job as the ports see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobView {
    /// The job.
    pub id: JobRef,
    /// Where it stands.
    pub life: JobLife,
    /// How many attempts it has had.
    pub attempts: u32,
    /// When the running lease expires (unix seconds), if it is running.
    pub lease_expires_at: Option<i64>,
}

/// What one job operation changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobChange {
    /// The job.
    pub id: JobRef,
    /// Its state before.
    pub before: JobLife,
    /// Its state after.
    pub after: JobLife,
}

/// The job port: read and change job state, never a raw row.
pub trait JobOps {
    /// The jobs currently failed.
    fn failed(&self) -> Vec<JobRef>;
    /// The jobs currently running.
    fn running(&self) -> Vec<JobRef>;
    /// One job, if it exists.
    fn job(&self, id: &JobRef) -> Option<JobView>;
    /// Retries a failed job, moving it to queued.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the job does not exist or is not failed.
    fn retry(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError>;
    /// Unsticks a running job whose lease has expired, moving it to queued.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the job does not exist, is not running, or its lease has not expired.
    fn unstick(&mut self, id: &JobRef, now: i64) -> Result<JobChange, OpError>;
    /// Restores a job to a prior state (the undo of retry or unstick).
    ///
    /// # Errors
    ///
    /// [`OpError`] when the job does not exist.
    fn restore(&mut self, id: &JobRef, life: JobLife) -> Result<(), OpError>;
}

/// How a retrieval index stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexHealth {
    /// The index reflects the current data.
    Fresh,
    /// The index lags the data and must be rebuilt.
    Stale,
}

impl IndexHealth {
    /// The stable snake_case text form, which is also the fingerprint form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            IndexHealth::Fresh => "fresh",
            IndexHealth::Stale => "stale",
        }
    }

    /// Parses the stable text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "fresh" => Some(IndexHealth::Fresh),
            "stale" => Some(IndexHealth::Stale),
            _ => None,
        }
    }
}

/// An index as the ports see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexView {
    /// The index.
    pub name: IndexRef,
    /// Its health.
    pub health: IndexHealth,
    /// Its generation number.
    pub generation: u64,
}

/// What one index operation changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexChange {
    /// The index.
    pub name: IndexRef,
    /// Its health before.
    pub before: IndexHealth,
    /// Its health after.
    pub after: IndexHealth,
    /// Its generation before.
    pub before_generation: u64,
    /// Its generation after.
    pub after_generation: u64,
}

/// The retrieval-index port: read health and rebuild, never a raw index handle.
pub trait IndexOps {
    /// The indexes that are stale.
    fn stale(&self) -> Vec<IndexRef>;
    /// One index, if it exists.
    fn index(&self, name: &IndexRef) -> Option<IndexView>;
    /// Rebuilds an index, leaving it fresh with a new generation.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the index does not exist.
    fn rebuild(&mut self, name: &IndexRef) -> Result<IndexChange, OpError>;
    /// Restores an index to a prior health and generation (the undo of rebuild).
    ///
    /// # Errors
    ///
    /// [`OpError`] when the index does not exist.
    fn restore(&mut self, change: &IndexChange) -> Result<(), OpError>;
}

/// The cache port: read entry counts and flush a namespace, never a raw cache handle.
pub trait CacheOps {
    /// The known namespaces.
    fn namespaces(&self) -> Vec<CacheNs>;
    /// How many entries a namespace holds.
    fn entries(&self, ns: &CacheNs) -> usize;
    /// Flushes a namespace, leaving it empty.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the namespace is unknown.
    fn flush(&mut self, ns: &CacheNs) -> Result<CacheChange, OpError>;
    /// Reverses a flush. A cache is derived data, so this records the reversal without
    /// restoring entries: the namespace refills from the source of truth.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the namespace is unknown.
    fn undo_flush(&mut self, change: &CacheChange) -> Result<(), OpError>;
}

/// What one cache flush changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CacheChange {
    /// The namespace.
    pub ns: CacheNs,
    /// Entries before.
    pub before: usize,
    /// Entries after.
    pub after: usize,
}

/// An edge as the ports see it: expected versus actual edge count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeView {
    /// The edge.
    pub edge: EdgeRef,
    /// The expected number of edges.
    pub expected: u64,
    /// The actual number.
    pub actual: u64,
}

/// What one edge repair changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EdgeChange {
    /// The edge.
    pub edge: EdgeRef,
    /// The actual count before.
    pub before: u64,
    /// The actual count after.
    pub after: u64,
}

/// The edge-sync port: read drift and repair it, never a raw graph handle.
pub trait EdgeOps {
    /// The edges that have drifted.
    fn drifted(&self) -> Vec<EdgeRef>;
    /// One edge, if it exists.
    fn edge(&self, edge: &EdgeRef) -> Option<EdgeView>;
    /// Repairs an edge so it matches its source.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the edge does not exist.
    fn repair(&mut self, edge: &EdgeRef) -> Result<EdgeChange, OpError>;
    /// Restores an edge to a prior count (the undo of repair).
    ///
    /// # Errors
    ///
    /// [`OpError`] when the edge does not exist.
    fn restore(&mut self, change: &EdgeChange) -> Result<(), OpError>;
}

/// Where a quarantined item stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuarantineState {
    /// Held for a decision.
    Held,
    /// Being retried.
    Retrying,
    /// Released into the system.
    Released,
    /// Discarded.
    Discarded,
}

impl QuarantineState {
    /// The stable snake_case text form, which is also the fingerprint form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            QuarantineState::Held => "held",
            QuarantineState::Retrying => "retrying",
            QuarantineState::Released => "released",
            QuarantineState::Discarded => "discarded",
        }
    }

    /// Parses the stable text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "held" => Some(QuarantineState::Held),
            "retrying" => Some(QuarantineState::Retrying),
            "released" => Some(QuarantineState::Released),
            "discarded" => Some(QuarantineState::Discarded),
            _ => None,
        }
    }
}

/// A quarantined item as the ports see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineItem {
    /// The item.
    pub id: QuarantineRef,
    /// Where it stands.
    pub state: QuarantineState,
}

/// What one quarantine retry changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineChange {
    /// The item.
    pub id: QuarantineRef,
    /// Its state before.
    pub before: QuarantineState,
    /// Its state after.
    pub after: QuarantineState,
}

/// The quarantine port: read held items and retry them, never a raw queue handle.
pub trait QuarantineOps {
    /// The items currently held.
    fn held(&self) -> Vec<QuarantineRef>;
    /// One item, if it exists.
    fn item(&self, id: &QuarantineRef) -> Option<QuarantineItem>;
    /// Retries a held item, moving it to retrying.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the item does not exist or is not held.
    fn retry(&mut self, id: &QuarantineRef) -> Result<QuarantineChange, OpError>;
    /// Restores an item to a prior state (the undo of retry).
    ///
    /// # Errors
    ///
    /// [`OpError`] when the item does not exist.
    fn restore(&mut self, change: &QuarantineChange) -> Result<(), OpError>;
}

/// Where a BPM case stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseLife {
    /// The case is stuck: no node is running and none is ready.
    Stuck,
    /// The case is progressing.
    Running,
    /// The case finished.
    Done,
}

impl CaseLife {
    /// The stable snake_case text form, which is also the fingerprint form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            CaseLife::Stuck => "stuck",
            CaseLife::Running => "running",
            CaseLife::Done => "done",
        }
    }

    /// Parses the stable text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "stuck" => Some(CaseLife::Stuck),
            "running" => Some(CaseLife::Running),
            "done" => Some(CaseLife::Done),
            _ => None,
        }
    }
}

/// A case as the ports see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseView {
    /// The case.
    pub id: CaseRef,
    /// Where it stands.
    pub life: CaseLife,
}

/// What one case unstick changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseChange {
    /// The case.
    pub id: CaseRef,
    /// Its state before.
    pub before: CaseLife,
    /// Its state after.
    pub after: CaseLife,
}

/// The BPM port: read case state and unstick a case, never a raw run handle.
pub trait CaseOps {
    /// The cases currently stuck.
    fn stuck(&self) -> Vec<CaseRef>;
    /// One case, if it exists.
    fn case(&self, id: &CaseRef) -> Option<CaseView>;
    /// Unsticks a case, moving it to running (through the BPM engine's own operations).
    ///
    /// # Errors
    ///
    /// [`OpError`] when the case does not exist or is not stuck.
    fn unstick(&mut self, id: &CaseRef, now: i64) -> Result<CaseChange, OpError>;
    /// Restores a case to a prior state (the undo of unstick).
    ///
    /// # Errors
    ///
    /// [`OpError`] when the case does not exist.
    fn restore(&mut self, change: &CaseChange) -> Result<(), OpError>;
}

/// Where a document stands in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocState {
    /// Editable.
    Draft,
    /// Submitted; a financial one is under the hard floor.
    Submitted,
    /// Cancelled.
    Cancelled,
}

impl DocState {
    /// The stable snake_case text form, which is also the fingerprint form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            DocState::Draft => "draft",
            DocState::Submitted => "submitted",
            DocState::Cancelled => "cancelled",
        }
    }

    /// Parses the stable text form.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "draft" => Some(DocState::Draft),
            "submitted" => Some(DocState::Submitted),
            "cancelled" => Some(DocState::Cancelled),
            _ => None,
        }
    }
}

/// A document as the ports see it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DocumentView {
    /// Its lifecycle state.
    pub state: DocState,
    /// Whether it is a financial record (the hard floor applies once submitted).
    pub financial: bool,
}

/// What one document operation changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentChange {
    /// The document.
    pub doc: DocRef,
    /// Its state before.
    pub before: DocState,
    /// Its state after.
    pub after: DocState,
    /// A sentence describing what happened.
    pub detail: String,
}

/// A typed document operation. The context runs it, enforcing the hard floor: a direct edit of a
/// submitted financial record is refused; only cancel, amend and journal entry pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DocOp {
    /// Recompute a derived field.
    Recompute {
        /// The field name.
        field: String,
    },
    /// Repost the document through the lifecycle.
    Repost,
    /// Relink the document's links.
    Relink {
        /// The link name.
        link: String,
    },
    /// Cancel the document.
    Cancel {
        /// Why.
        reason: String,
    },
    /// Amend the document.
    Amend,
    /// Post a journal entry against the document.
    JournalEntry {
        /// The entry reference.
        entry: String,
    },
}

impl DocOp {
    /// Whether this operation is an allowed route for a submitted financial record.
    #[must_use]
    pub fn is_allowed_financial_route(&self) -> bool {
        matches!(
            self,
            DocOp::Cancel { .. } | DocOp::Amend | DocOp::JournalEntry { .. }
        )
    }

    /// The operation's stable name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            DocOp::Recompute { .. } => "document_recompute",
            DocOp::Repost => "document_repost",
            DocOp::Relink { .. } => "document_relink",
            DocOp::Cancel { .. } => "document_cancel",
            DocOp::Amend => "document_amend",
            DocOp::JournalEntry { .. } => "document_journal_entry",
        }
    }
}

/// The document port: the lifecycle's typed operations, never a raw document handle.
pub trait DocumentOps {
    /// One document's view, if it exists.
    fn view(&self, doc: &DocRef) -> Option<DocumentView>;
    /// Runs a typed document operation.
    ///
    /// # Errors
    ///
    /// [`OpError`] when the document does not exist or the operation is invalid in its state.
    fn run(&mut self, doc: &DocRef, op: &DocOp) -> Result<DocumentChange, OpError>;
}

impl Change {
    /// A change on a job's lifecycle.
    #[must_use]
    pub fn job(id: &JobRef, before: JobLife, after: JobLife) -> Self {
        Change::new(
            &format!("job:{}", id.as_str()),
            before.as_str(),
            after.as_str(),
        )
    }

    /// A change on a cache namespace's entry count.
    #[must_use]
    pub fn cache(ns: &CacheNs, before: usize, after: usize) -> Self {
        Change::new(
            &format!("cache:{}", ns.as_str()),
            &format!("entries={before}"),
            &format!("entries={after}"),
        )
    }

    /// A change on an index from a before/after pair.
    #[must_use]
    pub fn index_pair(name: &IndexRef, change: &IndexChange) -> Self {
        Change::new(
            &format!("index:{}", name.as_str()),
            &format!("{}:gen{}", change.before.as_str(), change.before_generation),
            &format!("{}:gen{}", change.after.as_str(), change.after_generation),
        )
    }

    /// A change on an edge's actual count.
    #[must_use]
    pub fn edge(edge: &EdgeRef, before: u64, after: u64) -> Self {
        Change::new(
            &format!("edge:{}", edge.as_str()),
            &format!("edges={before}"),
            &format!("edges={after}"),
        )
    }

    /// A change on a quarantined item's state.
    #[must_use]
    pub fn quarantine(id: &QuarantineRef, before: QuarantineState, after: QuarantineState) -> Self {
        Change::new(
            &format!("quarantine:{}", id.as_str()),
            before.as_str(),
            after.as_str(),
        )
    }

    /// A change on a case's lifecycle.
    #[must_use]
    pub fn case(id: &CaseRef, before: CaseLife, after: CaseLife) -> Self {
        Change::new(
            &format!("case:{}", id.as_str()),
            before.as_str(),
            after.as_str(),
        )
    }

    /// A change on a document's lifecycle.
    #[must_use]
    pub fn document(doc: &DocRef, view: DocumentView, after: DocState) -> Self {
        Change::new(
            &format!("doc:{}", doc.as_str()),
            &document_fingerprint(view),
            &document_fingerprint(DocumentView {
                state: after,
                financial: view.financial,
            }),
        )
    }
}

/// The fingerprint of a document's state and financial flag.
#[must_use]
pub fn document_fingerprint(view: DocumentView) -> String {
    format!("{}:financial={}", view.state.as_str(), view.financial)
}
