//! Shared fixtures for the `spot-remedies` integration tests.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs, dead_code)]

use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Tier};
use spot_remedies::{Gate, RemedySettings};

/// A support agent with the given ceiling.
pub fn support_agent(id: &str, tier: Tier) -> AgentIdentity {
    AgentIdentity::new(id, "support-org", Domain::Support, tier)
}

/// An operations agent with the given ceiling.
pub fn operations_agent(id: &str, tier: Tier) -> AgentIdentity {
    AgentIdentity::new(id, "operations-org", Domain::Operations, tier)
}

/// A registered-deep support agent, so a T3 remedy may run unattended.
pub fn deep_agent(id: &str) -> AgentIdentity {
    let mut registry = AgentRegistry::new();
    registry.enroll(support_agent(id, Tier::T3)).unwrap();
    registry
        .register_deep(id, "support-lead", "evidence", 0)
        .unwrap();
    registry.get(id).unwrap().clone()
}

/// The default settings.
pub fn settings() -> RemedySettings {
    RemedySettings::default()
}

/// The change gate every remedy run uses.
pub fn gate() -> Gate {
    Gate::change()
}

/// A fixed instant.
pub const NOW: i64 = 1_700_000_000;
