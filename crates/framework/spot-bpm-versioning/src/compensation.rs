//! Compensation when an upstream document is cancelled.
//!
//! A Process may declare, for a Document node, what to do when the document that node produced
//! is cancelled later. [`CompensationModel`] holds those declarations; a
//! [`DeclaredCompensator`] raises the declared return step, or parks the Case at the
//! `needs_decision` task with the documents it depends on.
//!
//! [`CancellationWatcher`] consumes committed `doc_cancelled` outbox events. It finds the Cases
//! whose Case file (WP-BPM-04) contains the document, and for each one asks its
//! [`Compensator`] to act. Delivery is at least once, so the watcher deduplicates by event id and
//! by `(case, node, document)`: a redelivered event raises nothing twice.
//!
//! A cancelled upstream document never lets the Case continue on the happy path. The journal
//! records the Case as blocked until every raised step completes, or until a person resolves a
//! parked decision; [`CancellationWatcher::may_continue`] is the check the host makes before it
//! steps the Case again.
//!
//! # The swap to WP-BPM-15
//!
//! ADR-080's amendment says compensation must reuse `spot-bpm-effects` (WP-BPM-15) once it
//! exists. This module is the seam for that: [`Compensator`] is the small trait the watcher
//! calls (`compensate(request, trigger) -> outcome`), and [`DeclaredCompensator`] is the default
//! implementation with the declared-return-step behaviour. When the effects crate lands, a host
//! passes its `Compensator` implementation instead; the watcher, the Case lookup and the
//! deduplication do not change. This crate never deletes or edits a ledger-like row: it only
//! starts lifecycle work (a return step) or asks for a decision.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::prelude::{Name, RecordId, Version};
use spot_bpm_documents::{CaseFileStore, Entry, Touch};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;
use spot_storage_contract::OutboxRecord;

use crate::error::{Result, VersioningError};

/// The task a Case is parked at when no compensation is declared.
pub const NEEDS_DECISION_TASK: &str = "needs_decision";

/// The lifecycle topic the watcher consumes.
pub const DOCUMENT_CANCELLED_TOPIC: &str = "doc_cancelled";

/// One committed document-cancelled event, reduced to what compensation needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TriggerRecord {
    /// The outbox event id: the consumers' deduplication key.
    pub event: RecordId,
    /// The document's DocType.
    pub doctype: Name,
    /// The cancelled document.
    pub document: RecordId,
    /// The version the cancellation produced, when the event carries one.
    pub subject_version: Option<Version>,
}

impl TriggerRecord {
    /// Reads a lifecycle outbox event, when it is a `doc_cancelled` event.
    #[must_use]
    pub fn from_outbox(record: &OutboxRecord) -> Option<Self> {
        if record.event.topic.as_str() != DOCUMENT_CANCELLED_TOPIC {
            return None;
        }
        Some(Self {
            event: record.event.id,
            doctype: record.event.subject.entity.clone(),
            document: record.event.subject.id,
            subject_version: record.event.subject_version,
        })
    }

    /// The Case-file key of the cancelled document.
    #[must_use]
    pub fn touch(&self) -> Touch {
        Touch::new(self.doctype.clone(), self.document)
    }
}

/// What a Process declares for one Document node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompensationPath {
    /// Start (or branch to) this step of the process when the document is cancelled.
    ReturnStep {
        /// The step to raise.
        step: NodeId,
    },
    /// Park the Case at this task, with the documents it depends on.
    Task {
        /// The task name.
        task: String,
    },
}

/// The compensation declarations of one process version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CompensationModel {
    paths: BTreeMap<NodeId, CompensationPath>,
}

impl CompensationModel {
    /// An empty model: every cancellation parks the Case at [`NEEDS_DECISION_TASK`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares one Document node's path (builder form).
    #[must_use]
    pub fn with(mut self, node: NodeId, path: CompensationPath) -> Self {
        self.paths.insert(node, path);
        self
    }

    /// Declares one Document node's path in place.
    pub fn declare(&mut self, node: NodeId, path: CompensationPath) {
        self.paths.insert(node, path);
    }

    /// The path declared for a node, when there is one.
    #[must_use]
    pub fn path_for(&self, node: &NodeId) -> Option<&CompensationPath> {
        self.paths.get(node)
    }

    /// Every declaration, in node-id order.
    #[must_use]
    pub fn paths(&self) -> &BTreeMap<NodeId, CompensationPath> {
        &self.paths
    }
}

/// What the watcher asks a compensator to do for one Case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompensationRequest {
    /// The Case.
    pub case: CaseId,
    /// The Document node whose document was cancelled.
    pub node: NodeId,
    /// The cancelled document.
    pub document: Touch,
    /// Every document the Case file lists, so a parked decision can name its dependencies.
    pub dependencies: Vec<Touch>,
}

/// What a compensator did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompensationOutcome {
    /// A declared return step was raised.
    Raised {
        /// The Case.
        case: CaseId,
        /// The step raised.
        step: NodeId,
    },
    /// The Case was parked at a task, with the documents it depends on.
    Parked {
        /// The Case.
        case: CaseId,
        /// The task.
        task: String,
        /// The documents the decision depends on.
        dependencies: Vec<Touch>,
    },
}

/// The compensation policy the watcher calls.
///
/// This is the seam ADR-080's amendment names: `spot-bpm-effects`' undo machinery replaces the
/// default implementation, never the watcher.
pub trait Compensator: Send + Sync {
    /// Acts on one cancelled-document trigger for one Case.
    ///
    /// # Errors
    /// [`VersioningError::Compensation`] when the step could not be raised or the Case parked.
    fn compensate(
        &self,
        request: &CompensationRequest,
        trigger: &TriggerRecord,
    ) -> Result<CompensationOutcome>;
}

/// Where a raised step or a parked Case is applied.
///
/// The host implements this over its transaction: raising a step branches the Case and writing
/// the compensation record must commit together.
pub trait CompensationSink: Send + Sync {
    /// Raises one declared step for a Case.
    ///
    /// # Errors
    /// [`VersioningError::Compensation`] when the step could not be raised.
    fn raise_step(
        &self,
        request: &CompensationRequest,
        step: &NodeId,
        trigger: &TriggerRecord,
    ) -> Result<CompensationOutcome>;

    /// Parks one Case at a task.
    ///
    /// # Errors
    /// [`VersioningError::Compensation`] when the Case could not be parked.
    fn park(
        &self,
        request: &CompensationRequest,
        task: &str,
        dependencies: &[Touch],
        trigger: &TriggerRecord,
    ) -> Result<CompensationOutcome>;
}

/// The reference compensation policy: the declared return step, or a `needs_decision` task.
pub struct DeclaredCompensator {
    model: Arc<CompensationModel>,
    sink: Arc<dyn CompensationSink>,
}

impl DeclaredCompensator {
    /// A compensator over a model and a sink.
    #[must_use]
    pub fn new(model: Arc<CompensationModel>, sink: Arc<dyn CompensationSink>) -> Self {
        Self { model, sink }
    }

    /// The model the compensator reads.
    #[must_use]
    pub fn model(&self) -> &CompensationModel {
        &self.model
    }
}

impl Compensator for DeclaredCompensator {
    fn compensate(
        &self,
        request: &CompensationRequest,
        trigger: &TriggerRecord,
    ) -> Result<CompensationOutcome> {
        match self.model.path_for(&request.node) {
            Some(CompensationPath::ReturnStep { step }) => {
                self.sink.raise_step(request, step, trigger)
            }
            Some(CompensationPath::Task { task }) => {
                self.sink
                    .park(request, task, &request.dependencies, trigger)
            }
            None => self
                .sink
                .park(request, NEEDS_DECISION_TASK, &request.dependencies, trigger),
        }
    }
}

/// One step the watcher raised.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RaisedStep {
    /// The Case.
    pub case: CaseId,
    /// The Document node whose document was cancelled.
    pub node: NodeId,
    /// The step raised.
    pub step: NodeId,
    /// The event that caused it.
    pub trigger: RecordId,
}

/// One Case parked for a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedCase {
    /// The Case.
    pub case: CaseId,
    /// The task.
    pub task: String,
    /// The documents the decision depends on.
    pub dependencies: Vec<Touch>,
    /// The event that caused it.
    pub trigger: RecordId,
}

/// The Cases a cancelled document belongs to.
pub trait CaseIndex: Send + Sync {
    /// The Cases whose Case file contains `touch`.
    fn cases_for(&self, touch: &Touch) -> Vec<CaseId>;
}

/// The in-memory [`CaseIndex`] the tests use.
#[derive(Debug, Default)]
pub struct MemoryCaseIndex {
    inner: Mutex<BTreeMap<Touch, BTreeSet<CaseId>>>,
}

impl MemoryCaseIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, BTreeMap<Touch, BTreeSet<CaseId>>> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Indexes one Case-file entry.
    pub fn record(&self, entry: &Entry) {
        self.lock()
            .entry(Touch::new(entry.doctype.clone(), entry.document))
            .or_default()
            .insert(entry.case.clone());
    }

    /// Indexes a Case's whole file.
    pub fn record_all(&self, entries: &[Entry]) {
        for entry in entries {
            self.record(entry);
        }
    }
}

impl CaseIndex for MemoryCaseIndex {
    fn cases_for(&self, touch: &Touch) -> Vec<CaseId> {
        self.lock()
            .get(touch)
            .map(|cases| cases.iter().cloned().collect())
            .unwrap_or_default()
    }
}

/// What the watcher has already handled.
#[derive(Debug, Default)]
struct Handled {
    deliveries: BTreeMap<RecordId, usize>,
    raises: BTreeSet<(CaseId, String, RecordId)>,
}

/// The compensation journal: which Cases may not continue, and what was raised.
#[derive(Debug, Default)]
pub struct CompensationJournal {
    inner: Mutex<JournalInner>,
}

#[derive(Debug, Default)]
struct JournalInner {
    open_steps: BTreeMap<CaseId, BTreeSet<NodeId>>,
    parked: BTreeSet<CaseId>,
    raised: Vec<RaisedStep>,
    parked_cases: Vec<ParkedCase>,
}

impl CompensationJournal {
    /// An empty journal.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, JournalInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn raise(&self, raised: RaisedStep) {
        let mut inner = self.lock();
        inner
            .open_steps
            .entry(raised.case.clone())
            .or_default()
            .insert(raised.step.clone());
        inner.raised.push(raised);
    }

    fn park(&self, parked: ParkedCase) {
        let mut inner = self.lock();
        inner.parked.insert(parked.case.clone());
        inner.parked_cases.push(parked);
    }

    /// Whether the Case may take another engine step: no open raised step and no parked
    /// decision.
    #[must_use]
    pub fn may_continue(&self, case: &CaseId) -> bool {
        let inner = self.lock();
        !inner.parked.contains(case) && inner.open_steps.get(case).is_none_or(BTreeSet::is_empty)
    }

    /// Marks one raised step complete; the Case may continue once none is open.
    pub fn complete_step(&self, case: &CaseId, step: &NodeId) {
        let mut inner = self.lock();
        if let Some(open) = inner.open_steps.get_mut(case) {
            open.remove(step);
            if open.is_empty() {
                inner.open_steps.remove(case);
            }
        }
    }

    /// Resolves a parked decision; the Case may continue when nothing else is open.
    pub fn release(&self, case: &CaseId) {
        self.lock().parked.remove(case);
    }

    /// Every raised step, in the order the watcher raised it.
    #[must_use]
    pub fn raised(&self) -> Vec<RaisedStep> {
        self.lock().raised.clone()
    }

    /// Every parked Case, in the order the watcher parked it.
    #[must_use]
    pub fn parked(&self) -> Vec<ParkedCase> {
        self.lock().parked_cases.clone()
    }
}

/// The in-memory [`CompensationSink`] the tests use.
pub struct MemoryCompensationSink {
    journal: Arc<CompensationJournal>,
}

impl MemoryCompensationSink {
    /// A sink writing into `journal`.
    #[must_use]
    pub fn new(journal: Arc<CompensationJournal>) -> Self {
        Self { journal }
    }

    /// The journal it writes into.
    #[must_use]
    pub fn journal(&self) -> Arc<CompensationJournal> {
        self.journal.clone()
    }
}

impl CompensationSink for MemoryCompensationSink {
    fn raise_step(
        &self,
        request: &CompensationRequest,
        step: &NodeId,
        trigger: &TriggerRecord,
    ) -> Result<CompensationOutcome> {
        self.journal.raise(RaisedStep {
            case: request.case.clone(),
            node: request.node.clone(),
            step: step.clone(),
            trigger: trigger.event,
        });
        Ok(CompensationOutcome::Raised {
            case: request.case.clone(),
            step: step.clone(),
        })
    }

    fn park(
        &self,
        request: &CompensationRequest,
        task: &str,
        dependencies: &[Touch],
        trigger: &TriggerRecord,
    ) -> Result<CompensationOutcome> {
        self.journal.park(ParkedCase {
            case: request.case.clone(),
            task: task.to_owned(),
            dependencies: dependencies.to_vec(),
            trigger: trigger.event,
        });
        Ok(CompensationOutcome::Parked {
            case: request.case.clone(),
            task: task.to_owned(),
            dependencies: dependencies.to_vec(),
        })
    }
}

/// What one consumed cancellation event did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancellationReport {
    /// The event.
    pub event: RecordId,
    /// The cancelled document.
    pub document: Touch,
    /// How many Cases the index named.
    pub cases: usize,
    /// What was raised or parked.
    pub outcomes: Vec<CompensationOutcome>,
    /// How many redeliveries the watcher recognised and skipped.
    pub duplicates: usize,
}

/// Consumes document-cancelled events and raises the declared compensation.
pub struct CancellationWatcher {
    index: Arc<dyn CaseIndex>,
    file: Arc<dyn CaseFileStore>,
    compensator: Arc<dyn Compensator>,
    journal: Arc<CompensationJournal>,
    handled: Mutex<Handled>,
}

impl CancellationWatcher {
    /// Builds a watcher over the Case index, the Case file, the compensation policy and the
    /// journal the policy writes through.
    #[must_use]
    pub fn new(
        index: Arc<dyn CaseIndex>,
        file: Arc<dyn CaseFileStore>,
        compensator: Arc<dyn Compensator>,
        journal: Arc<CompensationJournal>,
    ) -> Self {
        Self {
            index,
            file,
            compensator,
            journal,
            handled: Mutex::new(Handled::default()),
        }
    }

    fn handled(&self) -> MutexGuard<'_, Handled> {
        self.handled
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Consumes one committed cancellation event.
    ///
    /// # Errors
    /// [`VersioningError::StoreFailed`] when the Case file cannot be read, or
    /// [`VersioningError::Compensation`] when the compensator refuses.
    pub fn consume(&self, trigger: &TriggerRecord) -> Result<CancellationReport> {
        let touch = trigger.touch();
        let cases = self.index.cases_for(&touch);
        let mut outcomes = Vec::new();
        let mut duplicates = 0;
        {
            let mut handled = self.handled();
            let seen = handled.deliveries.entry(trigger.event).or_insert(0);
            *seen = seen.saturating_add(1);
        }
        for case in &cases {
            let entries =
                self.file
                    .entries(case)
                    .map_err(|error| VersioningError::StoreFailed {
                        detail: error.to_string(),
                    })?;
            let matching: Vec<&Entry> = entries
                .iter()
                .filter(|entry| Touch::new(entry.doctype.clone(), entry.document) == touch)
                .collect();
            if matching.is_empty() {
                continue;
            }
            // Compensation covers the documents downstream of the cancelled one: the deliveries
            // the Case created from the order, for example. The cancelled document itself is
            // already cancelled, so its own entry is not a candidate. A Case that only lists
            // the cancelled document falls back to it, so a missing declaration still parks.
            let mut candidates: Vec<&Entry> = entries
                .iter()
                .filter(|entry| entry.document != trigger.document)
                .collect();
            if candidates.is_empty() {
                candidates.clone_from(&matching);
            }
            let dependencies: Vec<Touch> = entries
                .iter()
                .map(|entry| Touch::new(entry.doctype.clone(), entry.document))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            for entry in candidates {
                let key = (case.clone(), entry.node.to_string(), entry.document);
                {
                    let mut handled = self.handled();
                    if handled.raises.contains(&key) {
                        duplicates += 1;
                        continue;
                    }
                    handled.raises.insert(key);
                }
                let request = CompensationRequest {
                    case: case.clone(),
                    node: entry.node.clone(),
                    document: touch.clone(),
                    dependencies: dependencies.clone(),
                };
                // A failed compensation must stay retryable: forget the key so the redelivery
                // of this event raises it, instead of silently skipping a cancelled upstream.
                let outcome = match self.compensator.compensate(&request, trigger) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        self.handled().raises.remove(&(
                            case.clone(),
                            entry.node.to_string(),
                            entry.document,
                        ));
                        return Err(error);
                    }
                };
                outcomes.push(outcome);
            }
        }
        Ok(CancellationReport {
            event: trigger.event,
            document: touch,
            cases: cases.len(),
            outcomes,
            duplicates,
        })
    }

    /// Whether the Case may take another engine step.
    #[must_use]
    pub fn may_continue(&self, case: &CaseId) -> bool {
        self.journal.may_continue(case)
    }

    /// The journal the watcher writes through.
    #[must_use]
    pub fn journal(&self) -> Arc<CompensationJournal> {
        self.journal.clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_model_has_no_path() {
        let model = CompensationModel::new();
        assert_eq!(model.path_for(&NodeId::new("delivery").unwrap()), None);
    }

    #[test]
    fn a_declared_path_is_found() {
        let node = NodeId::new("delivery").unwrap();
        let step = NodeId::new("return_delivery").unwrap();
        let model = CompensationModel::new().with(
            node.clone(),
            CompensationPath::ReturnStep { step: step.clone() },
        );
        assert_eq!(
            model.path_for(&node),
            Some(&CompensationPath::ReturnStep { step })
        );
    }
}
