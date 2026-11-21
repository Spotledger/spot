//! The intake pipeline, in this fixed order (each step tested):
//!
//! 1. adapt the gateway's envelope to the core wire form (signature key ids are resolved
//!    against the **locally** trusted administrator keys, never keys the gateway supplies);
//! 2. a command id seen before is **never executed again**: the stored result is
//!    re-reported;
//! 3. `spot_agent_core::Agent::handle` verifies signature, audience, expiry, replay window,
//!    operation risk and arguments (the durable [`FileNonceStore`] records the nonce,
//!    fsynced, as part of acceptance);
//! 4. the command record is **persisted (fsync) before executing**: first `pending`, then
//!    `started` right before the handler runs;
//! 5. dispatch to the handler registry (no handler: `unsupported`, nothing executed);
//! 6. persist the result, then report it.
//!
//! Crash semantics (defined and tested): a command that was `pending` (accepted, handler
//! not started) is re-verified at restart and executed exactly once; one that was
//! `started` is reported as `indeterminate` (outcome `failed`, code `agent.indeterminate`)
//! and never re-run; a `done` result that was not yet acknowledged is re-reported.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use spot_agent_core::{
    Agent, CommandEnvelope, CriticalContext, Decision, DecisionRecord, ExecutionPlan,
    HandlerOutcome, HandlerRegistry, Policy, Refusal, ResultLog, TrustStore,
};
use spot_mgmt_ops::Catalogue;
use spot_redact::Redactor;

use super::client::{MAX_OUTPUT, ResultReport, truncate};
use super::store::{CmdState, CommandRecord, FileCommandStore, FileNonceStore, valid_command_id};
use crate::error::AgentError;
use crate::logging::Logger;
use crate::run::Shutdown;

/// Everything the pipeline is built from. The replay window is typed as
/// [`FileNonceStore`]: a production agent **cannot** be built with the in-memory store.
pub struct PipelineParts {
    /// This agent's id (the audience of commands).
    pub agent_id: String,
    /// The operation catalogue.
    pub catalogue: Catalogue,
    /// The locally trusted administrator keys.
    pub trust: TrustStore,
    /// The decision policy.
    pub policy: Policy,
    /// The durable replay window.
    pub nonce: Arc<FileNonceStore>,
    /// Builds the critical-operation context (called twice: live and recovery agents).
    pub critical: Box<dyn Fn() -> Box<dyn CriticalContext> + Send + Sync>,
    /// The local audit log.
    pub log: Arc<dyn ResultLog>,
    /// The registered handlers.
    pub handlers: Arc<HandlerRegistry>,
    /// The command record store.
    pub store: FileCommandStore,
    /// Redaction of output before it is stored or reported.
    pub redactor: Arc<Redactor>,
    /// The logger.
    pub logger: Logger,
    /// How long a running command may take to finish after a shutdown request.
    pub drain: Duration,
    /// The shutdown signal.
    pub shutdown: Shutdown,
}

/// The pipeline.
pub struct Pipeline {
    agent: Agent,
    /// Used only to re-verify commands that were `pending` at a restart: their replay
    /// record is the durable command record, so the nonce window is not consulted again.
    recovery: Agent,
    trust: TrustStore,
    handlers: Arc<HandlerRegistry>,
    store: FileCommandStore,
    redactor: Arc<Redactor>,
    log: Arc<dyn ResultLog>,
    logger: Logger,
    drain: Duration,
    shutdown: Shutdown,
}

/// Maps a core refusal to the reported outcome and code.
fn refusal_outcome(r: &Refusal) -> (&'static str, &'static str) {
    match r {
        Refusal::UnknownOperation | Refusal::UnknownHandler { .. } => {
            ("unsupported", "agent.unsupported")
        }
        other => ("refused", other.code()),
    }
}

/// Converts the contract's `AgentSignedCommand` (a command object plus `key_id`/`signature`
/// pairs) to the core wire form. The core form (`command` as a canonical JSON string with
/// `public_key` per signature) passes through. Key ids are resolved from the local trust
/// store: an id that is not locally trusted is refused here.
///
/// # Errors
/// [`Refusal::EnvelopeMalformed`] or [`Refusal::UnknownSigner`].
pub fn adapt_envelope(value: &Value, trust: &TrustStore) -> Result<String, Refusal> {
    let command = value.get("command").ok_or(Refusal::EnvelopeMalformed)?;
    let signatures = value
        .get("signatures")
        .and_then(Value::as_array)
        .ok_or(Refusal::EnvelopeMalformed)?;
    if signatures.is_empty() || signatures.len() > 8 {
        return Err(Refusal::EnvelopeMalformed);
    }
    let command_text = match command {
        Value::String(text) => text.clone(),
        Value::Object(_) => {
            serde_json::to_string(command).map_err(|_| Refusal::EnvelopeMalformed)?
        }
        _ => return Err(Refusal::EnvelopeMalformed),
    };
    let mut out = Vec::new();
    for sig in signatures {
        let key_id = sig
            .get("key_id")
            .and_then(Value::as_str)
            .ok_or(Refusal::EnvelopeMalformed)?;
        let signature = sig
            .get("signature")
            .and_then(Value::as_str)
            .ok_or(Refusal::EnvelopeMalformed)?;
        let public_key = match sig.get("public_key").and_then(Value::as_str) {
            Some(embedded) => embedded.to_owned(),
            None => {
                use base64::Engine as _;
                let key = trust
                    .iter()
                    .find(|k| k.key_id.as_str() == key_id)
                    .ok_or(Refusal::UnknownSigner)?;
                base64::engine::general_purpose::STANDARD.encode(key.public_key)
            }
        };
        out.push(
            serde_json::json!({"key_id": key_id, "public_key": public_key, "signature": signature}),
        );
    }
    Ok(serde_json::json!({"command": command_text, "signatures": out}).to_string())
}

/// The command id inside a gateway envelope, if it has a usable one.
#[must_use]
pub fn command_id_of(value: &Value) -> Option<String> {
    let command = value.get("command")?;
    let id = match command {
        Value::String(text) => serde_json::from_str::<Value>(text)
            .ok()?
            .get("id")?
            .as_str()?
            .to_owned(),
        Value::Object(map) => map.get("id")?.as_str()?.to_owned(),
        _ => return None,
    };
    valid_command_id(&id).then_some(id)
}

fn report_of(rec: &CommandRecord) -> ResultReport {
    ResultReport {
        command_id: rec.id.clone(),
        seq: rec.seq,
        outcome: rec.outcome.clone().unwrap_or_else(|| "failed".to_owned()),
        code: rec.code.clone(),
        output: rec.output.clone(),
    }
}

impl Pipeline {
    /// Builds the pipeline (the live agent and the restart re-check agent).
    #[must_use]
    pub fn new(parts: PipelineParts) -> Self {
        let trust_copy = parts.trust.clone();
        let live_nonce: Arc<dyn spot_signed_command::NonceStore> = parts.nonce;
        let agent = Agent::new(
            parts.agent_id.clone(),
            parts.catalogue.clone(),
            parts.trust.clone(),
            parts.policy.clone(),
            live_nonce,
            (parts.critical)(),
            parts.log.clone(),
        );
        // Internal and private: the durable command record is this agent's replay guard.
        let recovery = Agent::new(
            parts.agent_id,
            parts.catalogue,
            parts.trust,
            parts.policy,
            Arc::new(spot_signed_command::MemoryNonceStore::new()),
            (parts.critical)(),
            parts.log.clone(),
        );
        Self {
            agent,
            recovery,
            trust: trust_copy,
            handlers: parts.handlers,
            store: parts.store,
            redactor: parts.redactor,
            log: parts.log,
            logger: parts.logger,
            drain: parts.drain,
            shutdown: parts.shutdown,
        }
    }

    /// The command store (status, tests).
    #[must_use]
    pub fn store(&self) -> &FileCommandStore {
        &self.store
    }

    /// Handles one envelope from the gateway; returns the result to report, if any.
    ///
    /// # Errors
    /// [`AgentError::StateIo`] when a record cannot be persisted: the command is then NOT
    /// executed (persist-before-execute).
    pub async fn process(
        &self,
        value: &Value,
        now: i64,
    ) -> Result<Option<ResultReport>, AgentError> {
        let Some(id) = command_id_of(value) else {
            self.logger.warn(
                "command_dropped",
                "a command without a usable id was ignored",
            );
            return Ok(None);
        };
        if let Some(existing) = self.store.get(&id)? {
            // Seen before: never executed again; a finished one is re-reported.
            return Ok((existing.state == CmdState::Done).then(|| report_of(&existing)));
        }
        let wire = match adapt_envelope(value, &self.trust) {
            Ok(w) => w,
            Err(refusal) => return self.refuse(&id, String::new(), &refusal).map(Some),
        };
        let envelope = match CommandEnvelope::parse_str(&wire) {
            Ok(e) => e,
            Err(refusal) => return self.refuse(&id, String::new(), &refusal).map(Some),
        };
        match self.agent.handle(&envelope, now).await {
            Decision::Refuse(refusal) => self.refuse(&id, String::new(), &refusal).map(Some),
            Decision::Execute(plan) => self.execute(&id, &wire, &plan, now).await,
        }
    }

    /// Stores a refusal as the command's final result.
    fn refuse(
        &self,
        id: &str,
        envelope: String,
        refusal: &Refusal,
    ) -> Result<ResultReport, AgentError> {
        let (outcome, code) = refusal_outcome(refusal);
        self.finish(id, envelope, outcome, Some(code), None)
    }

    /// Persists the final record (with the next sequence number) and returns its report.
    fn finish(
        &self,
        id: &str,
        envelope: String,
        outcome: &str,
        code: Option<&str>,
        output: Option<String>,
    ) -> Result<ResultReport, AgentError> {
        let output = output.map(|o| truncate(self.redactor.redact_str(&o).as_ref(), MAX_OUTPUT));
        let record = CommandRecord {
            id: id.to_owned(),
            state: CmdState::Done,
            envelope: if outcome == "refused" || outcome == "unsupported" {
                String::new()
            } else {
                envelope
            },
            seq: self.store.next_seq()?,
            outcome: Some(outcome.to_owned()),
            code: code.map(str::to_owned),
            output,
            reported: false,
        };
        self.store.put(&record)?;
        Ok(report_of(&record))
    }

    /// Persist-before-execute, dispatch, persist the result.
    async fn execute(
        &self,
        id: &str,
        wire: &str,
        plan: &ExecutionPlan,
        now: i64,
    ) -> Result<Option<ResultReport>, AgentError> {
        let Some(handler) = self.handlers.get(plan.op()).cloned() else {
            return self
                .refuse(
                    id,
                    wire.to_owned(),
                    &Refusal::UnknownHandler {
                        op: plan.op().as_str().to_owned(),
                    },
                )
                .map(Some);
        };
        let mut record = CommandRecord {
            id: id.to_owned(),
            state: CmdState::Pending,
            envelope: wire.to_owned(),
            seq: 0,
            outcome: None,
            code: None,
            output: None,
            reported: false,
        };
        self.store.put(&record)?;
        record.state = CmdState::Started;
        self.store.put(&record)?;
        let work = plan.clone();
        let mut join = tokio::task::spawn_blocking(move || handler.handle(&work));
        let shutdown = self.shutdown.clone();
        let drain = self.drain;
        let joined = tokio::select! {
            result = &mut join => result,
            () = async { shutdown.wait().await; tokio::time::sleep(drain).await; } => {
                // Time-boxed: the record stays `started`, so a restart reports it as
                // indeterminate and never re-runs it.
                self.logger.warn("command_timeboxed", "shutdown while a command was running; its result is indeterminate");
                return Ok(None);
            }
        };
        let (outcome, code, output, audit_outcome) = match joined {
            Ok(HandlerOutcome::Succeeded) => ("succeeded", None, None, "executed"),
            Ok(HandlerOutcome::Failed { detail }) => (
                "failed",
                Some("agent.handler_failed"),
                Some(detail),
                "handler_failed",
            ),
            Err(_) => (
                "failed",
                Some("agent.handler_panicked"),
                None,
                "handler_failed",
            ),
        };
        let _ = self.log.append(&DecisionRecord {
            actor: format!("agent:{}", plan.agent_id()),
            command_id: plan.command_id().to_owned(),
            command_digest: *plan.command_digest(),
            op: plan.op().as_str().to_owned(),
            signers: plan
                .signers()
                .iter()
                .map(|k| k.as_str().to_owned())
                .collect(),
            outcome: audit_outcome.to_owned(),
            code: code.map(str::to_owned),
            at: now,
        });
        self.finish(id, wire.to_owned(), outcome, code, output)
            .map(Some)
    }

    /// Restart recovery: `started` commands become `indeterminate`; `pending` commands are
    /// re-verified and executed exactly once; the unreported results are returned.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub async fn recover(&self, now: i64) -> Result<Vec<ResultReport>, AgentError> {
        for rec in self.store.list()? {
            match rec.state {
                CmdState::Done => {}
                CmdState::Started => {
                    self.finish(
                        &rec.id,
                        String::new(),
                        "failed",
                        Some("agent.indeterminate"),
                        Some("the agent stopped while the command was running; its effect is unknown".to_owned()),
                    )?;
                }
                CmdState::Pending => {
                    let outcome = match CommandEnvelope::parse_str(&rec.envelope) {
                        Ok(env) => self.recovery.handle(&env, now).await,
                        Err(r) => Decision::Refuse(r),
                    };
                    match outcome {
                        Decision::Execute(plan) => {
                            self.execute(&rec.id, &rec.envelope, &plan, now).await?;
                        }
                        Decision::Refuse(r) => {
                            self.refuse(&rec.id, String::new(), &r)?;
                        }
                    }
                }
            }
        }
        self.unreported()
    }

    /// The finished results the gateway has not acknowledged, oldest first.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn unreported(&self) -> Result<Vec<ResultReport>, AgentError> {
        Ok(self
            .store
            .list()?
            .iter()
            .filter(|r| r.state == CmdState::Done && !r.reported)
            .map(report_of)
            .collect())
    }

    /// Marks a result acknowledged.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn mark_reported(&self, id: &str) -> Result<(), AgentError> {
        if let Some(mut rec) = self.store.get(id)? {
            rec.reported = true;
            self.store.put(&rec)?;
        }
        Ok(())
    }

    /// The number of commands accepted but not finished plus unreported results.
    ///
    /// # Errors
    /// [`AgentError::StateIo`].
    pub fn backlog(&self) -> Result<usize, AgentError> {
        Ok(self
            .store
            .list()?
            .iter()
            .filter(|r| r.state != CmdState::Done || !r.reported)
            .count())
    }
}
