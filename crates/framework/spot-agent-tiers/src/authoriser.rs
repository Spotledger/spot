//! The authoriser: nine checks, first failing check wins, every outcome typed and coded.
//!
//! [`Authoriser::decide`] is the pure core of the crate: no storage, no clock,
//! no network. The request carries the agent, the action name and a [`Context`]; the answer
//! is [`Decision::Allow`] with its reason, or [`Decision::Refuse`] with one
//! [`RefusalReason`] — the first failing check's, in the documented order (see
//! [`crate::order`]). The order is exercised in order by the integration suite's scenario
//! tests and locked by a property test.

use serde::{Deserialize, Serialize};

use crate::action::{ActionCatalogue, Domain};
use crate::identity::{AgentIdentity, Status};
use crate::tier::Tier;

/// The request's facts beyond agent and action.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    /// A human approval carried in the request (T3 for a non-deep agent needs one).
    #[serde(default)]
    pub human_approval: Option<Approval>,
    /// The change journal: a change may run only when the journal is on.
    #[serde(default)]
    pub journaled: bool,
    /// Change protection: a change may run only with a snapshot already taken.
    #[serde(default)]
    pub snapshot_taken: bool,
    /// The route a change to submitted financial records must take. [`FinancialRoute::Direct`]
    /// is the always-refused direct edit; cancel, amend and journal entry are the allowed
    /// routes.
    #[serde(default)]
    pub targets_submitted_financial: Option<FinancialRoute>,
}

impl Context {
    /// A read context: no approval, no journal, no snapshot, no financial target — the
    /// shape of every T0 request.
    #[must_use]
    pub fn read() -> Self {
        Self::default()
    }

    /// A change context for a target that is not a submitted financial record: journaled
    /// and snapshotted.
    #[must_use]
    pub fn change() -> Self {
        Self {
            journaled: true,
            snapshot_taken: true,
            ..Self::default()
        }
    }
}

/// Who approved, by what reference, and when — a request's carried approval.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approval {
    /// The approver: a person, never an agent.
    pub approver: String,
    /// The approval's reference (a ticket, a change request) — the brief's `ref` field,
    /// spelled `reference` in Rust because `ref` is a keyword.
    #[serde(rename = "ref")]
    pub reference: String,
    /// When the approval was given (unix seconds i64; the crate keeps no clock, so judging
    /// the approval's freshness is the caller's job with the same `now` it passes the
    /// authoriser).
    pub at: i64,
}

/// The route a change to a submitted financial record must take (blueprint WS13, the hard
/// floor: "submitted financial records change only through cancel, amend or journal
/// entries" — a direct edit is always refused).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinancialRoute {
    /// Cancel the record, then act on the successor.
    Cancel,
    /// Amend the submitted record.
    Amend,
    /// Post a journal entry instead of editing.
    JournalEntry,
    /// A direct edit of a submitted financial record — always refused.
    Direct,
}

impl FinancialRoute {
    /// Whether this route is one of the allowed ones (anything but a direct edit).
    #[must_use]
    pub fn is_allowed_route(self) -> bool {
        !matches!(self, FinancialRoute::Direct)
    }
}

/// The policy knobs the deployment sets; the checks read them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Require an approval from someone other than the agent's owner.
    pub separate_approver: bool,
}

impl Default for Policy {
    /// The default policy separates the approver from the agent's owner.
    fn default() -> Self {
        Self {
            separate_approver: true,
        }
    }
}

/// Why a decision said allow: the facts the checks saw, for the record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AllowReason {
    /// The tier the action needed.
    pub tier: Tier,
    /// Whether a carried human approval was the reason the request may run unattended.
    pub ran_on_approval: bool,
}

/// The decision: allow with its reason, or refuse with the first failing check's reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Every check passed, and the agent may proceed.
    Allow {
        /// Why the decision allowed.
        reason: AllowReason,
    },
    /// The first failing check refused, in the documented order.
    Refuse {
        /// The first failing check's reason.
        reason: RefusalReason,
    },
}

impl Decision {
    /// The stable `agent.*` code either way: `agent.allow` for an allow.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Decision::Allow { .. } => "agent.allow",
            Decision::Refuse { reason } => reason.code(),
        }
    }

    /// The plain-language sentence for the decision.
    #[must_use]
    pub fn sentence(&self) -> String {
        match self {
            Decision::Allow { .. } => {
                "The action is within the agent's tier, domain and the hard floor; it may proceed."
                    .to_owned()
            }
            Decision::Refuse { reason } => reason.sentence(),
        }
    }
}

/// The refusal reasons, typed, each with its stable `agent.*` code.
///
/// The variants appear in the checks' order (first failing check wins; see
/// [`crate::order`]).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RefusalReason {
    /// The action name is not in the catalogue: refused, never allowed by default.
    #[error(
        "the action `{action}` is not in the catalogue; an unknown action is refused, never allowed by default"
    )]
    UnknownAction {
        /// The name that is not catalogued.
        action: String,
    },

    /// Only an [`Status::Active`] agent may act (ADR-070 rule 2: deactivation, not delete).
    #[error("the agent `{agent}` is {status}; it may not act")]
    AgentNotActive {
        /// The agent's id.
        agent: String,
        /// The status that refused it.
        status: Status,
    },

    /// Support never operations, operations never support (blueprint WS13).
    #[error("the agent `{agent}` works {agent_domain}; the action `{action}` is {action_domain}")]
    DomainMismatch {
        /// The agent's id.
        agent: String,
        /// The agent's domain.
        agent_domain: Domain,
        /// The action's domain.
        action_domain: Domain,
        /// The action's name.
        action: String,
    },

    /// The agent's own ceiling: refusal stands however well the request is approved.
    #[error("the agent `{agent}` tops out at {max}; the action `{action}` needs {needed}")]
    AboveAgentMaxTier {
        /// The agent's id.
        agent: String,
        /// The agent's max tier.
        max: Tier,
        /// The tier the action needs.
        needed: Tier,
        /// The action's name.
        action: String,
    },

    /// T3 unattended needs a registered deep agent or a human approval in the request.
    #[error(
        "a T3 action unattended needs a registered deep agent or a human approval in the request"
    )]
    TierNeedsApproval {
        /// Whether the agent is registered deep.
        deep: bool,
    },

    /// A carried approval that does not validate (an agent approver, or — with
    /// [`Policy::separate_approver`] — the owner approving their own agent).
    #[error("the approval by `{approver}` is not valid: {detail}")]
    ApprovalInvalid {
        /// The invalid approver's text.
        approver: String,
        /// What made it invalid, in a fixed short phrase.
        detail: String,
    },

    /// The hard floor (blueprint WS13), journal check: a change (any effect above
    /// [`Effect::Read`](crate::action::Effect::Read)) runs only when the change journal is on.
    #[error("a change is refused while the change journal is off; journal first")]
    HardFloorNotJournaled,

    /// The hard floor, snapshot check: a change runs only with a snapshot already taken.
    #[error("a change is refused without a snapshot already taken; snapshot first")]
    HardFloorNoSnapshot,

    /// The hard floor, financial route: submitted financial records change only through
    /// cancel, amend or journal entries; a direct edit is always refused.
    #[error(
        "a submitted financial record changes only through cancel, amend or a journal entry; a direct edit is always refused"
    )]
    HardFloorFinancialRoute {
        /// The route the context declared (the refused one; a direct edit when the authoriser
        /// itself refuses).
        declared: FinancialRoute,
    },
}

impl RefusalReason {
    /// The stable `agent.*` code, for logs, API responses and the decision record.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            RefusalReason::UnknownAction { .. } => "agent.unknown_action",
            RefusalReason::AgentNotActive { .. } => "agent.agent_not_active",
            RefusalReason::DomainMismatch { .. } => "agent.domain_mismatch",
            RefusalReason::AboveAgentMaxTier { .. } => "agent.above_agent_max_tier",
            RefusalReason::TierNeedsApproval { .. } => "agent.tier_needs_approval",
            RefusalReason::ApprovalInvalid { .. } => "agent.approval_invalid",
            RefusalReason::HardFloorNotJournaled => "agent.hard_floor_not_journaled",
            RefusalReason::HardFloorNoSnapshot => "agent.hard_floor_no_snapshot",
            RefusalReason::HardFloorFinancialRoute { .. } => "agent.hard_floor_financial_route",
        }
    }

    /// The plain-language sentence for the refusal.
    #[must_use]
    pub fn sentence(&self) -> String {
        self.to_string()
    }
}

/// The authoriser: the catalogue and the policy, and the nine checks over them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Authoriser {
    catalogue: ActionCatalogue,
    policy: Policy,
}

impl Authoriser {
    /// An authoriser over a catalogue, with the default policy
    /// ([`separate_approver = true`](Policy::default)).
    #[must_use]
    pub fn new(catalogue: ActionCatalogue) -> Self {
        Self {
            catalogue,
            policy: Policy::default(),
        }
    }

    /// An authoriser over a catalogue with an explicit policy.
    #[must_use]
    pub fn with_policy(catalogue: ActionCatalogue, policy: Policy) -> Self {
        Self { catalogue, policy }
    }

    /// The authoriser's catalogue.
    #[must_use]
    pub fn catalogue(&self) -> &ActionCatalogue {
        &self.catalogue
    }

    /// The authoriser's policy.
    #[must_use]
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// Decides one request: nine checks in the documented order, first failing check wins.
    ///
    /// The checks, in order, are the nine codes of [`CHECK_ORDER`](crate::order::CHECK_ORDER):
    ///
    /// 1. `agent.unknown_action` — the name is not catalogued (a broken catalogue never
    ///    loads: [`ActionCatalogue::from_toml`] is total), so nothing else can run.
    /// 2. `agent.agent_not_active` — only an active agent may act.
    /// 3. `agent.domain_mismatch` — support never operations, operations never support.
    /// 4. `agent.above_agent_max_tier` — the agent's ceiling, checked before any approval
    ///    could matter.
    /// 5. `agent.tier_needs_approval` — T3 unattended for a non-deep agent.
    /// 6. `agent.approval_invalid` — a carried approval that does not validate (the
    ///    approver is an agent, or the agent's owner when the policy separates them).
    /// 7. `agent.hard_floor_not_journaled` — a change with the journal off.
    /// 8. `agent.hard_floor_no_snapshot` — a change without a snapshot.
    /// 9. `agent.hard_floor_financial_route` — a change to submitted financial records must
    ///    take an allowed route; a direct edit is always refused.
    ///
    /// The hard floor binds every agent, deep ones included: the deep flag lifts only the
    /// T3 approval requirement, never the floor.
    #[must_use]
    pub fn decide(&self, agent: &AgentIdentity, action: &str, ctx: &Context) -> Decision {
        // 1. The catalogue speaks first: an unknown name is refused, never allowed by
        //    default.
        let Some(spec) = self.catalogue.get(action) else {
            return Decision::Refuse {
                reason: RefusalReason::UnknownAction {
                    action: action.to_owned(),
                },
            };
        };

        // 2. Only an active agent may act.
        if agent.status != Status::Active {
            return Decision::Refuse {
                reason: RefusalReason::AgentNotActive {
                    agent: agent.id.clone(),
                    status: agent.status,
                },
            };
        }

        // 3. Support versus operations: they never cross.
        if agent.domain != spec.domain {
            return Decision::Refuse {
                reason: RefusalReason::DomainMismatch {
                    agent: agent.id.clone(),
                    agent_domain: agent.domain,
                    action_domain: spec.domain,
                    action: spec.name.clone(),
                },
            };
        }

        // 4. The agent's own ceiling: checked before an approval could matter, so no
        //    approval lifts an agent above its max tier.
        if spec.tier > agent.max_tier {
            return Decision::Refuse {
                reason: RefusalReason::AboveAgentMaxTier {
                    agent: agent.id.clone(),
                    max: agent.max_tier,
                    needed: spec.tier,
                    action: spec.name.clone(),
                },
            };
        }

        // 5. T3 unattended for a non-deep agent needs a human approval in the request.
        if spec.tier == Tier::T3 && ctx.human_approval.is_none() && !agent.registered_deep {
            return Decision::Refuse {
                reason: RefusalReason::TierNeedsApproval {
                    deep: agent.registered_deep,
                },
            };
        }

        // 6. A carried approval must validate: the approver is a person (never an agent),
        //    and — with `separate_approver` — not the agent's owner acting on the agent's
        //    behalf.
        if let Some(approval) = ctx.human_approval.as_ref() {
            if approval.approver.starts_with("agent:") {
                return Decision::Refuse {
                    reason: RefusalReason::ApprovalInvalid {
                        approver: approval.approver.clone(),
                        detail: "the approver is an agent identity; a person must approve"
                            .to_owned(),
                    },
                };
            }
            if self.policy.separate_approver && approval.approver == agent.owner.as_str() {
                return Decision::Refuse {
                    reason: RefusalReason::ApprovalInvalid {
                        approver: approval.approver.clone(),
                        detail: "the agent's owner may not approve the agent's request when \
                                 `separate_approver` is set"
                            .to_owned(),
                    },
                };
            }
        }

        // 7–9. The hard floor, for every agent, deep ones included. A carried approval lets
        //    a plain agent run T3; the floor still binds it, exactly as for a deep agent.
        if spec.is_change() {
            if !ctx.journaled {
                return Decision::Refuse {
                    reason: RefusalReason::HardFloorNotJournaled,
                };
            }
            if !ctx.snapshot_taken {
                return Decision::Refuse {
                    reason: RefusalReason::HardFloorNoSnapshot,
                };
            }
            if let Some(route) = ctx.targets_submitted_financial
                && !route.is_allowed_route()
            {
                return Decision::Refuse {
                    reason: RefusalReason::HardFloorFinancialRoute { declared: route },
                };
            }
        }

        Decision::Allow {
            reason: AllowReason {
                tier: spec.tier,
                ran_on_approval: spec.tier == Tier::T3 && ctx.human_approval.is_some(),
            },
        }
    }
}
