//! The service: the per-request files, the request lock, rewind and the undo sequence.
//!
//! [`RequestEffects`] is the reference, in-memory implementation. A host that needs the files to
//! survive a restart stores the [`RecordFile`] snapshot ([`RequestEffects::file`]) with its
//! Case and loads it back with [`RequestEffects::restore`]; the journal and the lock live in the
//! file, so a restored file resumes an interrupted undo exactly.
//!
//! # The request lock
//!
//! An undo holds the request from its first call to its end: while it is held a forward step
//! ([`RequestEffects::record`]), a rewind and a second undo with a different plan are refused with
//! `bpm_effects.request_locked`. Only one caller runs the plan at a time: while an
//! [`RequestEffects::execute_undo`] or [`RequestEffects::choose`] is in progress, a second call
//! (even with the same plan) is refused with `request_locked`, so a double click or a retry after
//! a timeout cannot reverse a step twice. A crashed undo keeps the lock;
//! [`RequestEffects::execute_undo`] with the same plan resumes it, and
//! [`RequestEffects::abandon`] (or `Abandon` at an open decision) releases it.
//!
//! # Authorisation
//!
//! This crate checks only that the principal is well-formed. The host must authorise who may
//! start an undo, [`RequestEffects::choose`] and [`RequestEffects::abandon`] before calling them.
//!
//! # Cost
//!
//! [`RequestEffects::plan_undo`] orders up to 4,096 entries and is quadratic in the number of
//! entries; hosts should plan off the request path.
//!
//! # Time and principals
//!
//! Every method that stamps a time takes it as an argument; nothing reads a clock. Reversals run
//! as the principal in the [`UndoContext`], never as a system actor.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_app_api::prelude::OffsetDateTime;
use spot_app_api::{Operation, OperationReceipt};
use spot_bpm_engine::CaseId;
use spot_dag::NodeId;

use crate::declarations::{Declarations, derive_reversals};
use crate::error::{EffectsError, Result, codes};
use crate::graph::GraphDependants;
use crate::model::{
    AuditRecord, EntryReversal, FileEntry, JournalKey, MAX_ENTRIES, MAX_LINKS, MAX_PRINCIPAL_LEN,
    NewEntry, PartnerWait, RecordFile, RecordRef, Reverses, ReviseHow, Revision, UndoRun,
};
use crate::port::{
    CancellationCall, PartnerPort, PortError, Refusal, ReversalCall, ReversalPort, Via, bound,
    stable_code,
};
use crate::undo::{
    Blocker, Choice, ChoiceOutcome, Decision, PlannedReversal, UndoOutcome, UndoPlan, UndoStep,
    build_plan,
};

/// Most effect entries one receipt may carry (the engine's own bound).
const MAX_RECEIPT_EFFECTS: usize = 256;

/// What a rewind needs to know about the path the request will take next.
#[derive(Clone, Debug, Default)]
pub struct RewindPath {
    /// The nodes the revised path will run again.
    pub will_run: BTreeSet<NodeId>,
    /// For nodes that would create a record again: how the process declares the revision
    /// (rule 6.8). A node with no mapping that would create twice refuses the rewind.
    pub revisions: BTreeMap<NodeId, ReviseHow>,
}

/// What a rewind did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewindOutcome {
    /// How many entries the file keeps: all of them.
    pub kept_entries: usize,
    /// The nodes whose next pass revises an existing record, and which.
    pub revisions: Vec<(NodeId, Revision)>,
}

/// What an undo or a choice runs with.
#[derive(Clone, Copy)]
pub struct UndoContext<'a> {
    /// The acting principal: reversals run as this principal.
    pub principal: &'a str,
    /// The instant of the call.
    pub now: OffsetDateTime,
    /// The lifecycle.
    pub port: &'a dyn ReversalPort,
    /// The partner.
    pub partner: &'a dyn PartnerPort,
}

/// The per-request files and the operations on them.
pub struct RequestEffects {
    declarations: Arc<dyn Declarations>,
    files: Mutex<BTreeMap<CaseId, RecordFile>>,
}

impl RequestEffects {
    /// A service deriving reversals from `declarations`.
    #[must_use]
    pub fn new(declarations: Arc<dyn Declarations>) -> Self {
        Self {
            declarations,
            files: Mutex::new(BTreeMap::new()),
        }
    }

    fn guard(&self) -> MutexGuard<'_, BTreeMap<CaseId, RecordFile>> {
        self.files.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn with<R>(&self, request: &CaseId, f: impl FnOnce(&mut RecordFile) -> Result<R>) -> Result<R> {
        let mut files = self.guard();
        let file = files.get_mut(request).ok_or_else(|| unknown(request))?;
        f(file)
    }

    /// A snapshot of a request's file.
    ///
    /// # Errors
    /// `bpm_effects.request_unknown` when the request has no file.
    pub fn file(&self, request: &CaseId) -> Result<RecordFile> {
        self.with(request, |f| Ok(f.clone()))
    }

    /// Puts a stored file back (after a restart).
    ///
    /// The file is trusted: it is not validated, so the host must only restore snapshots taken by
    /// [`RequestEffects::file`] from its own store. The running flag is cleared, because no
    /// caller of this process is executing the undo; the lock and the journal are kept.
    pub fn restore(&self, request: &CaseId, mut file: RecordFile) {
        if let Some(run) = file.undo.as_mut() {
            run.running = false;
        }
        self.guard().insert(request.clone(), file);
    }

    /// Records the operation a node ran. Returns the entry's sequence number.
    ///
    /// When a rewind left a revision for this node, the entry's `revise_of` names the record it
    /// revises. A forward step is refused while an undo holds the request.
    ///
    /// # Errors
    /// `request_locked`, `entry_invalid` or `limit_exceeded`.
    pub fn record(&self, request: &CaseId, new: NewEntry) -> Result<u64> {
        let r = request.as_str();
        if new.receipt.doctype.is_empty() || new.receipt.name.is_empty() {
            return Err(invalid(r, "the receipt names no record"));
        }
        if new.receipt.effects.len() > MAX_RECEIPT_EFFECTS {
            return Err(limit(r, "effects in one receipt", MAX_RECEIPT_EFFECTS));
        }
        if new.links.len() > MAX_LINKS {
            return Err(limit(r, "links of one entry", MAX_LINKS));
        }
        let reversals = derive_reversals(self.declarations.as_ref(), &new.receipt);
        let mut files = self.guard();
        let file = files.entry(request.clone()).or_default();
        if file.is_locked() {
            return Err(locked(request));
        }
        if file.entries.iter().filter(|e| e.is_forward()).count() >= MAX_ENTRIES {
            return Err(limit(r, "entries in a request file", MAX_ENTRIES));
        }
        let seq = next_seq(file);
        let record = RecordRef::new(new.receipt.doctype.clone(), new.receipt.name.clone());
        let revise_of = file.revisions.remove(&new.node).map(|rev| rev.record);
        file.entries.push(FileEntry {
            seq,
            node: new.node.clone(),
            operation: new.receipt.operation,
            record,
            version: new.receipt.version,
            receipt: new.receipt,
            at: new.at,
            links: new.links,
            reversals,
            reverses: None,
            revise_of,
        });
        file.visits.push((new.node.clone(), file.entries.len()));
        file.position = Some(new.node);
        Ok(seq)
    }

    /// Notes that a node that wrote nothing has finished, so a rewind or undo can target it.
    ///
    /// # Errors
    /// `request_locked` while an undo holds the request.
    pub fn note_visit(&self, request: &CaseId, node: &NodeId) -> Result<()> {
        let mut files = self.guard();
        let file = files.entry(request.clone()).or_default();
        if file.is_locked() {
            return Err(locked(request));
        }
        file.visits.push((node.clone(), file.entries.len()));
        file.position = Some(node.clone());
        Ok(())
    }

    /// Records (or clears) what the request waits on from a partner.
    ///
    /// # Errors
    /// `request_locked` while an undo holds the request.
    pub fn set_waiting(&self, request: &CaseId, wait: Option<PartnerWait>) -> Result<()> {
        let mut files = self.guard();
        let file = files.entry(request.clone()).or_default();
        if file.is_locked() {
            return Err(locked(request));
        }
        file.waiting = wait;
        Ok(())
    }

    /// The revision the next pass of `node` makes, when a rewind left one: the Document step
    /// handler receives it as `revise_of`.
    #[must_use]
    pub fn revise_of(&self, request: &CaseId, node: &NodeId) -> Option<Revision> {
        self.guard()
            .get(request)
            .and_then(|f| f.revision_for(node).cloned())
    }

    /// Moves the request back to `to`, keeping every effect.
    ///
    /// The next pass revises, through `update` or `correct`, the records the nodes after `to`
    /// created. It refuses (rule 6.8) when the revised path would create such a record again and
    /// the process declares no revision for that node.
    ///
    /// # Errors
    /// `request_unknown`, `node_unknown`, `request_locked`, `rewind_would_duplicate`.
    pub fn rewind(
        &self,
        request: &CaseId,
        to: &NodeId,
        path: &RewindPath,
    ) -> Result<RewindOutcome> {
        self.with(request, |file| {
            if file.is_locked() {
                return Err(locked(request));
            }
            let at = last_visit(file, to).ok_or_else(|| node_unknown(request, to))?;
            let boundary = file.visits[at].1;
            let mut revisions = BTreeMap::new();
            for entry in file
                .entries
                .iter()
                .skip(boundary)
                .filter(|e| e.is_forward())
            {
                if entry.operation != Operation::Create || !path.will_run.contains(&entry.node) {
                    continue;
                }
                let Some(how) = path.revisions.get(&entry.node) else {
                    return Err(EffectsError::RewindWouldDuplicate {
                        request: request.as_str().to_owned(),
                        to: to.as_str().to_owned(),
                        node: entry.node.as_str().to_owned(),
                    });
                };
                revisions.insert(
                    entry.node.clone(),
                    Revision {
                        record: entry.record.clone(),
                        how: *how,
                    },
                );
            }
            file.visits.truncate(at + 1);
            file.position = Some(to.clone());
            file.revisions = revisions.clone();
            Ok(RewindOutcome {
                kept_entries: file.entries.len(),
                revisions: revisions.into_iter().collect(),
            })
        })
    }

    /// Plans the reversal of everything after `to` (`None` is the start of the request).
    ///
    /// # Errors
    /// `request_unknown`, `node_unknown`, `request_locked`, `dependency_cycle`,
    /// `graph_unavailable`.
    pub fn plan_undo(
        &self,
        request: &CaseId,
        to: Option<&NodeId>,
        graph: &dyn GraphDependants,
    ) -> Result<UndoPlan> {
        let file = self.file(request)?;
        if file.is_locked() {
            return Err(locked(request));
        }
        let boundary = match to {
            None => 0,
            Some(node) => {
                let at = last_visit(&file, node).ok_or_else(|| node_unknown(request, node))?;
                file.visits[at].1
            }
        };
        let id = format!(
            "undo:{}:{}:{}",
            request.as_str(),
            to.map_or("start", NodeId::as_str),
            file.entries.len()
        );
        build_plan(request, &file, boundary, to, id, graph)
    }

    /// The plan of an interrupted undo, rebuilt from the file: the same plan id, the steps that
    /// are still to do.
    ///
    /// # Errors
    /// `no_undo` when the request is not held by an undo.
    pub fn resume_plan(&self, request: &CaseId, graph: &dyn GraphDependants) -> Result<UndoPlan> {
        let file = self.file(request)?;
        let run = file.undo.clone().ok_or_else(|| EffectsError::NoUndo {
            request: request.as_str().to_owned(),
        })?;
        build_plan(
            request,
            &file,
            run.boundary,
            run.to.as_ref(),
            run.plan_id,
            graph,
        )
    }

    /// Runs the plan: each reversal through the lifecycle as the acting principal, idempotently.
    ///
    /// It can be called again with the same plan after a crash, a refusal that has been
    /// resolved, or a choice: the journal skips what is done. It stops at the first refusal with
    /// a [`Decision`], and never forces anything. A plan with a point of no return runs nothing.
    ///
    /// # Errors
    /// `request_locked`, `stale_plan`, `port_unavailable`, `cancellation_failed`,
    /// `entry_invalid`.
    pub fn execute_undo(&self, plan: &UndoPlan, ctx: &UndoContext<'_>) -> Result<UndoOutcome> {
        check_principal(&plan.request, ctx.principal)?;
        let _running = self.acquire(plan, ctx.principal)?;

        if plan.is_blocked() {
            let decision = blocked_decision(plan);
            self.with(&plan.request, |file| {
                if file.undo.as_ref().is_some_and(|r| r.journal.is_empty()) {
                    file.undo = None;
                }
                Ok(())
            })?;
            return Ok(UndoOutcome::NeedsDecision(decision));
        }

        if let Some(cancel) = plan.cancellation.as_ref().filter(|c| c.wait.before_local) {
            self.send_cancellation(plan, &cancel.wait, ctx)?;
        }

        for step in &plan.steps {
            let key = JournalKey::Reversal {
                entry: step.entry,
                index: step.index,
            };
            if self.journaled(&plan.request, key)? {
                continue;
            }
            let outcome = match &step.action {
                PlannedReversal::Skip(_) => {
                    self.with(&plan.request, |f| {
                        journal(f, key);
                        Ok(())
                    })?;
                    continue;
                }
                PlannedReversal::Blocked => {
                    return Ok(UndoOutcome::NeedsDecision(blocked_decision(plan)));
                }
                PlannedReversal::Lifecycle(op) => {
                    self.call(plan, step, *op, None, Via::Declared, ctx, false)
                }
                PlannedReversal::Action(action) => match self.entry_operation(plan, step) {
                    Some(operation) => self.call(
                        plan,
                        step,
                        operation,
                        Some(action),
                        Via::Declared,
                        ctx,
                        true,
                    ),
                    None => Err(PortError::Unavailable("file_changed".to_owned())),
                },
            };
            match outcome {
                Ok(receipt) => self.apply(&plan.request, step, receipt, ctx.now, false)?,
                Err(PortError::Refused(refusal)) => {
                    return Ok(UndoOutcome::NeedsDecision(self.stop(plan, step, &refusal)?));
                }
                Err(PortError::Unavailable(code)) => return Err(unavailable(&plan.request, &code)),
            }
        }

        if let Some(cancel) = plan.cancellation.as_ref().filter(|c| !c.wait.before_local) {
            self.send_cancellation(plan, &cancel.wait, ctx)?;
        }

        let (reversed, skipped) =
            plan.steps
                .iter()
                .fold((0, 0), |(r, s), step| match step.action {
                    PlannedReversal::Skip(_) => (r, s + 1),
                    _ => (r + 1, s),
                });
        self.finish(plan)?;
        Ok(UndoOutcome::Done { reversed, skipped })
    }

    /// Records a person's choice at an open decision. Picking is audited, before anything runs.
    ///
    /// # Errors
    /// `no_undo`, `no_such_decision`, `port_unavailable`.
    pub fn choose(
        &self,
        request: &CaseId,
        choice: &Choice,
        ctx: &UndoContext<'_>,
    ) -> Result<ChoiceOutcome> {
        check_principal(request, ctx.principal)?;
        let (decision, entry) = self.with(request, |file| {
            match file.undo.as_ref() {
                None => {
                    return Err(EffectsError::NoUndo {
                        request: request.as_str().to_owned(),
                    });
                }
                Some(run) if run.running => return Err(locked(request)),
                Some(_) => {}
            }
            let decision = file.pending.clone().ok_or_else(|| no_decision(request))?;
            let entry = file
                .entries
                .iter()
                .find(|e| e.seq == decision.entry)
                .cloned()
                .ok_or_else(|| no_decision(request))?;
            if let Some(run) = file.undo.as_mut() {
                run.running = true;
            }
            Ok((decision, entry))
        })?;
        let _running = RunGuard { svc: self, request };

        let alternative = match choice {
            Choice::Abandon => None,
            Choice::Alternative(id) => Some(
                decision
                    .alternatives
                    .iter()
                    .find(|a| &a.id == id)
                    .cloned()
                    .ok_or_else(|| no_decision(request))?,
            ),
        };
        self.with(request, |file| {
            file.audit.push(AuditRecord {
                entry: decision.entry,
                by: ctx.principal.to_owned(),
                at: ctx.now,
                alternative: alternative.as_ref().map(|a| a.id.as_str().to_owned()),
            });
            if alternative.is_none() {
                file.pending = None;
                file.undo = None;
            }
            Ok(())
        })?;
        let Some(alternative) = alternative else {
            return Ok(ChoiceOutcome::Abandoned);
        };

        let step = UndoStep {
            entry: decision.entry,
            index: decision.index,
            node: entry.node.clone(),
            record: entry.record.clone(),
            action: PlannedReversal::Blocked,
        };
        let plan = UndoPlan {
            request: request.clone(),
            to: None,
            boundary: 0,
            file_len: 0,
            id: String::new(),
            steps: Vec::new(),
            outside: BTreeMap::new(),
            cancellation: None,
        };
        let (operation, action) = match &alternative.kind {
            crate::port::AlternativeKind::Lifecycle(op) => (*op, None),
            crate::port::AlternativeKind::Action(a) => (entry.operation, Some(a.clone())),
        };
        let via = Via::Alternative(alternative.id.clone());
        let result = self.call(
            &plan,
            &step,
            operation,
            action.as_ref(),
            via,
            ctx,
            action.is_some(),
        );
        match result {
            Ok(receipt) => {
                self.apply(request, &step, receipt, ctx.now, action.is_none())?;
                Ok(ChoiceOutcome::Applied)
            }
            Err(PortError::Refused(refusal)) => Ok(ChoiceOutcome::NeedsDecision(
                self.stop(&plan, &step, &refusal)?,
            )),
            Err(PortError::Unavailable(code)) => Err(unavailable(request, &code)),
        }
    }

    // -- internals -------------------------------------------------------------------------

    /// Takes the request for one runner: refuses a different plan, a second runner of the same
    /// plan and a stale plan. The returned guard clears the running flag on every exit path.
    fn acquire<'a>(&'a self, plan: &'a UndoPlan, principal: &str) -> Result<RunGuard<'a>> {
        self.with(&plan.request, |file| match file.undo.as_mut() {
            Some(run) if run.plan_id != plan.id || run.running => Err(locked(&plan.request)),
            Some(run) => {
                run.running = true;
                Ok(())
            }
            None => {
                if file.entries.len() != plan.file_len {
                    return Err(EffectsError::StalePlan {
                        request: plan.request.as_str().to_owned(),
                    });
                }
                file.undo = Some(UndoRun {
                    plan_id: plan.id.clone(),
                    to: plan.to.clone(),
                    boundary: plan.boundary,
                    journal: BTreeSet::new(),
                    started_by: principal.to_owned(),
                    running: true,
                });
                Ok(())
            }
        })?;
        Ok(RunGuard {
            svc: self,
            request: &plan.request,
        })
    }

    /// Releases a held request without a pending decision: the way out of a lock left by a port
    /// that keeps failing (`port_unavailable`, `receipt_mismatch`, `file_changed`) or a partner
    /// cancellation that keeps failing. What was reversed stays reversed; nothing is deleted. The
    /// choice is audited (against the open decision's entry, or the last entry when none is
    /// open). The host must authorise the caller.
    ///
    /// # Errors
    /// `no_undo` when no undo holds the request, `request_locked` while a caller is running it,
    /// `entry_invalid` for a malformed principal.
    pub fn abandon(&self, request: &CaseId, ctx: &UndoContext<'_>) -> Result<()> {
        check_principal(request, ctx.principal)?;
        self.with(request, |file| {
            match file.undo.as_ref() {
                None => {
                    return Err(EffectsError::NoUndo {
                        request: request.as_str().to_owned(),
                    });
                }
                Some(run) if run.running => return Err(locked(request)),
                Some(_) => {}
            }
            let entry = file
                .pending
                .as_ref()
                .map(|d| d.entry)
                .or_else(|| file.entries.last().map(|e| e.seq))
                .unwrap_or(0);
            file.audit.push(AuditRecord {
                entry,
                by: ctx.principal.to_owned(),
                at: ctx.now,
                alternative: None,
            });
            file.pending = None;
            file.undo = None;
            Ok(())
        })
    }

    /// The operation the entry ran, when the file still has it.
    fn entry_operation(&self, plan: &UndoPlan, step: &UndoStep) -> Option<Operation> {
        self.guard().get(&plan.request).and_then(|f| {
            f.entries
                .iter()
                .find(|e| e.seq == step.entry)
                .map(|e| e.operation)
        })
    }

    fn journaled(&self, request: &CaseId, key: JournalKey) -> Result<bool> {
        self.with(request, |f| {
            Ok(f.undo.as_ref().is_some_and(|r| r.journal.contains(&key)))
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn call(
        &self,
        plan: &UndoPlan,
        step: &UndoStep,
        operation: Operation,
        action: Option<&spot_app_api::ActionRef>,
        via: Via,
        ctx: &UndoContext<'_>,
        compensate: bool,
    ) -> std::result::Result<OperationReceipt, PortError> {
        let expected = self
            .guard()
            .get(&plan.request)
            .and_then(|f| {
                f.entries
                    .iter()
                    .find(|e| e.seq == step.entry)
                    .map(|e| e.version)
            })
            .ok_or_else(|| PortError::Unavailable("file_changed".to_owned()))?;
        let key = match &via {
            Via::Declared => format!(
                "undo:{}:{}:{}",
                plan.request.as_str(),
                step.entry,
                step.index
            ),
            Via::Alternative(id) => format!(
                "undo:{}:{}:{}:alt:{}",
                plan.request.as_str(),
                step.entry,
                step.index,
                id.as_str()
            ),
        };
        let call = ReversalCall {
            request: &plan.request,
            record: &step.record,
            expected,
            operation,
            action,
            via,
            principal: ctx.principal,
            key,
            now: ctx.now,
        };
        let receipt = if compensate {
            ctx.port.compensate(&call)?
        } else {
            ctx.port.reverse(&call)?
        };
        // A lifecycle reversal acts on the record itself; a receipt for another record is not
        // what was asked, so fail closed instead of recording it as the reversal.
        if !compensate
            && matches!(call.via, Via::Declared)
            && (receipt.doctype != step.record.doctype || receipt.name != step.record.name)
        {
            return Err(PortError::Unavailable("receipt_mismatch".to_owned()));
        }
        Ok(receipt)
    }

    /// Records the reversing receipt as a new entry and marks the reversal done.
    fn apply(
        &self,
        request: &CaseId,
        step: &UndoStep,
        receipt: OperationReceipt,
        now: OffsetDateTime,
        _lifecycle: bool,
    ) -> Result<()> {
        self.with(request, |file| {
            let done = Reverses {
                entry: step.entry,
                index: step.index,
            };
            let key = JournalKey::Reversal {
                entry: step.entry,
                index: step.index,
            };
            // Already reversed (a retry whose first attempt got as far as the file): never push
            // a second reversing entry for the same reversal.
            if file
                .entries
                .iter()
                .any(|e| e.reverses.as_ref() == Some(&done))
            {
                journal(file, key);
                file.pending = None;
                return Ok(());
            }
            let seq = next_seq(file);
            file.entries.push(FileEntry {
                seq,
                node: step.node.clone(),
                operation: receipt.operation,
                record: RecordRef::new(receipt.doctype.clone(), receipt.name.clone()),
                version: receipt.version,
                receipt,
                at: now,
                links: BTreeSet::new(),
                reversals: Vec::<EntryReversal>::new(),
                reverses: Some(Reverses {
                    entry: step.entry,
                    index: step.index,
                }),
                revise_of: None,
            });
            journal(
                file,
                JournalKey::Reversal {
                    entry: step.entry,
                    index: step.index,
                },
            );
            file.pending = None;
            Ok(())
        })
    }

    /// Stops the sequence at a refusal: builds and keeps the decision.
    fn stop(&self, plan: &UndoPlan, step: &UndoStep, refusal: &Refusal) -> Result<Decision> {
        let (dependants, alternatives) = bound(refusal);
        let decision = Decision {
            entry: step.entry,
            index: step.index,
            blockers: vec![Blocker {
                record: step.record.clone(),
                reason_code: stable_code(&refusal.code, codes::REVERSAL_REFUSED),
                dependants,
            }],
            alternatives,
        };
        self.with(&plan.request, |file| {
            file.pending = Some(decision.clone());
            Ok(())
        })?;
        Ok(decision)
    }

    fn send_cancellation(
        &self,
        plan: &UndoPlan,
        wait: &PartnerWait,
        ctx: &UndoContext<'_>,
    ) -> Result<()> {
        if self.journaled(&plan.request, JournalKey::Cancellation)? {
            return Ok(());
        }
        ctx.partner
            .send_cancellation(&CancellationCall {
                request: &plan.request,
                conversation: &wait.conversation,
                message: &wait.cancellation,
                principal: ctx.principal,
                key: format!("undo:{}:cancel", plan.request.as_str()),
                now: ctx.now,
            })
            .map_err(|code| EffectsError::CancellationFailed {
                request: plan.request.as_str().to_owned(),
                code: stable_code(&code, "cancellation_error"),
            })?;
        self.with(&plan.request, |f| {
            journal(f, JournalKey::Cancellation);
            Ok(())
        })
    }

    fn finish(&self, plan: &UndoPlan) -> Result<()> {
        self.with(&plan.request, |file| {
            file.undo = None;
            file.pending = None;
            file.revisions.clear();
            if plan.cancellation.is_some() {
                file.waiting = None;
            }
            match &plan.to {
                None => {
                    file.visits.clear();
                    file.position = None;
                }
                Some(node) => {
                    if let Some(at) = last_visit(file, node) {
                        file.visits.truncate(at + 1);
                    }
                    file.position = Some(node.clone());
                }
            }
            Ok(())
        })
    }
}

/// Clears the running flag when a runner leaves, on every path (return, `?`, panic).
struct RunGuard<'a> {
    svc: &'a RequestEffects,
    request: &'a CaseId,
}

impl Drop for RunGuard<'_> {
    fn drop(&mut self) {
        if let Some(run) = self
            .svc
            .guard()
            .get_mut(self.request)
            .and_then(|f| f.undo.as_mut())
        {
            run.running = false;
        }
    }
}

fn blocked_decision(plan: &UndoPlan) -> Decision {
    let blocked: Vec<&UndoStep> = plan
        .steps
        .iter()
        .filter(|s| s.action == PlannedReversal::Blocked)
        .collect();
    let first = blocked.first();
    Decision {
        entry: first.map_or(0, |s| s.entry),
        index: first.map_or(0, |s| s.index),
        blockers: blocked
            .iter()
            .map(|s| Blocker {
                record: s.record.clone(),
                reason_code: codes::POINT_OF_NO_RETURN.to_owned(),
                dependants: Vec::new(),
            })
            .collect(),
        alternatives: Vec::new(),
    }
}

fn journal(file: &mut RecordFile, key: JournalKey) {
    if let Some(run) = file.undo.as_mut() {
        run.journal.insert(key);
    }
}

fn next_seq(file: &RecordFile) -> u64 {
    file.entries.last().map_or(0, |e| e.seq + 1)
}

fn last_visit(file: &RecordFile, node: &NodeId) -> Option<usize> {
    file.visits.iter().rposition(|(n, _)| n == node)
}

fn check_principal(request: &CaseId, principal: &str) -> Result<()> {
    if principal.is_empty()
        || principal.len() > MAX_PRINCIPAL_LEN
        || principal.chars().any(char::is_control)
    {
        return Err(invalid(
            request.as_str(),
            "the acting principal is empty or malformed",
        ));
    }
    Ok(())
}

fn unknown(request: &CaseId) -> EffectsError {
    EffectsError::RequestUnknown {
        request: request.as_str().to_owned(),
    }
}

fn locked(request: &CaseId) -> EffectsError {
    EffectsError::RequestLocked {
        request: request.as_str().to_owned(),
    }
}

fn node_unknown(request: &CaseId, node: &NodeId) -> EffectsError {
    EffectsError::NodeUnknown {
        request: request.as_str().to_owned(),
        node: node.as_str().to_owned(),
    }
}

fn no_decision(request: &CaseId) -> EffectsError {
    EffectsError::NoSuchDecision {
        request: request.as_str().to_owned(),
    }
}

fn invalid(request: &str, what: &'static str) -> EffectsError {
    EffectsError::EntryInvalid {
        request: request.to_owned(),
        what,
    }
}

fn limit(request: &str, what: &'static str, limit: usize) -> EffectsError {
    EffectsError::LimitExceeded {
        request: request.to_owned(),
        what,
        limit,
    }
}

fn unavailable(request: &CaseId, code: &str) -> EffectsError {
    EffectsError::PortUnavailable {
        request: request.as_str().to_owned(),
        code: stable_code(code, "port_error"),
    }
}
