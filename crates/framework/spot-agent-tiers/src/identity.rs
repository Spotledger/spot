//! Agent identity (ADR-070 section 4: the `agent` user type): who an agent is, who answers
//! for it, its domain and ceiling, and whether it may act at all.

use serde::{Deserialize, Serialize};

use crate::action::Domain;
use crate::tier::Tier;

/// Who an agent answers for: a person or an organisation, as ADR-070 requires for audits.
///
/// The plain id text of the owner (a user id or an organisation name); the authoriser reads
/// it when [`Policy::separate_approver`](crate::authoriser::Policy::separate_approver)
/// forbids an owner approving their own agent.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Owner(pub String);

impl Owner {
    /// The owner's text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Owner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether an agent may act at all (ADR-070 rule 2: users are never deleted — they are
/// deactivated; an agent's life ends as `retired`, an investigated one is `suspended`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// May act, within its max tier and domain.
    Active,
    /// May not act; every decision is a refusal with a stable reason until reactivated.
    Suspended,
    /// Out of service by retirement; every decision is a refusal with a stable reason.
    Retired,
}

impl Status {
    /// The stable snake_case text form, which is also the serde and TOML form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Active => "active",
            Status::Suspended => "suspended",
            Status::Retired => "retired",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An AI agent identity (ADR-070 section 4, the `agent` user type): an identity with a tier.
///
/// `registered_deep` is deliberately **crate-only**: the flag cannot be set from outside
/// the crate at all — no constructor, no field, no data path — and inside it only
/// [`AgentRegistry::register_deep`](crate::registry::AgentRegistry::register_deep) ever
/// sets it, with its audit event. A data record that arrives with the flag set is refused
/// by [`AgentRegistry::enroll`](crate::registry::AgentRegistry::enroll) (serde on the type
/// exists for records and diagnostics; the enrolled pipeline keeps the flag registry-only).
/// Read it back with [`AgentIdentity::registered_deep`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentIdentity {
    /// The agent's stable id (the `Actor::User { id }` of the events contract, ADR-070
    /// rule 1).
    pub id: String,
    /// The person or organisation that answers for it.
    pub owner: Owner,
    /// The domain the agent works in; support and operations never cross.
    pub domain: Domain,
    /// The highest tier the agent may ever run.
    pub max_tier: Tier,
    /// Whether the agent is registered deep. `pub(crate)` on purpose: invisible and
    /// unsettable from outside the crate; inside it, only the registry's
    /// `register_deep`/`revoke_deep` touch it (see the type's docs).
    pub(crate) registered_deep: bool,
    /// Whether the agent may act at all.
    pub status: Status,
}

impl AgentIdentity {
    /// A plain, active, non-deep agent: the rest of the fields stay public data, but the
    /// deep flag starts false and only the registry can raise it.
    #[must_use]
    pub fn new(id: &str, owner: &str, domain: Domain, max_tier: Tier) -> Self {
        Self {
            id: id.to_owned(),
            owner: Owner(owner.to_owned()),
            domain,
            max_tier,
            registered_deep: false,
            status: Status::Active,
        }
    }

    /// Whether the agent is registered deep (read-only by design; see the type's docs).
    #[must_use]
    pub fn registered_deep(&self) -> bool {
        self.registered_deep
    }

    /// The agent with a different [`Status`] (suspensions, retirements — the pipeline's job
    /// to set, the authoriser's to enforce).
    #[must_use]
    pub fn with_status(mut self, status: Status) -> Self {
        self.status = status;
        self
    }
}
