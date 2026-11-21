//! The form step (ADR-080 section 2.2): a named person or role enters or reviews a set of
//! fields of a record, producing an ordinary draft record owned by the request.
//!
//! [`FormStepHandler`] is the engine-side half. When the engine reaches a form node it opens a
//! task for the assignee in the inbox and parks the node on a human-task wait. It writes
//! nothing: the draft is created or updated, as the assigned person and through the record
//! type's lifecycle, when the person completes the step, together with the step's transition
//! (see [`crate::FormService`]). The step can also collect plain request data with no record
//! ([`DraftMode::Collect`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value as Json, json};
use spot_app_api::prelude::OffsetDateTime;
use spot_bpm_engine::{NodeCtx, NodeHandler, NodeKind, NodeOutcome, WaitSpec};
use spot_bpm_model::DraftMode;

use crate::error::{EntryError, EntryResult};
use crate::label::RunLabel;
use crate::ownership::AbortPolicy;

/// A boxed future, as the async ports return it.
pub type PortFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The engine's token for the form node kind.
///
/// The engine's kind enum has no variant for it yet (see the report's open questions), so the
/// handler registers under the open `Unknown` kind with this token.
pub const FORM_KIND_TOKEN: &str = "form";

/// The configuration of a form node as the engine hands it to the handler: the model's
/// `FormConfig` keys, plus the abort policy and presentation settings the step owns.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FormNodeConfig {
    /// The record type the draft belongs to; absent only for [`DraftMode::Collect`].
    #[serde(default)]
    pub record_type: Option<String>,
    /// The fields the person enters or reviews; empty means every field of the type.
    #[serde(default)]
    pub fields: Vec<String>,
    /// Who is asked: a user, a `role:` name or an approver rule.
    #[serde(default)]
    pub assignee: Option<String>,
    /// Whether the step creates, updates or collects without a record.
    #[serde(default)]
    pub draft_mode: DraftMode,
    /// What happens to the draft when the request is cancelled or undone.
    #[serde(default)]
    pub on_abort: AbortPolicy,
    /// The task title; a default from the run label when absent.
    #[serde(default)]
    pub title: Option<String>,
    /// Seconds until the task is due.
    #[serde(default)]
    pub due_seconds: Option<u64>,
    /// For [`DraftMode::Update`]: the variable that holds the draft an earlier step made.
    #[serde(default)]
    pub draft_from: Option<String>,
}

impl FormNodeConfig {
    /// Parses and checks a node's configuration.
    ///
    /// # Errors
    /// [`EntryError::ConfigInvalid`] when it does not parse, a record-writing mode has no
    /// record type, or an update step names no draft to update.
    pub fn parse(config: &Json, node: &str) -> EntryResult<Self> {
        let parsed: Self =
            serde_json::from_value(config.clone()).map_err(|error| EntryError::ConfigInvalid {
                node: node.to_owned(),
                detail: format!("the form configuration did not parse: {error}"),
            })?;
        let invalid = |detail: &str| EntryError::ConfigInvalid {
            node: node.to_owned(),
            detail: detail.to_owned(),
        };
        let has_type = parsed
            .record_type
            .as_deref()
            .is_some_and(|name| !name.trim().is_empty());
        match parsed.draft_mode {
            DraftMode::Create | DraftMode::Update if !has_type => {
                return Err(invalid(
                    "a form step that writes a draft needs a `record_type`",
                ));
            }
            DraftMode::Update if parsed.draft_from.as_deref().is_none_or(str::is_empty) => {
                return Err(invalid("an update form step needs `draft_from`"));
            }
            _ => {}
        }
        Ok(parsed)
    }
}

/// One task a form step asks the inbox to hold.
#[derive(Clone, Debug, PartialEq)]
pub struct InboxItem {
    /// The request the step belongs to.
    pub request: String,
    /// The node.
    pub node: String,
    /// The process the request runs.
    pub process: String,
    /// The task title.
    pub title: String,
    /// The record type, when the step writes a draft.
    pub doctype: Option<String>,
    /// The record the step reviews, when it updates one.
    pub subject: Option<String>,
    /// Who is asked, as the node names it.
    pub assignee: Option<String>,
    /// The form payload: record type, fields and mode.
    pub form: Json,
    /// When the task is due.
    pub due: Option<OffsetDateTime>,
    /// Who started the request.
    pub created_by: Option<String>,
    /// When the task is created.
    pub created_at: OffsetDateTime,
}

/// The task as the inbox knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ticket {
    /// The task id the person completes.
    pub id: String,
    /// When it is due.
    pub due: Option<OffsetDateTime>,
}

/// Whether a ticket still waits for its person.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TicketState {
    /// Open: the person may complete it.
    Open,
    /// Already completed: a repeated completion is a no-op.
    Done,
}

/// The task inbox, as the form step uses it.
pub trait Inbox: Send + Sync {
    /// Opens the task; idempotent for a `(request, node)` pair.
    ///
    /// # Errors
    /// [`EntryError::NoAssignee`] when the step resolves to nobody, [`EntryError::Inbox`].
    fn open(&self, item: &InboxItem) -> EntryResult<Ticket>;

    /// Checks that `actor` may complete the ticket now.
    fn check<'a>(
        &'a self,
        ticket: &'a str,
        actor: &'a str,
        now: OffsetDateTime,
    ) -> PortFuture<'a, EntryResult<TicketState>>;

    /// Marks the ticket completed by `actor`.
    fn complete<'a>(
        &'a self,
        ticket: &'a str,
        actor: &'a str,
        now: OffsetDateTime,
    ) -> PortFuture<'a, EntryResult<()>>;

    /// Cancels the ticket because its request was cancelled.
    ///
    /// # Errors
    /// [`EntryError::Inbox`].
    fn cancel(&self, ticket: &str, now: OffsetDateTime) -> EntryResult<()>;
}

/// The engine handler of the form node.
pub struct FormStepHandler {
    inbox: Arc<dyn Inbox>,
    label: RunLabel,
}

impl FormStepHandler {
    /// A handler that opens tasks in `inbox`, titled from `label`.
    #[must_use]
    pub fn new(inbox: Arc<dyn Inbox>, label: RunLabel) -> Self {
        Self { inbox, label }
    }

    fn open(&self, ctx: &NodeCtx<'_>) -> EntryResult<Ticket> {
        let config = FormNodeConfig::parse(ctx.config, ctx.node.as_str())?;
        let subject = match (config.draft_mode, config.draft_from.as_deref()) {
            (DraftMode::Update, Some(path)) => Some(draft_id_at(ctx, path)?),
            _ => None,
        };
        let title = config
            .title
            .clone()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or_else(|| self.label.form_title());
        let due = ctx.deadline.or_else(|| {
            config
                .due_seconds
                .map(|seconds| ctx.now + std::time::Duration::from_secs(seconds))
        });
        let item = InboxItem {
            request: ctx.case.to_string(),
            node: ctx.node.to_string(),
            process: String::new(),
            title,
            doctype: config.record_type.clone(),
            subject,
            assignee: config.assignee.clone(),
            form: json!({
                "record_type": config.record_type,
                "fields": config.fields,
                "draft_mode": match config.draft_mode {
                    DraftMode::Create => "create",
                    DraftMode::Update => "update",
                    DraftMode::Collect => "collect",
                },
            }),
            due,
            created_by: ctx.principal.map(str::to_owned),
            created_at: ctx.now,
        };
        self.inbox.open(&item)
    }
}

fn draft_id_at(ctx: &NodeCtx<'_>, path: &str) -> EntryResult<String> {
    ctx.variable(path)
        .and_then(|draft| draft.get("id"))
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| EntryError::ConfigInvalid {
            node: ctx.node.to_string(),
            detail: format!("the variable `{path}` holds no draft to update"),
        })
}

impl NodeHandler for FormStepHandler {
    fn kind(&self) -> NodeKind {
        NodeKind::Unknown(FORM_KIND_TOKEN.to_owned())
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        match self.open(ctx) {
            Ok(ticket) => NodeOutcome::Wait(WaitSpec::HumanTask {
                key: ticket.id,
                due: ticket.due,
            }),
            Err(error) => NodeOutcome::fail(error.code(), error.to_string()),
        }
    }
}
