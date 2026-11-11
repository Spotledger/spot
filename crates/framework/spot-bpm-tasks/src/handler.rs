//! The Decision node handler: a person decides, and the engine waits on a human task.
//!
//! Registered with the engine's [`HandlerRegistry`](spot_bpm_engine::HandlerRegistry) for
//! [`NodeKind::Decision`], the handler parses the node's config, resolves its assignee (a user, a
//! role, or an approver rule), opens the task through the [`TaskService`] and returns
//! [`NodeOutcome::Wait`] with a [`WaitSpec::HumanTask`]. When no assignee resolves it **parks the
//! Case** with `bpm_tasks.no_assignee` instead of guessing. Replayed after a crash, it finds the
//! task it already opened and waits again.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value as Json;
use spot_bpm_engine::{NodeCtx, NodeHandler, NodeKind, NodeOutcome, WaitSpec};

use crate::error::TasksError;
use crate::resolve::{ApproverResolver, Directory, resolve_assignees};
use crate::service::TaskService;
use crate::sla::SlaPolicy;
use crate::task::{AssigneeSpec, Decision, Priority, TaskId, TaskOption, TaskSpec};

/// The Decision node's config, as the compiled node carries it.
///
/// Only `title`, `assignee` and `outcomes` are written by the process model today; the rest are
/// optional and read when a model or a host supplies them, so nothing here has to be undone when
/// the model grows.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct DecisionNodeConfig {
    /// The question the person is asked.
    #[serde(default)]
    title: String,
    /// Who is asked: `user:`, `role:`, `rule:`/`approver:`, or a bare user name.
    #[serde(default)]
    assignee: Option<String>,
    /// The extra named paths the person may choose.
    #[serde(default)]
    outcomes: Vec<String>,
    /// The task's priority.
    #[serde(default)]
    priority: Option<Priority>,
    /// A due offset in seconds after the node ran, when the node sets one.
    #[serde(default)]
    due_seconds: Option<u64>,
    /// Whether the creator of the underlying document may not decide.
    #[serde(default)]
    distinct_from_creator: bool,
    /// The duty completing the task exercises, for the segregation gate.
    #[serde(default)]
    duty: Option<String>,
    /// The duty's scope.
    #[serde(default)]
    scope: Option<String>,
    /// The form payload shown with the task.
    #[serde(default)]
    form: Json,
    /// The SLA policy for the node, when it carries one.
    #[serde(default)]
    sla: Option<SlaPolicy>,
    /// Whether the SLA's final action may approve the task by the clock. Required for an
    /// `auto-approve` final action; refused on a segregated node.
    #[serde(default)]
    sla_auto_approve: bool,
    /// The process (the pinned DAG key) the Case runs, when the config carries it.
    #[serde(default)]
    process: Option<String>,
    /// The DocType of the underlying document.
    #[serde(default)]
    doctype: Option<String>,
    /// The underlying document's id.
    #[serde(default)]
    subject: Option<String>,
    /// The underlying document's creator.
    #[serde(default)]
    created_by: Option<String>,
}

/// The Decision node handler.
pub struct DecisionNodeHandler {
    service: Arc<TaskService>,
    directory: Arc<dyn Directory>,
    resolver: Arc<dyn ApproverResolver>,
    policies: BTreeMap<String, SlaPolicy>,
}

impl DecisionNodeHandler {
    /// Builds the handler over the task service, a directory and an approver resolver.
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
            policies: BTreeMap::new(),
        }
    }

    /// Attaches an SLA policy for one node id, used when the node's own config carries none.
    #[must_use]
    pub fn with_policy(mut self, node: &str, policy: SlaPolicy) -> Self {
        self.policies.insert(node.to_owned(), policy);
        self
    }

    /// Opens (or finds) the task for this node attempt, or reports a [`NodeOutcome::Fail`].
    fn open(&self, ctx: &NodeCtx<'_>) -> Result<crate::task::Task, NodeOutcome> {
        let config: DecisionNodeConfig =
            serde_json::from_value(ctx.config.clone()).map_err(|error| {
                NodeOutcome::fail(
                    "bpm_tasks.config_invalid",
                    format!("the decision config did not parse: {error}"),
                )
            })?;
        if config.title.trim().is_empty() {
            return Err(NodeOutcome::fail(
                "bpm_tasks.config_invalid",
                "a decision node needs a non-empty `title`",
            ));
        }
        let spec = AssigneeSpec::parse(config.assignee.as_deref().unwrap_or(""));
        let resolved = resolve_assignees(
            &spec,
            self.directory.as_ref(),
            self.resolver.as_ref(),
            ctx.case.as_str(),
            ctx.node.as_str(),
            &config.form,
        );
        if resolved.users.is_empty() {
            return Err(NodeOutcome::fail(
                "bpm_tasks.no_assignee",
                format!("node `{}` resolved to no assignee", ctx.node),
            ));
        }
        let options = build_options(&config.outcomes);
        let due = ctx.deadline.or_else(|| {
            config
                .due_seconds
                .map(|seconds| ctx.now + std::time::Duration::from_secs(seconds))
        });
        let sla = config
            .sla
            .clone()
            .or_else(|| self.policies.get(ctx.node.as_str()).cloned())
            .map(|mut policy| {
                policy.auto_approve_declared |= config.sla_auto_approve;
                policy
            });
        let task_spec = TaskSpec {
            id: TaskId::for_node(ctx.case.as_str(), ctx.node.as_str()),
            case: ctx.case.to_string(),
            node: ctx.node.to_string(),
            process: config.process.unwrap_or_default(),
            title: config.title,
            doctype: config.doctype,
            subject: config.subject,
            created_by: config.created_by,
            distinct_from_creator: config.distinct_from_creator,
            duty: config.duty,
            scope: config.scope,
            assignee_spec: spec,
            assignees: resolved.users,
            roles: resolved.roles,
            options,
            form: config.form,
            priority: config.priority.unwrap_or_default(),
            due,
            sla,
            created_at: ctx.now,
        };
        self.service
            .open_task(task_spec)
            .map_err(|error| match error {
                TasksError::ConfigInvalid { node, detail } => NodeOutcome::fail(
                    "bpm_tasks.config_invalid",
                    format!("node `{node}`: {detail}"),
                ),
                other => {
                    NodeOutcome::fail(other.code(), format!("opening the task failed: {other}"))
                }
            })
    }
}

/// Builds the options: approve, reject, then one named path per non-standard outcome label.
fn build_options(outcomes: &[String]) -> Vec<TaskOption> {
    let mut options = vec![
        TaskOption {
            label: "Approve".to_owned(),
            decision: Decision::Approve,
            branch: "approve".to_owned(),
        },
        TaskOption {
            label: "Reject".to_owned(),
            decision: Decision::Reject,
            branch: "reject".to_owned(),
        },
    ];
    for label in outcomes {
        let lower = label.to_lowercase();
        if lower == "approve" || lower == "reject" {
            continue;
        }
        options.push(TaskOption {
            label: label.clone(),
            decision: Decision::Choose(label.clone()),
            branch: label.clone(),
        });
    }
    options
}

impl NodeHandler for DecisionNodeHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Decision
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        match self.open(ctx) {
            Ok(task) => NodeOutcome::Wait(WaitSpec::HumanTask {
                key: task.id.to_string(),
                due: task.due,
            }),
            Err(outcome) => outcome,
        }
    }
}
