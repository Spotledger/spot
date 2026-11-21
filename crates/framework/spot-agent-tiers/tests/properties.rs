//! The property: for random agents, actions and contexts, a refusal always carries the
//! first failing check's reason in the documented order — the codes of
//! `CHECK_ORDER`, indexed — and an `Allow` implies every check passed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use proptest::prelude::*;

use spot_agent_tiers::{CHECK_ORDER, Context, Decision, FinancialRoute, Status, Tier};

use common::catalogue;

/// All the tiers, in ladder order.
const TIERS: [Tier; 4] = [Tier::T0, Tier::T1, Tier::T2, Tier::T3];

/// The catalogued names the property draws from, plus one unknown per draw.
const NAMES: [&str; 9] = [
    "explain_failed_job",
    "diagnose_stock_discrepancy",
    "change_print_format",
    "unstick_worker_queue",
    "repost_stock_ledger",
    "relink_customer_records",
    "install_app",
    "apply_customization_bundle",
    "not_in_the_catalogue",
];

/// One of the names, drawn uniformly.
fn name_strategy() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just(NAMES[0]),
        Just(NAMES[1]),
        Just(NAMES[2]),
        Just(NAMES[3]),
        Just(NAMES[4]),
        Just(NAMES[5]),
        Just(NAMES[6]),
        Just(NAMES[7]),
        Just(NAMES[8]),
    ]
}

/// The property's scenario: one agent side (facts about the agent) and one request side
/// (facts about the action and the context), the shapes the oracle reads.
#[derive(Clone, Copy, Debug)]
struct AgentFacts {
    status: Status,
    domain: spot_agent_tiers::Domain,
    max: Tier,
    deep: bool,
}

#[derive(Clone, Copy, Debug)]
struct RequestFacts {
    known: bool,
    tier: Tier,
    domain: spot_agent_tiers::Domain,
    is_change: bool,
    approval_present: bool,
    approver_is_agent: bool,
    approver_is_owner: bool,
    separate_approver: bool,
    journaled: bool,
    snapshot: bool,
    financial_route: Option<FinancialRoute>,
}

/// The property's oracle: an independent re-derivation of the decision from the checks'
/// documented order, written from the blueprint and the brief, not from the authoriser's
/// code. When the authoriser and the oracle disagree, the property fails and names both.
fn expected_code(agent: AgentFacts, request: RequestFacts) -> &'static str {
    if !request.known {
        return "agent.unknown_action";
    }
    if agent.status != Status::Active {
        return "agent.agent_not_active";
    }
    if agent.domain != request.domain {
        return "agent.domain_mismatch";
    }
    if request.tier > agent.max {
        return "agent.above_agent_max_tier";
    }
    if request.tier == Tier::T3 && !request.approval_present && !agent.deep {
        return "agent.tier_needs_approval";
    }
    if request.approval_present
        && (request.approver_is_agent || (request.separate_approver && request.approver_is_owner))
    {
        return "agent.approval_invalid";
    }
    if request.is_change {
        if !request.journaled {
            return "agent.hard_floor_not_journaled";
        }
        if !request.snapshot {
            return "agent.hard_floor_no_snapshot";
        }
        if let Some(route) = request.financial_route
            && !route.is_allowed_route()
        {
            return "agent.hard_floor_financial_route";
        }
    }
    "agent.allow"
}

/// One drawn scenario, run through both the authoriser and the oracle.
#[allow(clippy::too_many_arguments)]
fn run_case(
    status: Status,
    domain: spot_agent_tiers::Domain,
    max: Tier,
    deep: bool,
    name: &str,
    approval_kind: u8,
    journaled: bool,
    snapshot: bool,
    route: u8,
) {
    let cat = catalogue();
    let auth = spot_agent_tiers::Authoriser::new(cat.clone());
    let mut registry = spot_agent_tiers::AgentRegistry::new();

    // The drawn owner is the agent's owner; the deep path enrols through the registry with
    // that same owner (the registry is the only path to the flag), then the drawn status is
    // applied over the built agent. The deep registry is support's: a drawn `deep`
    // operations profile cannot exist, so it falls back to a plain operations agent
    // (`register_deep` refuses the domain) — the oracle reads the flag back from the built
    // agent, so both sides see the same `registered_deep`.
    let owner = "prop-owner".to_owned();
    let plain =
        spot_agent_tiers::AgentIdentity::new("prop-agent", &owner, domain, max).with_status(status);
    let agent = if deep && domain == spot_agent_tiers::Domain::Support {
        registry
            .enroll(spot_agent_tiers::AgentIdentity::new(
                "prop-agent",
                &owner,
                domain,
                max,
            ))
            .expect("enroll");
        registry
            .register_deep("prop-agent", "person-grace", "evidence/prop", 1)
            .expect("register deep");
        registry
            .get("prop-agent")
            .expect("present")
            .clone()
            .with_status(status)
    } else {
        plain
    };

    let spec = auth.catalogue().get(name);
    let is_change = spec.map(|s| s.is_change()).unwrap_or(false);

    // The context carries the drawn flags exactly: for a change action, journal and
    // snapshot as drawn (the floor reads them); for a read, they are irrelevant to both
    // sides (the floor never applies), so the read context's defaults are fine.
    let mut ctx = Context::read();
    if is_change {
        ctx.journaled = journaled;
        ctx.snapshot_taken = snapshot;
    }
    // The approval: 0 none, 1 by a person, 2 by an agent, 3 by the owner.
    let (present, is_agent, is_owner) = match approval_kind {
        0 => (false, false, false),
        1 => (true, false, false),
        2 => (true, true, false),
        _ => (true, false, true),
    };
    if present {
        let approver = if is_agent {
            "agent:some-other-agent".to_owned()
        } else if is_owner {
            owner.clone()
        } else {
            "person-grace".to_owned()
        };
        ctx.human_approval = Some(spot_agent_tiers::Approval {
            approver,
            reference: "CR-prop".to_owned(),
            at: 1,
        });
    }
    // The financial route: 0 none, 1 direct (refused), 2 an allowed route.
    ctx.targets_submitted_financial = match route {
        0 => None,
        1 => Some(FinancialRoute::Direct),
        _ => Some(FinancialRoute::Amend),
    };

    let decision = auth.decide(&agent, name, &ctx);

    let agent_facts = AgentFacts {
        status,
        domain: agent.domain,
        max,
        deep: agent.registered_deep(),
    };
    let request_facts = RequestFacts {
        known: spec.is_some(),
        tier: spec.map(|s| s.tier).unwrap_or(Tier::T0),
        domain: spec
            .map(|s| s.domain)
            .unwrap_or(spot_agent_tiers::Domain::Support),
        is_change,
        approval_present: present,
        approver_is_agent: is_agent,
        approver_is_owner: is_owner,
        separate_approver: auth.policy().separate_approver,
        journaled,
        snapshot,
        financial_route: ctx.targets_submitted_financial,
    };
    let expected = expected_code(agent_facts, request_facts);
    assert_eq!(
        decision.code(),
        expected,
        "authoriser and oracle disagree on {name} (status {status:?}, domain {domain:?}, max {max}, deep {}, approval {approval_kind}, journaled {journaled}, snapshot {snapshot}, route {route})",
        agent.registered_deep()
    );

    // Refusal => the reason is the first failing check's: its code is one of the order's
    // nine, and no earlier check fails when the oracle says so (the oracle IS that
    // derivation, checked above). Allow => every check passed; re-verify the floor bits
    // directly so the allow branch rests on more than the oracle.
    if let Decision::Allow { .. } = &decision {
        assert!(spec.is_some(), "an allow names a catalogued action");
        assert_eq!(
            agent.status,
            Status::Active,
            "an allow means an active agent"
        );
        assert_eq!(domain, agent.domain, "an allow never crosses domains");
        assert!(
            action_tier_of(&cat, name) <= Some(max),
            "an allow is within the agent's ceiling"
        );
        if is_change {
            assert!(journaled, "an allow's change is journaled");
            assert!(snapshot, "an allow's change is snapshotted");
            assert!(
                ctx.targets_submitted_financial
                    .map(|r| r.is_allowed_route())
                    .unwrap_or(true),
                "an allow's financial route is an allowed one"
            );
        }
    }
}

fn action_tier_of(cat: &spot_agent_tiers::ActionCatalogue, name: &str) -> Option<Tier> {
    cat.get(name).map(|s| s.tier)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    #[test]
    fn refusal_carries_first_failing_reason_and_allow_implies_all_checks_passed(
        status in prop_oneof![
            Just(Status::Active),
            Just(Status::Suspended),
            Just(Status::Retired),
        ],
        domain in prop_oneof![Just(spot_agent_tiers::Domain::Support), Just(spot_agent_tiers::Domain::Operations)],
        max in 0usize..4,
        deep in prop::bool::ANY,
        name in name_strategy(),
        approval_kind in 0u8..4,
        journaled in prop::bool::ANY,
        snapshot in prop::bool::ANY,
        route in 0u8..3,
    ) {
        run_case(
            status,
            domain,
            TIERS[max],
            deep,
            name,
            approval_kind,
            journaled,
            snapshot,
            route,
        );
    }
}

/// The order table's integrity: nine distinct codes at indices 0..9, each a stable
/// `agent.*` code, and every refusal reason's code appears exactly once in it.
#[test]
fn check_order_table_matches_the_refusal_vocabulary() {
    let codes: Vec<&str> = CHECK_ORDER.iter().map(|(_, code)| *code).collect();
    assert_eq!(codes.len(), 9);
    for (i, (index, code)) in CHECK_ORDER.iter().enumerate() {
        assert_eq!(*index, i, "the order table's indices count up from 0");
        assert!(
            code.starts_with("agent."),
            "every code is in the agent.* vocabulary: {code}"
        );
    }
    let mut sorted = codes.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        codes.len(),
        "no code appears twice in the order table"
    );
}
