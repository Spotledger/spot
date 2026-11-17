//! The in-memory fake of [`TaskPort`](super::TaskPort) (WP-BPM-05.T7): assignee and supervisor
//! visibility, the version check, segregation, audit of refusals and `task_complete` replay.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Mutex;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::{
    CompleteIn, CompleteOut, DelegateIn, ReassignIn, TaskDetailOut, TaskEventKind, TaskEventOut,
    TaskFieldOut, TaskListIn, TaskListView, TaskOutcomeOut, TaskPageOut, TaskPort, TaskPortError,
    TaskState, TaskView,
};
use crate::api::service::{BoxFuture, RequestCtx};

/// A task to seed [`MemoryTaskPort`] with.
#[derive(Clone, Debug)]
pub struct TaskSeed {
    /// The task as the contract shows it.
    pub view: TaskView,
    /// The users who supervise it.
    pub supervisors: Vec<String>,
    /// The users who created or already decided the document the step guards.
    pub segregated: Vec<String>,
    /// The outcomes an actor may choose.
    pub outcomes: Vec<TaskOutcomeOut>,
    /// The form fields.
    pub fields: Vec<TaskFieldOut>,
}

impl TaskSeed {
    /// An open task at version 1 with the outcomes `approve` and `reject` and no form field.
    pub fn new(id: &str, process: &str, label: &str, assignees: &[&str]) -> Self {
        TaskSeed {
            view: TaskView {
                id: id.to_owned(),
                run: "run0000000000000001".to_owned(),
                process: process.to_owned(),
                node: "review".to_owned(),
                label: label.to_owned(),
                state: TaskState::Open,
                due_at: None,
                assignees: assignees.iter().map(|a| (*a).to_owned()).collect(),
                claimed_by: None,
                document: None,
                version: 1,
            },
            supervisors: Vec::new(),
            segregated: Vec::new(),
            outcomes: vec![
                TaskOutcomeOut {
                    outcome: "approve".to_owned(),
                    label: "Approve".to_owned(),
                },
                TaskOutcomeOut {
                    outcome: "reject".to_owned(),
                    label: "Reject".to_owned(),
                },
            ],
            fields: Vec::new(),
        }
    }
}

/// One audited call of the fake: who, which task, which operation and how it ended. It never
/// holds a value, a comment or a token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// The site.
    pub site: String,
    /// The caller.
    pub user: String,
    /// The task.
    pub task: String,
    /// The operation (`claim`, `release`, `complete`, `delegate`, `reassign`).
    pub operation: &'static str,
    /// `ok`, or the code of the refusal.
    pub result: &'static str,
}

struct Row {
    view: TaskView,
    supervisors: BTreeSet<String>,
    delegates: Vec<String>,
    segregated: BTreeSet<String>,
    outcomes: Vec<TaskOutcomeOut>,
    fields: Vec<TaskFieldOut>,
    events: Vec<TaskEventOut>,
}

#[derive(Clone, Copy)]
struct Role {
    acts: bool,
    supervises: bool,
}

impl Row {
    fn role_of(&self, user: &str) -> Role {
        Role {
            acts: self.view.assignees.iter().any(|a| a == user)
                || self.delegates.iter().any(|d| d == user),
            supervises: self.supervisors.contains(user),
        }
    }
}

struct Inner {
    /// (site, bearer token) to user.
    tokens: HashMap<(String, String), String>,
    /// (site, task id) to the task.
    tasks: BTreeMap<(String, String), Row>,
    /// (site, user, idempotency key) to the fingerprint and the first answer.
    replays: HashMap<(String, String, String), (String, TaskView)>,
    audit: Vec<AuditRecord>,
    now: OffsetDateTime,
}

/// An in-memory [`TaskPort`] for tests: tokens map to users per site, every site has its own
/// tasks, and the fake enforces the contract's rights, version check, segregation, idempotency
/// and audit. Time is an argument ([`MemoryTaskPort::new`], [`MemoryTaskPort::set_now`]); the
/// fake never reads a clock.
pub struct MemoryTaskPort {
    inner: Mutex<Inner>,
}

fn identify(
    tokens: &HashMap<(String, String), String>,
    ctx: &RequestCtx,
) -> Result<String, TaskPortError> {
    let token = ctx
        .credential
        .as_ref()
        .map(|c| c.token.clone())
        .ok_or(TaskPortError::Unauthenticated)?;
    tokens
        .get(&(ctx.site.as_str().to_owned(), token))
        .cloned()
        .ok_or(TaskPortError::Unauthenticated)
}

fn stamp(now: OffsetDateTime) -> String {
    now.to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .unwrap_or_default()
}

fn event(
    now: OffsetDateTime,
    kind: TaskEventKind,
    actor: &str,
    to: Option<&str>,
    outcome: Option<&str>,
) -> TaskEventOut {
    TaskEventOut {
        at: stamp(now),
        kind,
        actor: actor.to_owned(),
        on_behalf_of: None,
        to: to.map(str::to_owned),
        outcome: outcome.map(str::to_owned),
    }
}

impl MemoryTaskPort {
    /// An empty port whose clock reads `now`.
    pub fn new(now: OffsetDateTime) -> Self {
        MemoryTaskPort {
            inner: Mutex::new(Inner {
                tokens: HashMap::new(),
                tasks: BTreeMap::new(),
                replays: HashMap::new(),
                audit: Vec::new(),
                now,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Moves the fake's clock.
    pub fn set_now(&self, now: OffsetDateTime) {
        self.lock().now = now;
    }

    /// Lets `token` act as `user` on `site`; the user is then a known, active user of the site.
    pub fn grant(&self, site: &str, token: &str, user: &str) {
        self.lock()
            .tokens
            .insert((site.to_owned(), token.to_owned()), user.to_owned());
    }

    /// Adds a task to `site`.
    pub fn add_task(&self, site: &str, seed: TaskSeed) {
        let created = event(
            self.lock().now,
            TaskEventKind::Created,
            "system:engine",
            None,
            None,
        );
        let row = Row {
            view: seed.view.clone(),
            supervisors: seed.supervisors.into_iter().collect(),
            delegates: Vec::new(),
            segregated: seed.segregated.into_iter().collect(),
            outcomes: seed.outcomes,
            fields: seed.fields,
            events: vec![created],
        };
        self.lock()
            .tasks
            .insert((site.to_owned(), seed.view.id), row);
    }

    /// The audit records so far, oldest first.
    pub fn audit(&self) -> Vec<AuditRecord> {
        self.lock().audit.clone()
    }

    /// The task as stored, for a test that checks nothing changed.
    pub fn task(&self, site: &str, id: &str) -> Option<TaskView> {
        self.lock()
            .tasks
            .get(&(site.to_owned(), id.to_owned()))
            .map(|r| r.view.clone())
    }

    /// Runs one mutation: the caller's identity, the visibility gate (`NotFound`, never audited,
    /// so a task the caller may not see leaves no trace of its existence), the operation, and
    /// the audit of its end, a refusal included.
    fn mutate<T>(
        &self,
        ctx: &RequestCtx,
        id: &str,
        operation: &'static str,
        f: impl FnOnce(
            &mut Row,
            &str,
            Role,
            OffsetDateTime,
            &BTreeSet<String>,
        ) -> Result<T, TaskPortError>,
    ) -> Result<T, TaskPortError> {
        let mut guard = self.lock();
        let inner = &mut *guard;
        let user = identify(&inner.tokens, ctx)?;
        let site = ctx.site.as_str().to_owned();
        let known: BTreeSet<String> = inner
            .tokens
            .iter()
            .filter(|((s, _), _)| *s == site)
            .map(|(_, u)| u.clone())
            .collect();
        let now = inner.now;
        let Some(row) = inner.tasks.get_mut(&(site.clone(), id.to_owned())) else {
            return Err(TaskPortError::NotFound);
        };
        let role = row.role_of(&user);
        if !role.acts && !role.supervises {
            return Err(TaskPortError::NotFound);
        }
        let result = f(row, &user, role, now, &known);
        inner.audit.push(AuditRecord {
            site,
            user,
            task: id.to_owned(),
            operation,
            result: match &result {
                Ok(_) => "ok",
                Err(e) => e.code(),
            },
        });
        result
    }
}

/// The checks every mutation starts with, in the contract's order: the task is active, the
/// caller may do this (`needs_supervision`: supervise it; otherwise act on it), the version is current.
fn preflight(
    row: &Row,
    role: Role,
    needs_supervision: bool,
    expected: u64,
) -> Result<(), TaskPortError> {
    if !row.view.state.is_active() {
        return Err(TaskPortError::NotActive);
    }
    let allowed = if needs_supervision {
        role.supervises
    } else {
        role.acts
    };
    if !allowed {
        return Err(TaskPortError::PermissionDenied);
    }
    if row.view.version != expected {
        return Err(TaskPortError::StaleVersion {
            current: row.view.version,
        });
    }
    Ok(())
}

fn target_ok(to: &str, user: &str, known: &BTreeSet<String>) -> Result<(), TaskPortError> {
    if to == user || !known.contains(to) {
        return Err(TaskPortError::TargetInvalid);
    }
    Ok(())
}

fn fingerprint(id: &str, body: &CompleteIn) -> String {
    let values = body
        .values
        .as_ref()
        .and_then(|v| serde_json::to_string(v).ok())
        .unwrap_or_default();
    format!(
        "{id}\u{0}{}\u{0}{}\u{0}{values}",
        body.outcome,
        body.comment.as_deref().unwrap_or("")
    )
}

impl MemoryTaskPort {
    fn do_list(&self, ctx: &RequestCtx, query: &TaskListIn) -> Result<TaskPageOut, TaskPortError> {
        let guard = self.lock();
        let user = identify(&guard.tokens, ctx)?;
        let site = ctx.site.as_str();
        let after = match &query.cursor {
            None => None,
            Some(cursor) => Some(
                cursor
                    .strip_prefix("c.")
                    .filter(|id| !id.is_empty())
                    .ok_or(TaskPortError::CursorInvalid)?,
            ),
        };
        let mut tasks = Vec::new();
        let mut more = false;
        for ((row_site, id), row) in &guard.tasks {
            if row_site != site || after.is_some_and(|a| id.as_str() <= a) {
                continue;
            }
            let role = row.role_of(&user);
            if !row.view.state.is_active()
                || query
                    .process
                    .as_deref()
                    .is_some_and(|p| p != row.view.process)
            {
                continue;
            }
            let listed = match query.view {
                TaskListView::Mine => role.acts,
                TaskListView::Team => role.supervises,
                TaskListView::Overdue => {
                    role.acts
                        && row
                            .view
                            .due_at
                            .as_deref()
                            .and_then(|d| OffsetDateTime::parse(d, &Rfc3339).ok())
                            .is_some_and(|d| d < guard.now)
                }
            };
            if !listed {
                continue;
            }
            if tasks.len() == query.limit {
                more = true;
                break;
            }
            tasks.push(row.view.clone());
        }
        let next_cursor = if more {
            tasks.last().map(|t| format!("c.{}", t.id))
        } else {
            None
        };
        Ok(TaskPageOut { tasks, next_cursor })
    }

    fn do_get(&self, ctx: &RequestCtx, id: &str) -> Result<TaskDetailOut, TaskPortError> {
        let guard = self.lock();
        let user = identify(&guard.tokens, ctx)?;
        let row = guard
            .tasks
            .get(&(ctx.site.as_str().to_owned(), id.to_owned()))
            .ok_or(TaskPortError::NotFound)?;
        let role = row.role_of(&user);
        if !role.acts && !role.supervises {
            return Err(TaskPortError::NotFound);
        }
        Ok(TaskDetailOut {
            task: row.view.clone(),
            outcomes: if role.acts && row.view.state.is_active() {
                row.outcomes.clone()
            } else {
                Vec::new()
            },
            fields: row.fields.clone(),
            events: row.events.clone(),
        })
    }

    fn do_claim(
        &self,
        ctx: &RequestCtx,
        id: &str,
        expected: u64,
    ) -> Result<TaskView, TaskPortError> {
        self.mutate(ctx, id, "claim", |row, user, role, now, _known| {
            preflight(row, role, false, expected)?;
            match row.view.claimed_by.as_deref() {
                Some(holder) if holder == user => return Ok(row.view.clone()),
                Some(_) => return Err(TaskPortError::AlreadyClaimed),
                None => {}
            }
            row.view.claimed_by = Some(user.to_owned());
            row.view.state = TaskState::Claimed;
            row.view.version += 1;
            row.events
                .push(event(now, TaskEventKind::Claimed, user, None, None));
            Ok(row.view.clone())
        })
    }

    fn do_release(
        &self,
        ctx: &RequestCtx,
        id: &str,
        expected: u64,
    ) -> Result<TaskView, TaskPortError> {
        self.mutate(ctx, id, "release", |row, user, role, now, _known| {
            if !row.view.state.is_active() {
                return Err(TaskPortError::NotActive);
            }
            if row.view.version != expected {
                return Err(TaskPortError::StaleVersion {
                    current: row.view.version,
                });
            }
            let holds = row.view.claimed_by.as_deref() == Some(user);
            if !holds && !role.supervises {
                return Err(TaskPortError::NotClaimedByCaller);
            }
            if row.view.claimed_by.is_none() {
                return Ok(row.view.clone());
            }
            row.view.claimed_by = None;
            row.view.state = TaskState::Open;
            row.view.version += 1;
            row.events
                .push(event(now, TaskEventKind::Released, user, None, None));
            Ok(row.view.clone())
        })
    }

    fn do_complete(
        &self,
        ctx: &RequestCtx,
        id: &str,
        expected: u64,
        body: &CompleteIn,
    ) -> Result<CompleteOut, TaskPortError> {
        let print = fingerprint(id, body);
        let replay_key = {
            let guard = self.lock();
            let user = identify(&guard.tokens, ctx)?;
            ctx.idempotency_key
                .clone()
                .map(|key| (ctx.site.as_str().to_owned(), user, key))
        };
        if let Some(key) = &replay_key
            && let Some((first_print, first)) = self.lock().replays.get(key).cloned()
        {
            if first_print != print {
                return Err(TaskPortError::IdempotencyConflict);
            }
            return Ok(CompleteOut {
                task: first,
                replayed: true,
            });
        }
        let done = self.mutate(ctx, id, "complete", |row, user, role, now, _known| {
            preflight(row, role, false, expected)?;
            if row
                .view
                .claimed_by
                .as_deref()
                .is_some_and(|holder| holder != user)
            {
                return Err(TaskPortError::AlreadyClaimed);
            }
            if row.segregated.contains(user) {
                return Err(TaskPortError::Segregation);
            }
            if !row.outcomes.iter().any(|o| o.outcome == body.outcome) {
                return Err(TaskPortError::OutcomeInvalid);
            }
            row.view.state = TaskState::Completed;
            row.view.claimed_by = Some(user.to_owned());
            row.view.version += 1;
            row.events.push(event(
                now,
                TaskEventKind::Completed,
                user,
                None,
                Some(&body.outcome),
            ));
            Ok(row.view.clone())
        })?;
        if let Some(key) = replay_key {
            self.lock().replays.insert(key, (print, done.clone()));
        }
        Ok(CompleteOut {
            task: done,
            replayed: false,
        })
    }

    fn do_delegate(
        &self,
        ctx: &RequestCtx,
        id: &str,
        expected: u64,
        body: &DelegateIn,
    ) -> Result<TaskView, TaskPortError> {
        self.mutate(ctx, id, "delegate", |row, user, role, now, known| {
            preflight(row, role, false, expected)?;
            target_ok(&body.to, user, known)?;
            if body.until.is_some_and(|until| until <= now) {
                return Err(TaskPortError::DelegationInvalid);
            }
            if !row.delegates.contains(&body.to) {
                row.delegates.push(body.to.clone());
            }
            row.view.version += 1;
            row.events.push(event(
                now,
                TaskEventKind::Delegated,
                user,
                Some(&body.to),
                None,
            ));
            Ok(row.view.clone())
        })
    }

    fn do_reassign(
        &self,
        ctx: &RequestCtx,
        id: &str,
        expected: u64,
        body: &ReassignIn,
    ) -> Result<TaskView, TaskPortError> {
        self.mutate(ctx, id, "reassign", |row, user, role, now, known| {
            preflight(row, role, true, expected)?;
            target_ok(&body.to, user, known)?;
            row.view.assignees = vec![body.to.clone()];
            row.view.claimed_by = None;
            row.view.state = TaskState::Open;
            row.delegates.clear();
            row.view.version += 1;
            row.events.push(event(
                now,
                TaskEventKind::Reassigned,
                user,
                Some(&body.to),
                None,
            ));
            Ok(row.view.clone())
        })
    }
}

impl TaskPort for MemoryTaskPort {
    fn list<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a TaskListIn,
    ) -> BoxFuture<'a, Result<TaskPageOut, TaskPortError>> {
        Box::pin(async move { self.do_list(ctx, query) })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
    ) -> BoxFuture<'a, Result<TaskDetailOut, TaskPortError>> {
        Box::pin(async move { self.do_get(ctx, id) })
    }

    fn claim<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>> {
        Box::pin(async move { self.do_claim(ctx, id, expected) })
    }

    fn release<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>> {
        Box::pin(async move { self.do_release(ctx, id, expected) })
    }

    fn complete<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a CompleteIn,
    ) -> BoxFuture<'a, Result<CompleteOut, TaskPortError>> {
        Box::pin(async move { self.do_complete(ctx, id, expected, body) })
    }

    fn delegate<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a DelegateIn,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>> {
        Box::pin(async move { self.do_delegate(ctx, id, expected, body) })
    }

    fn reassign<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: &'a str,
        expected: u64,
        body: &'a ReassignIn,
    ) -> BoxFuture<'a, Result<TaskView, TaskPortError>> {
        Box::pin(async move { self.do_reassign(ctx, id, expected, body) })
    }
}
