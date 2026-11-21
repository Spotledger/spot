//! The undo plan: which reversals run, in which order, and what stops the sequence.

use std::collections::{BTreeMap, BTreeSet};

use spot_app_api::{ActionRef, Operation};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;

use crate::error::{EffectsError, Result};
use crate::graph::GraphDependants;
use crate::model::{EntryReversal, FileEntry, MAX_DEPENDANTS, PartnerWait, RecordFile, RecordRef};
use crate::port::Alternative;

/// Why a reversal needs no call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// The request itself cancelled what it posted, so the pair nets to zero already.
    AlreadyReversedInRequest,
    /// The entry changed nothing that needs reversing (the request cancelled what it posted).
    NothingToReverse,
}

/// What one planned step does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlannedReversal {
    /// Run a lifecycle operation on the record (`ReversibleBy`).
    Lifecycle(Operation),
    /// Run a named compensating action (`CompensableBy`).
    Action(ActionRef),
    /// A point of no return: the undo cannot pass it.
    Blocked,
    /// No call is needed.
    Skip(SkipReason),
}

/// One reversal of one entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoStep {
    /// The entry being reversed.
    pub entry: u64,
    /// The reversal's index within the entry.
    pub index: u32,
    /// The node that made the entry.
    pub node: NodeId,
    /// The record.
    pub record: RecordRef,
    /// What to do.
    pub action: PlannedReversal,
}

/// The partner cancellation a plan ends with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancellationStep {
    /// The wait being cancelled.
    pub wait: PartnerWait,
}

/// A reversal plan for one request, to a node (or to the start).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoPlan {
    pub(crate) request: CaseId,
    pub(crate) to: Option<NodeId>,
    pub(crate) boundary: usize,
    pub(crate) file_len: usize,
    pub(crate) id: String,
    pub(crate) steps: Vec<UndoStep>,
    pub(crate) outside: BTreeMap<u64, Vec<RecordRef>>,
    pub(crate) cancellation: Option<CancellationStep>,
}

impl UndoPlan {
    /// The plan's stable id: the same id resumes the same undo.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// The request.
    #[must_use]
    pub fn request(&self) -> &CaseId {
        &self.request
    }

    /// The node the undo goes back to; `None` is the start.
    #[must_use]
    pub fn to(&self) -> Option<&NodeId> {
        self.to.as_ref()
    }

    /// The steps, in the order they run.
    #[must_use]
    pub fn steps(&self) -> &[UndoStep] {
        &self.steps
    }

    /// Entries that have dependants outside the part of the file being undone, with those
    /// dependants: a warning, since only the lifecycle decides whether it refuses.
    #[must_use]
    pub fn outside_dependants(&self) -> &BTreeMap<u64, Vec<RecordRef>> {
        &self.outside
    }

    /// The partner cancellation the plan ends with, when the request waits on a partner.
    #[must_use]
    pub fn cancellation(&self) -> Option<&CancellationStep> {
        self.cancellation.as_ref()
    }

    /// Whether any step is a point of no return.
    #[must_use]
    pub fn is_blocked(&self) -> bool {
        self.steps
            .iter()
            .any(|s| s.action == PlannedReversal::Blocked)
    }
}

/// What stands in the way of one reversal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocker {
    /// The record that could not be reversed.
    pub record: RecordRef,
    /// A stable code.
    pub reason_code: String,
    /// The records that stand in the way.
    pub dependants: Vec<RecordRef>,
}

/// The sequence has stopped and a person decides (ADR-080 6.5). Nothing is forced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// The entry whose reversal stopped the sequence.
    pub entry: u64,
    /// The reversal's index within the entry.
    pub index: u32,
    /// What stands in the way.
    pub blockers: Vec<Blocker>,
    /// What the record type offers instead.
    pub alternatives: Vec<Alternative>,
}

/// Where an undo ended up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UndoOutcome {
    /// Everything after the target is reversed, in order.
    Done {
        /// Reversals that ran.
        reversed: usize,
        /// Reversals that needed no call.
        skipped: usize,
    },
    /// The sequence stopped; the state is consistent with the file and a person decides.
    NeedsDecision(Decision),
}

/// What a person chooses at a decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Choice {
    /// Carry out one of the offered alternatives.
    Alternative(crate::port::AlternativeId),
    /// Give up the undo; what was reversed stays reversed and the request is released.
    Abandon,
}

/// The effect of a choice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChoiceOutcome {
    /// The alternative ran and counts as the entry's reversal; resume the undo to continue.
    Applied,
    /// The undo was abandoned and the request released.
    Abandoned,
    /// The alternative was refused too; a new decision is open.
    NeedsDecision(Decision),
}

/// Builds the plan from the file. Pure: it reads the graph and writes nothing.
pub(crate) fn build_plan(
    request: &CaseId,
    file: &RecordFile,
    boundary: usize,
    to: Option<&NodeId>,
    id: String,
    graph: &dyn GraphDependants,
) -> Result<UndoPlan> {
    let scope: Vec<&FileEntry> = file
        .entries
        .iter()
        .skip(boundary)
        .filter(|e| e.is_forward())
        .collect();
    let in_scope: BTreeSet<&RecordRef> = scope.iter().map(|e| &e.record).collect();

    // Dependants of every in-scope record, from the file's links and the graph.
    let mut dependants: BTreeMap<&RecordRef, Vec<RecordRef>> = BTreeMap::new();
    for record in &in_scope {
        let found =
            file.dependants(record, graph)
                .map_err(|code| EffectsError::GraphUnavailable {
                    request: request.as_str().to_owned(),
                    code: crate::port::stable_code(&code, "graph_error"),
                })?;
        dependants.insert(record, found);
    }

    // a must be reversed before b: a depends on b, or a is a later entry of the same record.
    let n = scope.len();
    let mut before: Vec<BTreeSet<usize>> = vec![BTreeSet::new(); n];
    for (b, eb) in scope.iter().enumerate() {
        let deps = dependants.get(&eb.record).map_or(&[][..], Vec::as_slice);
        for (a, ea) in scope.iter().enumerate() {
            if a == b {
                continue;
            }
            let depends = ea.links.contains(&eb.record)
                || (ea.record != eb.record && deps.contains(&ea.record))
                || (ea.record == eb.record && ea.seq > eb.seq);
            if depends {
                before[b].insert(a);
            }
        }
    }

    // Kahn, ties by reverse time (the latest entry first).
    let mut done = vec![false; n];
    let mut order = Vec::with_capacity(n);
    while order.len() < n {
        let next = (0..n)
            .filter(|&i| !done[i] && before[i].iter().all(|&a| done[a]))
            .max_by_key(|&i| scope[i].seq);
        let Some(i) = next else {
            return Err(EffectsError::DependencyCycle {
                request: request.as_str().to_owned(),
            });
        };
        done[i] = true;
        order.push(i);
    }

    let mut steps = Vec::new();
    let mut outside: BTreeMap<u64, Vec<RecordRef>> = BTreeMap::new();
    for i in order {
        let entry = scope[i];
        let paired_cancel = scope.iter().any(|c| {
            c.record == entry.record && c.seq > entry.seq && c.operation == Operation::Cancel
        });
        let paired_post = entry.operation == Operation::Cancel
            && scope.iter().any(|p| {
                p.record == entry.record && p.seq < entry.seq && p.operation == Operation::Post
            });
        let mut any = false;
        for (index, reversal) in entry.reversals.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                break;
            };
            if file.is_reversed(entry.seq, index) {
                continue;
            }
            any = true;
            let action = if paired_post {
                PlannedReversal::Skip(SkipReason::NothingToReverse)
            } else {
                match reversal {
                    EntryReversal::Lifecycle(Operation::Cancel) if paired_cancel => {
                        PlannedReversal::Skip(SkipReason::AlreadyReversedInRequest)
                    }
                    EntryReversal::Lifecycle(op) => PlannedReversal::Lifecycle(*op),
                    EntryReversal::Action(a) => PlannedReversal::Action(a.clone()),
                    EntryReversal::PointOfNoReturn => PlannedReversal::Blocked,
                }
            };
            steps.push(UndoStep {
                entry: entry.seq,
                index,
                node: entry.node.clone(),
                record: entry.record.clone(),
                action,
            });
        }
        if any {
            let outer: Vec<RecordRef> = dependants
                .get(&entry.record)
                .map(|d| {
                    d.iter()
                        .filter(|r| !in_scope.contains(r))
                        .take(MAX_DEPENDANTS)
                        .cloned()
                        .collect()
                })
                .unwrap_or_default();
            if !outer.is_empty() {
                outside.insert(entry.seq, outer);
            }
        }
    }

    let cancellation = file.waiting.clone().map(|wait| CancellationStep { wait });
    Ok(UndoPlan {
        request: request.clone(),
        to: to.cloned(),
        boundary,
        file_len: file.entries.len(),
        id,
        steps,
        outside,
        cancellation,
    })
}
