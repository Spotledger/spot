//! The seams a site (or a test) fills in, and the module's counters.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_bpm_tasks::{Directory, SlaMetrics, TaskId};
use spot_http::api::ports::task::TaskPortError;
use spot_http::api::service::{BoxFuture, RequestCtx};
use spot_permissions::PermissionEngine;
use time::OffsetDateTime;

use super::AUDIT_TARGET;

/// Counter: task notices sent.
const NOTICES_SENT_TOTAL: &str = "spot_bpm_task_notices_sent_total";
/// Counter: task notices that failed to send.
const NOTICES_FAILED_TOTAL: &str = "spot_bpm_task_notices_failed_total";
/// Counter: task notices dropped.
const NOTICES_DROPPED_TOTAL: &str = "spot_bpm_task_notices_dropped_total";
/// Counter: tasks approved automatically.
const AUTO_APPROVED_TOTAL: &str = "spot_bpm_task_auto_approved_total";
/// Counter: task SLA ticks run.
const SLA_TICKS_TOTAL: &str = "spot_bpm_task_sla_ticks_total";
/// Counter: task SLA ticks that failed.
const SLA_TICK_FAILURES_TOTAL: &str = "spot_bpm_task_sla_tick_failures_total";

/// Who a request's credential names.
pub trait Callers: Send + Sync {
    /// The caller's user id, or why the request cannot proceed: [`TaskPortError::Unauthenticated`]
    /// for a missing or refused credential, [`TaskPortError::PermissionDenied`] for a session that
    /// may only choose a password, [`TaskPortError::Unavailable`] when sign-in cannot answer.
    fn caller<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<String, TaskPortError>>;
}

/// The directory the inbox reads: role members, managers, direct reports, a user's roles and
/// whether a user is active. Every method fails closed (nobody, `None`, `false`). The methods are
/// synchronous and are only called from plain threads.
pub trait TaskDirectory: Directory {
    /// Whether `user` is an active user. An unknown or unreadable user is not active.
    fn is_active(&self, user: &str) -> bool;
    /// The active users whose manager is `manager`; `None` when the directory cannot be read.
    fn reports_of(&self, manager: &str) -> Option<Vec<String>>;
    /// The names of the roles `user` holds now; `None` when the directory cannot be read.
    fn roles_of(&self, user: &str) -> Option<Vec<String>>;
}

/// The permission engine the site serves right now, read per decision so a swap is seen at once.
pub trait DecisionSource: Send + Sync {
    /// The engine, or `None` while the site has none (every read of a document is then denied).
    fn engine(&self) -> Option<Arc<PermissionEngine>>;
}

/// One task notice, as handed to the [`TaskNoticeSink`]. It carries ids and the task's own title;
/// it never carries a document value or a comment.
#[derive(Clone, Debug, PartialEq)]
pub struct TaskNotice {
    /// `assigned`, `reminder`, `escalated`, `delegated`, `reassigned`, `completed` or
    /// `auto_approved`.
    pub kind: &'static str,
    /// The task id.
    pub task: String,
    /// The Case (the Run) the task belongs to.
    pub case: String,
    /// The Decision node.
    pub node: String,
    /// The task's title.
    pub title: String,
    /// The singular name of a Run in this site's language.
    pub run_singular: String,
    /// When the task is due, if it has a due time.
    pub due: Option<OffsetDateTime>,
    /// The users the notice is for.
    pub recipients: Vec<String>,
}

/// Where task notices go: the notice feed or the mail service, provided by the module that owns
/// either. Optional: with none provided notices are counted as dropped and nothing else happens.
pub trait TaskNoticeSink: Send + Sync {
    /// Delivers one notice. An `Err` (or a panic) is counted and never fails the action.
    fn deliver<'a>(
        &'a self,
        notice: &'a TaskNotice,
    ) -> BoxFuture<'a, std::result::Result<(), String>>;
}

// ---------------------------------------------------------------------------------------------
// Counters.
// ---------------------------------------------------------------------------------------------

/// A snapshot of the module's counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StatsSnapshot {
    /// Notices a sink accepted.
    pub notices_sent: u64,
    /// Notices a sink refused or lost.
    pub notices_failed: u64,
    /// Notices dropped because no sink is provided.
    pub notices_dropped: u64,
    /// Tasks the SLA approved on its own.
    pub auto_approved: u64,
    /// SLA checks run.
    pub ticks: u64,
    /// SLA checks that ended in an error.
    pub tick_failures: u64,
}

/// The counters. Label-free: no task id, user or process name reaches a metric.
#[derive(Debug, Default)]
pub(super) struct Stats {
    notices_sent: AtomicU64,
    notices_failed: AtomicU64,
    notices_dropped: AtomicU64,
    auto_approved: AtomicU64,
    ticks: AtomicU64,
    tick_failures: AtomicU64,
}

impl Stats {
    pub(super) fn notice_sent(&self) {
        self.notices_sent.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(NOTICES_SENT_TOTAL).increment(1);
    }
    pub(super) fn notice_failed(&self) {
        self.notices_failed.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(NOTICES_FAILED_TOTAL).increment(1);
    }
    pub(super) fn notice_dropped(&self) {
        self.notices_dropped.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(NOTICES_DROPPED_TOTAL).increment(1);
    }
    pub(super) fn tick(&self, failed: bool) {
        self.ticks.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(SLA_TICKS_TOTAL).increment(1);
        if failed {
            self.tick_failures.fetch_add(1, Ordering::Relaxed);
            metrics::counter!(SLA_TICK_FAILURES_TOTAL).increment(1);
        }
    }
    pub(super) fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            notices_sent: self.notices_sent.load(Ordering::Relaxed),
            notices_failed: self.notices_failed.load(Ordering::Relaxed),
            notices_dropped: self.notices_dropped.load(Ordering::Relaxed),
            auto_approved: self.auto_approved.load(Ordering::Relaxed),
            ticks: self.ticks.load(Ordering::Relaxed),
            tick_failures: self.tick_failures.load(Ordering::Relaxed),
        }
    }
}

/// The SLA metric the task service reports to: one counter per automatic approval, and a log
/// line naming the task (an id, never a label).
pub(super) struct HostMetrics {
    pub(super) site: String,
    pub(super) stats: Arc<Stats>,
}

impl SlaMetrics for HostMetrics {
    fn auto_approved(&self, task: &TaskId) {
        self.stats.auto_approved.fetch_add(1, Ordering::Relaxed);
        metrics::counter!(AUTO_APPROVED_TOTAL).increment(1);
        tracing::info!(
            target: AUDIT_TARGET,
            site = %self.site,
            action = "auto_approve",
            who = "system:sla",
            task = %task,
            result = "ok",
            "task approved by its SLA"
        );
    }
}
