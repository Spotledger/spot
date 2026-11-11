//! The brief's scenario suite: the tiers with normal and deep agents, the domain
//! separation, the agent's own ceiling, inactive agents, the hard floor (each reason, for a
//! deep agent too), the unknown action, the approval rules, and the order documented.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_agent_tiers::{
    Approval, Authoriser, Context, Decision, Domain, FinancialRoute, Policy, RefusalReason, Status,
    Tier,
};

use common::{OWNER, PERSON, approval_by, catalogue, deep_agent, operations_agent, support_agent};

const NOW: i64 = 1_700_000_000;

fn refuse_code(decision: &Decision) -> &'static str {
    match decision {
        Decision::Refuse { reason } => reason.code(),
        Decision::Allow { .. } => panic!("expected a refusal, got allow"),
    }
}

fn allow(decision: Decision) {
    match decision {
        Decision::Allow { .. } => {}
        Decision::Refuse { reason } => panic!("expected allow, refused: {reason}"),
    }
}

#[test]
fn unknown_action_is_refused_and_never_allowed() {
    let auth = Authoriser::new(catalogue());
    let agent = support_agent("agent-t0", Tier::T3);
    let decision = auth.decide(&agent, "no_such_action", &Context::read());
    assert_eq!(
        refuse_code(&decision),
        "agent.unknown_action",
        "an unknown action is refused, never allowed by default"
    );
}

#[test]
fn each_tier_with_a_normal_agent() {
    let auth = Authoriser::new(catalogue());
    // A max-T3 normal support agent, over the tier ladder:
    let agent = support_agent("agent-normal", Tier::T3);

    // A support T0 read: allowed.
    allow(auth.decide(&agent, "explain_failed_job", &Context::read()));
    // A support T1 reversible change: allowed under the clean change context.
    allow(auth.decide(&agent, "change_print_format", &Context::change()));
    // A support T2 data repair: allowed under the clean change context.
    allow(auth.decide(&agent, "relink_customer_records", &Context::change()));
    // A support T3 structural action without approval: refused — tier_needs_approval (the
    // approval requirement is check 5, so it speaks before the floor's journal check).
    let decision = auth.decide(&agent, "install_app", &Context::read());
    assert_eq!(refuse_code(&decision), "agent.tier_needs_approval");
    // ...allowed with a valid human approval:
    let ctx = Context {
        human_approval: Some(approval_by("CR-204")),
        ..Context::change()
    };
    allow(auth.decide(&agent, "install_app", &ctx));
    // ...allowed unattended for a registered deep agent (the deep agent's own registry) —
    // within the hard floor, like every change: journaled and snapshotted.
    let deep = deep_agent("agent-deep", Tier::T3);
    allow(auth.decide(&deep, "install_app", &Context::change()));
}

#[test]
fn support_agent_refused_operations_action_and_reverse() {
    let auth = Authoriser::new(catalogue());
    let support = support_agent("agent-support", Tier::T3);
    let operations = operations_agent("agent-ops", Tier::T3);

    let decision = auth.decide(&support, "repost_stock_ledger", &Context::change());
    assert_eq!(refuse_code(&decision), "agent.domain_mismatch");

    let decision = auth.decide(&operations, "change_print_format", &Context::change());
    assert_eq!(refuse_code(&decision), "agent.domain_mismatch");
}

#[test]
fn agent_above_its_max_tier_refused_even_when_the_action_tier_would_otherwise_be_allowed() {
    let auth = Authoriser::new(catalogue());
    // A T2 agent: a catalogued T3 action refuses on the agent's ceiling, approval or no
    // approval.
    let agent = support_agent("agent-t2", Tier::T2);
    let ctx = Context {
        human_approval: Some(approval_by("CR-77")),
        ..Context::change()
    };
    let decision = auth.decide(&agent, "install_app", &ctx);
    assert_eq!(refuse_code(&decision), "agent.above_agent_max_tier");
}

#[test]
fn suspended_and_retired_agents_refused() {
    let auth = Authoriser::new(catalogue());
    let suspended = support_agent("agent-susp", Tier::T3).with_status(Status::Suspended);
    let retired = support_agent("agent-ret", Tier::T3).with_status(Status::Retired);

    for agent in [&suspended, &retired] {
        let decision = auth.decide(agent, "explain_failed_job", &Context::read());
        assert_eq!(
            refuse_code(&decision),
            "agent.agent_not_active",
            "a non-active agent may not act, however small the action"
        );
        let decision = auth.decide(agent, "change_print_format", &Context::change());
        assert_eq!(refuse_code(&decision), "agent.agent_not_active");
    }
}

#[test]
fn hard_floor_refuses_a_change_without_journal_and_snapshot() {
    let auth = Authoriser::new(catalogue());
    let agent = support_agent("agent-normal", Tier::T3);

    // A T2 change, journal off: refused first on the journal check.
    let decision = auth.decide(&agent, "relink_customer_records", &Context::read());
    assert_eq!(refuse_code(&decision), "agent.hard_floor_not_journaled");

    // Journal on, snapshot missing: refused on the snapshot check.
    let ctx = Context {
        journaled: true,
        snapshot_taken: false,
        ..Context::read()
    };
    let decision = auth.decide(&agent, "relink_customer_records", &ctx);
    assert_eq!(refuse_code(&decision), "agent.hard_floor_no_snapshot");
}

#[test]
fn hard_floor_financial_route_only_through_cancel_amend_journal_entry() {
    let auth = Authoriser::new(catalogue());
    let agent = support_agent("agent-normal", Tier::T3);

    // A direct edit of a submitted financial record: always refused.
    let ctx = Context {
        targets_submitted_financial: Some(FinancialRoute::Direct),
        ..Context::change()
    };
    let decision = auth.decide(&agent, "relink_customer_records", &ctx);
    assert_eq!(refuse_code(&decision), "agent.hard_floor_financial_route");

    // Cancel, amend and journal entry are the allowed routes: each allows.
    for route in [
        FinancialRoute::Cancel,
        FinancialRoute::Amend,
        FinancialRoute::JournalEntry,
    ] {
        let ctx = Context {
            targets_submitted_financial: Some(route),
            ..Context::change()
        };
        allow(auth.decide(&agent, "relink_customer_records", &ctx));
    }
}

#[test]
fn hard_floor_binds_a_registered_deep_agent_for_each_reason() {
    let auth = Authoriser::new(catalogue());
    let deep = deep_agent("agent-deep", Tier::T3);

    // T3 unattended works for deep agents even without an approval — but the floor does
    // not lift. Not journaled:
    let decision = auth.decide(&deep, "install_app", &Context::read());
    assert_eq!(refuse_code(&decision), "agent.hard_floor_not_journaled");
    // journaled but no snapshot:
    let ctx = Context {
        journaled: true,
        snapshot_taken: false,
        ..Context::read()
    };
    let decision = auth.decide(&deep, "install_app", &ctx);
    assert_eq!(refuse_code(&decision), "agent.hard_floor_no_snapshot");
    // and a direct financial edit:
    let ctx = Context {
        targets_submitted_financial: Some(FinancialRoute::Direct),
        ..Context::change()
    };
    let decision = auth.decide(&deep, "install_app", &ctx);
    assert_eq!(refuse_code(&decision), "agent.hard_floor_financial_route");

    // For contrast: the same deep agent, clean change context — allowed.
    allow(auth.decide(&deep, "install_app", &Context::change()));
}

#[test]
fn order_is_documented_and_first_failing_check_wins() {
    let auth = Authoriser::new(catalogue());

    // Unknown action beats an inactive agent (check 1 before check 2).
    let decision = auth.decide(
        &support_agent("agent-x", Tier::T3).with_status(Status::Suspended),
        "mystery_move",
        &Context::read(),
    );
    assert_eq!(refuse_code(&decision), "agent.unknown_action");

    // Inactive beats a domain mismatch (2 before 3).
    let decision = auth.decide(
        &operations_agent("agent-y", Tier::T3).with_status(Status::Suspended),
        "change_print_format",
        &Context::change(),
    );
    assert_eq!(refuse_code(&decision), "agent.agent_not_active");

    // Domain mismatch beats the agent's ceiling (3 before 4): a T1 operations agent against
    // a support T3 action.
    let decision = auth.decide(
        &operations_agent("agent-ops", Tier::T1),
        "install_app",
        &Context::read(),
    );
    assert_eq!(refuse_code(&decision), "agent.domain_mismatch");

    // The ceiling beats the approval requirement (4 before 5): a T2 agent against a T3
    // action without approval says above_max_tier, not tier_needs_approval.
    let decision = auth.decide(
        &support_agent("agent-t2", Tier::T2),
        "install_app",
        &Context::read(),
    );
    assert_eq!(refuse_code(&decision), "agent.above_agent_max_tier");

    // The approval requirement beats an invalid approval (5 before 6): there is no approval
    // to validate at all.
    let decision = auth.decide(
        &support_agent("agent-normal", Tier::T3),
        "install_app",
        &Context::read(),
    );
    assert_eq!(refuse_code(&decision), "agent.tier_needs_approval");

    // The approval check beats the hard floor's journal check (6 before 7): the invalid
    // approval is named, not the journal.
    let ctx = Context {
        human_approval: Some(Approval {
            approver: "agent:other-agent".to_owned(),
            reference: "CR-5".to_owned(),
            at: NOW,
        }),
        ..Context::read()
    };
    let decision = auth.decide(
        &support_agent("agent-normal", Tier::T3),
        "relink_customer_records",
        &ctx,
    );
    assert_eq!(refuse_code(&decision), "agent.approval_invalid");

    // The hard floor's journal check beats its snapshot check (7 before 8).
    let ctx = Context {
        journaled: false,
        snapshot_taken: false,
        ..Context::read()
    };
    let decision = auth.decide(
        &support_agent("agent-normal", Tier::T3),
        "relink_customer_records",
        &ctx,
    );
    assert_eq!(refuse_code(&decision), "agent.hard_floor_not_journaled");

    // The snapshot check beats the financial-route check (8 before 9): journaled, so check
    // 7 passes; no snapshot, so it speaks before the route check can.
    let ctx = Context {
        human_approval: None,
        journaled: true,
        snapshot_taken: false,
        targets_submitted_financial: Some(FinancialRoute::Direct),
    };
    let decision = auth.decide(
        &support_agent("agent-normal", Tier::T3),
        "relink_customer_records",
        &ctx,
    );
    assert_eq!(refuse_code(&decision), "agent.hard_floor_no_snapshot");
}

#[test]
fn an_approval_by_an_agent_identity_is_invalid() {
    let auth = Authoriser::new(catalogue());
    let agent = support_agent("agent-normal", Tier::T3);
    let ctx = Context {
        human_approval: Some(Approval {
            approver: "agent:other-agent".to_owned(),
            reference: "CR-9".to_owned(),
            at: NOW,
        }),
        ..Context::change()
    };
    let decision = auth.decide(&agent, "install_app", &ctx);
    assert_eq!(refuse_code(&decision), "agent.approval_invalid");
}

#[test]
fn the_owners_approval_is_refused_when_the_policy_separates_the_approver() {
    let auth = Authoriser::new(catalogue());
    let agent = support_agent("agent-normal", Tier::T3);
    // The agent's owner tries to bless its own agent's T3 request.
    let ctx = Context {
        human_approval: Some(Approval {
            approver: OWNER.to_owned(),
            reference: "CR-10".to_owned(),
            at: NOW,
        }),
        ..Context::change()
    };
    let decision = auth.decide(&agent, "install_app", &ctx);
    assert_eq!(refuse_code(&decision), "agent.approval_invalid");

    // Without the separation policy, the owner's approval is fine.
    let permissive = Authoriser::with_policy(
        catalogue(),
        Policy {
            separate_approver: false,
        },
    );
    allow(permissive.decide(&agent, "install_app", &ctx));
}

#[test]
fn tier_unattended_allowed_matches_the_blueprint_table() {
    assert!(Tier::T0.unattended_allowed(false));
    assert!(Tier::T1.unattended_allowed(false));
    assert!(Tier::T2.unattended_allowed(false));
    assert!(!Tier::T3.unattended_allowed(false));
    assert!(Tier::T3.unattended_allowed(true));
}

#[test]
fn an_operations_agent_runs_its_own_domain_end_to_end() {
    let auth = Authoriser::new(catalogue());
    let agent = operations_agent("agent-ops", Tier::T3);
    allow(auth.decide(&agent, "diagnose_stock_discrepancy", &Context::read()));
    allow(auth.decide(&agent, "unstick_worker_queue", &Context::change()));
    allow(auth.decide(&agent, "repost_stock_ledger", &Context::change()));

    // A T3 structural action unattended: operations agents cannot be registered deep (the
    // deep registry is support's), so the approval path is theirs.
    let decision = auth.decide(&agent, "apply_customization_bundle", &Context::read());
    assert_eq!(refuse_code(&decision), "agent.tier_needs_approval");
    let ctx = Context {
        human_approval: Some(approval_by("CR-31")),
        ..Context::change()
    };
    allow(auth.decide(&agent, "apply_customization_bundle", &ctx));
}

#[test]
fn the_registry_refuses_to_register_a_non_support_agent_deep() {
    let mut registry = spot_agent_tiers::AgentRegistry::new();
    registry
        .enroll(operations_agent("agent-ops", Tier::T3))
        .expect("enroll plain");
    let event = registry.register_deep("agent-ops", PERSON, "evidence/x", NOW);
    assert!(
        event.is_err(),
        "a non-support agent cannot be registered deep"
    );
    assert!(
        !registry
            .get("agent-ops")
            .expect("enrolled")
            .registered_deep()
    );
}

#[test]
fn the_registry_produces_events_and_the_flag_is_registry_only() {
    let mut registry = spot_agent_tiers::AgentRegistry::new();
    registry
        .enroll(support_agent("agent-deep2", Tier::T3))
        .expect("enroll");

    let event = registry
        .register_deep("agent-deep2", PERSON, "evidence/case-7", NOW)
        .expect("register");
    assert_eq!(event.event, "agent.deep_registered");
    assert_eq!(event.agent_id, "agent-deep2");
    assert_eq!(event.by, PERSON);
    assert_eq!(event.evidence_ref, "evidence/case-7");
    assert_eq!(event.at, NOW);
    assert!(
        registry
            .get("agent-deep2")
            .expect("present")
            .registered_deep()
    );

    let revoke = registry
        .revoke_deep("agent-deep2", NOW + 1)
        .expect("revoke");
    assert_eq!(revoke.event, "agent.deep_revoked");
    assert!(
        !registry
            .get("agent-deep2")
            .expect("present")
            .registered_deep()
    );

    // A second revoke: refused — nothing left to revoke.
    assert!(registry.revoke_deep("agent-deep2", NOW + 2).is_err());
}

#[test]
fn a_deep_agent_survives_clones_and_reads_suspension_alike() {
    let deep = deep_agent("agent-keep", Tier::T3);
    assert!(deep.registered_deep());
    assert!(deep.clone().registered_deep());

    let suspended_deep = deep.clone().with_status(Status::Suspended);
    let auth = Authoriser::new(catalogue());
    let decision = auth.decide(&suspended_deep, "install_app", &Context::change());
    assert_eq!(
        refuse_code(&decision),
        "agent.agent_not_active",
        "suspension binds deep agents too"
    );
}

#[test]
fn refusal_reasons_carry_their_stable_codes() {
    let reasons = [
        (
            RefusalReason::UnknownAction {
                action: "x".to_owned(),
            },
            "agent.unknown_action",
        ),
        (
            RefusalReason::AgentNotActive {
                agent: "a".to_owned(),
                status: spot_agent_tiers::Status::Suspended,
            },
            "agent.agent_not_active",
        ),
        (
            RefusalReason::DomainMismatch {
                agent: "a".to_owned(),
                agent_domain: spot_agent_tiers::Domain::Support,
                action_domain: spot_agent_tiers::Domain::Operations,
                action: "x".to_owned(),
            },
            "agent.domain_mismatch",
        ),
        (
            RefusalReason::AboveAgentMaxTier {
                agent: "a".to_owned(),
                max: Tier::T1,
                needed: Tier::T2,
                action: "x".to_owned(),
            },
            "agent.above_agent_max_tier",
        ),
        (
            RefusalReason::TierNeedsApproval { deep: false },
            "agent.tier_needs_approval",
        ),
        (
            RefusalReason::ApprovalInvalid {
                approver: "p".to_owned(),
                detail: "d".to_owned(),
            },
            "agent.approval_invalid",
        ),
        (
            RefusalReason::HardFloorNotJournaled,
            "agent.hard_floor_not_journaled",
        ),
        (
            RefusalReason::HardFloorNoSnapshot,
            "agent.hard_floor_no_snapshot",
        ),
        (
            RefusalReason::HardFloorFinancialRoute {
                declared: FinancialRoute::Direct,
            },
            "agent.hard_floor_financial_route",
        ),
    ];
    for (reason, code) in reasons {
        assert_eq!(reason.code(), code);
    }
}

#[test]
fn every_refusal_reason_sentences_to_something_readable() {
    let reasons = [
        RefusalReason::UnknownAction {
            action: "x".to_owned(),
        },
        RefusalReason::AgentNotActive {
            agent: "a".to_owned(),
            status: Status::Retired,
        },
        RefusalReason::DomainMismatch {
            agent: "a".to_owned(),
            agent_domain: Domain::Support,
            action_domain: Domain::Operations,
            action: "x".to_owned(),
        },
        RefusalReason::AboveAgentMaxTier {
            agent: "a".to_owned(),
            max: Tier::T0,
            needed: Tier::T3,
            action: "x".to_owned(),
        },
        RefusalReason::TierNeedsApproval { deep: true },
        RefusalReason::ApprovalInvalid {
            approver: "p".to_owned(),
            detail: "d".to_owned(),
        },
        RefusalReason::HardFloorNotJournaled,
        RefusalReason::HardFloorNoSnapshot,
        RefusalReason::HardFloorFinancialRoute {
            declared: FinancialRoute::Direct,
        },
    ];
    for reason in reasons {
        let sentence = reason.sentence();
        assert!(sentence.len() > 10, "a refusal explains itself: {sentence}");
    }
}
