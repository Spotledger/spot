//! Privilege separation: the unprivileged [`Broker`] and the privileged [`Executor`].
//!
//! The broker owns the network session, decodes messages and runs verification; it can
//! only ever hand the executor a [`PlanEnvelope`] — the original signed command plus a
//! [`ExecutionPlan`] the broker signs with its own key. The executor trusts no plan on
//! faith: it checks the broker, the broker's signature, the plan's own hash, and then
//! **re-derives** the plan from the carried command with exactly the code the broker ran.
//! A plan that is not the deterministic plan of a validly signed command is refused, so a
//! compromised broker can ask only for what a valid signed command already authorised.
//!
//! The executor runs only the handlers in its [`HandlerRegistry`], keyed by catalogue id;
//! there is no default handler and no ambient authority.

use std::sync::Arc;

use ed25519_dalek::{Signature, Verifier as _, VerifyingKey};
use spot_mgmt_ops::{Catalogue, OpId};
use spot_signed_command::{KeyId, SignedCommand};

use crate::agent::{Agent, Decision};
use crate::channel::LocalChannel;
use crate::envelope::CommandEnvelope;
use crate::error::{CoreError, Refusal};
use crate::handler::{HandlerOutcome, HandlerRegistry};
use crate::log::{DecisionRecord, ResultLog};
use crate::plan::ExecutionPlan;
use crate::policy::{CriticalContext, Policy};
use crate::trust::TrustStore;
use crate::verify::Evaluator;

/// What the broker hands the executor: the original signed command, the plan, and the
/// broker's signature over the plan hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanEnvelope {
    /// The original signed command the plan was derived from.
    pub command: SignedCommand,
    /// The plan the broker produced.
    pub plan: ExecutionPlan,
    /// The broker's key id.
    pub broker_key_id: KeyId,
    /// The broker's Ed25519 signature over `plan.plan_hash()`.
    pub broker_signature: Signature,
}

/// What running a handler produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    /// The handler completed.
    Succeeded,
    /// The handler failed; the detail is the handler's, never a secret from this crate.
    Failed {
        /// What went wrong, in the handler's words.
        detail: String,
    },
}

impl ExecutionStatus {
    /// The short outcome token recorded in the result log.
    fn token(&self) -> &'static str {
        match self {
            Self::Succeeded => "executed",
            Self::Failed { .. } => "handler_failed",
        }
    }
}

/// The result of dispatching a plan: the status and the log entry that records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionOutcome {
    /// The command's random id.
    pub command_id: String,
    /// The operation run.
    pub op: OpId,
    /// Whether the handler succeeded.
    pub status: ExecutionStatus,
    /// The result-log entry, ready to ship to the console with the result.
    pub log: spot_audit_chain::ChainEntry,
}

/// The privileged executor: independent re-derivation plus the registered handlers.
pub struct Executor {
    agent_id: String,
    catalogue: Catalogue,
    trust: TrustStore,
    policy: Policy,
    critical: Box<dyn CriticalContext>,
    broker_key_id: KeyId,
    broker_key: VerifyingKey,
    handlers: HandlerRegistry,
    log: Arc<dyn ResultLog>,
}

/// Builds an [`Executor`]; a builder because the fixed-part count exceeds the comfortable
/// argument count.
pub struct ExecutorBuilder {
    agent_id: String,
    catalogue: Catalogue,
    trust: TrustStore,
    policy: Policy,
    critical: Box<dyn CriticalContext>,
    log: Arc<dyn ResultLog>,
    broker: Option<(KeyId, [u8; 32])>,
    handlers: Option<HandlerRegistry>,
}

impl ExecutorBuilder {
    /// Starts a builder from the evaluation state and the result log.
    #[must_use]
    pub fn new(
        agent_id: impl Into<String>,
        catalogue: Catalogue,
        trust: TrustStore,
        policy: Policy,
        critical: Box<dyn CriticalContext>,
        log: Arc<dyn ResultLog>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            catalogue,
            trust,
            policy,
            critical,
            log,
            broker: None,
            handlers: None,
        }
    }

    /// Sets the trusted broker key and its raw public key.
    #[must_use]
    pub fn broker(mut self, key_id: KeyId, public_key: [u8; 32]) -> Self {
        self.broker = Some((key_id, public_key));
        self
    }

    /// Sets the handler registry.
    #[must_use]
    pub fn handlers(mut self, handlers: HandlerRegistry) -> Self {
        self.handlers = Some(handlers);
        self
    }

    /// Builds the executor.
    ///
    /// # Errors
    /// [`CoreError::Malformed`] when no broker or handlers were set, when the broker's key
    /// id is not derived from its public key, or when the key bytes are not a valid
    /// Ed25519 point.
    pub fn build(self) -> Result<Executor, CoreError> {
        let (broker_key_id, public_key) = self.broker.ok_or(CoreError::Malformed(
            "the executor needs a trusted broker key",
        ))?;
        let handlers = self.handlers.ok_or(CoreError::Malformed(
            "the executor needs a handler registry",
        ))?;
        let broker_key =
            VerifyingKey::from_bytes(&public_key).map_err(|_| CoreError::InvalidKey)?;
        if KeyId::from_public_key(&public_key) != broker_key_id {
            return Err(CoreError::Malformed(
                "the broker key id is not derived from its public key",
            ));
        }
        Ok(Executor {
            agent_id: self.agent_id,
            catalogue: self.catalogue,
            trust: self.trust,
            policy: self.policy,
            critical: self.critical,
            broker_key_id,
            broker_key,
            handlers,
            log: self.log,
        })
    }
}

impl Executor {
    /// Dispatches one plan: verify it, re-derive it, run the handler, record the result.
    ///
    /// # Errors
    /// [`Refusal::UntrustedBroker`], [`Refusal::BrokerSignatureInvalid`],
    /// [`Refusal::PlanHashMismatch`], [`Refusal::PlanUnverified`] when the plan is not the
    /// deterministic plan of a validly signed command; any refusal the shared evaluation
    /// produces; [`Refusal::UnknownHandler`] when no handler is registered; and
    /// [`Refusal::LogUnavailable`] when a successful run cannot be recorded.
    pub async fn dispatch(
        &self,
        envelope: &PlanEnvelope,
        now: i64,
    ) -> Result<ExecutionOutcome, Refusal> {
        match self.run(envelope, now).await {
            Ok(status) => {
                let entry = self.record(now, envelope, status.token(), None)?;
                Ok(ExecutionOutcome {
                    command_id: envelope.plan.command_id().to_owned(),
                    op: envelope.plan.op().clone(),
                    status,
                    log: entry,
                })
            }
            Err(refusal) => {
                // Record the refusal best-effort; the refusal is the answer either way.
                let _ = self.record(now, envelope, "executor_refused", Some(refusal.code()));
                Err(refusal)
            }
        }
    }

    /// The checks and the handler call, without the log.
    async fn run(&self, envelope: &PlanEnvelope, now: i64) -> Result<ExecutionStatus, Refusal> {
        // The broker must be the one this executor is configured with.
        if envelope.broker_key_id != self.broker_key_id {
            return Err(Refusal::UntrustedBroker);
        }

        // The broker's signature must verify over the plan hash.
        self.broker_key
            .verify(envelope.plan.plan_hash(), &envelope.broker_signature)
            .map_err(|_| Refusal::BrokerSignatureInvalid)?;

        // The plan's stored hash must match its own fields.
        if envelope.plan.compute_hash() != *envelope.plan.plan_hash() {
            return Err(Refusal::PlanHashMismatch);
        }

        // Re-derive the plan from the original command with the same code the broker ran:
        // a forged plan cannot survive this, and an unsigned or single-signed dangerous
        // command cannot be laundered through a compromised broker.
        let derived = Evaluator {
            agent_id: &self.agent_id,
            catalogue: &self.catalogue,
            trust: &self.trust,
            policy: &self.policy,
            critical: &*self.critical,
        }
        .evaluate(&envelope.command, None, now)
        .await?;
        if derived != envelope.plan {
            return Err(Refusal::PlanUnverified);
        }

        // No ambient authority: only a registered handler runs.
        let handler = self
            .handlers
            .get(derived.op())
            .ok_or_else(|| Refusal::UnknownHandler {
                op: derived.op().as_str().to_owned(),
            })?;
        Ok(match handler.handle(&derived) {
            HandlerOutcome::Succeeded => ExecutionStatus::Succeeded,
            HandlerOutcome::Failed { detail } => ExecutionStatus::Failed { detail },
        })
    }

    /// Appends one result record.
    fn record(
        &self,
        now: i64,
        envelope: &PlanEnvelope,
        outcome: &str,
        code: Option<&str>,
    ) -> Result<spot_audit_chain::ChainEntry, Refusal> {
        let plan = &envelope.plan;
        let signers: Vec<String> = plan
            .signers()
            .iter()
            .map(|key| key.as_str().to_owned())
            .collect();
        self.log.append(&DecisionRecord {
            actor: format!("agent:{}", self.agent_id),
            command_id: plan.command_id().to_owned(),
            command_digest: *plan.command_digest(),
            op: plan.op().as_str().to_owned(),
            signers,
            outcome: outcome.to_owned(),
            code: code.map(str::to_owned),
            at: now,
        })
    }
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("agent_id", &self.agent_id)
            .field("broker_key_id", &self.broker_key_id.as_str())
            .field("handlers", &self.handlers.len())
            .finish()
    }
}

/// The unprivileged broker: it verifies and produces a signed plan. It holds the network
/// side's key; it never runs anything.
pub struct Broker {
    agent: Agent,
    signer: Arc<dyn spot_signed_command::Signer>,
}

impl Broker {
    /// Builds a broker over an agent and its own signing key.
    #[must_use]
    pub fn new(agent: Agent, signer: Arc<dyn spot_signed_command::Signer>) -> Self {
        Self { agent, signer }
    }

    /// The agent the broker verifies with.
    #[must_use]
    pub fn agent(&self) -> &Agent {
        &self.agent
    }

    /// The broker's key id.
    #[must_use]
    pub fn key_id(&self) -> KeyId {
        self.signer.key_id()
    }

    /// Verifies one envelope (delegates to the agent's decision).
    #[must_use]
    pub async fn decide(&self, envelope: &CommandEnvelope, now: i64) -> Decision {
        self.agent.handle(envelope, now).await
    }

    /// Verifies and, on an execute decision, signs the plan.
    ///
    /// # Errors
    /// The agent's refusal, or [`Refusal::EnvelopeMalformed`] when the envelope no longer
    /// parses (it just did for the decision, so this is defensive).
    pub async fn plan(
        &self,
        envelope: &CommandEnvelope,
        now: i64,
    ) -> Result<PlanEnvelope, Refusal> {
        match self.agent.handle(envelope, now).await {
            Decision::Execute(plan) => {
                let command = envelope.to_signed()?;
                let broker_signature = self.signer.sign(plan.plan_hash());
                Ok(PlanEnvelope {
                    command,
                    plan,
                    broker_key_id: self.signer.key_id(),
                    broker_signature,
                })
            }
            Decision::Refuse(refusal) => Err(refusal),
        }
    }

    /// Sends a signed plan over `channel`.
    ///
    /// # Errors
    /// The agent's refusal, or the channel's failure.
    pub async fn route(
        &self,
        envelope: &CommandEnvelope,
        now: i64,
        channel: &dyn LocalChannel,
    ) -> Result<PlanEnvelope, Refusal> {
        let plan = self.plan(envelope, now).await?;
        channel.send(plan.clone())?;
        Ok(plan)
    }
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Broker")
            .field("agent_id", &self.agent.agent_id())
            .field("key_id", &self.signer.key_id().as_str())
            .finish()
    }
}
