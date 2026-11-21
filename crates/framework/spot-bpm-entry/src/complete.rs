//! Completing a form step: the draft and the step's transition commit together.
//!
//! [`FormService::complete`] is what runs when the assigned person submits the form. In order:
//!
//! 1. the inbox confirms the person may complete the task (a repeated completion of an already
//!    completed task is a no-op);
//! 2. the entered values are checked against the step's field list and ADR-12 (no floats), then
//!    against the record type's own validation ([`DraftWriter::validate`]). A refusal returns
//!    the field errors and **nothing is written**: the step stays open for another attempt;
//! 3. the step's transition is planned from the run, and the draft write and the transition are
//!    committed through [`Unit::commit_with`] in one storage transaction. If the draft write
//!    fails, the transaction rolls back, the node is exactly where it was and a retry finds it
//!    so;
//! 4. the inbox task is marked completed.
//!
//! The draft is created or updated through the record type's lifecycle as the assigned person,
//! flagged as owned by the request ([`crate::OWNER_FIELD`]); a step never writes a status or a
//! draft outside the lifecycle.

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::sync::Arc;

use serde_json::{Map, Value as Json, json};
use spot_app_api::prelude::{OffsetDateTime, RecordId, Version};
use spot_bpm_model::DraftMode;
use spot_dag::{CompiledDag, NodeId};
use spot_dag_exec::unit::WorkFuture;
use spot_dag_exec::{ExecStore, NodeOutcome, Run, RunId, Transition, Unit};
use spot_storage_contract::Transaction;

use crate::error::{EntryError, EntryResult, FieldIssue};
use crate::form::{FormNodeConfig, Inbox, PortFuture, TicketState};
use crate::guard::RequestAuthority;
use crate::ownership::{OWNER_FIELD, RequestContext};

/// What the writer is asked to save.
#[derive(Clone, Debug, PartialEq)]
pub struct DraftSave {
    /// Create a new draft, or update the one the request already owns.
    pub mode: DraftMode,
    /// The record type.
    pub doctype: String,
    /// The record: the new id for a create, the existing id for an update.
    pub document: RecordId,
    /// The version the person saw, for an update.
    pub expected: Option<Version>,
    /// The entered values, with the owner flag set to the request.
    pub values: Map<String, Json>,
    /// The person the lifecycle runs as.
    pub actor: String,
    /// The request context the lifecycle sees (its command id), so governance and the entry
    /// guard recognise the write as the request's own.
    pub context: RequestContext,
    /// The business time.
    pub now: OffsetDateTime,
}

/// The record type's lifecycle, as the form step drives it.
///
/// The host implements it over the document engine: `validate` runs the lifecycle's validation
/// without writing, and `write` runs its create or update inside the unit's transaction, so
/// every validation and permission applies exactly as on a click.
pub trait DraftWriter<Tx>: Send + Sync {
    /// Validates `save` through the record type's lifecycle without writing.
    ///
    /// # Errors
    /// The field issues, in the order found.
    fn validate(&self, save: &DraftSave) -> Result<(), Vec<FieldIssue>>;

    /// Creates or updates the draft inside `tx`.
    fn write<'t>(self: Arc<Self>, tx: &'t mut Tx, save: Arc<DraftSave>) -> WorkFuture<'t>;
}

/// The reading side of a step's configuration: the form node as the request's pinned process
/// version defines it, never as the caller says.
pub trait FormConfigs: Send + Sync {
    /// The configuration of `node` in `request`.
    ///
    /// # Errors
    /// [`EntryError::ConfigInvalid`] when the node is not a form step.
    fn config(&self, request: &str, node: &str) -> EntryResult<FormNodeConfig>;
}

/// A transition planned from a run, with the run version it applies to.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedCompletion {
    /// The transition that completes the node.
    pub transition: Transition,
    /// The run version the transition was computed from.
    pub expected_version: u64,
}

/// Plans the completion of a node.
pub trait CompletionPlanner: Send + Sync {
    /// Plans `node` of `request` done with `output`.
    fn plan<'a>(
        &'a self,
        request: &'a str,
        node: &'a str,
        output: Json,
        now: OffsetDateTime,
    ) -> PortFuture<'a, EntryResult<PlannedCompletion>>;
}

/// Finds the compiled graph a run is pinned to.
pub trait DagSource: Send + Sync {
    /// The graph of `run`, when it is known.
    fn dag(&self, run: &Run) -> Option<Arc<CompiledDag>>;
}

/// One graph for every run (a host with one process, and tests).
#[derive(Clone)]
pub struct StaticDag(pub Arc<CompiledDag>);

impl DagSource for StaticDag {
    fn dag(&self, _run: &Run) -> Option<Arc<CompiledDag>> {
        Some(self.0.clone())
    }
}

/// A planner over the DAG execution store.
pub struct ExecPlanner {
    store: Arc<dyn ExecStore>,
    dags: Arc<dyn DagSource>,
}

impl ExecPlanner {
    /// A planner reading runs from `store` and graphs from `dags`.
    #[must_use]
    pub fn new(store: Arc<dyn ExecStore>, dags: Arc<dyn DagSource>) -> Self {
        Self { store, dags }
    }
}

fn commit_error(detail: impl Into<String>) -> EntryError {
    EntryError::CommitFailed {
        detail: detail.into(),
    }
}

impl CompletionPlanner for ExecPlanner {
    fn plan<'a>(
        &'a self,
        request: &'a str,
        node: &'a str,
        output: Json,
        now: OffsetDateTime,
    ) -> PortFuture<'a, EntryResult<PlannedCompletion>> {
        Box::pin(async move {
            let run_id = RunId::new(request).map_err(|error| commit_error(error.to_string()))?;
            let node_id = NodeId::new(node).map_err(|error| commit_error(error.to_string()))?;
            let run = self
                .store
                .load_run(&run_id)
                .await
                .map_err(|error| commit_error(error.to_string()))?
                .ok_or_else(|| commit_error("the request does not exist"))?;
            let dag = self
                .dags
                .dag(&run)
                .ok_or_else(|| commit_error("the request's process graph is unknown"))?;
            let transition = spot_dag_exec::complete(
                &dag,
                &run,
                &node_id,
                NodeOutcome::Done {
                    output: Some(output),
                    branches: Vec::new(),
                },
                now,
            )
            .map_err(|error| commit_error(error.to_string()))?;
            Ok(PlannedCompletion {
                transition,
                expected_version: run.version,
            })
        })
    }
}

/// What the person submits.
#[derive(Clone, Debug, PartialEq)]
pub struct FormSubmission {
    /// The task being completed.
    pub ticket: String,
    /// The request the step belongs to.
    pub request: String,
    /// The form node.
    pub node: String,
    /// The node's attempt number.
    pub attempt: u32,
    /// The person completing it.
    pub actor: String,
    /// For a create: the id the new draft gets. For an update: the draft's id.
    pub draft: Option<RecordId>,
    /// For an update: the version the person saw.
    pub expected: Option<Version>,
    /// The entered values, as a JSON object. Decimals and dates travel as strings.
    pub values: Json,
    /// The business time.
    pub now: OffsetDateTime,
}

/// What a completion did.
#[derive(Clone, Debug, PartialEq)]
pub struct FormCompleted {
    /// The draft the step saved, when it writes one.
    pub draft: Option<(String, RecordId)>,
    /// True when the task had already been completed and nothing was written.
    pub already_done: bool,
    /// The output the node recorded.
    pub output: Json,
}

/// Completes form steps.
pub struct FormService<Tx: Transaction + 'static, U: Unit<Tx>> {
    unit: Arc<U>,
    planner: Arc<dyn CompletionPlanner>,
    writer: Arc<dyn DraftWriter<Tx>>,
    inbox: Arc<dyn Inbox>,
    configs: Arc<dyn FormConfigs>,
    authority: Arc<dyn RequestAuthority>,
    _tx: PhantomData<fn(Tx)>,
}

impl<Tx: Transaction + 'static, U: Unit<Tx>> FormService<Tx, U> {
    /// A service over the unit of work, the planner, the record type's lifecycle writer, the
    /// inbox, the step configurations and the request authority that issues the grant the
    /// draft write runs under.
    #[must_use]
    pub fn new(
        unit: Arc<U>,
        planner: Arc<dyn CompletionPlanner>,
        writer: Arc<dyn DraftWriter<Tx>>,
        inbox: Arc<dyn Inbox>,
        configs: Arc<dyn FormConfigs>,
        authority: Arc<dyn RequestAuthority>,
    ) -> Self {
        Self {
            unit,
            planner,
            writer,
            inbox,
            configs,
            authority,
            _tx: PhantomData,
        }
    }

    /// Completes the step.
    ///
    /// # Errors
    /// [`EntryError::FormInvalid`] when the values are refused (the step stays open and nothing
    /// is written), [`EntryError::StepNotOpen`] when the person may not complete it,
    /// [`EntryError::CommitFailed`] or [`EntryError::DraftFailed`] when the commit did not
    /// happen (the transaction rolled back), or [`EntryError::Inbox`].
    pub async fn complete(&self, submission: &FormSubmission) -> EntryResult<FormCompleted> {
        let state = self
            .inbox
            .check(&submission.ticket, &submission.actor, submission.now)
            .await?;
        if state == TicketState::Done {
            return Ok(FormCompleted {
                draft: None,
                already_done: true,
                output: Json::Null,
            });
        }
        let config = self.configs.config(&submission.request, &submission.node)?;
        let values = object_of(&submission.values)?;
        check_values(&config, &values)?;

        let token = self.authority.issue(
            &submission.request,
            &submission.node,
            submission.attempt,
            &submission.actor,
        )?;
        let context = RequestContext {
            request: submission.request.clone(),
            node: submission.node.clone(),
            attempt: submission.attempt,
            token,
        };
        let save = self.save_of(&config, submission, values, context)?;
        if let Some(save) = &save {
            self.writer
                .validate(save)
                .map_err(|fields| EntryError::FormInvalid { fields })?;
        }

        let output = output_of(&config, save.as_ref(), &submission.values);
        let planned = self
            .planner
            .plan(
                &submission.request,
                &submission.node,
                output.clone(),
                submission.now,
            )
            .await?;
        self.commit(&planned, save.clone()).await?;
        self.inbox
            .complete(&submission.ticket, &submission.actor, submission.now)
            .await?;
        Ok(FormCompleted {
            draft: save.map(|save| (save.doctype, save.document)),
            already_done: false,
            output,
        })
    }

    fn save_of(
        &self,
        config: &FormNodeConfig,
        submission: &FormSubmission,
        mut values: Map<String, Json>,
        context: RequestContext,
    ) -> EntryResult<Option<DraftSave>> {
        if config.draft_mode == DraftMode::Collect {
            return Ok(None);
        }
        let doctype = config
            .record_type
            .clone()
            .ok_or_else(|| EntryError::ConfigInvalid {
                node: submission.node.clone(),
                detail: "a form step that writes a draft needs a `record_type`".to_owned(),
            })?;
        let document = submission.draft.ok_or_else(|| EntryError::DraftFailed {
            detail: "the submission names no draft record".to_owned(),
        })?;
        values.insert(OWNER_FIELD.to_owned(), json!(submission.request));
        Ok(Some(DraftSave {
            mode: config.draft_mode,
            doctype,
            document,
            expected: submission.expected,
            values,
            actor: submission.actor.clone(),
            context,
            now: submission.now,
        }))
    }

    async fn commit(
        &self,
        planned: &PlannedCompletion,
        save: Option<DraftSave>,
    ) -> EntryResult<()> {
        let save = save.map(Arc::new);
        let writer = self.writer.clone();
        let mut work = work_for(writer, save);
        self.unit
            .commit_with(&planned.transition, planned.expected_version, &mut work)
            .await
            .map(|_| ())
            .map_err(|error| commit_error(error.to_string()))
    }
}

/// The unit's work: the draft write, when there is one. It owns what it needs, so the future it
/// returns borrows only the transaction.
fn work_for<Tx: 'static>(
    writer: Arc<dyn DraftWriter<Tx>>,
    save: Option<Arc<DraftSave>>,
) -> impl for<'t> FnMut(&'t mut Tx) -> WorkFuture<'t> + Send {
    move |tx| match &save {
        Some(save) => writer.clone().write(tx, save.clone()),
        None => Box::pin(async { Ok(()) }),
    }
}

fn object_of(values: &Json) -> EntryResult<Map<String, Json>> {
    match values {
        Json::Object(map) => Ok(map.clone()),
        Json::Null => Ok(Map::new()),
        _ => Err(EntryError::FormInvalid {
            fields: vec![FieldIssue::new(
                "values",
                "not_an_object",
                "The entered values must be a set of fields.",
            )],
        }),
    }
}

/// Checks the values against the step's field list and ADR-12: only fields the step shows may
/// be entered, and no value is a floating-point number.
fn check_values(config: &FormNodeConfig, values: &Map<String, Json>) -> EntryResult<()> {
    let allowed: BTreeSet<&str> = config.fields.iter().map(String::as_str).collect();
    let mut issues = Vec::new();
    for (field, value) in values {
        if field == OWNER_FIELD {
            issues.push(FieldIssue::new(
                field,
                "not_in_form",
                "This field is set by the request and cannot be entered.",
            ));
        } else if !allowed.is_empty() && !allowed.contains(field.as_str()) {
            issues.push(FieldIssue::new(
                field,
                "not_in_form",
                "This field is not part of this step.",
            ));
        }
        if has_float(value) {
            issues.push(FieldIssue::new(
                field,
                "no_float",
                "Write amounts and quantities as exact decimal text, not as a floating-point number.",
            ));
        }
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(EntryError::FormInvalid { fields: issues })
    }
}

fn has_float(value: &Json) -> bool {
    match value {
        Json::Number(number) => number.is_f64(),
        Json::Array(items) => items.iter().any(has_float),
        Json::Object(map) => map.values().any(has_float),
        _ => false,
    }
}

fn output_of(config: &FormNodeConfig, save: Option<&DraftSave>, values: &Json) -> Json {
    match save {
        Some(save) => json!({
            "draft": {
                "doctype": save.doctype,
                "id": save.document.to_string(),
                "on_abort": config.on_abort.token(),
            }
        }),
        None => json!({ "collected": values }),
    }
}
