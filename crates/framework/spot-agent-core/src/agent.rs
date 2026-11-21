//! The agent: verification, risk rules, signing-rule enforcement and the decision.
//!
//! [`Agent::handle`] is the whole decision. It decodes the envelope, runs the shared
//! [`crate::verify::Evaluator`] with the live nonce store, and returns
//! [`Decision::Execute`] or [`Decision::Refuse`]. Every decision is appended to the
//! [`ResultLog`] first; if the log cannot record it, an otherwise-runnable command is
//! refused ([`Refusal::LogUnavailable`]) rather than run without evidence.

use std::sync::Arc;

use spot_agent_enroll::identity::AgentIdentity;
use spot_mgmt_ops::Catalogue;
use spot_signed_command::NonceStore;

use crate::envelope::CommandEnvelope;
use crate::error::Refusal;
use crate::log::{DecisionRecord, ResultLog};
use crate::plan::{ExecutionPlan, command_digest};
use crate::policy::{CriticalContext, Policy};
use crate::trust::TrustStore;
use crate::verify::Evaluator;

/// The outcome of handling a command envelope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The command verified and may run: the plan is the executor's input.
    Execute(ExecutionPlan),
    /// The command was refused, with a stable reason.
    Refuse(Refusal),
}

impl Decision {
    /// The plan, when the decision is to execute.
    #[must_use]
    pub fn plan(&self) -> Option<&ExecutionPlan> {
        match self {
            Self::Execute(plan) => Some(plan),
            Self::Refuse(_) => None,
        }
    }

    /// The refusal, when the decision is to refuse.
    #[must_use]
    pub fn refusal(&self) -> Option<&Refusal> {
        match self {
            Self::Refuse(refusal) => Some(refusal),
            Self::Execute(_) => None,
        }
    }

    /// The decision's stable code: `execute`, or the refusal's code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Execute(_) => "execute",
            Self::Refuse(refusal) => refusal.code(),
        }
    }
}

/// The agent's decision core: the trust store, the catalogue, the policy, the nonce store,
/// the critical-operation context and the result log, all injected.
pub struct Agent {
    agent_id: String,
    catalogue: Catalogue,
    trust: TrustStore,
    policy: Policy,
    nonce: Arc<dyn NonceStore>,
    critical: Box<dyn CriticalContext>,
    log: Arc<dyn ResultLog>,
}

impl Agent {
    /// Builds the agent from its parts.
    #[must_use]
    pub fn new(
        agent_id: impl Into<String>,
        catalogue: Catalogue,
        trust: TrustStore,
        policy: Policy,
        nonce: Arc<dyn NonceStore>,
        critical: Box<dyn CriticalContext>,
        log: Arc<dyn ResultLog>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            catalogue,
            trust,
            policy,
            nonce,
            critical,
            log,
        }
    }

    /// Builds the agent for an enrolled identity: the audience is the identity's agent id.
    #[must_use]
    pub fn from_identity(
        identity: &AgentIdentity,
        catalogue: Catalogue,
        trust: TrustStore,
        policy: Policy,
        nonce: Arc<dyn NonceStore>,
        critical: Box<dyn CriticalContext>,
        log: Arc<dyn ResultLog>,
    ) -> Self {
        Self::new(
            identity.agent_id.clone(),
            catalogue,
            trust,
            policy,
            nonce,
            critical,
            log,
        )
    }

    /// The agent id commands must be addressed to.
    #[must_use]
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// The result log the agent records to.
    #[must_use]
    pub fn log(&self) -> &dyn ResultLog {
        &*self.log
    }

    /// Handles one command envelope at `now` (Unix seconds) and records the decision.
    #[must_use]
    pub async fn handle(&self, envelope: &CommandEnvelope, now: i64) -> Decision {
        let signed = match envelope.to_signed() {
            Ok(signed) => signed,
            Err(refusal) => {
                let _ = self.append_record(DecisionRecord {
                    actor: format!("agent:{}", self.agent_id),
                    command_id: String::new(),
                    command_digest: [0u8; 32],
                    op: String::new(),
                    signers: Vec::new(),
                    outcome: "refuse".to_owned(),
                    code: Some(refusal.code().to_owned()),
                    at: now,
                });
                return Decision::Refuse(refusal);
            }
        };
        self.handle_signed(&signed, now).await
    }

    /// Handles a parsed signed command: the shared evaluation plus the decision record.
    async fn handle_signed(
        &self,
        signed: &spot_signed_command::SignedCommand,
        now: i64,
    ) -> Decision {
        let evaluator = Evaluator {
            agent_id: &self.agent_id,
            catalogue: &self.catalogue,
            trust: &self.trust,
            policy: &self.policy,
            critical: &*self.critical,
        };
        match evaluator.evaluate(signed, Some(&self.nonce), now).await {
            Ok(plan) => {
                let signers: Vec<String> = plan
                    .signers()
                    .iter()
                    .map(|key| key.as_str().to_owned())
                    .collect();
                let recorded = self.append_record(DecisionRecord {
                    actor: format!("agent:{}", self.agent_id),
                    command_id: plan.command_id().to_owned(),
                    command_digest: *plan.command_digest(),
                    op: plan.op().as_str().to_owned(),
                    signers,
                    outcome: "execute".to_owned(),
                    code: None,
                    at: now,
                });
                if recorded.is_err() {
                    return Decision::Refuse(Refusal::LogUnavailable);
                }
                Decision::Execute(plan)
            }
            Err(refusal) => {
                let command = &signed.command;
                let digest = command_digest(command).unwrap_or([0u8; 32]);
                let _ = self.append_record(DecisionRecord {
                    actor: format!("agent:{}", self.agent_id),
                    command_id: command.id().to_owned(),
                    command_digest: digest,
                    op: command.op().to_owned(),
                    signers: Vec::new(),
                    outcome: "refuse".to_owned(),
                    code: Some(refusal.code().to_owned()),
                    at: now,
                });
                Decision::Refuse(refusal)
            }
        }
    }

    /// Appends one record to the result log.
    fn append_record(
        &self,
        record: DecisionRecord,
    ) -> Result<spot_audit_chain::ChainEntry, Refusal> {
        self.log.append(&record)
    }
}

impl std::fmt::Debug for Agent {
    /// Prints the identity and the trust-store size, never a key or a nonce.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("agent_id", &self.agent_id)
            .field("catalogue", &self.catalogue.len())
            .field("trusted_keys", &self.trust.len())
            .field("log_site", &self.log.site())
            .finish()
    }
}
