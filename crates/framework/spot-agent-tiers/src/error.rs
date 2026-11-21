//! The one error type for catalogue loads and registry changes: what went wrong, in words.
//!
//! Refusal reasons carry this crate's stable `agent.*` codes (see
//! [`crate::authoriser::RefusalReason::code`]); these load/registry errors carry none.

/// Something failed loading the action catalogue or applying a registry change.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TierError {
    /// A TOML document failed to parse. The message names the text and the parse error.
    #[error("cannot read {at}: {detail}")]
    Parse {
        /// The file or text the document came from.
        at: String,
        /// What the parser objected to.
        detail: String,
    },

    /// A value had the wrong shape: an empty action name, or an entry that is not an entry.
    #[error("{at}: {detail}")]
    ValueInvalid {
        /// The file or entry the value came from.
        at: String,
        /// What was wrong with it.
        detail: String,
    },

    /// Two entries declare the same action name; a name is the action's stable address.
    #[error("{at}: duplicate action `{name}`")]
    DuplicateAction {
        /// Where the duplicate occurred.
        at: String,
        /// The name declared twice.
        name: String,
    },

    /// An entry declares a tier below what its effect needs (the one minimum-tier table on
    /// [`crate::action::Effect::min_tier`]): a `structural` effect declared at T1 is refused.
    #[error(
        "{at}: action `{name}` declares tier {declared} but its {effect} effect needs at least {minimum}"
    )]
    EffectBelowFloor {
        /// Where the entry occurred.
        at: String,
        /// The entry's action name.
        name: String,
        /// The tier the entry declared.
        declared: String,
        /// The declared effect's name.
        effect: String,
        /// The minimum tier the effect needs.
        minimum: String,
    },

    /// No agent with this id is enrolled in the registry.
    #[error("no agent `{id}` is enrolled in the registry")]
    UnknownAgent {
        /// The id nothing is enrolled under.
        id: String,
    },

    /// Only a support-domain agent may be registered deep: the deep registry belongs to the
    /// support organisation (blueprint WS13, "registered deep agents").
    #[error("agent `{id}` is a {domain} agent; only support agents may be registered deep")]
    DeepNeedsSupportDomain {
        /// The agent's id.
        id: String,
        /// The domain the agent actually has.
        domain: String,
    },

    /// A data record arrived with `registered_deep` set: the flag cannot be set any other
    /// way than [`AgentRegistry::register_deep`](crate::registry::AgentRegistry::register_deep).
    #[error(
        "agent `{id}` arrives with `registered_deep` set; the flag is set only through the registry"
    )]
    DeepOnlyThroughRegistry {
        /// The agent's id.
        id: String,
    },

    /// The agent is already registered deep; revoke first to re-register against new evidence.
    #[error("agent `{id}` is already registered deep; revoke it first")]
    AlreadyDeep {
        /// The agent's id.
        id: String,
    },

    /// `revoke_deep` was called for an agent that is not registered deep.
    #[error("agent `{id}` is not registered deep")]
    NotDeep {
        /// The agent's id.
        id: String,
    },
}
