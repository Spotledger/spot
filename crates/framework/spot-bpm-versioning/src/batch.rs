//! Bulk Case migration as an ADR-071 batch run.
//!
//! A bulk migration is not a loop over Cases: it is a **run** with an identity, a status, a
//! per-item outcome recorded with the item's effect, a resumable cursor and the same
//! "start twice" rules an operator expects.
//!
//! - [`BatchRequest`] names the identifying parameters — the process, the two versions and the
//!   exact Case scope. Their canonical hash is the run's `instance_key`; the same scope cannot
//!   be started twice once it completed, and a paused or failed run is resumed rather than
//!   restarted.
//! - [`BatchLedger`] holds the runs and enforces that identity rule. It deliberately records the
//!   same state ADR-071 describes for the platform's `batch_run` row, without pretending to be
//!   that DocType.
//! - [`BulkMigrator`] drives a run: it plans nothing (the caller supplies the plans), applies
//!   each plan through [`MigrationTarget`], records `done`, `skipped` or `failed` with the error
//!   code, advances the checkpoint, honours a cooperative stop and resumes from the outcomes.
//!
//! Item outcomes are the truth; the checkpoint only saves re-scanning, exactly as ADR-071
//! requires. Time and the actor are arguments, never read here.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use sha2::{Digest, Sha256};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::CaseId;

use crate::error::{Result, VersioningError};
use crate::migration::{MigrationPlan, MigrationTarget};

/// The batch kind token of a bulk Case migration.
pub const MIGRATION_BATCH_KIND: &str = "bpm_case_migration";

/// Where a batch run stands (ADR-071 section 1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchStatus {
    /// Created, not driven yet.
    #[default]
    Planned,
    /// Items may be processed.
    Running,
    /// A stop was asked for; the current item commits before the run pauses.
    Stopping,
    /// Stopped and resumable.
    Paused,
    /// Every item has an outcome and none failed.
    Succeeded,
    /// Every item has an outcome; some were skipped.
    SucceededWithSkips,
    /// An item failed and the policy stopped the run.
    Failed,
    /// An operator gave the identity up; the run is kept for its history.
    Abandoned,
}

impl BatchStatus {
    /// The token the audit trail and reports spell.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Paused => "paused",
            Self::Succeeded => "succeeded",
            Self::SucceededWithSkips => "succeeded_with_skips",
            Self::Failed => "failed",
            Self::Abandoned => "abandoned",
        }
    }

    /// Whether the run is finished (successfully or not).
    #[must_use]
    pub fn is_finished(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::SucceededWithSkips | Self::Failed | Self::Abandoned
        )
    }
}

/// What happened to one item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    /// The migration applied.
    Done,
    /// The item was left out, with a code and a plain-language reason.
    Skipped,
    /// The migration failed the run.
    Failed,
}

/// One item's outcome. It is recorded in the same transaction as the migration's effect by a
/// host that implements the target port over its database.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemOutcome {
    /// What happened.
    pub state: ItemState,
    /// The stable code when the item was skipped or failed.
    pub code: Option<String>,
    /// A plain-language reason.
    pub detail: Option<String>,
    /// The attempt that produced the outcome (starts at 1).
    pub attempt: u32,
}

impl ItemOutcome {
    /// A `done` outcome.
    #[must_use]
    pub fn done(attempt: u32) -> Self {
        Self {
            state: ItemState::Done,
            code: None,
            detail: None,
            attempt,
        }
    }

    /// A `skipped` outcome.
    #[must_use]
    pub fn skipped(code: impl Into<String>, detail: impl Into<String>, attempt: u32) -> Self {
        Self {
            state: ItemState::Skipped,
            code: Some(code.into()),
            detail: Some(detail.into()),
            attempt,
        }
    }

    /// A `failed` outcome.
    #[must_use]
    pub fn failed(code: impl Into<String>, detail: impl Into<String>, attempt: u32) -> Self {
        Self {
            state: ItemState::Failed,
            code: Some(code.into()),
            detail: Some(detail.into()),
            attempt,
        }
    }
}

/// One Case in the run's stable item order.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchItem {
    /// The Case.
    pub case: CaseId,
    /// Its outcome, once processed.
    pub outcome: Option<ItemOutcome>,
}

/// The run's counters (ADR-071 section 1).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchCounters {
    /// Items presented to the process step.
    pub read: u64,
    /// Items that migrated.
    pub done: u64,
    /// Items that were skipped.
    pub skipped: u64,
    /// Items that failed.
    pub failed: u64,
}

/// What a caller asks a bulk migration to cover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRequest {
    /// The migration kind's version.
    pub kind_version: u32,
    /// The process name.
    pub process: String,
    /// The version the Cases should pin.
    pub from_version: u32,
    /// The version they move to.
    pub to_version: u32,
    /// The Case scope. Order is irrelevant; the run sorts and deduplicates it.
    pub cases: Vec<CaseId>,
    /// Whether this is a planning run only.
    pub dry_run: bool,
    /// The human or service that started it.
    pub actor: Option<String>,
}

impl BatchRequest {
    /// The stable identity parameters: everything that makes this one scope.
    #[must_use]
    pub fn identity(&self) -> Json {
        let mut cases: Vec<String> = self.cases.iter().map(ToString::to_string).collect();
        cases.sort();
        cases.dedup();
        serde_json::json!({
            "kind": MIGRATION_BATCH_KIND,
            "kind_version": self.kind_version,
            "process": self.process,
            "from_version": self.from_version,
            "to_version": self.to_version,
            "cases": cases,
        })
    }

    /// The non-identifying parameters.
    #[must_use]
    pub fn other(&self) -> Json {
        serde_json::json!({
            "dry_run": self.dry_run,
            "actor": self.actor,
        })
    }

    /// The SHA-256 identity key over the canonical identity parameters.
    #[must_use]
    pub fn instance_key(&self) -> String {
        let bytes = serde_json::to_vec(&self.identity()).unwrap_or_default();
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hex(&hasher.finalize())
    }

    /// The scope in stable order, without duplicates.
    #[must_use]
    pub fn ordered_cases(&self) -> Vec<CaseId> {
        let mut cases: Vec<CaseId> = self.cases.clone();
        cases.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        cases.dedup();
        cases
    }
}

/// One batch run: its identity, its items and its checkpoint.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BatchRun {
    /// The run's id.
    pub id: String,
    /// The kind token.
    pub kind: String,
    /// The kind's version at start.
    pub kind_version: u32,
    /// The SHA-256 identity key.
    pub instance_key: String,
    /// The identifying parameters.
    pub params_identity: Json,
    /// The non-identifying parameters.
    pub params_other: Json,
    /// Where the run stands.
    pub status: BatchStatus,
    /// The attempt number (starts at 1).
    pub attempt: u32,
    /// Who started it.
    pub actor: Option<String>,
    /// The counters.
    pub counters: BatchCounters,
    /// The cursor of the current step; outcomes remain the truth.
    pub checkpoint: usize,
    /// The items in stable order.
    pub items: Vec<BatchItem>,
    /// When it started.
    pub started_at: OffsetDateTime,
    /// When it ended, once finished.
    pub ended_at: Option<OffsetDateTime>,
    /// When the last item committed.
    pub last_progress_at: Option<OffsetDateTime>,
}

impl BatchRun {
    /// How many items have an outcome.
    #[must_use]
    pub fn processed(&self) -> usize {
        self.items
            .iter()
            .filter(|item| item.outcome.is_some())
            .count()
    }

    /// The next item without an outcome at or after the checkpoint.
    #[must_use]
    pub fn next_item(&self) -> Option<usize> {
        self.items
            .iter()
            .enumerate()
            .skip(self.checkpoint)
            .find(|(_, item)| item.outcome.is_none())
            .map(|(index, _)| index)
    }
}

/// The batch runs a host holds.
#[derive(Debug, Default)]
pub struct BatchLedger {
    inner: Mutex<LedgerInner>,
}

#[derive(Debug, Default)]
struct LedgerInner {
    by_id: BTreeMap<String, BatchRun>,
    by_identity: BTreeMap<String, String>,
}

impl BatchLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, LedgerInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Starts a run, or refuses because the identity already has one (ADR-071 section 2).
    ///
    /// # Errors
    /// [`VersioningError::BatchAlreadyCompleted`], [`VersioningError::BatchRestartRequired`] or
    /// [`VersioningError::BatchInProgress`].
    pub fn start(&self, request: &BatchRequest, now: OffsetDateTime) -> Result<BatchRun> {
        let instance = request.instance_key();
        let mut inner = self.lock();
        if let Some(id) = inner.by_identity.get(&instance)
            && let Some(existing) = inner.by_id.get(id)
        {
            return match existing.status {
                BatchStatus::Succeeded | BatchStatus::SucceededWithSkips => {
                    Err(VersioningError::BatchAlreadyCompleted { instance })
                }
                BatchStatus::Failed | BatchStatus::Paused | BatchStatus::Stopping => {
                    Err(VersioningError::BatchRestartRequired {
                        instance,
                        status: existing.status.token().to_owned(),
                    })
                }
                BatchStatus::Running | BatchStatus::Planned => {
                    Err(VersioningError::BatchInProgress { instance })
                }
                BatchStatus::Abandoned => {
                    drop(inner);
                    self.start(request, now)
                }
            };
        }
        let id = format!("mig_{}", &instance[..16]);
        let items = request
            .ordered_cases()
            .into_iter()
            .map(|case| BatchItem {
                case,
                outcome: None,
            })
            .collect();
        let run = BatchRun {
            id: id.clone(),
            kind: MIGRATION_BATCH_KIND.to_owned(),
            kind_version: request.kind_version,
            instance_key: instance.clone(),
            params_identity: request.identity(),
            params_other: request.other(),
            status: BatchStatus::Planned,
            attempt: 1,
            actor: request.actor.clone(),
            counters: BatchCounters::default(),
            checkpoint: 0,
            items,
            started_at: now,
            ended_at: None,
            last_progress_at: Some(now),
        };
        inner.by_identity.insert(instance, id.clone());
        inner.by_id.insert(id, run.clone());
        Ok(run)
    }

    /// The run with this id.
    #[must_use]
    pub fn load(&self, id: &str) -> Option<BatchRun> {
        self.lock().by_id.get(id).cloned()
    }

    /// Asks a running run to stop after the current item.
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`] or [`VersioningError::BatchNotRunning`].
    pub fn stop(&self, id: &str) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        if run.status != BatchStatus::Running {
            return Err(VersioningError::BatchNotRunning {
                id: id.to_owned(),
                status: run.status.token().to_owned(),
            });
        }
        run.status = BatchStatus::Stopping;
        Ok(run.clone())
    }

    /// Gives an identity up; the run is kept for its history (ADR-071 section 2).
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`].
    pub fn abandon(&self, id: &str) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        run.status = BatchStatus::Abandoned;
        let instance = run.instance_key.clone();
        let out = run.clone();
        inner.by_identity.remove(&instance);
        Ok(out)
    }

    /// Moves a paused, stopping or failed run back to running for another attempt.
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`] or [`VersioningError::BatchNotRunning`].
    pub fn resume(&self, id: &str, now: OffsetDateTime) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        match run.status {
            BatchStatus::Paused | BatchStatus::Stopping | BatchStatus::Failed => {
                run.status = BatchStatus::Running;
                run.attempt = run.attempt.saturating_add(1);
                run.ended_at = None;
                run.last_progress_at = Some(now);
                Ok(run.clone())
            }
            other => Err(VersioningError::BatchNotRunning {
                id: id.to_owned(),
                status: other.token().to_owned(),
            }),
        }
    }

    /// Resets failed items so the next attempt processes them again.
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`] or [`VersioningError::BatchNotRunning`].
    pub fn retry_failed(&self, id: &str, now: OffsetDateTime) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        if run.status != BatchStatus::Failed {
            return Err(VersioningError::BatchNotRunning {
                id: id.to_owned(),
                status: run.status.token().to_owned(),
            });
        }
        run.status = BatchStatus::Running;
        run.attempt = run.attempt.saturating_add(1);
        run.ended_at = None;
        run.last_progress_at = Some(now);
        let mut first = None;
        for (index, item) in run.items.iter_mut().enumerate() {
            if item
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.state == ItemState::Failed)
            {
                item.outcome = None;
                first.get_or_insert(index);
            }
        }
        if let Some(index) = first {
            run.checkpoint = index;
        }
        Ok(run.clone())
    }

    /// Sets a run's status for the driver.
    fn transition(&self, id: &str, status: BatchStatus, now: OffsetDateTime) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        run.status = status;
        run.last_progress_at = Some(now);
        if status.is_finished() {
            run.ended_at = Some(now);
        }
        Ok(run.clone())
    }

    /// Records one item outcome and moves the checkpoint.
    fn record(
        &self,
        id: &str,
        index: usize,
        outcome: ItemOutcome,
        checkpoint_every: usize,
        now: OffsetDateTime,
    ) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        let Some(item) = run.items.get_mut(index) else {
            return Err(VersioningError::StoreFailed {
                detail: "the item index is outside the run's scope".to_owned(),
            });
        };
        item.outcome = Some(outcome.clone());
        run.counters.read = run.counters.read.saturating_add(1);
        match outcome.state {
            ItemState::Done => run.counters.done = run.counters.done.saturating_add(1),
            ItemState::Skipped => run.counters.skipped = run.counters.skipped.saturating_add(1),
            ItemState::Failed => run.counters.failed = run.counters.failed.saturating_add(1),
        }
        run.last_progress_at = Some(now);
        let every = checkpoint_every.max(1);
        if index + 1 >= run.checkpoint + every || index + 1 == run.items.len() {
            run.checkpoint = (index + 1).min(run.items.len());
        }
        Ok(run.clone())
    }

    fn set_checkpoint(&self, id: &str, checkpoint: usize, now: OffsetDateTime) -> Result<BatchRun> {
        let mut inner = self.lock();
        let run = inner
            .by_id
            .get_mut(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        run.checkpoint = checkpoint.min(run.items.len());
        run.last_progress_at = Some(now);
        Ok(run.clone())
    }
}

/// How a driver finishes a run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchProgress {
    /// The run id.
    pub id: String,
    /// Its status after the turn.
    pub status: BatchStatus,
    /// Items processed in this turn.
    pub processed: usize,
    /// The counters after the turn.
    pub counters: BatchCounters,
    /// The checkpoint after the turn.
    pub checkpoint: usize,
}

/// The bulk migration driver.
pub struct BulkMigrator<'a> {
    /// Where runs live.
    pub ledger: &'a BatchLedger,
    /// Where a Case migration is stored.
    pub target: &'a dyn MigrationTarget,
    /// The plans, by Case. A Case without one is skipped or fails per the policy.
    pub plans: BTreeMap<CaseId, MigrationPlan>,
    /// How many items between checkpoint writes.
    pub checkpoint_every: usize,
    /// Whether a Case without a plan is skipped (`true`) or fails the run (`false`).
    pub skip_missing_plans: bool,
    /// Whether a failed item stops the run.
    pub stop_on_failure: bool,
}

impl<'a> BulkMigrator<'a> {
    /// A driver with the ADR-071 defaults: checkpoint every 100 items, a missing plan is a
    /// skip, and a failed item stops the run.
    #[must_use]
    pub fn new(ledger: &'a BatchLedger, target: &'a dyn MigrationTarget) -> Self {
        Self {
            ledger,
            target,
            plans: BTreeMap::new(),
            checkpoint_every: 100,
            skip_missing_plans: true,
            stop_on_failure: true,
        }
    }

    /// Adds one Case's plan.
    #[must_use]
    pub fn with_plan(mut self, plan: MigrationPlan) -> Self {
        self.plans.insert(plan.case().clone(), plan);
        self
    }

    /// Drives a run from its checkpoint, at most `limit` items this turn.
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`] or [`VersioningError::BatchNotRunning`].
    pub fn run(
        &self,
        id: &str,
        now: OffsetDateTime,
        limit: Option<usize>,
    ) -> Result<BatchProgress> {
        let mut run = self
            .ledger
            .load(id)
            .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
        if run.status == BatchStatus::Planned {
            run = self.ledger.transition(id, BatchStatus::Running, now)?;
        }
        if run.status != BatchStatus::Running {
            return Err(VersioningError::BatchNotRunning {
                id: id.to_owned(),
                status: run.status.token().to_owned(),
            });
        }
        let actor = run.actor.clone();
        let mut processed = 0;
        loop {
            if limit.is_some_and(|limit| processed >= limit) {
                break;
            }
            run = self
                .ledger
                .load(id)
                .ok_or_else(|| VersioningError::BatchUnknown { id: id.to_owned() })?;
            if run.status == BatchStatus::Stopping {
                run = self.ledger.transition(id, BatchStatus::Paused, now)?;
                break;
            }
            let Some(index) = run.next_item() else {
                let status = if run.counters.failed > 0 {
                    BatchStatus::Failed
                } else if run.counters.skipped > 0 {
                    BatchStatus::SucceededWithSkips
                } else {
                    BatchStatus::Succeeded
                };
                run = self.ledger.transition(id, status, now)?;
                break;
            };
            let case = run.items[index].case.clone();
            let attempt = run.attempt;
            let outcome = match self.plans.get(&case) {
                Some(plan) => match plan.apply(self.target, now, actor.as_deref()) {
                    Ok(_) => ItemOutcome::done(attempt),
                    Err(error) => {
                        let outcome = ItemOutcome::failed(error.code(), error.to_string(), attempt);
                        run = self
                            .ledger
                            .record(id, index, outcome, self.checkpoint_every, now)?;
                        processed += 1;
                        if self.stop_on_failure {
                            run = self.ledger.transition(id, BatchStatus::Failed, now)?;
                        }
                        if self.stop_on_failure {
                            break;
                        }
                        continue;
                    }
                },
                None if self.skip_missing_plans => ItemOutcome::skipped(
                    "bpm_versioning.plan_missing",
                    format!("case {case} has no migration plan in this batch"),
                    attempt,
                ),
                None => ItemOutcome::failed(
                    "bpm_versioning.plan_missing",
                    format!("case {case} has no migration plan in this batch"),
                    attempt,
                ),
            };
            let failed = outcome.state == ItemState::Failed;
            run = self
                .ledger
                .record(id, index, outcome, self.checkpoint_every, now)?;
            processed += 1;
            if failed && self.stop_on_failure {
                run = self.ledger.transition(id, BatchStatus::Failed, now)?;
                break;
            }
        }
        if run.status.is_finished() {
            run = self.ledger.set_checkpoint(id, run.items.len(), now)?;
        }
        Ok(BatchProgress {
            id: run.id.clone(),
            status: run.status,
            processed,
            counters: run.counters.clone(),
            checkpoint: run.checkpoint,
        })
    }

    /// Resumes a paused, stopping or failed run and drives it.
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`] or [`VersioningError::BatchNotRunning`].
    pub fn resume(
        &self,
        id: &str,
        now: OffsetDateTime,
        limit: Option<usize>,
    ) -> Result<BatchProgress> {
        self.ledger.resume(id, now)?;
        self.run(id, now, limit)
    }

    /// Resets failed items and drives the run again.
    ///
    /// # Errors
    /// [`VersioningError::BatchUnknown`] or [`VersioningError::BatchNotRunning`].
    pub fn retry_failed(
        &self,
        id: &str,
        now: OffsetDateTime,
        limit: Option<usize>,
    ) -> Result<BatchProgress> {
        self.ledger.retry_failed(id, now)?;
        self.run(id, now, limit)
    }
}

/// Lowercase hex of a byte slice.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn request() -> BatchRequest {
        BatchRequest {
            kind_version: 1,
            process: "order_to_cash".to_owned(),
            from_version: 1,
            to_version: 2,
            cases: vec![
                CaseId::new("case_b").unwrap(),
                CaseId::new("case_a").unwrap(),
            ],
            dry_run: false,
            actor: None,
        }
    }

    #[test]
    fn the_identity_is_order_independent() {
        let first = request();
        let mut second = request();
        second.cases.reverse();
        assert_eq!(first.instance_key(), second.instance_key());
        assert_eq!(
            second
                .ordered_cases()
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["case_a", "case_b"]
        );
    }

    #[test]
    fn a_batch_status_has_a_token() {
        assert_eq!(
            BatchStatus::SucceededWithSkips.token(),
            "succeeded_with_skips"
        );
        assert!(BatchStatus::Failed.is_finished());
        assert!(!BatchStatus::Running.is_finished());
    }
}
