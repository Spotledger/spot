//! Process insight, part a (WP-BPM-11.T2): the feed from the engine's node-run transition log
//! into `spot-bpm-insight`, a durable checkpoint, access from the permission engine and bounded
//! metrics. Reports and the schedule that refreshes them are WP-BPM-11.T4.
//!
//! # What it does
//!
//! `spot-bpm-insight` folds node-run events into cycle time, waiting time, bottlenecks and stuck
//! Runs. This module feeds it from the **node-run transition log** (`NodeRunLog`, written by the
//! engine's store in the commit transaction, WP-BPM-11.T5 and T6): bounded batches are mapped to
//! [`NodeRunEvent`]s and folded, and the accumulator's serialisable state is saved with the log
//! position in **one compare-and-set checkpoint** (consumer `process_insight`). It reads no
//! payload and never `spot_bpm_node_outcome` or `spot_bpm_case`.
//!
//! # The port
//!
//! [`ProcessInsightPort`] is what the report module (WP-BPM-11.T4) consumes: `fold_once`,
//! `snapshot`, `snapshot_for`, `access` and `stuck`. No report and no schedule is declared here.
//!
//! # One leader folds, every node reads
//!
//! `fold_once` takes the module's lease (through the jobs module's [`OutboxLease`] port, the
//! coordinator the job worker uses). The holder folds; any other node adopts the checkpoint the
//! leader saved, so a snapshot is at most one fold interval old. A lease lost between reading and
//! saving drops the batch and degrades readiness until the next successful fold.
//!
//! # Memory is bounded
//!
//! The accumulator is O(steps x histogram buckets) plus O(open attempts) (the WP-BPM-11.T7 bound):
//! a fixed-bucket histogram per step, never a list of samples. Percentiles are nearest-rank bucket
//! values: deterministic integers, relative error below 3.2% above 31 ms. The log is not
//! unbounded either: retention is the operator's `prune_before`, and a rebuild from zero after
//! pruning covers only the retained window.
//!
//! # Access
//!
//! [`PermissionProcessAccess`] answers `ProcessAccess` from a [`PermissionSource`] (the permission
//! engine in production): a process the viewer may not read is **absent**, never a row of zeros,
//! and a source that errors denies. The stuck list shows a Run only to its initiator, the
//! assignee of the stuck node, or a holder of `process_supervise`, and only ids, node, reason
//! code and owner.
//!
//! # Configuration and metrics
//!
//! `[services.process_insight]`: `enabled` (`true`), `batch_size` (500, `1..=1000`),
//! `fold_interval_secs` (10, `1..=300`), `stuck_after_secs` (3600, `>= 1`); unknown keys are
//! refused. The module is inert when the process module is absent or disabled. Metrics carry the
//! **process name** as their only label, bounded to [`MAX_METRIC_PROCESSES`] names (every other
//! is `_other`); no run id, user id, node id or error text is ever a label.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use serde::Deserialize;
use spot_bpm_engine::{
    CaseId, CaseStore, Checkpoint, LoggedState, LoggedWait, NodeKind, NodeRunLog, NodeRunRecord,
    NodeRunSeq,
};
use spot_bpm_insight::{
    Accumulator, AccumulatorState, AttemptRef, Insight, Millis, NodeRunEvent, ProcessAccess,
    ProcessKey, RunState, StuckPolicy, StuckReason, WaitReason,
};
use spot_bpm_pg::PgCaseStore;
use spot_core::{Clock, SiteId};
use spot_dag::NodeId;
use spot_permissions::{PermissionEngine, Principal};
use time::OffsetDateTime;
use tokio::sync::watch;

use crate::services::ports::{OutboxLease, PortError};
use crate::services::process::ProcessHost;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result, error};

/// Start order: after the process engine (1100) and every module that provides a task owner
/// port, before the report module that consumes this one.
pub const ORDER: u32 = 1190;

/// The checkpoint consumer and the lease name (`[a-z0-9_.-]`, also valid as a lease consumer).
pub const CONSUMER: &str = "process_insight";

/// The failure code recorded for an attempt the Case's cancellation closed.
pub const CANCELLED_CODE: &str = "bpm_insight.cancelled";

/// The most batches one `fold_once` folds, so a long backlog never holds the lease for ever.
pub const MAX_BATCHES_PER_FOLD: u32 = 20;

/// The most distinct process names a metric label ever carries; every other name is `_other`.
pub const MAX_METRIC_PROCESSES: usize = 50;

/// The most stuck entries one `stuck` call returns.
pub const MAX_STUCK: usize = 1000;

/// The platform permission that lets its holder read every process's insight.
pub const PERM_READ: &str = "process_read_versions";

/// The platform permission that lets its holder see every stuck Run.
pub const PERM_SUPERVISE: &str = "process_supervise";

// ---------------------------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------------------------

/// `[services.process_insight]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the module does anything at all.
    pub enabled: bool,
    /// How many log records one batch reads (`1..=1000`).
    pub batch_size: u32,
    /// How often the fold runs, in seconds (`1..=300`).
    pub fold_interval_secs: u64,
    /// How long an attempt may stay in one state before it is stuck, in seconds (`>= 1`).
    pub stuck_after_secs: i64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            batch_size: 500,
            fold_interval_secs: 10,
            stuck_after_secs: 3600,
        }
    }
}

impl Settings {
    /// Names the first setting that is out of range.
    ///
    /// # Errors
    /// The key that is out of range.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if !(1..=1000).contains(&self.batch_size) {
            return Err("batch_size");
        }
        if !(1..=300).contains(&self.fold_interval_secs) {
            return Err("fold_interval_secs");
        }
        if self.stuck_after_secs < 1 {
            return Err("stuck_after_secs");
        }
        Ok(())
    }

    /// The stuck policy these settings give: one default expected duration for every node.
    #[must_use]
    pub fn stuck_policy(&self) -> StuckPolicy {
        StuckPolicy::new(Millis::from_seconds(self.stuck_after_secs))
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// Why a port call failed. A fixed code and sentence: never a value, an id or a driver message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsightPortError {
    /// The call named a site other than the one this module serves.
    WrongSite,
    /// The node-run log or the Case store failed.
    Store,
    /// The lease could not be taken or checked (not the same as another node holding it).
    Lease,
    /// A stored checkpoint does not decode: rebuild it deliberately, never silently.
    CorruptState,
    /// The lease was lost between reading a batch and saving it; the batch was dropped.
    LeaseLost,
}

impl InsightPortError {
    /// The stable code.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::WrongSite => "process_insight.wrong_site",
            Self::Store => "process_insight.store_failed",
            Self::Lease => "process_insight.lease_failed",
            Self::CorruptState => "process_insight.state_corrupt",
            Self::LeaseLost => "process_insight.lease_lost",
        }
    }
}

impl std::fmt::Display for InsightPortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::WrongSite => "the call names another site",
            Self::Store => "the node-run log could not be read",
            Self::Lease => "the insight lease could not be checked",
            Self::CorruptState => "the stored insight state does not decode",
            Self::LeaseLost => "the lease was lost; the batch was dropped",
        };
        write!(f, "{}: {text}", self.code())
    }
}

impl std::error::Error for InsightPortError {}

// ---------------------------------------------------------------------------------------------
// Seams: node kinds, task owners, permissions
// ---------------------------------------------------------------------------------------------

/// A boxed `Send` future.
pub type PortFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where a record's node kind comes from: the pinned process definition of its process version.
pub trait NodeKinds: Send + Sync {
    /// The kind of `node` in `process` version `version`, or `None` when the definition is not
    /// known (the record is then skipped and counted by process name).
    fn kind_of(&self, process: &str, version: u32, node: &NodeId) -> Option<NodeKind>;

    /// Makes the definitions available before the first fold (the host loads its registry).
    /// `Err` means not yet: the fold waits and tries again.
    fn prepare(&self) -> PortFut<'_, std::result::Result<(), ()>> {
        Box::pin(async { Ok(()) })
    }
}

/// The task inbox seen from insight: who owns the open task of a node. Optional: the stuck list
/// shows no owner, and shows a Run to no one as an assignee, when the port is absent or errors.
pub trait TaskOwners: Send + Sync {
    /// The assignee of the open human task of `node` in `case`, when there is exactly one
    /// answer. `Err(())` means the inbox could not answer.
    fn owner_of<'a>(
        &'a self,
        case: &'a str,
        node: &'a str,
    ) -> PortFut<'a, std::result::Result<Option<String>, ()>>;
}

/// The permission question insight asks. An `Err` is an unreadable source and means **no**.
pub trait PermissionSource: Send + Sync {
    /// Whether `principal` may read the runs of `process`.
    #[allow(clippy::result_unit_err)]
    fn may_read(&self, principal: &Principal, process: &str) -> std::result::Result<bool, ()>;
    /// Whether `principal` may see every stuck Run (`process_supervise`).
    #[allow(clippy::result_unit_err)]
    fn may_supervise(&self, principal: &Principal) -> std::result::Result<bool, ()>;
}

/// The production [`PermissionSource`]: the platform permissions of the site's permission engine.
/// `process_supervise` or `process_read_versions` read every process.
pub struct EnginePermissions(pub Arc<PermissionEngine>);

impl PermissionSource for EnginePermissions {
    fn may_read(&self, principal: &Principal, _process: &str) -> std::result::Result<bool, ()> {
        Ok(self.0.app_permission(principal, PERM_SUPERVISE).allowed
            || self.0.app_permission(principal, PERM_READ).allowed)
    }

    fn may_supervise(&self, principal: &Principal) -> std::result::Result<bool, ()> {
        Ok(self.0.app_permission(principal, PERM_SUPERVISE).allowed)
    }
}

/// `ProcessAccess` for one viewer over a [`PermissionSource`]. A process the viewer may not read
/// is absent; a source that fails answers no.
pub struct PermissionProcessAccess {
    source: Arc<dyn PermissionSource>,
    principal: Principal,
}

impl PermissionProcessAccess {
    /// The access of `principal`.
    #[must_use]
    pub fn new(source: Arc<dyn PermissionSource>, principal: Principal) -> Self {
        Self { source, principal }
    }
}

impl ProcessAccess for PermissionProcessAccess {
    fn may_read(&self, process: &ProcessKey) -> bool {
        self.source
            .may_read(&self.principal, &process.name)
            .unwrap_or(false)
    }
}

// ---------------------------------------------------------------------------------------------
// The port
// ---------------------------------------------------------------------------------------------

/// What one `fold_once` did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FoldReport {
    /// Records folded into the accumulator.
    pub folded: u64,
    /// Records skipped because their process version is unknown.
    pub skipped: u64,
    /// Batches saved.
    pub batches: u32,
    /// This node did not hold the lease and adopted the leader's checkpoint instead.
    pub standby: bool,
    /// The log position after the fold.
    pub position: i64,
}

/// One stuck Run as a viewer may see it: ids, node, reason code and owner only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StuckEntry {
    /// The process name.
    pub process: String,
    /// The process version.
    pub version: u32,
    /// The Run id.
    pub run: String,
    /// The node the Run is stuck at.
    pub node: String,
    /// The reason code: `waiting_for_task`, `timer`, `integration_retry` or `running`.
    pub reason: &'static str,
    /// The assignee, when the task inbox resolved one.
    pub owner: Option<String>,
}

/// A port future.
pub type InsightFuture<'a, T> =
    Pin<Box<dyn Future<Output = std::result::Result<T, InsightPortError>> + Send + 'a>>;

/// The one trait WP-BPM-11.T4 consumes.
pub trait ProcessInsightPort: Send + Sync {
    /// Folds one bounded run of batches under the lease. `now` is the instant of the call.
    fn fold_once(&self, now: OffsetDateTime) -> InsightFuture<'_, FoldReport>;

    /// The whole insight of the site, at the last fold. No viewer filter: only for a caller that
    /// filters itself; a report goes through [`snapshot_for`](Self::snapshot_for).
    ///
    /// # Errors
    /// [`InsightPortError::WrongSite`] when `site` is not this module's site.
    fn snapshot(&self, site: &SiteId) -> std::result::Result<Insight, InsightPortError>;

    /// The insight as `viewer` may see it: a process they may not read is absent.
    ///
    /// # Errors
    /// [`InsightPortError::WrongSite`] when `site` is not this module's site.
    fn snapshot_for(
        &self,
        site: &SiteId,
        viewer: &Principal,
    ) -> std::result::Result<Insight, InsightPortError>;

    /// `viewer`'s process-level read access.
    fn access(&self, viewer: &Principal) -> Box<dyn ProcessAccess + Send + Sync>;

    /// The stuck Runs at `now` that `viewer` may see: as initiator, as the assignee of the stuck
    /// node, or as a holder of `process_supervise`.
    fn stuck<'a>(
        &'a self,
        viewer: &'a Principal,
        now: OffsetDateTime,
    ) -> InsightFuture<'a, Vec<StuckEntry>>;
}

// ---------------------------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------------------------

/// Counter: process events folded into the insight read model.
const FOLDED_TOTAL: &str = "spot_bpm_insight_folded_total";
/// Counter: process events the insight fold skipped, by `process`.
const SKIPPED_TOTAL: &str = "spot_bpm_insight_skipped_total";
/// Gauge: open attempts of the process, by `process`.
const OPEN_ATTEMPTS: &str = "spot_bpm_insight_open_attempts";
/// Gauge: runs of the process that are stuck, by `process`.
const STUCK_RUNS: &str = "spot_bpm_insight_stuck_runs";

/// Bounds the process names a label may carry: the first [`MAX_METRIC_PROCESSES`] distinct
/// well-formed names, then `_other`.
#[derive(Debug, Default)]
pub struct MetricLabels {
    seen: Mutex<BTreeSet<String>>,
}

impl MetricLabels {
    /// The label to use for `process`.
    #[must_use]
    pub fn label(&self, process: &str) -> String {
        let well_formed = !process.is_empty()
            && process.len() <= 64
            && process
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
        if !well_formed {
            return "_other".to_owned();
        }
        let mut seen = self.seen.lock().unwrap_or_else(|p| p.into_inner());
        if seen.contains(process) {
            return process.to_owned();
        }
        if seen.len() < MAX_METRIC_PROCESSES {
            seen.insert(process.to_owned());
            return process.to_owned();
        }
        "_other".to_owned()
    }

    /// How many distinct names have been given out.
    #[must_use]
    pub fn distinct(&self) -> usize {
        self.seen.lock().unwrap_or_else(|p| p.into_inner()).len()
    }
}

/// A snapshot of the counters, for tests and readiness.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    /// Records folded since start.
    pub folded: u64,
    /// Records skipped since start (unknown process version).
    pub skipped: u64,
}

#[derive(Debug, Default)]
struct Metrics {
    folded: AtomicU64,
    skipped: AtomicU64,
}

// ---------------------------------------------------------------------------------------------
// Mapping a log record to an insight event
// ---------------------------------------------------------------------------------------------

fn wait_reason(wait: LoggedWait) -> WaitReason {
    match wait {
        LoggedWait::Timer => WaitReason::Timer,
        LoggedWait::Event => WaitReason::Event,
        LoggedWait::HumanTask => WaitReason::HumanTask,
        LoggedWait::IntegrationRetry => WaitReason::IntegrationRetry,
    }
}

/// The insight event a log record is, or `None` when its instant is out of range.
///
/// A cancelled attempt has no insight state of its own: it closes as a failure with
/// [`CANCELLED_CODE`], so it never lingers as a stuck Run. A `human` key is dropped here: the
/// owner is resolved at read time, never stored.
#[must_use]
pub fn event_of(record: &NodeRunRecord, kind: NodeKind) -> Option<NodeRunEvent> {
    let at = record.at.to_offset().ok()?;
    let attempt = AttemptRef::new(
        ProcessKey::new(record.dag_key.clone(), record.dag_version),
        record.case.clone(),
        record.node.clone(),
        record.attempt,
        kind,
    );
    let code = || {
        record
            .failure_code
            .clone()
            .unwrap_or_else(|| spot_bpm_engine::node_run_log::UNSPECIFIED_FAILURE_CODE.to_owned())
    };
    Some(match record.state {
        LoggedState::Started => NodeRunEvent::started(attempt, at),
        LoggedState::Waiting => match record.wait {
            Some(LoggedWait::IntegrationRetry) => NodeRunEvent::retrying(attempt, at, code()),
            Some(wait) => NodeRunEvent::waiting(attempt, at, wait_reason(wait)),
            None => NodeRunEvent::new(attempt, at, RunState::Waiting, None),
        },
        LoggedState::Done => NodeRunEvent::done(attempt, at),
        LoggedState::Failed => NodeRunEvent::failed(attempt, at, code()),
        LoggedState::Cancelled => NodeRunEvent::failed(attempt, at, CANCELLED_CODE),
    })
}

fn reason_code(reason: StuckReason) -> &'static str {
    match reason {
        StuckReason::WaitingForTask => "waiting_for_task",
        StuckReason::Timer => "timer",
        StuckReason::IntegrationRetry => "integration_retry",
        StuckReason::Running => "running",
    }
}

// ---------------------------------------------------------------------------------------------
// ProcessInsight
// ---------------------------------------------------------------------------------------------

/// The accumulator and the log position it has folded to.
struct Fold {
    acc: Accumulator,
    seq: NodeRunSeq,
    /// Whether `acc` and `seq` are known to match a checkpoint (or the empty start).
    loaded: bool,
}

/// The process insight of one site.
pub struct ProcessInsight {
    site: SiteId,
    log: Arc<dyn NodeRunLog>,
    cases: Arc<dyn CaseStore>,
    kinds: Arc<dyn NodeKinds>,
    outbox: Arc<dyn OutboxLease>,
    permissions: Arc<dyn PermissionSource>,
    owners: Option<Arc<dyn TaskOwners>>,
    settings: Settings,
    consumer: String,
    fold: tokio::sync::Mutex<Fold>,
    cache: RwLock<Arc<Insight>>,
    degraded: Mutex<Option<InsightPortError>>,
    metrics: Metrics,
    labels: MetricLabels,
}

impl ProcessInsight {
    /// An insight for `site` over its log, Case store, definition source, lease and permissions.
    #[must_use]
    pub fn new(
        site: SiteId,
        log: Arc<dyn NodeRunLog>,
        cases: Arc<dyn CaseStore>,
        kinds: Arc<dyn NodeKinds>,
        outbox: Arc<dyn OutboxLease>,
        permissions: Arc<dyn PermissionSource>,
        settings: Settings,
    ) -> Self {
        Self {
            site,
            log,
            cases,
            kinds,
            outbox,
            permissions,
            owners: None,
            settings,
            consumer: CONSUMER.to_owned(),
            fold: tokio::sync::Mutex::new(Fold {
                acc: Accumulator::new(),
                seq: NodeRunSeq(0),
                loaded: false,
            }),
            cache: RwLock::new(Arc::new(Accumulator::new().finish())),
            degraded: Mutex::new(None),
            metrics: Metrics::default(),
            labels: MetricLabels::default(),
        }
    }

    /// The same insight with a task-owner source.
    #[must_use]
    pub fn with_owners(mut self, owners: Arc<dyn TaskOwners>) -> Self {
        self.owners = Some(owners);
        self
    }

    /// The same insight folding under another checkpoint consumer name (a rebuild beside the
    /// live one). The name is `[a-z0-9_.-]{1,64}`.
    ///
    /// # Errors
    /// [`InsightPortError::Store`] for a name the log refuses.
    pub fn with_consumer(mut self, consumer: &str) -> std::result::Result<Self, InsightPortError> {
        spot_bpm_engine::validate_consumer(consumer).map_err(|_| InsightPortError::Store)?;
        self.consumer = consumer.to_owned();
        Ok(self)
    }

    /// The site this module serves.
    #[must_use]
    pub fn site(&self) -> &SiteId {
        &self.site
    }

    /// The counters.
    #[must_use]
    pub fn metrics(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            folded: self.metrics.folded.load(Ordering::Relaxed),
            skipped: self.metrics.skipped.load(Ordering::Relaxed),
        }
    }

    /// The metric label for a process name (bounded).
    #[must_use]
    pub fn metric_label(&self, process: &str) -> String {
        self.labels.label(process)
    }

    /// Why the module is degraded, when it is.
    #[must_use]
    pub fn degraded(&self) -> Option<InsightPortError> {
        *self.degraded.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The log position the in-memory accumulator has folded to.
    pub async fn position(&self) -> i64 {
        self.fold.lock().await.seq.0
    }

    /// The state of the accumulator, as it would be checkpointed.
    pub async fn export_state(&self) -> AccumulatorState {
        self.fold.lock().await.acc.export_state()
    }

    fn set_degraded(&self, value: Option<InsightPortError>) {
        *self.degraded.lock().unwrap_or_else(|p| p.into_inner()) = value;
    }

    fn publish(&self, acc: &Accumulator) {
        let insight = Arc::new(acc.clone().finish());
        *self.cache.write().unwrap_or_else(|p| p.into_inner()) = insight;
    }

    fn cached(&self) -> Arc<Insight> {
        Arc::clone(&self.cache.read().unwrap_or_else(|p| p.into_inner()))
    }

    /// Replaces the in-memory accumulator with the checkpoint's (or the empty one).
    fn restore(
        fold: &mut Fold,
        checkpoint: Option<Checkpoint>,
    ) -> std::result::Result<(), InsightPortError> {
        match checkpoint {
            None => {
                fold.acc = Accumulator::new();
                fold.seq = NodeRunSeq(0);
            }
            Some(cp) => {
                let state: AccumulatorState =
                    serde_json::from_value(cp.state).map_err(|_| InsightPortError::CorruptState)?;
                fold.acc =
                    Accumulator::from_state(state).map_err(|_| InsightPortError::CorruptState)?;
                fold.seq = cp.seq;
            }
        }
        fold.loaded = true;
        Ok(())
    }

    /// A node that does not lead adopts the leader's checkpoint when it is ahead of its own.
    async fn adopt_checkpoint(&self) -> std::result::Result<FoldReport, InsightPortError> {
        let checkpoint = self
            .log
            .load_checkpoint(&self.consumer)
            .await
            .map_err(|_| InsightPortError::Store)?;
        let mut fold = self.fold.lock().await;
        let ahead = checkpoint.as_ref().map_or(0, |c| c.seq.0);
        if !fold.loaded || ahead != fold.seq.0 {
            Self::restore(&mut fold, checkpoint)?;
            self.publish(&fold.acc);
        }
        Ok(FoldReport {
            standby: true,
            position: fold.seq.0,
            ..FoldReport::default()
        })
    }

    async fn fold_as_leader(
        &self,
        lease: &crate::services::ports::ConsumerLease,
        now: OffsetDateTime,
    ) -> std::result::Result<FoldReport, InsightPortError> {
        let mut fold = self.fold.lock().await;
        let checkpoint = self
            .log
            .load_checkpoint(&self.consumer)
            .await
            .map_err(|_| InsightPortError::Store)?;
        let expected = checkpoint.as_ref().map_or(NodeRunSeq(0), |c| c.seq);
        if !fold.loaded || fold.seq != expected {
            Self::restore(&mut fold, checkpoint)?;
        }
        let mut report = FoldReport::default();
        while report.batches < MAX_BATCHES_PER_FOLD {
            let records = self
                .log
                .list_after(fold.seq, self.settings.batch_size)
                .await
                .map_err(|_| InsightPortError::Store)?;
            let Some(last) = records.last().map(|r| r.seq) else {
                break;
            };
            let mut next = fold.acc.clone();
            let mut skipped: BTreeMap<String, u64> = BTreeMap::new();
            let mut folded = 0_u64;
            for record in &records {
                let event = self
                    .kinds
                    .kind_of(&record.dag_key, record.dag_version, &record.node)
                    .and_then(|kind| event_of(record, kind));
                match event {
                    Some(event) => {
                        next.push(event);
                        folded += 1;
                    }
                    None => *skipped.entry(record.dag_key.clone()).or_default() += 1,
                }
            }
            let state = serde_json::to_value(next.export_state())
                .map_err(|_| InsightPortError::CorruptState)?;
            match self.outbox.still_held(lease).await {
                Ok(true) => {}
                Ok(false) => {
                    fold.loaded = false;
                    return Err(InsightPortError::LeaseLost);
                }
                Err(_) => return Err(InsightPortError::Lease),
            }
            let saved = self
                .log
                .save_checkpoint(&self.consumer, fold.seq, Checkpoint { seq: last, state })
                .await
                .map_err(|_| InsightPortError::Store)?;
            if !saved {
                // Another writer moved the checkpoint: this batch is dropped, memory is reloaded
                // from the checkpoint on the next fold.
                fold.loaded = false;
                return Err(InsightPortError::LeaseLost);
            }
            fold.acc = next;
            fold.seq = last;
            report.folded += folded;
            report.batches += 1;
            self.metrics.folded.fetch_add(folded, Ordering::Relaxed);
            metrics::counter!(FOLDED_TOTAL).increment(folded);
            for (process, count) in skipped {
                report.skipped += count;
                self.metrics.skipped.fetch_add(count, Ordering::Relaxed);
                metrics::counter!(SKIPPED_TOTAL, "process" => self.labels.label(&process))
                    .increment(count);
            }
            if u64::try_from(records.len()).unwrap_or(u64::MAX)
                < u64::from(self.settings.batch_size)
            {
                break;
            }
        }
        report.position = fold.seq.0;
        self.publish(&fold.acc);
        self.set_gauges(now);
        Ok(report)
    }

    /// Open attempts and stuck Runs per process, as gauges labelled by (bounded) process name.
    fn set_gauges(&self, now: OffsetDateTime) {
        let insight = self.cached();
        let mut open: BTreeMap<String, u64> = BTreeMap::new();
        for attempt in insight.open_attempts() {
            *open
                .entry(self.labels.label(&attempt.process.name))
                .or_default() += 1;
        }
        let mut stuck: BTreeMap<String, u64> = open.keys().map(|k| (k.clone(), 0)).collect();
        for case in insight.stuck_cases(now, &self.settings.stuck_policy()) {
            *stuck
                .entry(self.labels.label(&case.process.name))
                .or_default() += 1;
        }
        for (process, count) in open {
            metrics::gauge!(OPEN_ATTEMPTS, "process" => process).set(count as f64);
        }
        for (process, count) in stuck {
            metrics::gauge!(STUCK_RUNS, "process" => process).set(count as f64);
        }
    }

    async fn visible_owner(&self, case: &str, node: &str) -> Option<String> {
        let owners = self.owners.as_ref()?;
        owners.owner_of(case, node).await.ok().flatten()
    }
}

impl ProcessInsightPort for ProcessInsight {
    fn fold_once(&self, now: OffsetDateTime) -> InsightFuture<'_, FoldReport> {
        Box::pin(async move {
            let lease = match self.outbox.acquire(CONSUMER).await {
                Ok(lease) => lease,
                Err(PortError::LeaseUnavailable) => return self.adopt_checkpoint().await,
                Err(_) => {
                    self.set_degraded(Some(InsightPortError::Lease));
                    return Err(InsightPortError::Lease);
                }
            };
            let result = self.fold_as_leader(&lease, now).await;
            let _ = self.outbox.release(&lease).await;
            self.set_degraded(result.as_ref().err().copied());
            result
        })
    }

    fn snapshot(&self, site: &SiteId) -> std::result::Result<Insight, InsightPortError> {
        if site != &self.site {
            return Err(InsightPortError::WrongSite);
        }
        Ok((*self.cached()).clone())
    }

    fn snapshot_for(
        &self,
        site: &SiteId,
        viewer: &Principal,
    ) -> std::result::Result<Insight, InsightPortError> {
        let insight = self.snapshot(site)?;
        Ok(insight.visible_to(self.access(viewer).as_ref()))
    }

    fn access(&self, viewer: &Principal) -> Box<dyn ProcessAccess + Send + Sync> {
        Box::new(PermissionProcessAccess::new(
            Arc::clone(&self.permissions),
            viewer.clone(),
        ))
    }

    fn stuck<'a>(
        &'a self,
        viewer: &'a Principal,
        now: OffsetDateTime,
    ) -> InsightFuture<'a, Vec<StuckEntry>> {
        Box::pin(async move {
            // A permission source that fails is "not a supervisor": fail closed.
            let supervisor = self.permissions.may_supervise(viewer).unwrap_or(false);
            let mut entries = Vec::new();
            for case in self
                .cached()
                .stuck_cases(now, &self.settings.stuck_policy())
            {
                if entries.len() >= MAX_STUCK {
                    break;
                }
                let run = case.case.to_string();
                let node = case.node.to_string();
                let owner = if case.reason == StuckReason::WaitingForTask {
                    self.visible_owner(&run, &node).await
                } else {
                    None
                };
                let is_owner =
                    !viewer.user.is_empty() && owner.as_deref() == Some(viewer.user.as_str());
                let visible = supervisor || is_owner || self.is_initiator(&case.case, viewer).await;
                if visible {
                    entries.push(StuckEntry {
                        process: case.process.name.clone(),
                        version: case.process.version,
                        run,
                        node,
                        reason: reason_code(case.reason),
                        owner,
                    });
                }
            }
            Ok(entries)
        })
    }
}

impl ProcessInsight {
    /// Whether `viewer` started `case`. A store that fails answers no.
    async fn is_initiator(&self, case: &CaseId, viewer: &Principal) -> bool {
        if viewer.user.is_empty() {
            return false;
        }
        match self.cases.load(case).await {
            Ok(Some(record)) => record.initiator.as_deref() == Some(viewer.user.as_str()),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The hosted service and register()
// ---------------------------------------------------------------------------------------------

/// The registry-backed [`NodeKinds`]: the pinned definition of a version in the engine's registry.
struct HostKinds(Arc<ProcessHost>);

impl NodeKinds for HostKinds {
    fn kind_of(&self, process: &str, version: u32, node: &NodeId) -> Option<NodeKind> {
        let engine = self.0.engine().ok()?;
        let definition = engine.processes().get(process, version)?;
        definition
            .version
            .nodes
            .iter()
            .find(|candidate| candidate.id.as_str() == node.as_str())
            .map(|candidate| NodeKind::from_model(&candidate.kind))
    }

    fn prepare(&self) -> PortFut<'_, std::result::Result<(), ()>> {
        Box::pin(async move {
            self.0
                .reload_definitions()
                .await
                .map(|_| ())
                .map_err(|_| ())
        })
    }
}

struct InsightService {
    insight: Arc<ProcessInsight>,
    interval: Duration,
    clock: Arc<dyn Clock>,
}

impl Service for InsightService {
    fn name(&self) -> &'static str {
        "process_insight"
    }

    fn level(&self) -> Level {
        // Insight is a read model: losing it degrades the site, it never stops documents.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            match self.insight.degraded() {
                None => Ok(()),
                Some(why) => Err(why.to_string()),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let mut prepared = false;
            loop {
                if *stop.borrow() {
                    return Ok(());
                }
                if !prepared {
                    prepared = self.insight.kinds.prepare().await.is_ok();
                }
                if prepared {
                    let now = self.clock.now_utc();
                    if let Err(e) = self.insight.fold_once(now).await {
                        tracing::warn!(code = e.code(), "the process insight fold failed");
                    }
                }
                tokio::select! {
                    changed = stop.changed() => {
                        if changed.is_err() || *stop.borrow() {
                            return Ok(());
                        }
                    }
                    () = tokio::time::sleep(self.interval) => {}
                }
            }
        })
    }
}

/// Registers process insight: its Case store handle, the insight port, and the fold service.
/// Inert when `services.process_insight.enabled = false`, and when the process module is absent.
///
/// # Errors
/// A setting that does not decode or is out of range; a port another module already provided;
/// the jobs module's lease port being absent.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_insight")?;
    settings
        .validate()
        .map_err(|key| Error(format!("services.process_insight.{key}: out of range")))?;
    if !settings.enabled || !ctx.assembling() {
        return Ok(());
    }
    let Some(host) = ctx.port::<ProcessHost>() else {
        return Ok(());
    };
    let env = *ctx.env()?;
    let site = SiteId::new(&env.site.site).map_err(error)?;
    let store = Arc::new(PgCaseStore::new(env.backend.pool()).with_tenant(site.as_str()));
    store.prepare().await.map_err(error)?;
    let outbox = ctx.require_port::<dyn OutboxLease>()?;
    let permissions: Arc<dyn PermissionSource> =
        Arc::new(EnginePermissions(Arc::clone(&env.permission.decision)));
    let interval = Duration::from_secs(settings.fold_interval_secs);
    let mut insight = ProcessInsight::new(
        site,
        Arc::clone(&store) as Arc<dyn NodeRunLog>,
        store as Arc<dyn CaseStore>,
        Arc::new(HostKinds(host)),
        outbox,
        permissions,
        settings,
    );
    if let Some(owners) = ctx.port::<dyn TaskOwners>() {
        insight = insight.with_owners(owners);
    }
    let insight = Arc::new(insight);
    ctx.provide::<dyn ProcessInsightPort>(Arc::clone(&insight) as Arc<dyn ProcessInsightPort>)?;
    ctx.add(Arc::new(InsightService {
        insight,
        interval,
        clock: Arc::clone(ctx.clock()),
    }));
    Ok(())
}
