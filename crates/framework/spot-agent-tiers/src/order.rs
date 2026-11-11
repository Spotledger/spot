//! The decision order, kept in one place.
//!
//! [`Authoriser::decide`](crate::Authoriser::decide) runs nine checks, first failing check
//! wins; this module is the single home of that order so the code, the docs and the tests
//! can never disagree. The order is the one the brief lists:
//!
//! 1. [`UnknownAction`] — the action name is not in the catalogue (refuse, never allow by
//!    default).
//! 2. [`AgentNotActive`] — [`Status::Active`](crate::identity::Status) only.
//! 3. [`DomainMismatch`] — support never operations, operations never support.
//! 4. [`AboveAgentMaxTier`] — the agent's own ceiling, checked before an approval matters.
//! 5. [`TierNeedsApproval`] — T3 unattended for a non-deep agent.
//! 6. [`ApprovalInvalid`] — a carried approval that does not validate.
//! 7. [`HardFloorNotJournaled`] — a change (effect above [`Read`](crate::action::Effect::Read))
//!    with `journaled` false.
//! 8. [`HardFloorNoSnapshot`] — a change with `snapshot_taken` false.
//! 9. [`HardFloorFinancialRoute`] — a change touching a submitted financial record must go
//!    through cancel, amend or a journal entry, never a direct edit.
//!
//! [`UnknownAction`]: crate::authoriser::RefusalReason::UnknownAction
//! [`AgentNotActive`]: crate::authoriser::RefusalReason::AgentNotActive
//! [`DomainMismatch`]: crate::authoriser::RefusalReason::DomainMismatch
//! [`AboveAgentMaxTier`]: crate::authoriser::RefusalReason::AboveAgentMaxTier
//! [`TierNeedsApproval`]: crate::authoriser::RefusalReason::TierNeedsApproval
//! [`ApprovalInvalid`]: crate::authoriser::RefusalReason::ApprovalInvalid
//! [`HardFloorNotJournaled`]: crate::authoriser::RefusalReason::HardFloorNotJournaled
//! [`HardFloorNoSnapshot`]: crate::authoriser::RefusalReason::HardFloorNoSnapshot
//! [`HardFloorFinancialRoute`]: crate::authoriser::RefusalReason::HardFloorFinancialRoute

/// The nine checks in their decision order: `(index, code)`.
///
/// The proptest in the integration suite locks each pair together — if a check's position
/// and its code ever drift apart, the property fails — so any future reorder happens here,
/// once, with the property as the referee.
pub const CHECK_ORDER: &[(usize, &str)] = &[
    (0, "agent.unknown_action"),
    (1, "agent.agent_not_active"),
    (2, "agent.domain_mismatch"),
    (3, "agent.above_agent_max_tier"),
    (4, "agent.tier_needs_approval"),
    (5, "agent.approval_invalid"),
    (6, "agent.hard_floor_not_journaled"),
    (7, "agent.hard_floor_no_snapshot"),
    (8, "agent.hard_floor_financial_route"),
];
