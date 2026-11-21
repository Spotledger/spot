//! Batch start (ADR-080 section 2.3, ADR-071 semantics): a bulk import is **one request that
//! owns many records** through a batch step, not one request per row.
//!
//! [`BatchRunner::start_batch`] starts the entry process once, as the calling principal, and
//! drives a stream of items through [`ItemApplier`] under that request. Each item is applied in
//! its own transaction by the record type's lifecycle (the request's context rides on the
//! command id as an authenticated grant, so the `process_only` guard recognises every item as
//! the request's own), and its
//! outcome is recorded:
//!
//! * `done`, `skipped` (the lifecycle refused the item's content) or `failed`;
//! * per-item problems are **reported, not fatal**, unless the spec says otherwise
//!   ([`FailurePolicy`]);
//! * a **checkpoint** (the position and counters) is saved every chunk. It is an optimisation:
//!   correctness comes from the outcome rows. After a crash a resume restarts from the
//!   checkpoint, and an item that already has an outcome is not presented again, so no item is
//!   applied twice. An item whose effect committed but whose outcome row did not is presented
//!   again with the same idempotency key, which the applier's lifecycle dedupes (ADR-056);
//! * the stream is read once, item by item: memory stays bounded however large the batch is.
//!
//! The command id of an item is a grant the request authority issued (bound to the calling
//! principal and the current attempt), which is what the guards verify; the per-item
//! idempotency key is [`ItemContext::item_key`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_app_api::prelude::OffsetDateTime;

use crate::guard::RequestAuthority;

use crate::codes;
use crate::entry::ProcessRef;
use crate::error::{EntryError, EntryResult};

/// How many problem items a report lists.
pub const REPORT_PROBLEM_LIMIT: usize = 100;

/// The default number of items between checkpoints.
pub const DEFAULT_CHUNK: usize = 500;

/// One item of the stream.
#[derive(Clone, Debug, PartialEq)]
pub struct BatchInput {
    /// A stable key, unique in the batch (for an import, the source row number or a natural
    /// key). It is the item's identity across a resume.
    pub key: String,
    /// The item's values.
    pub values: Json,
}

/// What a failed item does to the run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FailurePolicy {
    /// Stop the run at the first problem item (it becomes `failed`, and resumes at that item).
    pub abort_on_problem: bool,
    /// Stop the run when more than this many items are skipped or failed.
    pub max_problems: Option<usize>,
}

/// What to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchSpec {
    /// The entry process the one request runs.
    pub process: ProcessRef,
    /// The record type the items create.
    pub record_type: String,
    /// The batch step's node in that process.
    pub node: String,
    /// Items between checkpoints; at least 1.
    pub chunk_size: usize,
    /// What a problem item does to the run.
    pub failure: FailurePolicy,
}

impl BatchSpec {
    /// A spec with the default chunk and the default (report, do not abort) policy.
    #[must_use]
    pub fn new(process: ProcessRef, record_type: &str, node: &str) -> Self {
        Self {
            process,
            record_type: record_type.to_owned(),
            node: node.to_owned(),
            chunk_size: DEFAULT_CHUNK,
            failure: FailurePolicy::default(),
        }
    }
}

/// Who starts the batch and the key that makes a repeated start the same batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchIdentity {
    /// The calling principal; items run as this person.
    pub actor: String,
    /// The scope the records are created in.
    pub scope: String,
    /// The idempotency key of the start (the import's id, for example).
    pub key: String,
}

/// The state of a batch run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchStatus {
    /// Being driven, or its driver died; it can be resumed.
    Running,
    /// Stopped on request; it can be resumed.
    Paused,
    /// Stopped by its failure policy; it can be resumed.
    Failed,
    /// Every item is done.
    Succeeded,
    /// Finished; some items were skipped or failed and are listed in the report.
    SucceededWithSkips,
}

impl BatchStatus {
    /// Whether the run is over.
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::SucceededWithSkips)
    }
}

/// What happened to one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemOutcome {
    /// Applied.
    Done,
    /// The lifecycle refused the item's content; the run goes on.
    Skipped {
        /// The stable code.
        code: String,
        /// The plain-language reason.
        detail: String,
    },
    /// The item failed for another reason.
    Failed {
        /// The stable code.
        code: String,
        /// The plain-language reason.
        detail: String,
    },
}

/// How an applier refuses an item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemFailure {
    /// The item's content failed validation: it is skipped and the run goes on.
    Invalid {
        /// The stable code.
        code: String,
        /// The reason.
        detail: String,
    },
    /// The item failed for another reason.
    Failed {
        /// The stable code.
        code: String,
        /// The reason.
        detail: String,
    },
    /// Nothing can be applied (the store is down): the run stops here and can be resumed.
    Stop {
        /// The reason.
        detail: String,
    },
}

/// What an applier needs to apply one item.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ItemContext {
    /// The request that owns the records.
    pub request: String,
    /// The batch step.
    pub node: String,
    /// The attempt (1, then one more per resume).
    pub attempt: u32,
    /// The item's key.
    pub item_key: String,
    /// The item's position in the stream.
    pub index: usize,
    /// The command id for the lifecycle: the grant the request authority issued for this
    /// request, step, attempt and calling principal, which the guards verify.
    pub command_id: String,
    /// The calling principal.
    pub actor: String,
    /// The scope.
    pub scope: String,
    /// The business time.
    pub now: OffsetDateTime,
}

/// Creates one item's draft through the record type's lifecycle, in its own transaction, as the
/// calling principal, flagged as owned by the request.
pub trait ItemApplier: Send + Sync {
    /// Applies `item`. Must be idempotent on [`ItemContext::item_key`].
    ///
    /// # Errors
    /// [`ItemFailure`].
    fn apply(&self, item: &BatchInput, ctx: &ItemContext) -> Result<(), ItemFailure>;
}

/// Starts the one request, and finishes its batch step.
pub trait RequestStarter: Send + Sync {
    /// Starts `process` as the calling principal; idempotent on `identity.key`. Returns the
    /// request id.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn start(
        &self,
        process: &ProcessRef,
        identity: &BatchIdentity,
        now: OffsetDateTime,
    ) -> EntryResult<String>;

    /// Completes the batch step of the request with the summary.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn finish(
        &self,
        request: &str,
        node: &str,
        summary: &Json,
        now: OffsetDateTime,
    ) -> EntryResult<()>;
}

/// Counts of a run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchCounters {
    /// Items presented to the applier or found already decided.
    pub read: usize,
    /// Items done.
    pub done: usize,
    /// Items skipped.
    pub skipped: usize,
    /// Items failed.
    pub failed: usize,
}

impl BatchCounters {
    /// Skipped plus failed.
    #[must_use]
    pub fn problems(&self) -> usize {
        self.skipped + self.failed
    }
}

/// A batch run record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRun {
    /// The run id: the request's id.
    pub id: String,
    /// What was asked.
    pub spec: BatchSpec,
    /// Who started it.
    pub identity: BatchIdentity,
    /// The state.
    pub status: BatchStatus,
    /// The attempt, from 1.
    pub attempt: u32,
    /// Items before this position are decided (the checkpoint).
    pub checkpoint: usize,
    /// The counters as of the checkpoint (or the last stop).
    pub counters: BatchCounters,
}

/// One problem item, for the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProblemItem {
    /// The item's key.
    pub key: String,
    /// What happened to it.
    pub outcome: ItemOutcome,
}

/// Durable batch state. Implementations write an item's outcome row; the applier's own
/// transaction carries the item's effect (ADR-071: outcome and effect commit together in the
/// real store).
pub trait BatchStore: Send + Sync {
    /// Stores a new run unless one with the same start key exists; returns the stored run and
    /// whether it was created now.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn open(&self, run: BatchRun) -> EntryResult<(BatchRun, bool)>;

    /// The run with this id.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn load(&self, id: &str) -> EntryResult<Option<BatchRun>>;

    /// Saves the run's state, checkpoint and counters.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn save(&self, run: &BatchRun) -> EntryResult<()>;

    /// The recorded outcome of an item.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn outcome(&self, run: &str, item_key: &str) -> EntryResult<Option<ItemOutcome>>;

    /// Records the outcome of an item.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn record(&self, run: &str, item_key: &str, outcome: &ItemOutcome) -> EntryResult<()>;

    /// Up to `limit` skipped or failed items, in the order they were recorded.
    ///
    /// # Errors
    /// [`EntryError::BatchFailed`].
    fn problems(&self, run: &str, limit: usize) -> EntryResult<Vec<ProblemItem>>;
}

#[derive(Debug, Default)]
struct MemoryState {
    runs: BTreeMap<String, BatchRun>,
    by_key: BTreeMap<String, String>,
    outcomes: BTreeMap<(String, String), ItemOutcome>,
    order: Vec<(String, String)>,
}

/// An in-memory batch store for tests and single-site hosts.
#[derive(Debug, Default)]
pub struct MemoryBatchStore {
    state: Mutex<MemoryState>,
}

impl MemoryBatchStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Wraps the store for sharing.
    #[must_use]
    pub fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// How many outcome rows a run has.
    #[must_use]
    pub fn outcome_count(&self, run: &str) -> usize {
        self.lock()
            .outcomes
            .keys()
            .filter(|(id, _)| id == run)
            .count()
    }
}

impl BatchStore for MemoryBatchStore {
    fn open(&self, run: BatchRun) -> EntryResult<(BatchRun, bool)> {
        let mut state = self.lock();
        if let Some(existing) = state
            .by_key
            .get(&run.identity.key)
            .and_then(|id| state.runs.get(id))
        {
            return Ok((existing.clone(), false));
        }
        state
            .by_key
            .insert(run.identity.key.clone(), run.id.clone());
        state.runs.insert(run.id.clone(), run.clone());
        Ok((run, true))
    }

    fn load(&self, id: &str) -> EntryResult<Option<BatchRun>> {
        Ok(self.lock().runs.get(id).cloned())
    }

    fn save(&self, run: &BatchRun) -> EntryResult<()> {
        self.lock().runs.insert(run.id.clone(), run.clone());
        Ok(())
    }

    fn outcome(&self, run: &str, item_key: &str) -> EntryResult<Option<ItemOutcome>> {
        Ok(self
            .lock()
            .outcomes
            .get(&(run.to_owned(), item_key.to_owned()))
            .cloned())
    }

    fn record(&self, run: &str, item_key: &str, outcome: &ItemOutcome) -> EntryResult<()> {
        let mut state = self.lock();
        let key = (run.to_owned(), item_key.to_owned());
        if state
            .outcomes
            .insert(key.clone(), outcome.clone())
            .is_none()
        {
            state.order.push(key);
        }
        Ok(())
    }

    fn problems(&self, run: &str, limit: usize) -> EntryResult<Vec<ProblemItem>> {
        let state = self.lock();
        Ok(state
            .order
            .iter()
            .filter(|(id, _)| id == run)
            .filter_map(|key| {
                let outcome = state.outcomes.get(key)?;
                (*outcome != ItemOutcome::Done).then(|| ProblemItem {
                    key: key.1.clone(),
                    outcome: outcome.clone(),
                })
            })
            .take(limit)
            .collect())
    }
}

/// A stop request, checked between items.
pub struct BatchControl<'a> {
    stop: Option<&'a dyn Fn() -> bool>,
}

impl<'a> BatchControl<'a> {
    /// No stop request.
    #[must_use]
    pub fn none() -> Self {
        Self { stop: None }
    }

    /// Stops cooperatively once `stop` returns true (between items, never inside one).
    #[must_use]
    pub fn stop_when(stop: &'a dyn Fn() -> bool) -> Self {
        Self { stop: Some(stop) }
    }

    fn stop_requested(&self) -> bool {
        self.stop.is_some_and(|stop| stop())
    }
}

/// What a start or a resume reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchReport {
    /// The request that owns the records.
    pub request: String,
    /// Where the run stands.
    pub status: BatchStatus,
    /// The attempt.
    pub attempt: u32,
    /// The counters.
    pub counters: BatchCounters,
    /// The first problem items, with their reasons.
    pub problems: Vec<ProblemItem>,
    /// The stable code of the reason a run stopped by policy or failure, when it did.
    pub stopped_by: Option<String>,
}

/// Starts and resumes batch runs.
pub struct BatchRunner {
    store: Arc<dyn BatchStore>,
    starter: Arc<dyn RequestStarter>,
    applier: Arc<dyn ItemApplier>,
    authority: Arc<dyn RequestAuthority>,
}

impl BatchRunner {
    /// A runner over the store, the request starter, the item applier and the request
    /// authority that issues the grant every item runs under.
    #[must_use]
    pub fn new(
        store: Arc<dyn BatchStore>,
        starter: Arc<dyn RequestStarter>,
        applier: Arc<dyn ItemApplier>,
        authority: Arc<dyn RequestAuthority>,
    ) -> Self {
        Self {
            store,
            starter,
            applier,
            authority,
        }
    }

    /// Starts the entry process once and drives `inputs` through it.
    ///
    /// # Errors
    /// [`EntryError::BatchState`] when a batch with the same start key already exists (a
    /// finished one is not run again, an unfinished one must be [`resume`](Self::resume)d), or
    /// [`EntryError::BatchFailed`].
    pub fn start_batch<I>(
        &self,
        spec: &BatchSpec,
        inputs: I,
        identity: &BatchIdentity,
        control: &BatchControl<'_>,
        now: OffsetDateTime,
    ) -> EntryResult<BatchReport>
    where
        I: IntoIterator<Item = BatchInput>,
    {
        let request = self.starter.start(&spec.process, identity, now)?;
        let fresh = BatchRun {
            id: request.clone(),
            spec: spec.clone(),
            identity: identity.clone(),
            status: BatchStatus::Running,
            attempt: 1,
            checkpoint: 0,
            counters: BatchCounters::default(),
        };
        let (run, created) = self.store.open(fresh)?;
        if !created {
            let detail = if run.status.is_finished() {
                "it was already run to the end; it is not run again"
            } else {
                "it has not finished; resume it instead of starting it again"
            };
            return Err(EntryError::BatchState {
                run: run.id,
                detail: detail.to_owned(),
            });
        }
        self.drive(run, inputs, control, now)
    }

    /// Resumes an unfinished run from its checkpoint. Items that already have an outcome are not
    /// presented again.
    ///
    /// # Errors
    /// [`EntryError::BatchState`] for an unknown or finished run, or
    /// [`EntryError::BatchFailed`].
    pub fn resume<I>(
        &self,
        run_id: &str,
        inputs: I,
        control: &BatchControl<'_>,
        now: OffsetDateTime,
    ) -> EntryResult<BatchReport>
    where
        I: IntoIterator<Item = BatchInput>,
    {
        let Some(mut run) = self.store.load(run_id)? else {
            return Err(EntryError::BatchState {
                run: run_id.to_owned(),
                detail: "there is no such batch".to_owned(),
            });
        };
        if run.status.is_finished() {
            return Err(EntryError::BatchState {
                run: run.id,
                detail: "it already ran to the end".to_owned(),
            });
        }
        run.attempt += 1;
        run.status = BatchStatus::Running;
        self.store.save(&run)?;
        self.drive(run, inputs, control, now)
    }

    fn drive<I>(
        &self,
        mut run: BatchRun,
        inputs: I,
        control: &BatchControl<'_>,
        now: OffsetDateTime,
    ) -> EntryResult<BatchReport>
    where
        I: IntoIterator<Item = BatchInput>,
    {
        let chunk = run.spec.chunk_size.max(1);
        // One grant per attempt, bound to the calling principal: every item of the attempt runs
        // under it, and issuing the next attempt's grant ends this one.
        let command_id =
            self.authority
                .issue(&run.id, &run.spec.node, run.attempt, &run.identity.actor)?;
        let mut counters = run.counters;
        let mut stopped_by: Option<String> = None;
        let mut seen = 0_usize;
        let mut finished_stream = true;
        for (index, item) in inputs.into_iter().enumerate() {
            seen = index + 1;
            if index < run.checkpoint {
                continue;
            }
            if control.stop_requested() {
                run.checkpoint = index;
                run.status = BatchStatus::Paused;
                finished_stream = false;
                break;
            }
            let decided = match self.store.outcome(&run.id, &item.key)? {
                // An item that is done or skipped is never presented again; a failed one is
                // tried again on a resume.
                Some(outcome @ (ItemOutcome::Done | ItemOutcome::Skipped { .. })) => Some(outcome),
                _ => None,
            };
            let outcome = match decided {
                Some(outcome) => outcome,
                None => {
                    let ctx = Self::context(&run, &item, index, &command_id, now);
                    match self.applier.apply(&item, &ctx) {
                        Ok(()) => ItemOutcome::Done,
                        Err(ItemFailure::Invalid { code, detail }) => {
                            ItemOutcome::Skipped { code, detail }
                        }
                        Err(ItemFailure::Failed { code, detail }) => {
                            ItemOutcome::Failed { code, detail }
                        }
                        Err(ItemFailure::Stop { .. }) => {
                            run.checkpoint = index;
                            run.status = BatchStatus::Failed;
                            stopped_by = Some(codes::BATCH_FAILED.to_owned());
                            finished_stream = false;
                            break;
                        }
                    }
                }
            };
            self.store.record(&run.id, &item.key, &outcome)?;
            counters.read += 1;
            match &outcome {
                ItemOutcome::Done => counters.done += 1,
                ItemOutcome::Skipped { .. } => counters.skipped += 1,
                ItemOutcome::Failed { .. } => counters.failed += 1,
            }
            let problem = outcome != ItemOutcome::Done;
            let over_limit = run
                .spec
                .failure
                .max_problems
                .is_some_and(|max| counters.problems() > max);
            if problem && (run.spec.failure.abort_on_problem || over_limit) {
                // The item is decided; the next one is where a resume starts.
                run.checkpoint = index + 1;
                run.status = BatchStatus::Failed;
                stopped_by = Some(
                    if over_limit {
                        codes::BATCH_LIMIT
                    } else {
                        codes::BATCH_FAILED
                    }
                    .to_owned(),
                );
                finished_stream = false;
                break;
            }
            if (index + 1) % chunk == 0 {
                run.checkpoint = index + 1;
                run.counters = counters;
                self.store.save(&run)?;
            }
        }
        if finished_stream {
            run.checkpoint = run.checkpoint.max(seen);
            run.status = if counters.problems() == 0 {
                BatchStatus::Succeeded
            } else {
                BatchStatus::SucceededWithSkips
            };
        }
        run.counters = counters;
        self.store.save(&run)?;
        let problems = self.store.problems(&run.id, REPORT_PROBLEM_LIMIT)?;
        if run.status.is_finished() {
            let summary = json!({
                "read": counters.read,
                "done": counters.done,
                "skipped": counters.skipped,
                "failed": counters.failed,
            });
            self.starter
                .finish(&run.id, &run.spec.node, &summary, now)?;
        }
        Ok(BatchReport {
            request: run.id,
            status: run.status,
            attempt: run.attempt,
            counters,
            problems,
            stopped_by,
        })
    }

    fn context(
        run: &BatchRun,
        item: &BatchInput,
        index: usize,
        command_id: &str,
        now: OffsetDateTime,
    ) -> ItemContext {
        ItemContext {
            request: run.id.clone(),
            node: run.spec.node.clone(),
            attempt: run.attempt,
            item_key: item.key.clone(),
            index,
            command_id: command_id.to_owned(),
            actor: run.identity.actor.clone(),
            scope: run.identity.scope.clone(),
            now,
        }
    }
}
