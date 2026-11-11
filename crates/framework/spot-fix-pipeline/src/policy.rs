//! The policy that decides a plan's path: direct apply or rehearse first, pre-images or a
//! snapshot, and whether a T3 plan needs a human.
//!
//! The blueprint (WS13) says T1/T2 low-risk fixes may run unattended, a T3 structural action
//! always rehearses, and every agent — registered deep ones included — stays under the hard
//! floor: journaled and snapshotted first, audit on. This module is the one place those rules
//! are turned into decisions; the engine asks it and never decides for itself.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_agent_tiers::Tier;

use crate::model::{AgentRef, FixPlan, Mechanism, Risk};

/// The path a plan takes through the pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPath {
    /// No rehearsal: the plan applies and verifies directly (T1/T2 low risk, rehearsal-optional).
    Direct,
    /// Rehearse on a clone first, then apply and verify.
    Rehearse,
}

impl PlanPath {
    /// The stable snake_case text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Rehearse => "rehearse",
        }
    }
}

/// The configured rules of the fix pipeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixPolicy {
    /// How long a verified fix stays undoable, in seconds.
    pub undo_window_secs: i64,
    /// The remedy ids whose authors marked rehearsal optional (T1/T2 low risk only).
    pub rehearsal_optional: BTreeSet<String>,
    /// The largest number of affected records that still counts as a small, pre-image fix.
    pub small_fix_max_affected: usize,
    /// Whether a registered deep agent may self-approve its own T3 fix.
    pub deep_agents_self_approve_t3: bool,
}

impl Default for FixPolicy {
    fn default() -> Self {
        Self {
            undo_window_secs: 86_400,
            rehearsal_optional: BTreeSet::new(),
            small_fix_max_affected: 50,
            deep_agents_self_approve_t3: true,
        }
    }
}

impl FixPolicy {
    /// The policy with one remedy marked rehearsal optional.
    #[must_use]
    pub fn with_rehearsal_optional(mut self, remedy: &str) -> Self {
        self.rehearsal_optional.insert(remedy.to_owned());
        self
    }

    /// The policy with a different undo window.
    #[must_use]
    pub fn with_undo_window(mut self, seconds: i64) -> Self {
        self.undo_window_secs = seconds;
        self
    }

    /// Whether `plan` may skip rehearsal.
    ///
    /// A T3 plan never may. A T1/T2 plan may only when it is low risk, the source is a remedy
    /// and the remedy's author marked it rehearsal optional.
    #[must_use]
    pub fn may_skip_rehearsal(&self, plan: &FixPlan) -> bool {
        if plan.tier == Tier::T3 || plan.risk != Risk::Low {
            return false;
        }
        plan.remedy()
            .is_some_and(|remedy| self.rehearsal_optional.contains(remedy.as_str()))
    }

    /// The path `plan` takes.
    #[must_use]
    pub fn path(&self, plan: &FixPlan) -> PlanPath {
        if self.may_skip_rehearsal(plan) {
            PlanPath::Direct
        } else {
            PlanPath::Rehearse
        }
    }

    /// The mechanism the apply uses.
    ///
    /// A T3 plan, or one too large for the small-fix path, takes a snapshot; a small T1/T2 fix
    /// captures logical pre-images and needs no maintenance gate.
    #[must_use]
    pub fn mechanism(&self, plan: &FixPlan) -> Mechanism {
        if plan.tier == Tier::T3 || plan.affected.len() > self.small_fix_max_affected {
            Mechanism::Snapshot
        } else {
            Mechanism::PreImage
        }
    }

    /// Whether the apply must take a snapshot.
    #[must_use]
    pub fn requires_snapshot(&self, plan: &FixPlan) -> bool {
        self.mechanism(plan) == Mechanism::Snapshot
    }

    /// Whether the apply needs the maintenance gate.
    #[must_use]
    pub fn requires_maintenance_gate(&self, plan: &FixPlan) -> bool {
        self.requires_snapshot(plan)
    }

    /// Whether `agent` may run `plan` at all.
    ///
    /// # Errors
    ///
    /// [`FixError::TierNotFixable`](crate::FixError::TierNotFixable) for T0 (it only reads) and
    /// [`FixError::Refused`](crate::FixError::Refused) when the plan needs a higher tier than the
    /// agent holds.
    pub fn admit(&self, plan: &FixPlan, agent: &AgentRef) -> Result<(), crate::FixError> {
        if plan.tier == Tier::T0 {
            return Err(crate::FixError::TierNotFixable {
                tier: Tier::T0.as_str().to_owned(),
            });
        }
        if agent.tier < plan.tier {
            return Err(crate::FixError::Refused {
                detail: format!(
                    "the agent holds tier {} but the plan needs {}",
                    agent.tier.as_str(),
                    plan.tier.as_str()
                ),
            });
        }
        if matches!(plan.source, crate::model::FixSource::Structural { .. })
            && agent.tier < Tier::T3
        {
            return Err(crate::FixError::Refused {
                detail: "a structural plan needs a T3 agent".to_owned(),
            });
        }
        Ok(())
    }

    /// Whether `plan` needs a human approval for `agent`.
    ///
    /// A T3 plan needs one unless the agent is registered deep and the policy lets it approve
    /// its own structural change. T1 and T2 run unattended.
    #[must_use]
    pub fn requires_approval(&self, plan: &FixPlan, agent: &AgentRef) -> bool {
        if plan.tier != Tier::T3 {
            return false;
        }
        !(agent.deep && self.deep_agents_self_approve_t3)
    }

    /// Whether `agent` may self-approve its T3 plan.
    #[must_use]
    pub fn may_self_approve(&self, plan: &FixPlan, agent: &AgentRef) -> bool {
        plan.tier == Tier::T3 && agent.deep && self.deep_agents_self_approve_t3
    }
}
