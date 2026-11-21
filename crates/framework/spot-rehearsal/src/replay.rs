//! The replay policy: whether recent transactions are replayed over the clone after the plan is
//! applied (behavioural replay itself is WP-MIG-06.T2).
//!
//! The policy is a per-site setting. `off` never replays (routine updates); `auto` replays when
//! the plan contains a behaviour-changing or high-sensitivity entry; `always` replays whatever
//! the plan holds (sites where failure is not an option). [`decide_replay`] resolves a policy
//! against the plan and the [`ReleaseDeclaration`](crate::ReleaseDeclaration).

use serde::{Deserialize, Serialize};

use spot_migration_plan::ReleasePlan;

use crate::declaration::ReleaseDeclaration;

/// The per-site replay setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayPolicy {
    /// Never replay.
    #[default]
    Off,
    /// Replay when the plan declares a behaviour change or a high-sensitivity entry.
    Auto,
    /// Always replay.
    Always,
}

impl ReplayPolicy {
    /// The policy's name, as it is written in a site setting.
    pub fn as_str(self) -> &'static str {
        match self {
            ReplayPolicy::Off => "off",
            ReplayPolicy::Auto => "auto",
            ReplayPolicy::Always => "always",
        }
    }
}

/// The decision: whether to replay, why, and which plan changes triggered it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayDecision {
    /// The policy the decision was made under.
    pub policy: ReplayPolicy,
    /// Whether behavioural replay runs.
    pub replay: bool,
    /// Plain-language reason, for the report.
    pub reason: String,
    /// The plan change ids that triggered `auto`, in plan order. Empty otherwise.
    pub triggers: Vec<String>,
}

/// Resolves `policy` against `plan` and `declaration`.
pub fn decide_replay(
    policy: ReplayPolicy,
    plan: &ReleasePlan,
    declaration: &dyn ReleaseDeclaration,
) -> ReplayDecision {
    match policy {
        ReplayPolicy::Off => ReplayDecision {
            policy,
            replay: false,
            reason: "the site's replay policy is off".to_owned(),
            triggers: Vec::new(),
        },
        ReplayPolicy::Always => ReplayDecision {
            policy,
            replay: true,
            reason: "the site's replay policy is always".to_owned(),
            triggers: Vec::new(),
        },
        ReplayPolicy::Auto => {
            let triggers: Vec<String> = plan
                .changes
                .iter()
                .filter(|change| {
                    declaration.is_behaviour_change(change)
                        || declaration.is_high_sensitivity(change)
                })
                .map(|change| change.id.clone())
                .collect();
            let replay = !triggers.is_empty();
            let reason = if replay {
                format!(
                    "auto: the plan declares {} behaviour-changing or high-sensitivity change(s)",
                    triggers.len()
                )
            } else {
                "auto: the plan declares no behaviour-changing or high-sensitivity change"
                    .to_owned()
            };
            ReplayDecision {
                policy,
                replay,
                reason,
                triggers,
            }
        }
    }
}
