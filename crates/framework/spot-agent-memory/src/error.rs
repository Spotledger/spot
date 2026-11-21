//! The one error type for agent memory, with stable machine-readable `agent_memory.*` codes.
//!
//! Each code has exactly one entry in `crates/framework/spot-errors/catalogue/agent_memory.toml`.
//! The codes are this crate's stable vocabulary: they keep their meaning within a major
//! version. A message names the *class* of the failure and the id or field it is about — never
//! a stored value, a document's content or a secret (ADR-26, `spot-redact`).

/// The stable `agent_memory.*` codes, in one place. Each has one catalogue entry.
pub mod codes {
    /// A tenant, entry, procedure, candidate or CI-run id is not well formed.
    pub const INVALID_ID: &str = "agent_memory.invalid_id";
    /// The content carries a value of a denied field class, or a numeric amount.
    pub const UNSANITISED: &str = "agent_memory.unsanitised";
    /// The content is longer than a memory entry may hold, or carries control characters.
    pub const CONTENT_TOO_LONG: &str = "agent_memory.content_too_long";
    /// A memory entry breaks a structural rule (empty symptoms, no steps, no audits).
    pub const ENTRY_INVALID: &str = "agent_memory.entry_invalid";
    /// The reading agent's tier is below the procedure's tier.
    pub const TIER_REFUSED: &str = "agent_memory.tier_refused";
    /// The recall query is larger than the crate will embed.
    pub const QUERY_TOO_LONG: &str = "agent_memory.query_too_long";
    /// The embedding provider failed, or returned a shape this crate cannot accept.
    pub const EMBED_FAILED: &str = "agent_memory.embed_failed";
    /// The memory store refused a read or a write.
    pub const STORE_FAILED: &str = "agent_memory.store_failed";
    /// The procedure has fewer confirming incidents than the learning policy requires.
    pub const UNPROVEN: &str = "agent_memory.unproven";
    /// The procedure caused harm, so it may never become a candidate remedy.
    pub const HARMFUL: &str = "agent_memory.harmful";
    /// A candidate status does not allow the requested transition.
    pub const STATUS_INVALID: &str = "agent_memory.status_invalid";
    /// Promotion named no CI run, or a run other than the one that passed.
    pub const CI_RUN_MISSING: &str = "agent_memory.ci_run_missing";
    /// A candidate remedy breaks a rule (no steps, parameters that do not fit the schema).
    pub const CANDIDATE_INVALID: &str = "agent_memory.candidate_invalid";
    /// The candidate bundle (its spec and scenario files) could not be rendered.
    pub const BUNDLE_FAILED: &str = "agent_memory.bundle_failed";
    /// A memory record or export could not be serialised.
    pub const SERIALISE: &str = "agent_memory.serialise";
}

/// Every `agent_memory.*` code, so the error-code scan and the catalogue stay in step.
pub const CODES: [&str; 15] = [
    codes::INVALID_ID,
    codes::UNSANITISED,
    codes::CONTENT_TOO_LONG,
    codes::ENTRY_INVALID,
    codes::TIER_REFUSED,
    codes::QUERY_TOO_LONG,
    codes::EMBED_FAILED,
    codes::STORE_FAILED,
    codes::UNPROVEN,
    codes::HARMFUL,
    codes::STATUS_INVALID,
    codes::CI_RUN_MISSING,
    codes::CANDIDATE_INVALID,
    codes::BUNDLE_FAILED,
    codes::SERIALISE,
];

/// Something failed in agent memory.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum MemoryError {
    /// A tenant, entry, procedure, candidate or CI-run id is not well formed.
    #[error("the {what} `{value}` is not valid: {reason}")]
    InvalidId {
        /// What kind of id was refused (`tenant`, `episode`, ...).
        what: String,
        /// The refused text. Ids are addresses, never values.
        value: String,
        /// Why it was refused.
        reason: String,
    },

    /// The content carries a value of a denied field class, or a numeric amount.
    #[error(
        "the content carries a {class} value; memory may reference records only by type and id"
    )]
    Unsanitised {
        /// The stable class name (`secret`, `financial`, `pii`).
        class: String,
        /// What shape the scan caught (`field_value`, `pattern`, `amount`).
        shape: String,
    },

    /// The content is longer than a memory entry may hold, or carries control characters.
    #[error("the content is {detail}")]
    Unstorable {
        /// What was wrong, in words (`4096 bytes long, and 2048 is the maximum`).
        detail: String,
    },

    /// A memory entry breaks a structural rule.
    #[error("the {kind} entry `{id}` is invalid: {reason}")]
    EntryInvalid {
        /// The entry kind.
        kind: String,
        /// The entry id.
        id: String,
        /// Which rule it broke.
        reason: String,
    },

    /// The reading agent's tier is below the procedure's tier.
    #[error("a {agent} agent may not read the {required} procedure `{procedure}`")]
    TierRefused {
        /// The reading agent's tier.
        agent: String,
        /// The tier the procedure needs.
        required: String,
        /// The procedure id.
        procedure: String,
    },

    /// The recall query is larger than the crate will embed.
    #[error("the query is {bytes} bytes long, and {max} is the maximum")]
    QueryTooLong {
        /// The query's byte length.
        bytes: usize,
        /// The maximum the crate accepts.
        max: usize,
    },

    /// The embedding provider failed, or returned a shape this crate cannot accept.
    #[error("the embedding provider failed: {detail}")]
    Embed {
        /// The provider's message, which names no document value.
        detail: String,
    },

    /// The memory store refused a read or a write.
    #[error("the memory store failed: {detail}")]
    Store {
        /// What the store refused.
        detail: String,
    },

    /// The procedure has fewer confirming incidents than the learning policy requires.
    #[error(
        "the procedure `{procedure}` has {confirmations} confirming incidents and needs {required}"
    )]
    Unproven {
        /// The procedure id.
        procedure: String,
        /// How many successes were recorded.
        confirmations: u32,
        /// How many the policy requires.
        required: u32,
    },

    /// The procedure caused harm, so it may never become a candidate remedy.
    #[error("the procedure `{procedure}` caused harm in {incidents} incident(s)")]
    Harmful {
        /// The procedure id.
        procedure: String,
        /// How many recorded incidents ended in harm.
        incidents: usize,
    },

    /// A candidate status does not allow the requested transition.
    #[error("the candidate `{candidate}` is {status}; {action} needs it to be {required}")]
    Status {
        /// The candidate id.
        candidate: String,
        /// Its current status.
        status: String,
        /// The status the transition needs.
        required: String,
        /// The transition that was refused.
        action: String,
    },

    /// Promotion named no CI run, or a run other than the one that passed.
    #[error("the candidate `{candidate}` cannot be promoted: {reason}")]
    CiRun {
        /// The candidate id.
        candidate: String,
        /// Why the run was refused.
        reason: String,
    },

    /// A candidate remedy breaks a rule.
    #[error("the candidate `{candidate}` is invalid: {reason}")]
    Candidate {
        /// The candidate id.
        candidate: String,
        /// Which rule it broke.
        reason: String,
    },

    /// The candidate bundle could not be rendered.
    #[error("the candidate bundle for `{candidate}` could not be rendered: {detail}")]
    Bundle {
        /// The candidate id.
        candidate: String,
        /// The renderer's message.
        detail: String,
    },

    /// A memory record or export could not be serialised.
    #[error("the {what} could not be serialised: {detail}")]
    Serialise {
        /// What was being serialised.
        what: String,
        /// The serialiser's message.
        detail: String,
    },
}

impl MemoryError {
    /// The stable `agent_memory.*` code, the one string callers match on.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidId { .. } => codes::INVALID_ID,
            Self::Unsanitised { .. } => codes::UNSANITISED,
            Self::Unstorable { .. } => codes::CONTENT_TOO_LONG,
            Self::EntryInvalid { .. } => codes::ENTRY_INVALID,
            Self::TierRefused { .. } => codes::TIER_REFUSED,
            Self::QueryTooLong { .. } => codes::QUERY_TOO_LONG,
            Self::Embed { .. } => codes::EMBED_FAILED,
            Self::Store { .. } => codes::STORE_FAILED,
            Self::Unproven { .. } => codes::UNPROVEN,
            Self::Harmful { .. } => codes::HARMFUL,
            Self::Status { .. } => codes::STATUS_INVALID,
            Self::CiRun { .. } => codes::CI_RUN_MISSING,
            Self::Candidate { .. } => codes::CANDIDATE_INVALID,
            Self::Bundle { .. } => codes::BUNDLE_FAILED,
            Self::Serialise { .. } => codes::SERIALISE,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CODES, MemoryError};

    #[test]
    fn every_code_is_dotted_lower_case_and_unique() {
        for code in CODES {
            let segments: Vec<&str> = code.split('.').collect();
            assert!(segments.len() >= 2, "{code} has at least two segments");
            assert_eq!(
                segments[0], "agent_memory",
                "{code} carries the crate prefix"
            );
            for segment in segments {
                let mut chars = segment.chars();
                assert!(
                    matches!(chars.next(), Some(first) if first.is_ascii_lowercase()),
                    "{code} starts every segment with a lower-case letter"
                );
                assert!(
                    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                    "{code} is dotted lower-case"
                );
            }
        }
        let mut sorted = CODES.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), CODES.len(), "no code is listed twice");
    }

    #[test]
    fn a_refusal_carries_its_code_and_never_a_value() {
        let error = MemoryError::Unsanitised {
            class: "financial".to_owned(),
            shape: "amount".to_owned(),
        };
        assert_eq!(error.code(), "agent_memory.unsanitised");
        let text = error.to_string();
        assert!(text.contains("financial"));
        assert!(text.contains("by type and id"));
    }
}
