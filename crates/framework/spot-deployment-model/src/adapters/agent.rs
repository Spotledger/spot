//! The agent adapter: the agent's own maximum tier.
//!
//! `spot-agent-tiers` owns the identity and its ceiling. The deployment model reports that
//! ceiling as `agent_tier`, so an action planned from the model is planned against the tier
//! the authoriser will actually enforce — not a tier the prompt claimed.

use spot_agent_tiers::{AgentIdentity, Tier};
use spot_self_model::EvidenceRef;

use crate::DeploymentError;
use crate::facts::Facts;
use crate::model::FactSourceKind;

/// The agent registry's ground truth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFacts {
    epoch: u64,
    tier: Tier,
    evidence: Option<EvidenceRef>,
}

impl AgentFacts {
    /// Reads an agent identity's maximum tier.
    #[must_use]
    pub fn new(epoch: u64, identity: &AgentIdentity) -> Self {
        Self {
            epoch,
            tier: identity.max_tier,
            evidence: None,
        }
    }

    /// A tier without an identity, for a caller that already resolved one.
    #[must_use]
    pub fn from_tier(epoch: u64, tier: Tier) -> Self {
        Self {
            epoch,
            tier,
            evidence: None,
        }
    }

    /// Names the record that proves the read.
    #[must_use]
    pub fn with_evidence(mut self, evidence: EvidenceRef) -> Self {
        self.evidence = Some(evidence);
        self
    }
}

impl crate::facts::FactSource for AgentFacts {
    fn source(&self) -> FactSourceKind {
        FactSourceKind::Agent
    }

    fn epoch(&self) -> u64 {
        self.epoch
    }

    fn facts(&self) -> Result<Facts, DeploymentError> {
        Ok(Facts {
            agent_tier: Some(self.tier),
            ..Facts::new()
        })
    }

    fn evidence(&self) -> Option<EvidenceRef> {
        self.evidence.clone()
    }
}
