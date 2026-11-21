//! The agent registry: enrolment, deep registration and revocation, each an audit event
//! (ADR-070 section 8 rule 4: assignment-style changes carry source, reason and time).

use serde::{Deserialize, Serialize};

use crate::error::TierError;
use crate::identity::AgentIdentity;

/// One audit event from the registry (ADR-070 section 8 rule 4, in our own words for the
/// agent registry): what happened to whom, by whose hand, on what evidence, when.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegistryEvent {
    /// What happened: `"agent.enrolled"`, `"agent.deep_registered"`, `"agent.deep_revoked"`.
    pub event: String,
    /// The agent the event is about.
    pub agent_id: String,
    /// Who caused it (a person's id, or the registry text for a data enrolment).
    pub by: String,
    /// The evidence reference for a deep registration (`evidence_ref`); empty otherwise.
    pub evidence_ref: String,
    /// When it happened (unix seconds i64; the caller supplies the time, the crate keeps no
    /// clock).
    pub at: i64,
}

/// The agents the deployment knows, and the only path to `registered_deep`.
///
/// A data record cannot arrive deep:
///
/// ```compile_fail
/// use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Tier};
/// let mut registry = AgentRegistry::new();
/// let mut agent = AgentIdentity::new("agent-x", "owner", Domain::Support, Tier::T3);
/// // The field is not visible outside the crate — a data record cannot arrive deep;
/// // only `AgentRegistry::register_deep` sets it, with its audit event.
/// agent.registered_deep = true;
/// registry.enroll(agent).unwrap();
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentRegistry {
    agents: Vec<AgentIdentity>,
}

impl AgentRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enrols an agent from its data record.
    ///
    /// Enrolment is the data path — it may never set `registered_deep` (that is
    /// [`register_deep`](Self::register_deep)'s event alone), so an agent arriving with the
    /// flag set is refused.
    ///
    /// # Errors
    ///
    /// [`TierError::ValueInvalid`] when the id is empty;
    /// [`TierError::DeepOnlyThroughRegistry`] when a data record arrives with
    /// `registered_deep = true` (the flag cannot be set any other way than the registry).
    pub fn enroll(&mut self, agent: AgentIdentity) -> Result<(), TierError> {
        if agent.id.trim().is_empty() {
            return Err(TierError::ValueInvalid {
                at: "agent".to_owned(),
                detail: "`id` is required".to_owned(),
            });
        }
        if agent.registered_deep {
            return Err(TierError::DeepOnlyThroughRegistry {
                id: agent.id.clone(),
            });
        }
        self.agents.push(agent);
        Ok(())
    }

    /// Registers an agent deep: sets [`registered_deep`](crate::identity::AgentIdentity::registered_deep),
    /// and returns the audit event proving it happened.
    ///
    /// Registering deep means full depth (blueprint WS13) for an agent registered to the
    /// support organisation — a non-support agent is refused.
    ///
    /// # Errors
    ///
    /// [`TierError::UnknownAgent`] when no agent with that id is enrolled;
    /// [`TierError::DeepNeedsSupportDomain`] when the agent's domain is not support;
    /// [`TierError::AlreadyDeep`] when it is already registered deep.
    pub fn register_deep(
        &mut self,
        agent_id: &str,
        registered_by: &str,
        evidence_ref: &str,
        at: i64,
    ) -> Result<RegistryEvent, TierError> {
        let agent = self
            .agents
            .iter_mut()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| TierError::UnknownAgent {
                id: agent_id.to_owned(),
            })?;
        if agent.registered_deep {
            return Err(TierError::AlreadyDeep {
                id: agent.id.clone(),
            });
        }
        if agent.domain != crate::action::Domain::Support {
            return Err(TierError::DeepNeedsSupportDomain {
                id: agent.id.clone(),
                domain: agent.domain.as_str().to_owned(),
            });
        }
        agent.registered_deep = true;
        Ok(RegistryEvent {
            event: "agent.deep_registered".to_owned(),
            agent_id: agent.id.clone(),
            by: registered_by.to_owned(),
            evidence_ref: evidence_ref.to_owned(),
            at,
        })
    }

    /// Revokes an agent's deep registration and returns the audit event.
    ///
    /// # Errors
    ///
    /// [`TierError::UnknownAgent`] when no agent with that id is enrolled;
    /// [`TierError::NotDeep`] when it is not registered deep.
    pub fn revoke_deep(&mut self, agent_id: &str, at: i64) -> Result<RegistryEvent, TierError> {
        let agent = self
            .agents
            .iter_mut()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| TierError::UnknownAgent {
                id: agent_id.to_owned(),
            })?;
        if !agent.registered_deep {
            return Err(TierError::NotDeep {
                id: agent.id.clone(),
            });
        }
        agent.registered_deep = false;
        Ok(RegistryEvent {
            event: "agent.deep_revoked".to_owned(),
            agent_id: agent.id.clone(),
            by: String::new(),
            evidence_ref: String::new(),
            at,
        })
    }

    /// The enrolled agent with this id, if any.
    #[must_use]
    pub fn get(&self, agent_id: &str) -> Option<&AgentIdentity> {
        self.agents.iter().find(|a| a.id == agent_id)
    }
}
