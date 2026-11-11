//! The task service: every action on a task, the inbox queries, and the SLA tick.
//!
//! The service is the one place a host acts on tasks. Each action loads the task, authorises the
//! actor, applies the state change and appends exactly one immutable [`TaskEvent`], all against
//! the store's optimistic version so a race has one winner. A completion then resumes the Case
//! through the [`TaskEngine`] **exactly once**: the completion commits first, the signal follows,
//! and a retry that finds a completed-but-not-resumed task signals instead of deciding again.
//!
//! The SLA runs through [`TaskService::tick`], which fires the due reminders, escalations and the
//! final action from the [`SlaTimerPort`], so the timers survive a restart.

use std::collections::{BTreeSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::{Value as Json, json};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{CaseId, CaseStore, Engine, TimerPort};
use spot_dag::NodeId;

use crate::access::{AssigneeAccess, InboxAccess, is_direct_manager, is_personal_assignee};
use crate::error::{TasksError, TasksResult};
use crate::event::{SLA_ACTOR, TaskAction, TaskEvent};
use crate::metrics::{NoMetrics, SlaMetrics};
use crate::notify::{NoNotifier, Notification, NotificationKind, Notifier};
use crate::resolve::{Directory, MapDirectory};
use crate::segregation::{DistinctFromCreator, Segregation, SegregationRefusal};
use crate::sla::{
    BusinessCalendar, EscalationTarget, FinalAction, InMemorySlaTimerPort, Recipient, SlaStepKind,
    SlaTimer, SlaTimerPort, WallClockCalendar,
};
use crate::store::TaskStore;
use crate::task::{
    Decision, Delegation, DelegationRequest, Task, TaskFilter, TaskId, TaskSpec, TaskStatus,
};
use crate::text::RunLabel;

/// A boxed, `Send` future: the return type of every asynchronous [`TaskService`] method, so the
/// service can hold its engine as an `Arc<dyn TaskEngine>`.
pub type TaskFuture<'a, T> = Pin<Box<dyn Future<Output = TasksResult<T>> + Send + 'a>>;

/// Resumes a Case when a task completes.
pub trait TaskEngine: Send + Sync {
    /// Signals the `(case, node)` wait with the decision's payload, completing the node.
    ///
    /// # Errors
    /// [`TasksError::EngineFailed`] when the engine cannot be reached or the node is not waiting.
    fn signal<'a>(
        &'a self,
        case: &'a CaseId,
        node: &'a str,
        payload: Option<Json>,
        now: OffsetDateTime,
    ) -> TaskFuture<'a, ()>;
}

/// One recorded signal, for tests.
#[derive(Clone, Debug, PartialEq)]
pub struct SignalRecord {
    /// The Case signalled.
    pub case: String,
    /// The node signalled.
    pub node: String,
    /// The payload handed to the engine.
    pub payload: Option<Json>,
    /// The instant it was signalled at.
    pub at: OffsetDateTime,
}

/// A [`TaskEngine`] that records its signals instead of running an engine.
#[derive(Debug, Default)]
pub struct RecordingEngine {
    signals: Mutex<Vec<SignalRecord>>,
}

impl RecordingEngine {
    /// An engine that has signalled nothing.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Vec<SignalRecord>> {
        self.signals
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Every signal recorded, in order.
    #[must_use]
    pub fn signals(&self) -> Vec<SignalRecord> {
        self.lock().clone()
    }

    /// How many signals were recorded.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock().len()
    }
}

impl TaskEngine for RecordingEngine {
    fn signal<'a>(
        &'a self,
        case: &'a CaseId,
        node: &'a str,
        payload: Option<Json>,
        now: OffsetDateTime,
    ) -> TaskFuture<'a, ()> {
        Box::pin(async move {
            self.lock().push(SignalRecord {
                case: case.to_string(),
                node: node.to_owned(),
                payload,
                at: now,
            });
            Ok(())
        })
    }
}

/// A [`TaskEngine`] over the real `spot-bpm-engine` engine.
pub struct EngineAdapter<S: CaseStore + 'static, T: TimerPort + 'static> {
    engine: Arc<Engine<S, T>>,
}

impl<S: CaseStore + 'static, T: TimerPort + 'static> EngineAdapter<S, T> {
    /// Wraps an engine.
    #[must_use]
    pub fn new(engine: Arc<Engine<S, T>>) -> Self {
        Self { engine }
    }
}

impl<S: CaseStore + 'static, T: TimerPort + 'static> TaskEngine for EngineAdapter<S, T> {
    fn signal<'a>(
        &'a self,
        case: &'a CaseId,
        node: &'a str,
        payload: Option<Json>,
        now: OffsetDateTime,
    ) -> TaskFuture<'a, ()> {
        Box::pin(async move {
            let node = NodeId::new(node).map_err(|error| TasksError::EngineFailed {
                detail: error.to_string(),
            })?;
            self.engine
                .signal(case, &node, payload, now)
                .await
                .map_err(|error| TasksError::EngineFailed {
                    detail: error.to_string(),
                })?;
            Ok(())
        })
    }
}

/// The task service: actions, inbox queries and the SLA tick.
pub struct TaskService {
    store: Arc<dyn TaskStore>,
    engine: Arc<dyn TaskEngine>,
    directory: Arc<dyn Directory>,
    access: Option<Arc<dyn InboxAccess>>,
    segregation: Arc<dyn Segregation>,
    notifier: Arc<dyn Notifier>,
    /// Notifications stored-state actions have queued and the notifier has not yet been given.
    /// The lock is held only to push or pop one entry, never across a send.
    pending: Mutex<VecDeque<Notification>>,
    calendar: Arc<dyn BusinessCalendar>,
    timers: Arc<dyn SlaTimerPort>,
    metrics: Arc<dyn SlaMetrics>,
    label: RunLabel,
}

impl TaskService {
    /// Builds a service over a store and an engine, with the reference defaults: a
    /// [`MapDirectory`], the [`AssigneeAccess`] rule, the [`DistinctFromCreator`] gate, a
    /// [`NoNotifier`], the wall clock and an in-memory SLA timer port.
    #[must_use]
    pub fn new(store: Arc<dyn TaskStore>, engine: Arc<dyn TaskEngine>) -> Self {
        Self {
            store,
            engine,
            directory: Arc::new(MapDirectory::new()),
            access: None,
            segregation: Arc::new(DistinctFromCreator),
            notifier: Arc::new(NoNotifier),
            pending: Mutex::new(VecDeque::new()),
            calendar: Arc::new(WallClockCalendar),
            timers: Arc::new(InMemorySlaTimerPort::new()),
            metrics: Arc::new(NoMetrics),
            label: RunLabel::shipped(),
        }
    }

    /// Sets the directory (roles and managers).
    #[must_use]
    pub fn with_directory(mut self, directory: Arc<dyn Directory>) -> Self {
        self.directory = directory;
        self
    }

    /// Sets the inbox access rule, overriding the default assignee rule.
    #[must_use]
    pub fn with_access(mut self, access: Arc<dyn InboxAccess>) -> Self {
        self.access = Some(access);
        self
    }

    /// Sets the segregation gate.
    #[must_use]
    pub fn with_segregation(mut self, segregation: Arc<dyn Segregation>) -> Self {
        self.segregation = segregation;
        self
    }

    /// Sets the notifier.
    #[must_use]
    pub fn with_notifier(mut self, notifier: Arc<dyn Notifier>) -> Self {
        self.notifier = notifier;
        self
    }

    /// Sets the business calendar.
    #[must_use]
    pub fn with_calendar(mut self, calendar: Arc<dyn BusinessCalendar>) -> Self {
        self.calendar = calendar;
        self
    }

    /// Sets the SLA timer port.
    #[must_use]
    pub fn with_timers(mut self, timers: Arc<dyn SlaTimerPort>) -> Self {
        self.timers = timers;
        self
    }

    /// Sets the metrics port that counts automatic approvals.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<dyn SlaMetrics>) -> Self {
        self.metrics = metrics;
        self
    }

    /// Sets the run label (ADR-080).
    #[must_use]
    pub fn with_label(mut self, label: RunLabel) -> Self {
        self.label = label;
        self
    }

    /// The inbox access rule in force.
    fn access(&self) -> Arc<dyn InboxAccess> {
        self.access
            .clone()
            .unwrap_or_else(|| Arc::new(AssigneeAccess::new(self.directory.clone())))
    }

    /// Loads a task, or reports it missing.
    ///
    /// # Errors
    /// [`TasksError::TaskNotFound`] when no task has the id, or a store failure.
    pub fn load(&self, id: &TaskId) -> TasksResult<Task> {
        self.store
            .load(id)?
            .ok_or_else(|| TasksError::TaskNotFound { id: id.to_string() })
    }

    /// The task's audit trail.
    ///
    /// # Errors
    /// A store failure.
    pub fn events(&self, id: &TaskId) -> TasksResult<Vec<TaskEvent>> {
        self.store.events(id)
    }

    /// Opens a task, idempotently for its `(case, node)` pair: when the pair already has a task,
    /// that task is returned and nothing is registered or notified again.
    ///
    /// The task is opened synchronously (the Case engine's node handler is synchronous), so its
    /// `Assigned` notification is queued rather than sent: it goes out with the next
    /// [`TaskService::flush_notifications`] or the next async action. Opening never waits on
    /// a notifier.
    ///
    /// # Errors
    /// A store failure.
    pub fn open_task(&self, spec: TaskSpec) -> TasksResult<Task> {
        if let Some(existing) = self.store.find_by_node(&spec.case, &spec.node)? {
            return Ok(existing);
        }
        if let Some(policy) = spec
            .sla
            .as_ref()
            .filter(|policy| policy.final_action == FinalAction::AutoApprove)
        {
            if !policy.auto_approve_declared {
                return Err(TasksError::AutoApproveUndeclared { node: spec.node });
            }
            if spec.distinct_from_creator || spec.duty.is_some() {
                return Err(TasksError::AutoApproveWithSegregation { node: spec.node });
            }
        }
        let task = Task::open(spec);
        let event = TaskEvent::new(&task.id, TaskAction::Created, "system", task.created_at);
        let created = self.store.create(task, event)?;
        self.register_sla_step(&created, 0);
        self.enqueue(&created, NotificationKind::Assigned, None);
        Ok(created)
    }

    /// Claims a task for `actor`. Idempotent when `actor` already holds the claim.
    ///
    /// # Errors
    /// [`TasksError::NotAssignee`], [`TasksError::DelegationExpired`],
    /// [`TasksError::TaskNotOpen`], or a store failure.
    pub fn claim(&self, id: &TaskId, actor: &str, now: OffsetDateTime) -> TasksResult<Task> {
        let task = self.load(id)?;
        if task.status == TaskStatus::Claimed && task.claimed_by.as_deref() == Some(actor) {
            return Ok(task);
        }
        task.ensure_open()?;
        self.ensure_may_act(&task, actor, now)?;
        if task
            .claimed_by
            .as_deref()
            .is_some_and(|claimed| claimed != actor)
        {
            return Err(TasksError::TaskNotOpen {
                id: id.to_string(),
                status: TaskStatus::Claimed.as_str().to_owned(),
            });
        }
        let mut next = task.clone();
        next.status = TaskStatus::Claimed;
        next.claimed_by = Some(actor.to_owned());
        let event = TaskEvent::new(id, TaskAction::Claimed, actor, now);
        self.store.commit(id, task.version, next, Some(event))
    }

    /// Releases `actor`'s claim. A supervisor may release anyone's claim.
    ///
    /// # Errors
    /// [`TasksError::NotAssignee`], [`TasksError::TaskNotOpen`], or a store failure.
    pub fn release(&self, id: &TaskId, actor: &str, now: OffsetDateTime) -> TasksResult<Task> {
        let task = self.load(id)?;
        task.ensure_open()?;
        let is_claimer = task.claimed_by.as_deref() == Some(actor);
        if !is_claimer && !is_direct_manager(actor, &task, self.directory.as_ref()) {
            return Err(TasksError::NotAssignee {
                task: id.to_string(),
                user: actor.to_owned(),
            });
        }
        let mut next = task.clone();
        next.claimed_by = None;
        next.status = TaskStatus::Open;
        let event = TaskEvent::new(id, TaskAction::Released, actor, now);
        self.store.commit(id, task.version, next, Some(event))
    }

    /// Delegates a task to `to`, with a scope and an optional expiry.
    ///
    /// # Errors
    /// [`TasksError::NotAssignee`], [`TasksError::TaskNotOpen`], or a store failure.
    pub async fn delegate(
        &self,
        id: &TaskId,
        actor: &str,
        request: DelegationRequest,
        now: OffsetDateTime,
    ) -> TasksResult<Task> {
        let task = self.load(id)?;
        task.ensure_open()?;
        self.ensure_may_act(&task, actor, now)?;
        let mut next = task.clone();
        next.delegation = Some(Delegation {
            to: request.to.clone(),
            by: actor.to_owned(),
            scope: request.scope,
            at: now,
            expires_at: request.expires_at,
        });
        let mut event =
            TaskEvent::new(id, TaskAction::Delegated, actor, now).with_detail(&request.to);
        if let Some(reason) = &request.reason {
            event = event.with_reason(reason);
        }
        let delegated = self.store.commit(id, task.version, next, Some(event))?;
        self.notify(
            &delegated,
            NotificationKind::Delegated,
            Some(vec![request.to.clone()]),
        )
        .await;
        Ok(delegated)
    }

    /// Reassigns a task to `to`, which a supervisor only may do.
    ///
    /// # Errors
    /// [`TasksError::NotSupervisor`], [`TasksError::TaskNotOpen`], or a store failure.
    pub async fn reassign(
        &self,
        id: &TaskId,
        actor: &str,
        to: &str,
        reason: &str,
        now: OffsetDateTime,
    ) -> TasksResult<Task> {
        let task = self.load(id)?;
        task.ensure_open()?;
        if !is_direct_manager(actor, &task, self.directory.as_ref()) {
            return Err(TasksError::NotSupervisor {
                task: id.to_string(),
                user: actor.to_owned(),
            });
        }
        let mut next = task.clone();
        next.assignees = BTreeSet::from([to.to_owned()]);
        next.roles.clear();
        next.claimed_by = None;
        next.delegation = None;
        let event = TaskEvent::new(id, TaskAction::Reassigned, actor, now)
            .with_reason(reason)
            .with_detail(to);
        let reassigned = self.store.commit(id, task.version, next, Some(event))?;
        self.notify(
            &reassigned,
            NotificationKind::Reassigned,
            Some(vec![to.to_owned()]),
        )
        .await;
        Ok(reassigned)
    }

    /// Completes a task with `decision`, resuming the Case exactly once.
    ///
    /// A duplicate submission of the same decision is idempotent and does not signal again; a
    /// different decision on a completed task is refused.
    ///
    /// # Errors
    /// [`TasksError::NotAssignee`], [`TasksError::DelegationExpired`],
    /// [`TasksError::DistinctFromCreator`], [`TasksError::SegregationBlocked`],
    /// [`TasksError::InvalidChoice`], [`TasksError::TaskNotOpen`], [`TasksError::EngineFailed`],
    /// or a store failure.
    pub fn complete<'a>(
        &'a self,
        id: &'a TaskId,
        actor: &'a str,
        decision: Decision,
        comment: Option<&'a str>,
        now: OffsetDateTime,
    ) -> TaskFuture<'a, Task> {
        Box::pin(async move {
            let task = self.load(id)?;
            if task.status == TaskStatus::Completed {
                // A repeat tells the caller the outcome and may resume the Case, so only those
                // who completed it or may act on or supervise it get that far.
                self.ensure_may_know_completion(&task, actor)?;
                if task.outcome.as_ref() == Some(&decision) {
                    return self.ensure_resumed(task, now).await;
                }
                return Err(TasksError::TaskNotOpen {
                    id: id.to_string(),
                    status: task.status.as_str().to_owned(),
                });
            }
            task.ensure_open()?;
            self.ensure_may_act(&task, actor, now)?;
            let on_behalf_of = self.on_behalf_of(&task, actor, now)?;
            self.segregation
                .may_decide(&task, actor)
                .map_err(|refusal| map_refusal(refusal, &task, actor))?;
            let Some(branch) = task.branch_for(&decision) else {
                return Err(TasksError::InvalidChoice {
                    task: id.to_string(),
                    choice: decision.as_text(),
                });
            };
            let mut next = task.clone();
            next.status = TaskStatus::Completed;
            next.completed_at = Some(now);
            next.outcome = Some(decision.clone());
            let mut event = TaskEvent::new(id, TaskAction::Completed, actor, now)
                .with_choice(decision, &branch);
            if let Some(by) = &on_behalf_of {
                event = event.on_behalf_of(by);
            }
            if let Some(comment) = comment {
                event = event.with_reason(comment);
            }
            let completed = self.store.commit(id, task.version, next, Some(event))?;
            self.timers.clear(id);
            // Queued first and sent after the Case is resumed, so a slow notifier cannot hold
            // the resume back.
            self.enqueue(&completed, NotificationKind::Completed, None);
            let resumed = self.ensure_resumed(completed, now).await;
            self.flush_notifications().await;
            resumed
        })
    }

    /// Cancels a task because its Case was cancelled.
    ///
    /// # Errors
    /// [`TasksError::TaskNotFound`], or a store failure.
    pub fn cancel(&self, id: &TaskId, now: OffsetDateTime) -> TasksResult<Task> {
        let task = self.load(id)?;
        if !task.is_open() {
            return Ok(task);
        }
        let mut next = task.clone();
        next.status = TaskStatus::Cancelled;
        let event = TaskEvent::new(id, TaskAction::Cancelled, "system", now);
        let cancelled = self.store.commit(id, task.version, next, Some(event))?;
        self.timers.clear(id);
        Ok(cancelled)
    }

    /// Fires every SLA step due at `now` and returns the tasks it acted on.
    ///
    /// # Errors
    /// [`TasksError::EngineFailed`] when an auto-completion cannot resume its Case, or a store
    /// failure.
    pub fn tick<'a>(&'a self, now: OffsetDateTime) -> TaskFuture<'a, Vec<TaskId>> {
        Box::pin(async move {
            let due = self.timers.due(now);
            let mut fired = Vec::new();
            for timer in due {
                let Some(task) = self.store.load(&timer.task)? else {
                    self.timers.clear(&timer.task);
                    continue;
                };
                if !task.is_open() {
                    self.timers.clear(&timer.task);
                    continue;
                }
                let Some(policy) = task.sla.clone() else {
                    continue;
                };
                let Some(step) = policy.step(timer.step) else {
                    continue;
                };
                let mut task = task;
                match step.kind {
                    SlaStepKind::Reminder(index) => {
                        let recipients =
                            self.resolve_recipients(&policy.reminders[index].to, &task);
                        let event = TaskEvent::new(&task.id, TaskAction::Reminded, "system", now)
                            .with_detail(&format!("reminder {index}"));
                        task =
                            self.store
                                .commit(&task.id, task.version, task.clone(), Some(event))?;
                        // Queued, not sent, until the step is stored and the next timer registered.
                        self.enqueue(&task, NotificationKind::Reminder, Some(recipients));
                    }
                    SlaStepKind::Escalation(index) => {
                        let escalation = &policy.escalations[index];
                        let target = self.resolve_target(&escalation.to, &task);
                        let mut next = task.clone();
                        if let Some(user) = &target {
                            next.assignees = BTreeSet::from([user.clone()]);
                            next.roles.clear();
                            next.claimed_by = None;
                            next.delegation = None;
                        }
                        let detail = target
                            .clone()
                            .unwrap_or_else(|| "no target resolved".to_owned());
                        let event = TaskEvent::new(&task.id, TaskAction::Escalated, "system", now)
                            .with_detail(&detail);
                        task = self
                            .store
                            .commit(&task.id, task.version, next, Some(event))?;
                        let mut recipients = self.resolve_recipients(&escalation.notify, &task);
                        if let Some(user) = target {
                            recipients.push(user);
                        }
                        self.enqueue(&task, NotificationKind::Escalated, Some(recipients));
                    }
                    SlaStepKind::Final => match policy.final_action {
                        FinalAction::AutoApprove => {
                            task = self.auto_complete(task, Decision::Approve, now).await?;
                        }
                        FinalAction::AutoReject => {
                            task = self.auto_complete(task, Decision::Reject, now).await?;
                        }
                        FinalAction::EscalateAgain => {
                            if let Some(last) = policy.escalations.last() {
                                let target = self.resolve_target(&last.to, &task);
                                let mut next = task.clone();
                                if let Some(user) = &target {
                                    next.assignees = BTreeSet::from([user.clone()]);
                                    next.roles.clear();
                                    next.claimed_by = None;
                                    next.delegation = None;
                                }
                                let detail = target
                                    .clone()
                                    .unwrap_or_else(|| "no target resolved".to_owned());
                                let event =
                                    TaskEvent::new(&task.id, TaskAction::Escalated, "system", now)
                                        .with_detail(&detail);
                                task =
                                    self.store
                                        .commit(&task.id, task.version, next, Some(event))?;
                                let mut recipients = self.resolve_recipients(&last.notify, &task);
                                if let Some(user) = target {
                                    recipients.push(user);
                                }
                                self.enqueue(&task, NotificationKind::Escalated, Some(recipients));
                            }
                        }
                        FinalAction::None => {}
                    },
                }
                fired.push(task.id.clone());
                if task.is_open() {
                    self.register_sla_step(&task, timer.step + 1);
                }
                // Last, after the state and the next timer are stored: a slow or dropped send
                // can neither stall the step nor make it fire again.
                self.flush_notifications().await;
            }
            Ok(fired)
        })
    }

    /// The tasks `user` may act on.
    ///
    /// # Errors
    /// A store failure.
    pub async fn my_tasks(&self, user: &str, now: OffsetDateTime) -> TasksResult<Vec<Task>> {
        let _ = now;
        let access = self.access();
        let mut seen = Vec::new();
        for task in self.store.list(&open_filter())? {
            if access.may_see(user, &task).await {
                seen.push(task);
            }
        }
        Ok(seen)
    }

    /// The tasks `user` may supervise.
    ///
    /// # Errors
    /// A store failure.
    pub fn team_tasks(&self, user: &str, now: OffsetDateTime) -> TasksResult<Vec<Task>> {
        let _ = now;
        let access = self.access();
        Ok(self
            .store
            .list(&open_filter())?
            .into_iter()
            .filter(|task| access.may_supervise(user, task))
            .collect())
    }

    /// The open tasks of `process` that `user` may act on or supervise.
    ///
    /// # Errors
    /// A store failure.
    pub async fn by_process(
        &self,
        user: &str,
        process: &str,
        now: OffsetDateTime,
    ) -> TasksResult<Vec<Task>> {
        let _ = now;
        let access = self.access();
        let filter = TaskFilter {
            status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
            process: Some(process.to_owned()),
            ..TaskFilter::default()
        };
        let mut seen = Vec::new();
        for task in self.store.list(&filter)? {
            if access.may_supervise(user, &task) || access.may_see(user, &task).await {
                seen.push(task);
            }
        }
        Ok(seen)
    }

    /// The open tasks due at or before `now` that `user` may act on or supervise.
    ///
    /// # Errors
    /// A store failure.
    pub async fn overdue(&self, user: &str, now: OffsetDateTime) -> TasksResult<Vec<Task>> {
        let access = self.access();
        let filter = TaskFilter {
            status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
            due_before: Some(now),
            ..TaskFilter::default()
        };
        let mut seen = Vec::new();
        for task in self.store.list(&filter)? {
            if access.may_supervise(user, &task) || access.may_see(user, &task).await {
                seen.push(task);
            }
        }
        Ok(seen)
    }

    /// Refuses unless `actor` completed the task, or may act on or supervise it.
    fn ensure_may_know_completion(&self, task: &Task, actor: &str) -> TasksResult<()> {
        let events = self.store.events(&task.id)?;
        let completed_it = events
            .iter()
            .rev()
            .find(|event| {
                matches!(
                    event.action,
                    TaskAction::Completed | TaskAction::AutoCompleted
                )
            })
            .is_some_and(|event| {
                event.actor == actor || event.on_behalf_of.as_deref() == Some(actor)
            });
        let access = self.access();
        if completed_it
            || (is_personal_assignee(actor, task, self.directory.as_ref())
                && access.may_act(actor, task))
            || access.may_supervise(actor, task)
        {
            return Ok(());
        }
        Err(TasksError::NotAssignee {
            task: task.id.to_string(),
            user: actor.to_owned(),
        })
    }

    /// Refuses unless `actor` is an assignee or an active delegate (and the access rule agrees).
    fn ensure_may_act(&self, task: &Task, actor: &str, now: OffsetDateTime) -> TasksResult<()> {
        if is_personal_assignee(actor, task, self.directory.as_ref())
            && self.access().may_act(actor, task)
        {
            if let Some(delegation) = &task.delegation
                && delegation.to == actor
                && !delegation.is_active(now)
            {
                return Err(TasksError::DelegationExpired {
                    task: task.id.to_string(),
                    delegate: actor.to_owned(),
                });
            }
            return Ok(());
        }
        Err(TasksError::NotAssignee {
            task: task.id.to_string(),
            user: actor.to_owned(),
        })
    }

    /// The delegator `actor` is acting for, when `actor` is the active delegate.
    fn on_behalf_of(
        &self,
        task: &Task,
        actor: &str,
        now: OffsetDateTime,
    ) -> TasksResult<Option<String>> {
        let Some(delegation) = &task.delegation else {
            return Ok(None);
        };
        if delegation.to != actor {
            return Ok(None);
        }
        if !delegation.is_active(now) {
            return Err(TasksError::DelegationExpired {
                task: task.id.to_string(),
                delegate: actor.to_owned(),
            });
        }
        Ok(Some(delegation.by.clone()))
    }

    /// Resumes the Case for a completed task, exactly once.
    async fn ensure_resumed(&self, task: Task, now: OffsetDateTime) -> TasksResult<Task> {
        if task.resumed_case {
            return Ok(task);
        }
        let payload = Some(json!({
            "decision": task.outcome.as_ref().map(Decision::as_text),
            "branch": task.outcome.as_ref().and_then(|decision| task.branch_for(decision)),
            "case": task.case,
            "node": task.node,
            "auto": task.auto_completed,
            "note": task.auto_completed.then_some("approved automatically by SLA"),
        }));
        let case = CaseId::new(&task.case).map_err(|error| TasksError::EngineFailed {
            detail: error.to_string(),
        })?;
        self.engine.signal(&case, &task.node, payload, now).await?;
        let mut next = task.clone();
        next.resumed_case = true;
        let id = next.id.clone();
        self.store.commit(&id, task.version, next, None)
    }

    /// Completes a task as the system principal, for the SLA's final action.
    async fn auto_complete(
        &self,
        task: Task,
        decision: Decision,
        now: OffsetDateTime,
    ) -> TasksResult<Task> {
        let Some(branch) = task.branch_for(&decision) else {
            return Ok(task);
        };
        let approving = decision == Decision::Approve;
        // Fail closed: a task stored before the declaration existed, or one that became
        // segregated, is never approved by the clock. It stays open for a person.
        if approving
            && (!task
                .sla
                .as_ref()
                .is_some_and(|policy| policy.auto_approve_declared)
                || task.distinct_from_creator
                || task.duty.is_some())
        {
            return Ok(task);
        }
        let policy_id = task
            .sla
            .as_ref()
            .and_then(|policy| policy.id.clone())
            .unwrap_or_else(|| format!("sla:{}", task.node));
        let mut next = task.clone();
        next.status = TaskStatus::Completed;
        next.completed_at = Some(now);
        next.outcome = Some(decision.clone());
        next.auto_completed = true;
        let event = TaskEvent::new(&task.id, TaskAction::AutoCompleted, SLA_ACTOR, now)
            .with_choice(decision, &branch)
            .automatic(&policy_id);
        let completed = self
            .store
            .commit(&task.id, task.version, next, Some(event))?;
        self.timers.clear(&task.id);
        self.enqueue(&completed, NotificationKind::Completed, None);
        if approving {
            self.metrics.auto_approved(&completed.id);
            let supervisors = self.resolve_recipients(&[Recipient::Manager], &completed);
            self.enqueue(
                &completed,
                NotificationKind::AutoApproved,
                Some(supervisors),
            );
        }
        let resumed = self.ensure_resumed(completed, now).await;
        self.flush_notifications().await;
        resumed
    }

    /// Registers the SLA step at `index`, or clears the task's timer when there is none.
    fn register_sla_step(&self, task: &Task, index: usize) {
        let Some(policy) = &task.sla else {
            return;
        };
        match policy.step(index) {
            Some(step) => {
                let due = self
                    .calendar
                    .instant_after(task.created_at, step.after_seconds);
                self.timers.register(SlaTimer {
                    task: task.id.clone(),
                    case: task.case.clone(),
                    node: task.node.clone(),
                    step: index,
                    due,
                });
            }
            None => self.timers.clear(&task.id),
        }
    }

    /// The users an escalation target resolves to, when one resolves.
    fn resolve_target(&self, target: &EscalationTarget, task: &Task) -> Option<String> {
        match target {
            EscalationTarget::User(user) => Some(user.clone()),
            EscalationTarget::Role(role) => self.directory.users_in_role(role).into_iter().next(),
            EscalationTarget::Manager => task
                .assignees
                .iter()
                .find_map(|assignee| self.directory.manager_of(assignee)),
        }
    }

    /// The users a reminder or escalation's recipients resolve to, deduplicated and sorted.
    fn resolve_recipients(&self, recipients: &[Recipient], task: &Task) -> Vec<String> {
        let mut users = BTreeSet::new();
        for recipient in recipients {
            match recipient {
                Recipient::Assignees => {
                    users.extend(task.assignees.iter().cloned());
                }
                Recipient::User(user) => {
                    users.insert(user.clone());
                }
                Recipient::Role(role) => {
                    users.extend(self.directory.users_in_role(role));
                }
                Recipient::Manager => {
                    for assignee in &task.assignees {
                        if let Some(manager) = self.directory.manager_of(assignee) {
                            users.insert(manager);
                        }
                    }
                }
            }
        }
        users.into_iter().collect()
    }

    /// Queues a notification for the given recipients, or the assignees when none are given.
    fn enqueue(&self, task: &Task, kind: NotificationKind, recipients: Option<Vec<String>>) {
        let recipients = recipients.unwrap_or_else(|| task.assignees.iter().cloned().collect());
        let mut queue = self.pending_queue();
        if queue.len() >= MAX_PENDING_NOTIFICATIONS {
            // The host is not flushing: keep the newest, drop the oldest, never grow without bound.
            queue.pop_front();
        }
        queue.push_back(Notification {
            task: task.id.clone(),
            case: task.case.clone(),
            node: task.node.clone(),
            kind,
            run_singular: self.label.singular.clone(),
            title: task.title.clone(),
            due: task.due,
            recipients,
        });
    }

    fn pending_queue(&self) -> MutexGuard<'_, VecDeque<Notification>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Queues a notification and gives every queued one to the notifier. Called only after the
    /// state it reports is stored.
    async fn notify(&self, task: &Task, kind: NotificationKind, recipients: Option<Vec<String>>) {
        self.enqueue(task, kind, recipients);
        self.flush_notifications().await;
    }

    /// Gives every queued notification to the notifier, oldest first, and returns when the
    /// notifier has answered for each.
    ///
    /// The lock is released before each send, and a notification leaves the queue before it is
    /// sent, so dropping this future mid-send loses at most the notification in flight (which
    /// the [`Notifier`] contract allows) and never blocks or fails a stored state change.
    pub async fn flush_notifications(&self) {
        loop {
            let next = self.pending_queue().pop_front();
            let Some(notification) = next else {
                return;
            };
            self.notifier.notify(&notification).await;
        }
    }
}

/// How many queued notifications the service keeps for a host that never flushes; past it the
/// oldest is dropped (a lost notification is allowed, unbounded memory is not).
const MAX_PENDING_NOTIFICATIONS: usize = 1024;

/// The filter for the open inbox: open and claimed tasks.
fn open_filter() -> TaskFilter {
    TaskFilter {
        status: Some(vec![TaskStatus::Open, TaskStatus::Claimed]),
        ..TaskFilter::default()
    }
}

/// Maps a segregation refusal onto this crate's error vocabulary.
fn map_refusal(refusal: SegregationRefusal, task: &Task, actor: &str) -> TasksError {
    if refusal.code == "bpm_tasks.distinct_from_creator" {
        TasksError::DistinctFromCreator {
            task: task.id.to_string(),
            user: actor.to_owned(),
        }
    } else {
        TasksError::SegregationBlocked {
            task: task.id.to_string(),
            user: actor.to_owned(),
            detail: refusal.detail,
        }
    }
}
