//! The task service's ports: the engine resume, the inbox access, segregation, the approver
//! resolver and the notifier.

use std::sync::Arc;

use serde_json::Value as Json;
use spot_bpm_engine::CaseId;
use spot_bpm_tasks::notify::NotifyFuture;
use spot_bpm_tasks::{
    ApproverResolver, Directory, DocFactsSource, EngineAdapter, InboxAccess, Notification,
    Notifier, PermissionAccess, PrincipalSource, Segregation, SegregationRefusal, Task, TaskAction,
    TaskEngine, TaskFilter, TaskFuture, TaskStatus, TaskStore, TasksError,
};
use time::OffsetDateTime;
use tokio::runtime::Handle;

use super::seams::{DecisionSource, Stats, TaskDirectory, TaskNotice, TaskNoticeSink};
use super::threads::{Hop, drive};
use super::{DISTINCT_CODE, SOD_CODE};
use crate::services::process::ProcessHost;

/// Resumes the Case through the process engine, as a task on the host's runtime.
pub struct HostEngine {
    process: Arc<ProcessHost>,
    hop: Hop,
}

impl HostEngine {
    /// An engine port over `process`, running its calls on `runtime`.
    #[must_use]
    pub fn new(process: Arc<ProcessHost>, runtime: Handle) -> Self {
        Self {
            process,
            hop: Hop::new(runtime),
        }
    }
}

impl TaskEngine for HostEngine {
    fn signal<'a>(
        &'a self,
        case: &'a CaseId,
        node: &'a str,
        payload: Option<Json>,
        now: OffsetDateTime,
    ) -> TaskFuture<'a, ()> {
        Box::pin(async move {
            let engine = self
                .process
                .engine()
                .map_err(|error| TasksError::EngineFailed {
                    detail: error.to_string(),
                })?;
            let case = case.clone();
            let node = node.to_owned();
            let answer = self
                .hop
                .call_async(async move {
                    let adapter = EngineAdapter::new(engine);
                    adapter.signal(&case, &node, payload, now).await
                })
                .await;
            match answer {
                Some(result) => result,
                None => Err(TasksError::EngineFailed {
                    detail: "the runtime stopped before the Case could resume".to_owned(),
                }),
            }
        })
    }
}

/// `Directory` over a [`TaskDirectory`] (a plain delegating wrapper, so no trait upcast is
/// needed).
pub(super) struct DirectoryOf(pub(super) Arc<dyn TaskDirectory>);

impl Directory for DirectoryOf {
    fn users_in_role(&self, role: &str) -> Vec<String> {
        self.0.users_in_role(role)
    }

    fn manager_of(&self, user: &str) -> Option<String> {
        self.0.manager_of(user)
    }
}

/// `InboxAccess` as `PermissionAccess` over the permission engine the site serves now: acting and
/// supervising come from the directory, seeing a task as a reader of the guarded document comes
/// from the engine. No engine, no principal and no facts all deny.
pub(super) struct HostAccess {
    pub(super) decisions: Arc<dyn DecisionSource>,
    pub(super) principals: Arc<dyn PrincipalSource>,
    pub(super) directory: Arc<dyn Directory>,
    pub(super) facts: Arc<dyn DocFactsSource>,
}

impl InboxAccess for HostAccess {
    fn may_act(&self, user: &str, task: &Task) -> bool {
        spot_bpm_tasks::is_personal_assignee(user, task, self.directory.as_ref())
    }

    fn may_see<'a>(
        &'a self,
        user: &'a str,
        task: &'a Task,
    ) -> spot_bpm_tasks::AccessFuture<'a, bool> {
        Box::pin(async move {
            if self.may_act(user, task) {
                return true;
            }
            let Some(engine) = self.decisions.engine() else {
                return false;
            };
            let access = PermissionAccess::new(
                engine,
                Arc::clone(&self.principals),
                Arc::clone(&self.directory),
                Arc::clone(&self.facts),
            );
            access.may_see(user, task).await
        })
    }

    fn may_supervise(&self, user: &str, task: &Task) -> bool {
        spot_bpm_tasks::is_direct_manager(user, task, self.directory.as_ref())
    }
}

/// Segregation over the guarded document's creator and the Case's prior deciders.
///
/// - A task that declares `distinct_from_creator` is refused to the user the node config names as
///   the document's creator **and** to the creator the live document names (the config value is
///   data from a process definition; the document is the truth).
/// - A task that declares `distinct_from_creator` or a `duty` is refused to a user who already
///   decided another task of the same Case (the same duty, when the task names one), as the person
///   or on someone's behalf. A store that cannot answer refuses (fail closed).
pub(super) struct HostSegregation {
    pub(super) store: Arc<dyn TaskStore>,
    pub(super) facts: Arc<dyn DocFactsSource>,
}

fn refusal(code: &'static str, detail: &str) -> SegregationRefusal {
    SegregationRefusal {
        code,
        detail: detail.to_owned(),
    }
}

impl HostSegregation {
    fn decided_before(&self, task: &Task, user: &str) -> std::result::Result<bool, TasksError> {
        let filter = TaskFilter {
            status: Some(vec![TaskStatus::Completed]),
            case: Some(task.case.clone()),
            ..TaskFilter::default()
        };
        for other in self.store.list(&filter)? {
            if other.id == task.id {
                continue;
            }
            if task.duty.is_some() && other.duty != task.duty {
                continue;
            }
            let decided = self.store.events(&other.id)?.iter().any(|event| {
                matches!(event.action, TaskAction::Completed)
                    && !event.auto
                    && (event.actor == user || event.on_behalf_of.as_deref() == Some(user))
            });
            if decided {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

impl Segregation for HostSegregation {
    fn may_decide(&self, task: &Task, user: &str) -> std::result::Result<(), SegregationRefusal> {
        if task.distinct_from_creator {
            if task.created_by.as_deref() == Some(user) {
                return Err(refusal(
                    DISTINCT_CODE,
                    "the user created the underlying document",
                ));
            }
            let document_creator = drive(self.facts.facts(task)).and_then(|facts| facts.created_by);
            if document_creator.as_deref() == Some(user) {
                return Err(refusal(
                    DISTINCT_CODE,
                    "the user created the underlying document",
                ));
            }
        }
        if task.distinct_from_creator || task.duty.is_some() {
            match self.decided_before(task, user) {
                Ok(false) => {}
                Ok(true) => {
                    return Err(refusal(SOD_CODE, "the user already decided this Case"));
                }
                Err(_) => {
                    return Err(refusal(
                        SOD_CODE,
                        "the Case's earlier decisions cannot be read",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Approvers named by a rule: the rule graph's resolver when a module provides one, filtered to
/// active users (an inactive user is never an approver); without one, rule forms resolve nobody.
pub(super) struct HostResolver {
    pub(super) rules: Arc<dyn ApproverResolver>,
    pub(super) directory: Arc<dyn TaskDirectory>,
}

impl ApproverResolver for HostResolver {
    fn resolve(&self, rule: &str, case: &str, node: &str, form: &Json) -> Option<Vec<String>> {
        let users = self.rules.resolve(rule, case, node, form)?;
        Some(
            users
                .into_iter()
                .filter(|user| self.directory.is_active(user))
                .collect(),
        )
    }
}

/// Task notices: handed to the sink on the runtime; absent, failing or panicking sinks are
/// counted and never reach the action.
pub(super) struct HostNotifier {
    pub(super) sink: Option<Arc<dyn TaskNoticeSink>>,
    pub(super) hop: Hop,
    pub(super) stats: Arc<Stats>,
}

impl Notifier for HostNotifier {
    fn notify<'a>(&'a self, notification: &'a Notification) -> NotifyFuture<'a> {
        Box::pin(async move {
            let Some(sink) = self.sink.clone() else {
                self.stats.notice_dropped();
                return;
            };
            let notice = TaskNotice {
                kind: notification.kind.as_str(),
                task: notification.task.to_string(),
                case: notification.case.clone(),
                node: notification.node.clone(),
                title: notification.title.clone(),
                run_singular: notification.run_singular.clone(),
                due: notification.due,
                recipients: notification.recipients.clone(),
            };
            let kind = notice.kind;
            let task = notice.task.clone();
            let answer = self
                .hop
                .call_async(async move { sink.deliver(&notice).await })
                .await;
            if matches!(answer, Some(Ok(()))) {
                self.stats.notice_sent();
            } else {
                self.stats.notice_failed();
                tracing::warn!(kind, task = %task, "a task notice was not delivered");
            }
        })
    }
}
