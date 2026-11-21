//! What a stale node does when a user posts (ADR-065 section 4).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The document type's declared `stale_policy`. The platform default is `AllowStamp`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StalePolicy {
    /// Post, stamp it stale, raise a finding at the hub if a rule was missed.
    #[default]
    AllowStamp,
    /// The user must confirm a plain warning; the confirmation goes in the stamp.
    Warn,
    /// Refuse, unless the actor holds `sync.override_stale`.
    Block,
}

/// The outcome of a posting attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostDecision {
    /// No class the document requires is stale: post normally.
    Allow,
    /// Post, and stamp the document stale.
    AllowStamped,
    /// Ask the user to confirm the warning, then post and record the confirmation.
    NeedWarningConfirmed {
        /// Message key of the warning text.
        text_key: String,
        /// Parameters of the message (`classes`: comma-separated stale class names).
        params: BTreeMap<String, String>,
    },
    /// Refuse, with an explanation and what to do.
    Block {
        /// Message key of the reason.
        reason_key: String,
        /// Message key of the remedy (connect, or ask a supervisor).
        remedy_key: String,
    },
    /// A supervisor holding `sync.override_stale` overrode a block. Audited.
    AllowWithOverride {
        /// The stale classes that were overridden.
        stale: Vec<String>,
    },
}

impl PostDecision {
    /// Whether the decision must be flagged for audit (an override).
    #[must_use]
    pub fn requires_audit(&self) -> bool {
        matches!(self, Self::AllowWithOverride { .. })
    }
}

/// Decide what posting does. Only classes the document type `requires_fresh` and that are stale
/// matter; a stale class the type does not depend on never affects it.
#[must_use]
pub fn decide(
    stale_classes: &[String],
    requires_fresh: &[String],
    policy: StalePolicy,
    actor_can_override: bool,
) -> PostDecision {
    let relevant: Vec<String> = requires_fresh
        .iter()
        .filter(|c| stale_classes.contains(c))
        .fold(Vec::new(), |mut acc, c| {
            if !acc.contains(c) {
                acc.push(c.clone());
            }
            acc
        });
    if relevant.is_empty() {
        return PostDecision::Allow;
    }
    match policy {
        StalePolicy::AllowStamp => PostDecision::AllowStamped,
        StalePolicy::Warn => {
            let mut params = BTreeMap::new();
            params.insert("classes".to_owned(), relevant.join(","));
            PostDecision::NeedWarningConfirmed {
                text_key: "sync_stale_warn".to_owned(),
                params,
            }
        }
        StalePolicy::Block if actor_can_override => {
            PostDecision::AllowWithOverride { stale: relevant }
        }
        StalePolicy::Block => PostDecision::Block {
            reason_key: "sync_stale_block_reason".to_owned(),
            remedy_key: "sync_stale_block_remedy".to_owned(),
        },
    }
}
