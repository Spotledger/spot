//! Shared fixtures: the seeded catalogue (loaded through the public loader, so the loader
//! is exercised on every run), agent builders (plain and registered-deep through the
//! registry), and decision readers.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_agent_tiers::{
    ActionCatalogue, AgentIdentity, AgentRegistry, Approval, Context, Domain, Tier,
};

/// The seed catalogue's text, included so the tests and the file cannot drift apart.
pub const ACTIONS_TOML: &str = include_str!("../actions.toml");

/// A person's id for approvals.
pub const PERSON: &str = "person-ada";

/// The agents' owner in these fixtures (an organisation that answers for its agents).
pub const OWNER: &str = "ops-team";

/// The catalogue from the seed file.
pub fn catalogue() -> ActionCatalogue {
    ActionCatalogue::from_toml(ACTIONS_TOML).expect("seed catalogue loads")
}

/// A plain active support agent with the given max tier.
pub fn support_agent(id: &str, max_tier: Tier) -> AgentIdentity {
    AgentIdentity::new(id, OWNER, Domain::Support, max_tier)
}

/// A plain active operations agent with the given max tier.
pub fn operations_agent(id: &str, max_tier: Tier) -> AgentIdentity {
    AgentIdentity::new(id, OWNER, Domain::Operations, max_tier)
}

/// A support agent registered deep through the registry — the only path to the flag.
pub fn deep_support_agent(registry: &mut AgentRegistry, id: &str, max_tier: Tier) -> AgentIdentity {
    registry
        .enroll(support_agent(id, max_tier))
        .expect("enroll");
    registry
        .register_deep(id, PERSON, "evidence/support-case-42", 1_000)
        .expect("register deep");
    registry.get(id).expect("enrolled").clone()
}

/// A deep support agent over its own fresh registry.
pub fn deep_agent(id: &str, max_tier: Tier) -> AgentIdentity {
    deep_support_agent(&mut AgentRegistry::new(), id, max_tier)
}

/// The named action's default context from the catalogue spec: a read context for read
/// actions, a clean change context (journaled, snapshotted, no financial target) for
/// changes; an unknown action gets the read context (the authoriser refuses it before the
/// floor matters).
pub fn context_for(catalogue: &ActionCatalogue, action: &str) -> Context {
    let change = catalogue
        .get(action)
        .map(|spec| spec.is_change())
        .unwrap_or(false);
    if change {
        Context::change()
    } else {
        Context::read()
    }
}

/// A human approval by a person.
pub fn approval_by(reference: &str) -> Approval {
    Approval {
        approver: PERSON.to_owned(),
        reference: reference.to_owned(),
        at: 900,
    }
}
