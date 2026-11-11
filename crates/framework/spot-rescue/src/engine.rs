//! The supervised restore-and-re-entry engine.
//!
//! [`RescueEngine::run`] is the last-resort path of ADR-22:
//!
//! 1. require a two-person `restore` ticket from the approval seam;
//! 2. close the maintenance gate and keep it closed;
//! 3. restore the pre-migration snapshot at `T` (never deleting the post-migration database);
//! 4. check invariants on the restored old version;
//! 5. re-enter every operation committed after `T`, in causal order and in bounded batches,
//!    as an ordinary write through the old version's [`Reenter`] engine;
//! 6. check invariants again and open the gate only when that final pass is clean.
//!
//! A blocked item, a rejected item or a failing invariant halts the run and returns
//! [`RescueOutcome::Halted`] with a report. The original payload and principal of every item
//! are kept in the report, and nothing is deleted.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_core::SiteId;
use spot_migration_apply::{AdmissionGate, GateStatus, SnapshotId, SnapshotPort};
use spot_permissions::Principal;

use crate::BoxFuture;
use crate::codes;
use crate::error::{Error, Result};
use crate::faults::{Faults, KillPoint, NoFaults};
use crate::invariants::{InvariantPhase, InvariantReport, Invariants};
use crate::journal::{
    InMemoryRescueJournal, ItemKey, ItemOutcome, JournalRecord, RescueJournal, RunIdentity,
    RunProgress, Seq,
};
use crate::reentry::{
    Classification, OperationKey, OperationSource, PlanCounts, PlannedOperation, ReentryItem,
    ReentryPlan, ReentryPlanner,
};
use crate::ticket::{RestoreApprovals, RestoreTicket};

/// What the old version's engine answered for one item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum ReenterOutcome {
    /// The old engine validated, posted and committed the operation.
    Done,
    /// The old engine deliberately did not write it, with a stable reason.
    Skipped {
        /// The stable reason code.
        code: String,
        /// A secret-free explanation.
        detail: String,
    },
}

/// A failure the old engine reported for one item.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReenterError {
    /// The old version's stable error code.
    pub code: String,
    /// A secret-free explanation.
    pub detail: String,
}

impl ReenterError {
    /// A failure with `code` and `detail`.
    #[must_use]
    pub fn new(code: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for ReenterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

/// The old version's operation engine.
///
/// Every re-entered item is a normal operation through this trait: the old version runs its
/// usual validations, permissions and postings. A rescue never writes rows behind its back.
pub trait Reenter: Send + Sync {
    /// Re-enters one item.
    ///
    /// # Errors
    /// A [`ReenterError`] when the old version rejects the operation; the run stops and keeps
    /// the report.
    fn reenter<'a>(
        &'a self,
        item: &'a ReentryItem,
    ) -> BoxFuture<'a, std::result::Result<ReenterOutcome, ReenterError>>;
}

/// Everything one rescue run needs.
pub struct RescueRequest {
    /// The run id (unique per ADR-071 identity).
    pub run_id: String,
    /// The ADR-071 identity of the run.
    pub identity: RunIdentity,
    /// The snapshot to restore, when the run has one.
    pub snapshot: Option<SnapshotId>,
    /// The snapshot time `T`; operations strictly after it are re-entered.
    pub snapshot_at: i64,
    /// Unix microseconds; the engine reads no clock.
    pub now: i64,
    /// The outbox and document-history reader.
    pub source: Arc<dyn OperationSource>,
}

impl RescueRequest {
    /// A request for `snapshot_at` reading operations from `source`.
    #[must_use]
    pub fn new(
        run_id: impl Into<String>,
        identity: RunIdentity,
        snapshot: Option<SnapshotId>,
        snapshot_at: i64,
        now: i64,
        source: Arc<dyn OperationSource>,
    ) -> Self {
        Self {
            run_id: run_id.into(),
            identity,
            snapshot,
            snapshot_at,
            now,
            source,
        }
    }
}

/// How the run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RescueStatus {
    /// Every item was re-entered, the final invariants held and the gate opened.
    Completed,
    /// The run stopped; the gate stays closed and a person is needed.
    Halted,
}

/// Counts per outcome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeCounts {
    /// Operations in the plan.
    pub planned: u64,
    /// Operations that needed no transform.
    pub replayable: u64,
    /// Operations a declared transform prepared.
    pub needs_transform: u64,
    /// Operations the old version cannot express.
    pub blocked: u64,
    /// Items the old engine accepted.
    pub done: u64,
    /// Items the old engine deliberately skipped.
    pub skipped: u64,
    /// Items the old engine rejected.
    pub failed: u64,
    /// Planned items with no outcome yet.
    pub unattempted: u64,
}

/// One item in the report, with its original payload and principal intact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemReport {
    /// The item key.
    pub key: OperationKey,
    /// The DocType.
    pub doctype: String,
    /// The operation kind.
    pub kind: crate::reentry::OperationKind,
    /// The planner's classification.
    pub classification: Classification,
    /// The old engine's outcome, when the item ran.
    pub outcome: Option<ItemOutcome>,
    /// The payload exactly as the new version committed it.
    pub original_payload: serde_json::Value,
    /// The identity the operation committed under.
    pub principal: Principal,
}

/// An item that needs a person, with the data needed to decide.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NeedsPerson {
    /// The item key.
    pub key: OperationKey,
    /// The DocType.
    pub doctype: String,
    /// Why it needs a person.
    pub reason: String,
    /// The original payload, preserved.
    pub original_payload: serde_json::Value,
    /// The original principal.
    pub principal: Principal,
}

/// The report of one rescue attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RescueReport {
    /// The run id.
    pub run_id: String,
    /// The restored snapshot.
    pub snapshot: SnapshotId,
    /// The snapshot time.
    pub snapshot_at: i64,
    /// How the run ended.
    pub status: RescueStatus,
    /// Counts per outcome.
    pub counts: OutcomeCounts,
    /// Every planned item, in causal order.
    pub items: Vec<ItemReport>,
    /// Every item that needs a person.
    pub needs_person: Vec<NeedsPerson>,
    /// Every invariant pass of the attempt.
    pub invariants: Vec<InvariantReport>,
    /// Whether the gate is open (only a completed, verified run opens it).
    pub gate_open: bool,
}

/// What a rescue run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RescueOutcome {
    /// Every item re-entered, the final invariants held and the gate opened.
    Completed(RescueReport),
    /// The run stopped; the gate stayed closed.
    Halted(RescueReport),
}

impl RescueOutcome {
    /// The report, whichever way the run ended.
    #[must_use]
    pub fn report(&self) -> &RescueReport {
        match self {
            Self::Completed(report) | Self::Halted(report) => report,
        }
    }
}

/// The report fields assembled at each exit.
struct ReportData<'a> {
    request: &'a RescueRequest,
    snapshot: &'a SnapshotId,
    plan: &'a ReentryPlan,
    status: RescueStatus,
    outcomes: &'a BTreeMap<ItemKey, ItemOutcome>,
    invariants: &'a [InvariantReport],
    gate_open: bool,
}

/// The supervised restore-and-re-entry engine for one site.
pub struct RescueEngine {
    site: SiteId,
    journal: Arc<dyn RescueJournal>,
    gate: Arc<dyn AdmissionGate>,
    snapshot: Arc<dyn SnapshotPort>,
    approvals: Arc<dyn RestoreApprovals>,
    reenter: Arc<dyn Reenter>,
    invariants: Arc<dyn Invariants>,
    planner: ReentryPlanner,
    faults: Arc<dyn Faults>,
    batch_size: usize,
}

impl RescueEngine {
    /// An engine over the rescue collaborators, with the reference in-memory planner defaults.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        site: SiteId,
        journal: Arc<dyn RescueJournal>,
        gate: Arc<dyn AdmissionGate>,
        snapshot: Arc<dyn SnapshotPort>,
        approvals: Arc<dyn RestoreApprovals>,
        reenter: Arc<dyn Reenter>,
        invariants: Arc<dyn Invariants>,
    ) -> Self {
        Self {
            site,
            journal,
            gate,
            snapshot,
            approvals,
            reenter,
            invariants,
            planner: ReentryPlanner::default(),
            faults: Arc::new(NoFaults),
            batch_size: 100,
        }
    }

    /// The same engine with `planner`.
    #[must_use]
    pub fn with_planner(mut self, planner: ReentryPlanner) -> Self {
        self.planner = planner;
        self
    }

    /// The same engine with a fault-injection switch.
    #[must_use]
    pub fn with_faults(mut self, faults: Arc<dyn Faults>) -> Self {
        self.faults = faults;
        self
    }

    /// The same engine with this many items between checkpoints (at least 1).
    #[must_use]
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size.max(1);
        self
    }

    /// A convenience constructor over the reference in-memory journal.
    #[must_use]
    pub fn with_memory_journal(
        site: SiteId,
        gate: Arc<dyn AdmissionGate>,
        snapshot: Arc<dyn SnapshotPort>,
        approvals: Arc<dyn RestoreApprovals>,
        reenter: Arc<dyn Reenter>,
        invariants: Arc<dyn Invariants>,
    ) -> (Self, Arc<InMemoryRescueJournal>) {
        let journal = Arc::new(InMemoryRescueJournal::new());
        let engine = Self::new(
            site,
            journal.clone(),
            gate,
            snapshot,
            approvals,
            reenter,
            invariants,
        );
        (engine, journal)
    }

    /// Runs `request`.
    ///
    /// # Errors
    /// A missing or mismatched ticket, no snapshot, a snapshot/gate/journal failure, or a kill
    /// at a fault-injection boundary. Blocked items, rejected items and failing invariants are
    /// [`RescueOutcome::Halted`] reports, because the run stopped safely and kept the data.
    pub async fn run(&self, request: RescueRequest) -> Result<RescueOutcome> {
        if request.run_id.trim().is_empty() {
            return Err(Error::InvalidInput {
                what: "run id".to_owned(),
                reason: "must not be empty".to_owned(),
            });
        }
        let Some(snapshot) = request.snapshot.clone() else {
            return Err(Error::NoSnapshot);
        };
        let progress = RunProgress::fold(self.journal.entries().await?);
        let operations = request.source.operations_after(request.snapshot_at).await?;
        let plan = self
            .planner
            .plan(request.snapshot_at, &request.run_id, &operations)?;
        if progress.started {
            self.check_same_run(&progress, &request, &snapshot)?;
            if progress.gate_open {
                let invariants = progress.invariants.clone();
                return Ok(RescueOutcome::Completed(self.report(ReportData {
                    request: &request,
                    snapshot: &snapshot,
                    plan: &plan,
                    status: RescueStatus::Completed,
                    outcomes: &progress.outcomes,
                    invariants: &invariants,
                    gate_open: self.gate.status().is_open(),
                })));
            }
            if progress.halted.is_some() {
                let invariants = progress.invariants.clone();
                let report = self.report(ReportData {
                    request: &request,
                    snapshot: &snapshot,
                    plan: &plan,
                    status: RescueStatus::Halted,
                    outcomes: &progress.outcomes,
                    invariants: &invariants,
                    gate_open: self.gate.status().is_open(),
                });
                return Ok(RescueOutcome::Halted(report));
            }
            return self.resume(request, snapshot, plan, progress).await;
        }

        // A fresh run needs the two-person restore ticket before it journals anything.
        let ticket = self.require_ticket(&request).await?;
        let mut seq = progress.next_seq();
        self.write(
            &mut seq,
            request.now,
            JournalRecord::Started {
                run_id: request.run_id.clone(),
                site: self.site.to_string(),
                identity: request.identity.clone(),
                snapshot: snapshot.to_string(),
                snapshot_at: request.snapshot_at,
                attempt: 1,
                ticket_id: ticket.id().to_owned(),
                approvers: ticket.approvers().to_vec(),
            },
        )
        .await?;
        self.run_gated(request, snapshot, plan, 1, progress, seq)
            .await
    }

    /// Resumes an unfinished run as attempt + 1.
    async fn resume(
        &self,
        request: RescueRequest,
        snapshot: SnapshotId,
        plan: ReentryPlan,
        progress: RunProgress,
    ) -> Result<RescueOutcome> {
        let attempt = progress.attempt.saturating_add(1);
        let mut seq = progress.next_seq();
        self.write(&mut seq, request.now, JournalRecord::Resumed { attempt })
            .await?;
        self.run_gated(request, snapshot, plan, attempt, progress, seq)
            .await
    }

    /// Closes the gate, restores, re-enters and reopens.
    async fn run_gated(
        &self,
        request: RescueRequest,
        snapshot: SnapshotId,
        plan: ReentryPlan,
        attempt: u32,
        progress: RunProgress,
        mut seq: Seq,
    ) -> Result<RescueOutcome> {
        self.write(&mut seq, request.now, JournalRecord::GateClosing)
            .await?;
        self.gate
            .close("a migration rescue is running", request.now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        self.write(&mut seq, request.now, JournalRecord::GateClosed)
            .await?;

        let mut invariants = progress.invariants.clone();
        let mut outcomes = progress.outcomes.clone();

        if progress.restored.is_none() {
            self.faults.at(&KillPoint::BeforeRestore)?;
            let exists =
                self.snapshot
                    .exists(&snapshot)
                    .await
                    .map_err(|error| Error::SnapshotFailed {
                        message: error.to_string(),
                    })?;
            if !exists {
                return Err(Error::SnapshotFailed {
                    message: format!("snapshot {snapshot} was not taken"),
                });
            }
            self.write(
                &mut seq,
                request.now,
                JournalRecord::RestoreStarted {
                    snapshot: snapshot.to_string(),
                },
            )
            .await?;
            let restored = self
                .snapshot
                .restore(&snapshot, request.now)
                .await
                .map_err(|error| Error::SnapshotFailed {
                    message: error.to_string(),
                })?;
            self.write(
                &mut seq,
                request.now,
                JournalRecord::Restored {
                    snapshot: restored.snapshot.to_string(),
                    location: restored.location,
                    at: restored.at,
                },
            )
            .await?;
        }
        self.faults.at(&KillPoint::AfterRestore)?;

        if !has_passed(&invariants, InvariantPhase::AfterRestore) {
            let report = self.invariants.check(InvariantPhase::AfterRestore).await?;
            self.write(
                &mut seq,
                request.now,
                JournalRecord::InvariantsChecked {
                    report: report.clone(),
                },
            )
            .await?;
            if !report.passed() {
                self.write(
                    &mut seq,
                    request.now,
                    JournalRecord::Halted {
                        code: codes::INVARIANTS_FAILED.to_owned(),
                        reason: "the restored old version failed its invariants".to_owned(),
                    },
                )
                .await?;
                invariants.push(report);
                return Ok(RescueOutcome::Halted(self.report(ReportData {
                    request: &request,
                    snapshot: &snapshot,
                    plan: &plan,
                    status: RescueStatus::Halted,
                    outcomes: &outcomes,
                    invariants: &invariants,
                    gate_open: false,
                })));
            }
            invariants.push(report);
        }

        let mut since_checkpoint = 0usize;
        for (index, planned) in plan.items.iter().enumerate() {
            let key = planned.key();
            if outcomes.contains_key(&key) {
                continue;
            }
            if let Classification::Blocked { code, detail } = &planned.classification {
                let reason = format!("{code}: {detail}");
                self.write(
                    &mut seq,
                    request.now,
                    JournalRecord::Halted {
                        code: codes::BLOCKED.to_owned(),
                        reason: reason.clone(),
                    },
                )
                .await?;
                return Ok(RescueOutcome::Halted(self.report(ReportData {
                    request: &request,
                    snapshot: &snapshot,
                    plan: &plan,
                    status: RescueStatus::Halted,
                    outcomes: &outcomes,
                    invariants: &invariants,
                    gate_open: false,
                })));
            }
            self.faults.at(&KillPoint::BeforeItem(index))?;
            let item = planned.item(&request.run_id, attempt);
            let outcome = match self.reenter.reenter(&item).await {
                Ok(ReenterOutcome::Done) => ItemOutcome::Done,
                Ok(ReenterOutcome::Skipped { code, detail }) => ItemOutcome::skipped(code, detail),
                Err(error) => {
                    let failed = ItemOutcome::failed(codes::REENTRY_FAILED, error.to_string());
                    self.write(
                        &mut seq,
                        request.now,
                        JournalRecord::ItemOutcome {
                            key: key.clone(),
                            outcome: failed.clone(),
                        },
                    )
                    .await?;
                    outcomes.insert(key, failed);
                    self.write(
                        &mut seq,
                        request.now,
                        JournalRecord::Halted {
                            code: codes::REENTRY_FAILED.to_owned(),
                            reason: error.to_string(),
                        },
                    )
                    .await?;
                    return Ok(RescueOutcome::Halted(self.report(ReportData {
                        request: &request,
                        snapshot: &snapshot,
                        plan: &plan,
                        status: RescueStatus::Halted,
                        outcomes: &outcomes,
                        invariants: &invariants,
                        gate_open: false,
                    })));
                }
            };
            self.write(
                &mut seq,
                request.now,
                JournalRecord::ItemOutcome {
                    key: key.clone(),
                    outcome: outcome.clone(),
                },
            )
            .await?;
            outcomes.insert(key.clone(), outcome);
            since_checkpoint += 1;
            self.faults.at(&KillPoint::AfterItem(index))?;
            if since_checkpoint >= self.batch_size {
                self.checkpoint(&mut seq, request.now, &outcomes, Some(key))
                    .await?;
                since_checkpoint = 0;
            }
        }
        if since_checkpoint > 0 {
            let last = plan.items.last().map(PlannedOperation::key);
            self.checkpoint(&mut seq, request.now, &outcomes, last)
                .await?;
        }

        self.faults.at(&KillPoint::BeforeVerify)?;
        if !has_passed(&invariants, InvariantPhase::AfterReentry) {
            let report = self.invariants.check(InvariantPhase::AfterReentry).await?;
            self.write(
                &mut seq,
                request.now,
                JournalRecord::InvariantsChecked {
                    report: report.clone(),
                },
            )
            .await?;
            if !report.passed() {
                self.write(
                    &mut seq,
                    request.now,
                    JournalRecord::Halted {
                        code: codes::INVARIANTS_FAILED.to_owned(),
                        reason: "the re-entered state failed its final invariants".to_owned(),
                    },
                )
                .await?;
                invariants.push(report);
                return Ok(RescueOutcome::Halted(self.report(ReportData {
                    request: &request,
                    snapshot: &snapshot,
                    plan: &plan,
                    status: RescueStatus::Halted,
                    outcomes: &outcomes,
                    invariants: &invariants,
                    gate_open: false,
                })));
            }
            invariants.push(report);
        }
        self.faults.at(&KillPoint::AfterVerify)?;

        self.gate
            .open(request.now)
            .await
            .map_err(|error| Error::GateFailed {
                message: error.to_string(),
            })?;
        self.write(&mut seq, request.now, JournalRecord::GateOpened)
            .await?;
        Ok(RescueOutcome::Completed(self.report(ReportData {
            request: &request,
            snapshot: &snapshot,
            plan: &plan,
            status: RescueStatus::Completed,
            outcomes: &outcomes,
            invariants: &invariants,
            gate_open: self.gate.status().is_open(),
        })))
    }

    /// Obtains and validates the two-person restore ticket.
    async fn require_ticket(&self, request: &RescueRequest) -> Result<RestoreTicket> {
        let Some(ticket) = self
            .approvals
            .restore_ticket(request.snapshot_at, request.now)
            .await?
        else {
            return Err(Error::NoTicket);
        };
        if ticket.approvers().len() < 2 {
            return Err(Error::NotTwoPerson {
                approvers: ticket.approvers().len(),
            });
        }
        if !ticket.authorises(request.snapshot_at) {
            return Err(Error::TicketMismatch {
                ticket: ticket.snapshot_at(),
                snapshot_at: request.snapshot_at,
            });
        }
        Ok(ticket)
    }

    /// Refuses a journal that belongs to a different identity or snapshot.
    fn check_same_run(
        &self,
        progress: &RunProgress,
        request: &RescueRequest,
        snapshot: &SnapshotId,
    ) -> Result<()> {
        let same_identity = progress.identity.as_ref() == Some(&request.identity);
        let same_snapshot = progress.snapshot.as_deref() == Some(snapshot.as_str());
        let same_time = progress.snapshot_at == Some(request.snapshot_at);
        if same_identity && same_snapshot && same_time {
            Ok(())
        } else {
            let found = format!(
                "run {} snapshot {} at {}",
                progress
                    .identity
                    .as_ref()
                    .map(|identity| identity.instance_key.as_str())
                    .unwrap_or("<none>"),
                progress.snapshot.as_deref().unwrap_or("<none>"),
                progress
                    .snapshot_at
                    .map(|at| at.to_string())
                    .unwrap_or_else(|| "<none>".to_owned())
            );
            Err(Error::RunMismatch {
                found,
                expected: format!(
                    "run {} snapshot {} at {}",
                    request.identity.instance_key, snapshot, request.snapshot_at
                ),
            })
        }
    }

    /// Writes one journal record under `seq` and advances the sequence number.
    async fn write(&self, seq: &mut Seq, at: i64, record: JournalRecord) -> Result<()> {
        self.journal.append(*seq, record, at).await?;
        *seq += 1;
        Ok(())
    }

    /// Writes a batch checkpoint.
    async fn checkpoint(
        &self,
        seq: &mut Seq,
        at: i64,
        outcomes: &BTreeMap<ItemKey, ItemOutcome>,
        last_key: Option<OperationKey>,
    ) -> Result<()> {
        let done = outcomes
            .values()
            .filter(|outcome| matches!(outcome, ItemOutcome::Done))
            .count() as u64;
        let skipped = outcomes
            .values()
            .filter(|outcome| matches!(outcome, ItemOutcome::Skipped { .. }))
            .count() as u64;
        let failed = outcomes
            .values()
            .filter(|outcome| matches!(outcome, ItemOutcome::Failed { .. }))
            .count() as u64;
        self.write(
            seq,
            at,
            JournalRecord::Checkpoint {
                processed: outcomes.len() as u64,
                done,
                skipped,
                failed,
                last_key,
            },
        )
        .await
    }

    /// Builds the report from a plan and the outcomes so far.
    fn report(&self, data: ReportData<'_>) -> RescueReport {
        let PlanCounts {
            replayable,
            needs_transform,
            blocked,
        } = data.plan.counts();
        let mut items = Vec::with_capacity(data.plan.items.len());
        let mut needs_person = Vec::new();
        let mut counts = OutcomeCounts {
            planned: data.plan.items.len() as u64,
            replayable,
            needs_transform,
            blocked,
            ..OutcomeCounts::default()
        };
        for planned in &data.plan.items {
            let key = planned.key();
            let outcome = data.outcomes.get(&key).cloned();
            match &outcome {
                Some(ItemOutcome::Done) => counts.done += 1,
                Some(ItemOutcome::Skipped { .. }) => counts.skipped += 1,
                Some(ItemOutcome::Failed { .. }) => counts.failed += 1,
                None => counts.unattempted += 1,
            }
            if let Classification::Blocked { code, detail } = &planned.classification {
                needs_person.push(NeedsPerson {
                    key: key.clone(),
                    doctype: planned.operation.doctype.clone(),
                    reason: format!("{code}: {detail}"),
                    original_payload: planned.operation.payload.clone(),
                    principal: planned.operation.principal.clone(),
                });
            }
            if let Some(ItemOutcome::Failed { code, detail }) = &outcome {
                needs_person.push(NeedsPerson {
                    key: key.clone(),
                    doctype: planned.operation.doctype.clone(),
                    reason: format!("{code}: {detail}"),
                    original_payload: planned.operation.payload.clone(),
                    principal: planned.operation.principal.clone(),
                });
            }
            items.push(ItemReport {
                key,
                doctype: planned.operation.doctype.clone(),
                kind: planned.operation.kind,
                classification: planned.classification.clone(),
                outcome,
                original_payload: planned.operation.payload.clone(),
                principal: planned.operation.principal.clone(),
            });
        }
        let gate_open = data.gate_open && matches!(self.gate.status(), GateStatus::Open);
        RescueReport {
            run_id: data.request.run_id.clone(),
            snapshot: data.snapshot.clone(),
            snapshot_at: data.request.snapshot_at,
            status: data.status,
            counts,
            items,
            needs_person,
            invariants: data.invariants.to_vec(),
            gate_open,
        }
    }
}

/// Whether `reports` contains a passed pass for `phase`.
fn has_passed(reports: &[InvariantReport], phase: InvariantPhase) -> bool {
    reports
        .iter()
        .any(|report| report.phase == Some(phase) && report.passed())
}
