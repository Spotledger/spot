//! The form step's task in the inbox of `spot-bpm-tasks`.
//!
//! [`TaskInbox`] opens one task per `(request, node)` for the resolved assignee, so it appears
//! in their inbox and in the team inbox of the role, with the usual claim, delegate, SLA and
//! notification behaviour. The task is completed by [`crate::FormService`] together with the
//! step's transition, so the task service must be built over [`NoSignalEngine`]: the form
//! service owns the engine transition, and a second signal from the task service would try to
//! complete the node twice.

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::CaseId;
use spot_bpm_tasks::{
    ApproverResolver, AssigneeSpec, Decision, Directory, Priority, TaskEngine, TaskFuture, TaskId,
    TaskOption, TaskService, TaskSpec, TaskStatus, TasksError, resolve_assignees,
};

use crate::approvals::ApprovalStanding;
use crate::error::{EntryError, EntryResult};
use crate::form::{Inbox, InboxItem, PortFuture, Ticket, TicketState};

/// A task engine that signals nothing: the form service commits the node's transition itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoSignalEngine;

impl TaskEngine for NoSignalEngine {
    fn signal<'a>(
        &'a self,
        _case: &'a CaseId,
        _node: &'a str,
        _payload: Option<serde_json::Value>,
        _now: OffsetDateTime,
    ) -> TaskFuture<'a, ()> {
        Box::pin(async { Ok(()) })
    }
}

/// The branch the single "Submit" option selects.
pub const SUBMIT_BRANCH: &str = "submit";

/// An [`Inbox`] over the task service.
pub struct TaskInbox {
    service: Arc<TaskService>,
    directory: Arc<dyn Directory>,
    resolver: Arc<dyn ApproverResolver>,
}

impl TaskInbox {
    /// An inbox over `service` (built over [`NoSignalEngine`]), resolving assignees through
    /// `directory` and `resolver`.
    #[must_use]
    pub fn new(
        service: Arc<TaskService>,
        directory: Arc<dyn Directory>,
        resolver: Arc<dyn ApproverResolver>,
    ) -> Self {
        Self {
            service,
            directory,
            resolver,
        }
    }

    /// The approvals on `ticket`, counted through `human_approvers` only: an SLA auto-approval is
    /// never a person.
    ///
    /// # Errors
    /// [`EntryError::StepNotOpen`] for a bad id, [`EntryError::Inbox`] when the trail cannot be read.
    pub fn approvals(&self, ticket: &str) -> EntryResult<ApprovalStanding> {
        let id = Self::id_of(ticket)?;
        let events = self
            .service
            .events(&id)
            .map_err(|error| inbox_error(&error))?;
        Ok(ApprovalStanding::from_events(&events))
    }

    fn id_of(ticket: &str) -> EntryResult<TaskId> {
        TaskId::new(ticket).ok_or_else(|| EntryError::StepNotOpen {
            task: ticket.to_owned(),
            detail: "not a task id".to_owned(),
        })
    }
}

fn inbox_error(error: &TasksError) -> EntryError {
    EntryError::Inbox {
        detail: error.to_string(),
    }
}

impl Inbox for TaskInbox {
    fn open(&self, item: &InboxItem) -> EntryResult<Ticket> {
        let spec = AssigneeSpec::parse(item.assignee.as_deref().unwrap_or(""));
        let resolved = resolve_assignees(
            &spec,
            self.directory.as_ref(),
            self.resolver.as_ref(),
            &item.request,
            &item.node,
            &item.form,
        );
        if resolved.users.is_empty() {
            return Err(EntryError::NoAssignee {
                node: item.node.clone(),
            });
        }
        let task_spec = TaskSpec {
            id: TaskId::for_node(&item.request, &item.node),
            case: item.request.clone(),
            node: item.node.clone(),
            process: item.process.clone(),
            title: item.title.clone(),
            doctype: item.doctype.clone(),
            subject: item.subject.clone(),
            created_by: item.created_by.clone(),
            distinct_from_creator: false,
            duty: None,
            scope: None,
            assignee_spec: spec,
            assignees: resolved.users,
            roles: resolved.roles,
            options: vec![TaskOption {
                label: "Submit".to_owned(),
                decision: Decision::Approve,
                branch: SUBMIT_BRANCH.to_owned(),
            }],
            form: item.form.clone(),
            priority: Priority::Normal,
            due: item.due,
            sla: None,
            created_at: item.created_at,
        };
        let task = self
            .service
            .open_task(task_spec)
            .map_err(|error| inbox_error(&error))?;
        Ok(Ticket {
            id: task.id.to_string(),
            due: task.due,
        })
    }

    fn check<'a>(
        &'a self,
        ticket: &'a str,
        actor: &'a str,
        _now: OffsetDateTime,
    ) -> PortFuture<'a, EntryResult<TicketState>> {
        Box::pin(async move {
            let id = Self::id_of(ticket)?;
            let task = self
                .service
                .load(&id)
                .map_err(|error| inbox_error(&error))?;
            match task.status {
                TaskStatus::Completed => return Ok(TicketState::Done),
                TaskStatus::Open | TaskStatus::Claimed => {}
                TaskStatus::Cancelled | TaskStatus::Expired => {
                    return Err(EntryError::StepNotOpen {
                        task: ticket.to_owned(),
                        detail: "the step is no longer open".to_owned(),
                    });
                }
            }
            let members: BTreeSet<String> = task
                .roles
                .iter()
                .flat_map(|role| self.directory.users_in_role(role))
                .collect();
            if task.assignees.contains(actor) || members.contains(actor) {
                Ok(TicketState::Open)
            } else {
                Err(EntryError::StepNotOpen {
                    task: ticket.to_owned(),
                    detail: "the step was not given to this person".to_owned(),
                })
            }
        })
    }

    fn complete<'a>(
        &'a self,
        ticket: &'a str,
        actor: &'a str,
        now: OffsetDateTime,
    ) -> PortFuture<'a, EntryResult<()>> {
        Box::pin(async move {
            let id = Self::id_of(ticket)?;
            self.service
                .complete(&id, actor, Decision::Approve, None, now)
                .await
                .map(|_| ())
                .map_err(|error| inbox_error(&error))
        })
    }

    fn cancel(&self, ticket: &str, now: OffsetDateTime) -> EntryResult<()> {
        let id = Self::id_of(ticket)?;
        self.service
            .cancel(&id, now)
            .map(|_| ())
            .map_err(|error| inbox_error(&error))
    }
}

impl From<crate::label::RunLabel> for spot_bpm_tasks::RunLabel {
    fn from(label: crate::label::RunLabel) -> Self {
        Self {
            singular: label.singular,
            plural: label.plural,
        }
    }
}
