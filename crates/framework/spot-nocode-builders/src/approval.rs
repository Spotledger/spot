//! The approval-workflow starter: a business user says who approves, how long they have, who
//! takes over when they are too slow, and what approval and rejection do — and gets a
//! `spot-bpm-model` process.
//!
//! The built graph is the platform's *approval with escalation* shape
//! ([`spot_bpm_model::templates::APPROVAL_ESCALATION`]): a decision for the approver, a wait
//! for the SLA, an escalated decision for the next person, and a terminal end for each
//! outcome, with the record steps the template asks for in between. It is **validated by the
//! BPM validator and compiled onto the DAG runtime** before it is returned, and it is a
//! **draft** version: publishing a process version is the BPM designer's and the release
//! pipeline's business (BPM-07), not the builder's.

use spot_bpm_model::validate::codes as bpm_codes;
use spot_bpm_model::{
    DecisionConfig, DocumentAction, DocumentConfig, Edge, Limits, LogicConfig, MetaView, Node,
    NodeId, NodeKind, Process, ProcessVersion, TriggerConfig, TriggerType, VersionStatus,
    WaitConfig, WaitType, compile_checked, to_text, validate,
};

use crate::codes;
use crate::error::{Issue, is_blocked};
use crate::report::is_name;

/// Who is asked to approve.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Approver {
    /// Everyone holding a role.
    Role {
        /// The role's name.
        role: String,
    },
    /// One named person.
    User {
        /// The user's name.
        user: String,
    },
    /// The person named in a field of the record.
    Field {
        /// The field holding the user.
        field: String,
    },
    /// The field's manager, then the manager's manager, up to the escalation.
    ManagerChain {
        /// The field holding the first person.
        field: String,
    },
}

impl Approver {
    /// A role.
    #[must_use]
    pub fn role(name: &str) -> Self {
        Self::Role {
            role: name.to_owned(),
        }
    }

    /// One person.
    #[must_use]
    pub fn user(name: &str) -> Self {
        Self::User {
            user: name.to_owned(),
        }
    }

    /// The person named in a field of the record.
    #[must_use]
    pub fn field(name: &str) -> Self {
        Self::Field {
            field: name.to_owned(),
        }
    }

    /// A chain of managers starting from a field.
    #[must_use]
    pub fn manager_chain(field: &str) -> Self {
        Self::ManagerChain {
            field: field.to_owned(),
        }
    }

    /// The assignee expression the process's decision node carries.
    #[must_use]
    pub fn assignee(&self) -> String {
        match self {
            Self::Role { role } => format!("role:{role}"),
            Self::User { user } => format!("user:{user}"),
            Self::Field { field } => format!("field:{field}"),
            Self::ManagerChain { field } => format!("manager_chain:{field}"),
        }
    }

    /// The name inside the approver.
    fn name(&self) -> &str {
        match self {
            Self::Role { role } => role,
            Self::User { user } => user,
            Self::Field { field } | Self::ManagerChain { field } => field,
        }
    }

    /// Checks the approver is one a person could resolve.
    fn check(&self, at: &str, issues: &mut Vec<Issue>) {
        if self.name().trim().is_empty() || !is_name(self.name()) {
            issues.push(Issue::at(
                codes::BAD_APPROVER,
                at,
                "An approver is not a name the site can resolve.",
                "Choose a role, a person, or a field that holds the approver.",
            ));
        }
    }
}

/// One step that happens to the record when an outcome is reached.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ApprovalAction {
    /// Write a value into a field.
    SetField {
        /// The field.
        field: String,
        /// The value, as text.
        value: String,
    },
    /// Submit (post) the record through its own lifecycle.
    Submit,
}

impl ApprovalAction {
    /// A step that writes `value` into `field`.
    #[must_use]
    pub fn set_field(field: &str, value: &str) -> Self {
        Self::SetField {
            field: field.to_owned(),
            value: value.to_owned(),
        }
    }

    /// A step that submits the record.
    #[must_use]
    pub fn submit() -> Self {
        Self::Submit
    }

    /// The step's node kind.
    fn kind(&self, doctype: &str) -> NodeKind {
        let config = match self {
            Self::SetField { field, value } => DocumentConfig {
                action: DocumentAction::Update,
                doctype: doctype.to_owned(),
                operation: None,
                mapping: Some(format!("{field} = {value}")),
                principal: None,
            },
            Self::Submit => DocumentConfig {
                action: DocumentAction::Post,
                doctype: doctype.to_owned(),
                operation: None,
                mapping: None,
                principal: None,
            },
        };
        NodeKind::Document(config)
    }

    /// Checks the step names what it needs.
    fn check(&self, at: &str, issues: &mut Vec<Issue>) {
        if let Self::SetField { field, .. } = self
            && !is_name(field)
        {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                at,
                "A step writes into a field that is not a valid field name.",
                "Choose the field the step writes into.",
            ));
        }
    }
}

/// When the approval starts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StartOn {
    /// When a record of the type is created.
    #[default]
    Created,
    /// When a person starts it.
    Manual,
}

/// The approval workflow a business user describes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalTemplate {
    /// The process name.
    pub name: String,
    /// The heading a person reads.
    pub title: String,
    /// The app that owns the process.
    pub app: String,
    /// The record type being approved.
    pub doctype: String,
    /// The first approver.
    pub approvers: Approver,
    /// How long the approver has, in whole seconds, before escalation.
    pub sla_seconds: u64,
    /// Who takes over when the SLA runs out.
    pub escalate_to: Approver,
    /// What happens to the record when it is approved.
    #[serde(default)]
    pub on_approve: Vec<ApprovalAction>,
    /// What happens to the record when it is rejected.
    #[serde(default)]
    pub on_reject: Vec<ApprovalAction>,
    /// When the approval starts.
    #[serde(default)]
    pub start_on: StartOn,
}

impl ApprovalTemplate {
    /// A standard template: one day to decide, then escalation, and the record is submitted
    /// when it is approved.
    #[must_use]
    pub fn new(
        name: &str,
        title: &str,
        doctype: &str,
        approvers: Approver,
        escalate_to: Approver,
    ) -> Self {
        Self {
            name: name.to_owned(),
            title: title.to_owned(),
            app: "workflow".to_owned(),
            doctype: doctype.to_owned(),
            approvers,
            sla_seconds: 86_400,
            escalate_to,
            on_approve: vec![ApprovalAction::Submit],
            on_reject: Vec::new(),
            start_on: StartOn::Created,
        }
    }

    /// The same template with a different SLA, in whole seconds.
    #[must_use]
    pub fn with_sla(mut self, seconds: u64) -> Self {
        self.sla_seconds = seconds;
        self
    }

    /// The same template with different steps when the record is approved.
    #[must_use]
    pub fn approving(mut self, actions: Vec<ApprovalAction>) -> Self {
        self.on_approve = actions;
        self
    }

    /// The same template with steps when the record is rejected.
    #[must_use]
    pub fn rejecting(mut self, actions: Vec<ApprovalAction>) -> Self {
        self.on_reject = actions;
        self
    }

    /// Every structural problem with the template.
    #[must_use]
    pub fn issues(&self) -> Vec<Issue> {
        let mut issues = Vec::new();
        if !is_name(&self.name) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                "name",
                "The approval's name is not a valid name.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            ));
        }
        if !is_name(&self.app) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                "app",
                "The approval's app is not a valid name.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            ));
        }
        if !is_name(&self.doctype) {
            issues.push(Issue::at(
                codes::BAD_NAME,
                "doctype",
                "The approval does not name a valid kind of record.",
                "Choose the kind of record that needs approval.",
            ));
        }
        if self.title.trim().is_empty() {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                "title",
                "The approval has no title.",
                "Give the approval a title a person can read.",
            ));
        }
        self.approvers.check("approvers", &mut issues);
        if self.sla_seconds == 0 {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                "sla_seconds",
                "The approver has no time at all to decide, so every request would escalate.",
                "Give the approver at least one second.",
            ));
        }
        self.escalate_to.check("escalate_to", &mut issues);
        let mut submits = 0;
        for (index, action) in self.on_approve.iter().enumerate() {
            action.check(&format!("on_approve[{index}]"), &mut issues);
            if matches!(action, ApprovalAction::Submit) {
                submits += 1;
            }
        }
        if submits > 1 {
            issues.push(Issue::at(
                codes::BAD_VALUE,
                "on_approve",
                "The record is submitted more than once when it is approved.",
                "Keep a single submit step.",
            ));
        }
        for (index, action) in self.on_reject.iter().enumerate() {
            action.check(&format!("on_reject[{index}]"), &mut issues);
        }
        issues
    }
}

/// A validated approval process, ready to publish as a draft version.
#[derive(Clone, Debug)]
pub struct ApprovalPlan {
    /// The process, with its one draft version.
    pub process: Process,
    /// The draft version.
    pub version: ProcessVersion,
    /// The version's canonical text form, the form a bundle carries.
    pub text: String,
}

/// Builds the approval process from `template`, validating it against the site's metadata.
///
/// # Errors
///
/// Every issue: the template's own, or the BPM validator's. Nothing is stored.
pub fn build(
    template: &ApprovalTemplate,
    meta: &dyn MetaView,
    version: u32,
) -> Result<ApprovalPlan, Vec<Issue>> {
    let mut issues = template.issues();
    if version == 0 {
        issues.push(Issue::at(
            codes::BAD_VALUE,
            "version",
            "A process version starts at one.",
            "Publish the approval as version 1 or later.",
        ));
    }
    if is_blocked(&issues) {
        return Err(issues);
    }

    let mut graph = Graph::default();
    let start = graph.node(
        "start",
        NodeKind::Trigger(TriggerConfig {
            trigger: match template.start_on {
                StartOn::Created => TriggerType::DocumentEvent,
                StartOn::Manual => TriggerType::Manual,
            },
            doctype: Some(template.doctype.clone()),
            event: match template.start_on {
                StartOn::Created => Some("create".to_owned()),
                StartOn::Manual => None,
            },
            cron: None,
            path: None,
        }),
    )?;
    let first = graph.node(
        "first_approval",
        NodeKind::Decision(DecisionConfig {
            title: "Approve this request?".to_owned(),
            assignee: Some(template.approvers.assignee()),
            outcomes: vec!["approve".to_owned(), "reject".to_owned()],
        }),
    )?;
    let wait = graph.node(
        "wait_for_sla",
        NodeKind::Wait(WaitConfig {
            wait: WaitType::Duration,
            event: None,
            seconds: Some(template.sla_seconds),
            until: None,
        }),
    )?;
    let escalate = graph.node(
        "escalated_approval",
        NodeKind::Decision(DecisionConfig {
            title: "Escalated approval".to_owned(),
            assignee: Some(template.escalate_to.assignee()),
            outcomes: vec!["approve".to_owned(), "reject".to_owned()],
        }),
    )?;
    let done = graph.terminal("approved_end")?;
    let rejected = graph.terminal("rejected_end")?;

    graph.add_edge(&start, &first);
    graph.add_default(&first, &wait);
    graph.add_edge(&wait, &escalate);

    // Each path gets its own record steps, so one decision never runs a step twice.
    let first_yes = graph.chain(&template.on_approve, "approved", &template.doctype)?;
    let first_no = graph.chain(&template.on_reject, "rejected", &template.doctype)?;
    graph.add_condition(&first, &first_yes.head, "doc.approved == true");
    graph.add_condition(&first, &first_no.head, "doc.approved == false");
    graph.add_edge(&first_yes.tail, &done);
    graph.add_edge(&first_no.tail, &rejected);

    let escalated_yes = graph.chain(
        &template.on_approve,
        "escalated_approved",
        &template.doctype,
    )?;
    let escalated_no = graph.chain(&template.on_reject, "escalated_rejected", &template.doctype)?;
    graph.add_condition(
        &escalate,
        &escalated_yes.head,
        "doc.escalated_approved == true",
    );
    graph.add_default(&escalate, &escalated_no.head);
    graph.add_edge(&escalated_yes.tail, &done);
    graph.add_edge(&escalated_no.tail, &rejected);

    let mut process_version = match ProcessVersion::new(
        template.name.clone(),
        template.app.clone(),
        version,
        VersionStatus::Draft,
        graph.nodes,
        graph.edges,
        Vec::new(),
    ) {
        Ok(version) => version,
        Err(_) => {
            return Err(vec![Issue::at(
                codes::BAD_NAME,
                "name",
                "The approval's name is not one the workflow model accepts.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            )]);
        }
    };
    process_version.title = Some(template.title.clone());
    let process = match Process::new(
        process_version.name.clone(),
        process_version.app.clone(),
        vec![process_version.clone()],
    ) {
        Ok(process) => process,
        Err(_) => {
            return Err(vec![Issue::at(
                codes::BAD_NAME,
                "name",
                "The approval's name is not one the workflow model accepts.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            )]);
        }
    };

    let problems = validate(&process, meta, &Limits::new());
    if !problems.is_empty() {
        return Err(problems.iter().map(map_problem).collect());
    }
    if let Err(problems) = compile_checked(&process_version, meta, &Limits::new()) {
        return Err(problems.iter().map(map_problem).collect());
    }

    let text = to_text(&process_version);
    Ok(ApprovalPlan {
        process,
        version: process_version,
        text,
    })
}

/// The entry and exit of one path's record steps.
#[derive(Clone, Debug)]
struct Path {
    head: NodeId,
    tail: NodeId,
}

/// The graph being assembled.
#[derive(Default)]
struct Graph {
    nodes: Vec<Node>,
    edges: Vec<Edge>,
}

impl Graph {
    fn node(&mut self, id: &str, kind: NodeKind) -> Result<NodeId, Issue> {
        let node_id = NodeId::new(id).map_err(|_| {
            Issue::at(
                codes::BAD_NAME,
                id,
                "The approval names a step the workflow model does not accept.",
                "Use lower-case letters, digits and underscores, starting with a letter.",
            )
        })?;
        self.nodes.push(Node::new(node_id.clone(), kind));
        Ok(node_id)
    }

    fn terminal(&mut self, id: &str) -> Result<NodeId, Issue> {
        let node_id = self.node(id, NodeKind::Logic(LogicConfig::default()))?;
        if let Some(node) = self.nodes.last_mut() {
            node.terminal = true;
        }
        Ok(node_id)
    }

    fn add_edge(&mut self, from: &NodeId, to: &NodeId) {
        self.edges.push(Edge::new(from.clone(), to.clone()));
    }

    fn add_condition(&mut self, from: &NodeId, to: &NodeId, condition: &str) {
        self.edges
            .push(Edge::new(from.clone(), to.clone()).with_condition(condition));
    }

    fn add_default(&mut self, from: &NodeId, to: &NodeId) {
        self.edges
            .push(Edge::new(from.clone(), to.clone()).as_default());
    }

    /// Adds the record steps for `actions` and returns the path's first and last node. An
    /// empty action list gives an explicit pass-through step, so every branch has a node to
    /// name.
    fn chain(
        &mut self,
        actions: &[ApprovalAction],
        prefix: &str,
        doctype: &str,
    ) -> Result<Path, Issue> {
        if actions.is_empty() {
            let id = self.node(
                &format!("{prefix}_nothing"),
                NodeKind::Logic(LogicConfig::default()),
            )?;
            return Ok(Path {
                head: id.clone(),
                tail: id,
            });
        }
        let mut head: Option<NodeId> = None;
        let mut previous: Option<NodeId> = None;
        for (index, action) in actions.iter().enumerate() {
            let node_id = self.node(&format!("{prefix}_step_{index}"), action.kind(doctype))?;
            if head.is_none() {
                head = Some(node_id.clone());
            }
            if let Some(previous) = &previous {
                self.add_edge(previous, &node_id);
            }
            previous = Some(node_id);
        }
        match (head, previous) {
            (Some(head), Some(tail)) => Ok(Path { head, tail }),
            _ => Err(Issue::error(
                codes::BAD_VALUE,
                "The approval has no step to run.",
                "Add a step, or remove it.",
            )),
        }
    }
}

/// A BPM validation problem, as the designer reads it.
fn map_problem(problem: &spot_bpm_model::Problem) -> Issue {
    let at = problem
        .node
        .as_ref()
        .map(|node| node.as_str().to_owned())
        .unwrap_or_else(|| "process".to_owned());
    let issue = match problem.code {
        bpm_codes::UNKNOWN_DOCTYPE => Issue::at(
            codes::UNKNOWN_DOCTYPE,
            at,
            "The approval starts from a kind of record the site does not have.",
            "Choose a kind of record this site has.",
        ),
        bpm_codes::UNKNOWN_OPERATION => Issue::at(
            codes::INVALID_PROCESS,
            at,
            "The approval asks the record to do something its own lifecycle does not allow.",
            "Remove the step, or choose another kind of record.",
        ),
        _ => Issue::at(
            codes::INVALID_PROCESS,
            at,
            format!("The approval flow is not valid yet: {}.", problem.message),
            "Check the approver, the record type and the record steps.",
        ),
    };
    issue.from_source(problem.code)
}
