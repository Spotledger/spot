//! Durable delivery of the `*_committed` steps (ADR-51 phase 4, ADR-53): the job is written in
//! the operation's own transaction, so a crash between the commit and the steps loses nothing.
//!
//! # How it works
//!
//! [`InProcessDelivery`](super::InProcessDelivery) runs the steps in the caller's task right
//! after the commit; a crash in between loses them. [`DurableDelivery`] instead implements
//! [`CommittedDelivery::prepare`], which the engine calls **inside the transaction, after the
//! last write and before the commit**. It enqueues, through the same outbox path as a phase-3
//! job (topic [`TOPIC_JOB`](super::TOPIC_JOB), decoded by
//! [`job_from_event`](super::job_from_event)), one job [`JOB_COMMITTED_STEPS`] per document write
//! that has committed steps. The job therefore exists if and only if the operation committed:
//! a rollback (a refusal, a failing participant, a failed commit) removes it with the document.
//! [`deliver`](CommittedDelivery::deliver) runs nothing and returns no messages: the caller gets
//! the operation's own messages only, as the steps now run later, in the job engine.
//!
//! # The job
//!
//! One job per `(DocType, document, operation, version)` of the batch, in the order the writes
//! happened; its steps are the committed points of that write, in ADR-51 order (for a create:
//! `create_committed`, then `save_committed`). Its arguments are [`CommittedJobArgs`]: the
//! site, DocType, document id, version, operation, command id, the acting identity, the
//! operation's instant, the language and the point names. **No field value is ever carried**: not
//! a document field, not a masked or secret one. The runner reads the document when it runs
//! (see [`StepSource`]). A deletion carries no image: its steps see the document's id and
//! DocType only.
//!
//! The idempotency key is `<command_id>:<operation>:<document id>:<version>` (`-` when the
//! operation has no command id). A second enqueue of the same step, for example a replayed
//! outbox event, is one job. Everything in the job is a function of the operation and its
//! invocation: the same operation produces the same arguments and key, and no wall clock is
//! read.
//!
//! # Isolation
//!
//! The job carries its site; the runner refuses a job whose arguments name another site than
//! the job's own, and builds the invocation from them, never from ambient state.

use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_app_api::context::{Invocation, JobRequest};
use spot_app_api::lifecycle::{Phase, Target, op_name};
use spot_app_api::prelude::{CommandId, OffsetDateTime, RecordId, SiteId};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, Document, ErrorClass, HookPoint, Message, Operation,
};
use spot_storage_contract::StorageBackend;
use time::format_description::well_known::Rfc3339;

use super::committed::{
    CommittedBatch, CommittedDelivery, CommittedStep, CommittedSteps, OutboxSink, StepOutcome,
    StepSource,
};
use super::ops::EngineSteps;
use super::{Engine, IdGenerator, outbox};

/// The job that runs the committed steps of one document write.
pub const JOB_COMMITTED_STEPS: &str = "sys:committed_steps";

/// The version of the argument shape.
pub const ARGS_SCHEMA: u64 = 1;

/// The arguments of a [`JOB_COMMITTED_STEPS`] job. No document value is among them.
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedJobArgs {
    /// The site the operation ran for.
    pub site: SiteId,
    /// The DocType of the document.
    pub doctype: String,
    /// The document.
    pub id: RecordId,
    /// The version the write produced; for a deletion, the last version before it.
    pub version: u64,
    /// The operation whose committed phase this is.
    pub operation: Operation,
    /// The command id of the operation, if it had one.
    pub command_id: Option<CommandId>,
    /// Who acted.
    pub actor: Actor,
    /// The original operation's instant: the steps see the same `now`.
    pub now: OffsetDateTime,
    /// The language of the invocation.
    pub language: Option<String>,
    /// The committed points to run, by ADR-51 name, in order.
    pub steps: Vec<String>,
}

fn bad(what: impl std::fmt::Display) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "platform.event",
        format!("The committed-steps job could not be encoded or decoded: {what}"),
    )
}

impl CommittedJobArgs {
    /// The arguments in their JSON form.
    ///
    /// # Errors
    ///
    /// `platform.event` if the instant cannot be formatted.
    pub fn to_json(&self) -> AppResult<Json> {
        let now = self.now.format(&Rfc3339).map_err(bad)?;
        let actor = serde_json::to_value(&self.actor).map_err(bad)?;
        Ok(json!({
            "schema": ARGS_SCHEMA,
            "site": self.site.as_str(),
            "doctype": self.doctype,
            "id": self.id.to_string(),
            "version": self.version,
            "operation": op_name(self.operation),
            "command_id": self.command_id.as_ref().map(CommandId::as_str),
            "actor": actor,
            "now": now,
            "language": self.language,
            "steps": self.steps,
        }))
    }

    /// Reads the arguments back.
    ///
    /// # Errors
    ///
    /// `platform.event` if a member is missing, mistyped or of an unknown schema.
    pub fn from_json(args: &Json) -> AppResult<Self> {
        let text = |key: &str| {
            args.get(key)
                .and_then(Json::as_str)
                .ok_or_else(|| bad(format!("`{key}` is missing")))
        };
        if args.get("schema").and_then(Json::as_u64) != Some(ARGS_SCHEMA) {
            return Err(bad("unknown schema"));
        }
        let site = SiteId::new(text("site")?).map_err(bad)?;
        let id = outbox::parse_id(text("id")?).ok_or_else(|| bad("`id` is not an id"))?;
        let version = args
            .get("version")
            .and_then(Json::as_u64)
            .ok_or_else(|| bad("`version` is missing"))?;
        let operation = operation_named(text("operation")?)
            .ok_or_else(|| bad("`operation` is not an operation"))?;
        let command_id = match args.get("command_id") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => Some(CommandId::new(s).map_err(bad)?),
            Some(_) => return Err(bad("`command_id` is not text")),
        };
        let actor = serde_json::from_value(args.get("actor").cloned().unwrap_or(Json::Null))
            .map_err(bad)?;
        let now = OffsetDateTime::parse(text("now")?, &Rfc3339).map_err(bad)?;
        let language = match args.get("language") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => Some(s.clone()),
            Some(_) => return Err(bad("`language` is not text")),
        };
        let steps = args
            .get("steps")
            .and_then(Json::as_array)
            .ok_or_else(|| bad("`steps` is missing"))?
            .iter()
            .map(|s| {
                s.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| bad("a step is not text"))
            })
            .collect::<AppResult<Vec<_>>>()?;
        Ok(Self {
            site,
            doctype: text("doctype")?.to_owned(),
            id,
            version,
            operation,
            command_id,
            actor,
            now,
            language,
            steps,
        })
    }

    /// The invocation the steps run under: the original actor, site, command id and instant.
    pub fn invocation(&self) -> Invocation {
        Invocation {
            site: self.site.clone(),
            actor: self.actor.clone(),
            command_id: self.command_id.clone(),
            language: self.language.clone(),
            now: self.now,
        }
    }

    /// The idempotency key of the job: `<command_id>:<operation>:<document id>:<version>`.
    pub fn idempotency_key(&self) -> String {
        key(
            self.command_id.as_ref(),
            self.operation,
            self.id,
            self.version,
        )
    }

    /// The position of this write in its document's history, for ordering jobs: versions in
    /// order, and a deletion after the write that produced the version it deletes.
    pub fn order(&self) -> (u64, bool) {
        (self.version, self.operation == Operation::Delete)
    }
}

fn key(command: Option<&CommandId>, op: Operation, id: RecordId, version: u64) -> String {
    format!(
        "{}:{}:{id}:{version}",
        command.map_or("-", CommandId::as_str),
        op_name(op)
    )
}

/// The operation with the ADR-51 name `name`.
pub fn operation_named(name: &str) -> Option<Operation> {
    Operation::ALL.into_iter().find(|op| op_name(*op) == name)
}

/// The committed hook point with the ADR-51 name `name` (`create_committed`,
/// `save_committed`, ...); `None` for any other name.
pub fn committed_point(name: &str) -> Option<HookPoint> {
    let target = name.strip_suffix("_committed")?;
    if target == "save" {
        return Some(HookPoint::of(Phase::Committed, Target::Save));
    }
    let op = operation_named(target)?;
    Some(HookPoint::of(Phase::Committed, Target::Op(op)))
}

/// One document write's committed points.
struct Group<'a> {
    doc: &'a Document,
    operation: Operation,
    version: u64,
    points: Vec<String>,
}

/// The batch grouped by write (DocType, document, operation, version), in first-seen order.
fn groups(batch: &CommittedBatch) -> AppResult<Vec<Group<'_>>> {
    let mut out: Vec<Group<'_>> = Vec::new();
    for step in &batch.steps {
        let version = step
            .doc
            .version()
            .ok_or_else(|| bad("a committed step's document has no version"))?
            .0;
        let name = step.point.name();
        let found = out.iter_mut().find(|g| {
            g.operation == step.operation
                && g.version == version
                && g.doc.id() == step.doc.id()
                && g.doc.doctype() == step.doc.doctype()
        });
        match found {
            Some(group) => {
                if !group.points.contains(&name) {
                    group.points.push(name);
                }
            }
            None => out.push(Group {
                doc: &step.doc,
                operation: step.operation,
                version,
                points: vec![name],
            }),
        }
    }
    Ok(out)
}

/// Delivery through the job engine: the job is written in the operation's own transaction (see
/// the module docs). Install it with [`Engine::with_delivery`].
#[derive(Clone)]
pub struct DurableDelivery {
    ids: Arc<dyn IdGenerator>,
}

impl std::fmt::Debug for DurableDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DurableDelivery").finish_non_exhaustive()
    }
}

impl DurableDelivery {
    /// A durable delivery whose job events take their ids from `ids` (use the engine's).
    pub fn new(ids: Arc<dyn IdGenerator>) -> Self {
        Self { ids }
    }

    /// The jobs `batch` enqueues, one per document write, as outbox events in order.
    ///
    /// # Errors
    ///
    /// `platform.event` if a job cannot be encoded.
    pub fn events(
        &self,
        batch: &CommittedBatch,
    ) -> AppResult<Vec<spot_storage_contract::OutboxEvent>> {
        let inv = &batch.invocation;
        groups(batch)?
            .into_iter()
            .map(|g| {
                let args = CommittedJobArgs {
                    site: inv.site.clone(),
                    doctype: g.doc.doctype().as_str().to_owned(),
                    id: g.doc.id(),
                    version: g.version,
                    operation: g.operation,
                    command_id: inv.command_id.clone(),
                    actor: inv.actor.clone(),
                    now: inv.now,
                    language: inv.language.clone(),
                    steps: g.points,
                };
                let request = JobRequest {
                    job: JOB_COMMITTED_STEPS.to_owned(),
                    args: args.to_json()?,
                    idempotency_key: Some(args.idempotency_key()),
                    not_before: None,
                };
                outbox::job(&*self.ids, inv, g.doc, &request)
            })
            .collect()
    }
}

impl CommittedDelivery for DurableDelivery {
    fn prepare<'a>(
        &'a self,
        sink: &'a mut dyn OutboxSink,
        batch: &'a CommittedBatch,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            for event in self.events(batch)? {
                sink.enqueue(&event).await?;
            }
            Ok(())
        })
    }

    fn deliver<'a>(
        &'a self,
        _steps: &'a dyn CommittedSteps,
        _batch: CommittedBatch,
    ) -> BoxFuture<'a, Vec<Message>> {
        // The job engine runs the steps; the job is already in the outbox.
        Box::pin(async { Vec::new() })
    }
}

/// An engine as the job that runs committed steps sees it: it runs a step and reads the
/// document the step is about. Built with [`Engine::runner`].
pub struct EngineRunner<B: StorageBackend> {
    engine: Arc<Engine<B>>,
}

impl<B: StorageBackend> Clone for EngineRunner<B> {
    fn clone(&self) -> Self {
        Self {
            engine: self.engine.clone(),
        }
    }
}

impl<B: StorageBackend> Engine<B> {
    /// The engine as a runner of committed steps for the job engine (WP-DOC-04.T4).
    pub fn runner(self: &Arc<Self>) -> EngineRunner<B> {
        EngineRunner {
            engine: self.clone(),
        }
    }
}

impl<B: StorageBackend> CommittedSteps for EngineRunner<B> {
    fn run<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> BoxFuture<'a, Vec<Message>> {
        Box::pin(async move { self.run_checked(invocation, step).await.messages })
    }

    fn run_checked<'a>(
        &'a self,
        invocation: &'a Invocation,
        step: CommittedStep,
    ) -> BoxFuture<'a, StepOutcome> {
        Box::pin(async move {
            let snap = self.engine.snapshot();
            let steps = EngineSteps {
                engine: &self.engine,
                snap: &snap,
                announced: Mutex::new(Vec::new()),
            };
            steps.run_checked(invocation, step).await
        })
    }
}

impl<B: StorageBackend> StepSource for EngineRunner<B> {
    fn current<'a>(
        &'a self,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Option<Document>>> {
        Box::pin(async move {
            let snap = self.engine.snapshot();
            self.engine.read_document(&snap, doctype, id).await
        })
    }
}
